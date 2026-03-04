//! Browser automation tool that wraps Pinchtab's REST API.
//!
//! Handles tab lifecycle (acquisition, locking, cleanup) automatically so the
//! LLM never has to manage UUIDs, locks, or race conditions. Each tool call
//! generates a single bash script that is executed via the shell session,
//! batching multiple curl commands into one `shell_exec` invocation.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use types::{FunctionDecl, SafetyTier, Tool, ToolError, ToolExecutionContext};

use crate::{invalid_args, parse_args, sandbox::ShellSession};

/// Tool name exposed to the LLM.
pub const BROWSER_TOOL_NAME: &str = "browser";

/// Default lock timeout in seconds for tab locking.
const LOCK_TIMEOUT_SECS: u32 = 120;

/// Default command timeout for browser operations.  Browser actions involve
/// navigation waits (`sleep 3`) and potentially large snapshot responses, so
/// we allow more headroom than the default shell command timeout.
const BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(90);

/// Maximum number of tabs before the tool refuses to create more.
const MAX_TABS: u32 = 20;

/// Seconds to wait after navigation for the accessibility tree to stabilize.
const NAVIGATE_WAIT_SECS: u32 = 3;

/// Seconds to wait after an action for the page to update.
const ACTION_WAIT_SECS: u32 = 1;

pub struct BrowserTool {
    pinchtab_url: String,
    session: Arc<Mutex<Box<dyn ShellSession>>>,
}

impl BrowserTool {
    pub fn new(pinchtab_url: String, session: Arc<Mutex<Box<dyn ShellSession>>>) -> Self {
        Self {
            pinchtab_url,
            session,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserParams {
    action: String,
    url: Option<String>,
    #[serde(rename = "tabId")]
    tab_id: Option<String>,
    #[serde(rename = "ref")]
    element_ref: Option<String>,
    kind: Option<String>,
    text: Option<String>,
    key: Option<String>,
    value: Option<String>,
    selector: Option<String>,
    diff: Option<bool>,
    #[serde(rename = "waitNav")]
    wait_nav: Option<bool>,
}

#[async_trait]
impl Tool for BrowserTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            BROWSER_TOOL_NAME,
            Some(
                "Control a headless Chrome browser. Automatically acquires/locks a tab, \
                 performs the operation, and releases the lock."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["navigate", "snapshot", "act", "text", "screenshot", "tabs", "health"],
                        "description": "The browser operation to perform"
                    },
                    "url": {
                        "type": "string",
                        "description": "URL to navigate to (for navigate action)"
                    },
                    "tabId": {
                        "type": "string",
                        "description": "Target tab ID. If omitted, auto-acquired."
                    },
                    "ref": {
                        "type": "string",
                        "description": "Element reference to act on (e.g. e5)"
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["click", "type", "press", "fill", "hover", "scroll", "select", "focus"],
                        "description": "Action kind (for act action)"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type or fill"
                    },
                    "key": {
                        "type": "string",
                        "description": "Key to press (for act with kind=press)"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value to select (for act with kind=select)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for snapshot scoping or fill target"
                    },
                    "diff": {
                        "type": "boolean",
                        "description": "Return only changes since last snapshot"
                    },
                    "waitNav": {
                        "type": "boolean",
                        "description": "Wait for navigation after click"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let params: BrowserParams = parse_args(BROWSER_TOOL_NAME, args)?;
        let owner = context
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let script = match params.action.as_str() {
            "navigate" => self.build_navigate_script(&owner, &params)?,
            "snapshot" => self.build_snapshot_script(&owner, &params),
            "act" => self.build_act_script(&owner, &params)?,
            "text" => self.build_text_script(&owner, &params),
            "screenshot" => self.build_screenshot_script(&owner, &params),
            "tabs" => self.build_tabs_script(),
            "health" => self.build_health_script(),
            other => {
                return Err(invalid_args(
                    BROWSER_TOOL_NAME,
                    format!(
                        "unknown action `{other}`; expected one of: \
                         navigate, snapshot, act, text, screenshot, tabs, health"
                    ),
                ));
            }
        };

        crate::execute_with_shell_session(&self.session, &script, self.timeout()).await
    }

    fn timeout(&self) -> Duration {
        BROWSER_COMMAND_TIMEOUT
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Privileged
    }
}

// ── Script Builders ─────────────────────────────────────────────────────────

impl BrowserTool {
    /// Build the preamble that defines BASE, AUTH, and OWNER variables.
    fn script_header(&self, owner: &str) -> String {
        format!(
            r#"#!/bin/sh
set -e
BASE="{base}"
AUTH="Authorization: Bearer $BRIDGE_TOKEN"
OWNER="{owner}"
"#,
            base = self.pinchtab_url,
            owner = shell_escape(owner),
        )
    }

    /// Build tab acquisition + cleanup trap for actions that need a locked tab.
    ///
    /// If `requested_tab` is `Some`, locks that specific tab.  Otherwise finds
    /// an unlocked tab or creates a new one, then locks it.
    fn tab_acquisition_and_cleanup(requested_tab: Option<&str>) -> String {
        let max_tabs = MAX_TABS;
        let lock_timeout = LOCK_TIMEOUT_SECS;

        if let Some(tab_id) = requested_tab {
            // Lock a specific requested tab; still set up cleanup trap.
            format!(
                r#"
TAB="{tab_id}"
curl -sf -X POST "$BASE/tab/lock" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{{"tabId":"'{tab_id}'","owner":"'"$OWNER"'","timeoutSec":{lock_timeout}}}' \
  > /dev/null 2>&1 || true

unlock() {{
  curl -sf -X POST "$BASE/tab/unlock" \
    -H "$AUTH" -H 'Content-Type: application/json' \
    -d '{{"tabId":"'"$TAB"'","owner":"'"$OWNER"'"}}' \
    > /dev/null 2>&1 || true
}}
trap unlock EXIT
"#,
                tab_id = shell_escape(tab_id),
            )
        } else {
            // Auto-acquire: list → find unlocked → create if needed → lock
            format!(
                r#"
TABS_JSON=$(curl -sf "$BASE/tabs" -H "$AUTH")
TAB=$(printf '%s' "$TABS_JSON" | jq -r '.tabs[] | select(.owner == null) | .id' | head -1)

if [ -z "$TAB" ]; then
  TOTAL=$(printf '%s' "$TABS_JSON" | jq '.tabs | length')
  if [ "$TOTAL" -lt {max_tabs} ]; then
    TAB=$(curl -sf -X POST "$BASE/tab" \
      -H "$AUTH" -H 'Content-Type: application/json' \
      -d '{{"action":"new"}}' | jq -r '.tabId')
  else
    echo '{{"error":"all {max_tabs} tabs are locked, retry later"}}'
    exit 0
  fi
fi

LOCK_RESULT=$(curl -sf -o /dev/null -w '%{{http_code}}' -X POST "$BASE/tab/lock" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{{"tabId":"'"$TAB"'","owner":"'"$OWNER"'","timeoutSec":{lock_timeout}}}' \
  2>/dev/null) || true

if [ "$LOCK_RESULT" != "200" ]; then
  TOTAL=$(printf '%s' "$TABS_JSON" | jq '.tabs | length')
  if [ "$TOTAL" -lt {max_tabs} ]; then
    TAB=$(curl -sf -X POST "$BASE/tab" \
      -H "$AUTH" -H 'Content-Type: application/json' \
      -d '{{"action":"new"}}' | jq -r '.tabId')
    curl -sf -X POST "$BASE/tab/lock" \
      -H "$AUTH" -H 'Content-Type: application/json' \
      -d '{{"tabId":"'"$TAB"'","owner":"'"$OWNER"'","timeoutSec":{lock_timeout}}}' \
      > /dev/null 2>&1 || true
  fi
fi

unlock() {{
  curl -sf -X POST "$BASE/tab/unlock" \
    -H "$AUTH" -H 'Content-Type: application/json' \
    -d '{{"tabId":"'"$TAB"'","owner":"'"$OWNER"'"}}' \
    > /dev/null 2>&1 || true
}}
trap unlock EXIT
"#,
            )
        }
    }

    fn build_navigate_script(
        &self,
        owner: &str,
        params: &BrowserParams,
    ) -> Result<String, ToolError> {
        let url = params.url.as_deref().ok_or_else(|| {
            invalid_args(
                BROWSER_TOOL_NAME,
                "navigate action requires a `url` parameter",
            )
        })?;

        let mut script = self.script_header(owner);
        script.push_str(&Self::tab_acquisition_and_cleanup(params.tab_id.as_deref()));

        // Navigate
        script.push_str(&format!(
            r#"
curl -sf -X POST "$BASE/navigate" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{{"url":"{url}","tabId":"'"$TAB"'"}}'

sleep {wait}

echo "---SNAPSHOT---"
curl -sf "$BASE/snapshot?tabId=$TAB&format=compact&filter=interactive&maxTokens=2000" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
"#,
            url = shell_escape(url),
            wait = NAVIGATE_WAIT_SECS,
        ));

        Ok(script)
    }

    fn build_snapshot_script(&self, owner: &str, params: &BrowserParams) -> String {
        let mut script = self.script_header(owner);
        script.push_str(&Self::tab_acquisition_and_cleanup(params.tab_id.as_deref()));

        // Build query params
        let mut query_parts = vec!["format=compact".to_owned(), "filter=interactive".to_owned()];
        if params.diff == Some(true) {
            query_parts.push("diff=true".to_owned());
        }
        if let Some(sel) = &params.selector {
            query_parts.push(format!("selector={}", shell_escape(sel)));
        }
        let query = query_parts.join("&");

        script.push_str(&format!(
            r#"
echo "---SNAPSHOT---"
curl -sf "$BASE/snapshot?tabId=$TAB&{query}" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
"#,
        ));

        script
    }

    fn build_act_script(&self, owner: &str, params: &BrowserParams) -> Result<String, ToolError> {
        let kind = params.kind.as_deref().ok_or_else(|| {
            invalid_args(BROWSER_TOOL_NAME, "act action requires a `kind` parameter")
        })?;

        let mut script = self.script_header(owner);
        script.push_str(&Self::tab_acquisition_and_cleanup(params.tab_id.as_deref()));

        // Renew lock before performing the action
        script.push_str(&format!(
            r#"
curl -sf -X POST "$BASE/tab/lock" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{{"tabId":"'"$TAB"'","owner":"'"$OWNER"'","timeoutSec":{lock_timeout}}}' \
  > /dev/null 2>&1 || true
"#,
            lock_timeout = LOCK_TIMEOUT_SECS,
        ));

        // Build the action JSON body
        let action_body = build_action_body(kind, params);

        script.push_str(&format!(
            r#"
curl -sf -X POST "$BASE/action" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{action_body}'

sleep {wait}

echo "---SNAPSHOT---"
curl -sf "$BASE/snapshot?tabId=$TAB&format=compact&filter=interactive&diff=true" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
"#,
            action_body = action_body,
            wait = ACTION_WAIT_SECS,
        ));

        Ok(script)
    }

    fn build_text_script(&self, owner: &str, params: &BrowserParams) -> String {
        let mut script = self.script_header(owner);
        script.push_str(&Self::tab_acquisition_and_cleanup(params.tab_id.as_deref()));

        script.push_str(
            r#"
curl -sf "$BASE/text?tabId=$TAB" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
"#,
        );

        script
    }

    fn build_screenshot_script(&self, owner: &str, params: &BrowserParams) -> String {
        let mut script = self.script_header(owner);
        script.push_str(&Self::tab_acquisition_and_cleanup(params.tab_id.as_deref()));

        script.push_str(
            r#"
curl -sf "$BASE/screenshot?tabId=$TAB&raw=true" \
  -H "$AUTH" -o /shared/screenshot.png
echo '{"saved":"/shared/screenshot.png"}'
echo ""
echo "---TAB_ID---"
echo "$TAB"
"#,
        );

        script
    }

    fn build_tabs_script(&self) -> String {
        let mut script = format!(
            r#"#!/bin/sh
set -e
BASE="{base}"
AUTH="Authorization: Bearer $BRIDGE_TOKEN"
"#,
            base = self.pinchtab_url,
        );

        script.push_str(
            r#"
curl -sf "$BASE/tabs" -H "$AUTH"
"#,
        );

        script
    }

    fn build_health_script(&self) -> String {
        let mut script = format!(
            r#"#!/bin/sh
set -e
BASE="{base}"
AUTH="Authorization: Bearer $BRIDGE_TOKEN"
"#,
            base = self.pinchtab_url,
        );

        script.push_str(
            r#"
curl -sf "$BASE/health" -H "$AUTH"
"#,
        );

        script
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build the JSON body for an `/action` call from the parsed params.
///
/// The body always includes `kind` and `tabId` (via shell variable).  Extra
/// fields are appended based on the action kind:
///
/// - **click**: optional `waitNav`
/// - **type**: `ref`, `text`
/// - **press**: `key`
/// - **fill**: `text`, optional `selector`
/// - **hover** / **scroll** / **focus**: `ref`
/// - **select**: `ref`, `value`
fn build_action_body(kind: &str, params: &BrowserParams) -> String {
    let mut parts: Vec<String> = vec![
        format!(r#""kind":"{}""#, shell_escape(kind)),
        r#""tabId":"'"$TAB"'""#.to_owned(),
    ];

    // Add ref when present
    if let Some(r) = &params.element_ref {
        parts.push(format!(r#""ref":"{}""#, shell_escape(r)));
    }

    match kind {
        "click" => {
            if params.wait_nav == Some(true) {
                parts.push(r#""waitNav":true"#.to_owned());
            }
        }
        "type" => {
            if let Some(t) = &params.text {
                parts.push(format!(r#""text":"{}""#, shell_escape(t)));
            }
        }
        "press" => {
            if let Some(k) = &params.key {
                parts.push(format!(r#""key":"{}""#, shell_escape(k)));
            }
        }
        "fill" => {
            if let Some(t) = &params.text {
                parts.push(format!(r#""text":"{}""#, shell_escape(t)));
            }
            if let Some(s) = &params.selector {
                parts.push(format!(r#""selector":"{}""#, shell_escape(s)));
            }
        }
        "select" => {
            if let Some(v) = &params.value {
                parts.push(format!(r#""value":"{}""#, shell_escape(v)));
            }
        }
        "scroll" | "hover" | "focus" => {
            // ref is already handled above
        }
        _ => {}
    }

    format!("{{{}}}", parts.join(","))
}

/// Escape a string for safe inclusion inside a single-quoted shell string
/// or inside a JSON value embedded in shell.  We escape characters that
/// could break JSON parsing or shell quoting.
fn shell_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => escaped.push_str(r#"\""#),
            '\\' => escaped.push_str(r"\\"),
            '\n' => escaped.push_str(r"\n"),
            '\r' => escaped.push_str(r"\r"),
            '\t' => escaped.push_str(r"\t"),
            '\'' => escaped.push_str("'\"'\"'"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Script content tests ────────────────────────────────────────────

    fn test_tool() -> BrowserTool {
        // Use a mock session that is never actually called in script-generation
        // tests — we only inspect the generated script string.
        let session: Box<dyn ShellSession> = Box::new(MockShellSession);
        BrowserTool::new(
            "http://127.0.0.1:9867".to_owned(),
            Arc::new(Mutex::new(session)),
        )
    }

    fn default_context() -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: Some("test-session-001".to_owned()),
            ..Default::default()
        }
    }

    fn context_without_session() -> ToolExecutionContext {
        ToolExecutionContext::default()
    }

    #[test]
    fn navigate_script_includes_lock_and_trap_exit() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "navigate".to_owned(),
            url: Some("https://example.com".to_owned()),
            tab_id: None,
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_navigate_script("test-owner", &params).unwrap();

        assert!(script.contains("tab/lock"), "script must lock the tab");
        assert!(
            script.contains("trap unlock EXIT"),
            "script must set trap for cleanup"
        );
        assert!(
            script.contains("tab/unlock"),
            "script must define unlock function"
        );
        assert!(
            script.contains("/navigate"),
            "script must call navigate endpoint"
        );
        assert!(
            script.contains("sleep 3"),
            "script must wait for accessibility tree"
        );
        assert!(
            script.contains("/snapshot"),
            "navigate should auto-snapshot"
        );
        assert!(
            script.contains("format=compact"),
            "snapshot should use compact format"
        );
        assert!(
            script.contains("---TAB_ID---"),
            "script should output tab ID"
        );
    }

    #[test]
    fn act_script_includes_lock_renewal() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc123".to_owned()),
            element_ref: Some("e5".to_owned()),
            kind: Some("click".to_owned()),
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_act_script("test-owner", &params).unwrap();

        // Should have the initial lock from tab_acquisition_and_cleanup AND
        // the renewal lock before the action
        let lock_count = script.matches("tab/lock").count();
        assert!(
            lock_count >= 2,
            "act script should renew the lock before action (found {lock_count} lock calls)"
        );
        assert!(
            script.contains("/action"),
            "script must call action endpoint"
        );
        assert!(
            script.contains("diff=true"),
            "act should auto diff-snapshot"
        );
    }

    #[test]
    fn tabs_script_has_no_locking() {
        let tool = test_tool();
        let script = tool.build_tabs_script();

        assert!(!script.contains("tab/lock"), "tabs should not lock");
        assert!(!script.contains("trap"), "tabs should not set trap");
        assert!(script.contains("/tabs"), "tabs should list tabs");
    }

    #[test]
    fn health_script_has_no_locking() {
        let tool = test_tool();
        let script = tool.build_health_script();

        assert!(!script.contains("tab/lock"), "health should not lock");
        assert!(!script.contains("trap"), "health should not set trap");
        assert!(script.contains("/health"), "health should call health");
    }

    #[test]
    fn owner_derived_from_session_id() {
        let ctx = default_context();
        let owner = ctx
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        assert_eq!(owner, "test-session-001", "owner should be the session_id");
    }

    #[test]
    fn owner_fallback_when_no_session_id() {
        let ctx = context_without_session();
        let owner = ctx
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Should be a valid UUID v4
        assert!(
            uuid::Uuid::parse_str(&owner).is_ok(),
            "fallback owner should be a valid UUID, got: {owner}"
        );
    }

    #[test]
    fn navigate_script_uses_correct_snapshot_params() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "navigate".to_owned(),
            url: Some("https://example.com".to_owned()),
            tab_id: None,
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_navigate_script("owner", &params).unwrap();

        assert!(script.contains("format=compact"));
        assert!(script.contains("filter=interactive"));
        assert!(script.contains("maxTokens=2000"));
    }

    #[test]
    fn navigate_requires_url() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "navigate".to_owned(),
            url: None,
            tab_id: None,
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let err = tool.build_navigate_script("owner", &params).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn act_requires_kind() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: Some("e5".to_owned()),
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let err = tool.build_act_script("owner", &params).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn snapshot_script_includes_diff_when_requested() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "snapshot".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: Some(true),
            wait_nav: None,
        };
        let script = tool.build_snapshot_script("owner", &params);
        assert!(
            script.contains("diff=true"),
            "diff snapshot should pass diff=true"
        );
    }

    #[test]
    fn snapshot_script_includes_selector_when_provided() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "snapshot".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: Some("main".to_owned()),
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_snapshot_script("owner", &params);
        assert!(
            script.contains("selector=main"),
            "should pass selector param"
        );
    }

    #[test]
    fn act_click_with_wait_nav() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: Some("e5".to_owned()),
            kind: Some("click".to_owned()),
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: Some(true),
        };
        let script = tool.build_act_script("owner", &params).unwrap();
        assert!(
            script.contains("waitNav"),
            "click with waitNav should include it"
        );
    }

    #[test]
    fn act_type_includes_text() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: Some("e12".to_owned()),
            kind: Some("type".to_owned()),
            text: Some("hello world".to_owned()),
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_act_script("owner", &params).unwrap();
        assert!(
            script.contains("hello world"),
            "type action should include text"
        );
    }

    #[test]
    fn act_press_includes_key() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: Some("press".to_owned()),
            text: None,
            key: Some("Enter".to_owned()),
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_act_script("owner", &params).unwrap();
        assert!(script.contains("Enter"), "press action should include key");
    }

    #[test]
    fn act_fill_includes_text_and_selector() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: Some("fill".to_owned()),
            text: Some("user@example.com".to_owned()),
            key: None,
            value: None,
            selector: Some("#email".to_owned()),
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_act_script("owner", &params).unwrap();
        assert!(script.contains("user@example.com"));
        assert!(script.contains("#email"));
    }

    #[test]
    fn act_select_includes_value() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "act".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: Some("e10".to_owned()),
            kind: Some("select".to_owned()),
            text: None,
            key: None,
            value: Some("option2".to_owned()),
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_act_script("owner", &params).unwrap();
        assert!(
            script.contains("option2"),
            "select action should include value"
        );
    }

    #[test]
    fn text_script_calls_text_endpoint() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "text".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_text_script("owner", &params);
        assert!(script.contains("/text?tabId="));
    }

    #[test]
    fn screenshot_script_saves_to_shared() {
        let tool = test_tool();
        let params = BrowserParams {
            action: "screenshot".to_owned(),
            url: None,
            tab_id: Some("tab_abc".to_owned()),
            element_ref: None,
            kind: None,
            text: None,
            key: None,
            value: None,
            selector: None,
            diff: None,
            wait_nav: None,
        };
        let script = tool.build_screenshot_script("owner", &params);
        assert!(script.contains("/screenshot?tabId="));
        assert!(script.contains("-o /shared/screenshot.png"));
    }

    #[test]
    fn shell_escape_handles_special_chars() {
        assert_eq!(shell_escape(r#"he"llo"#), r#"he\"llo"#);
        assert_eq!(shell_escape("back\\slash"), r"back\\slash");
        assert_eq!(shell_escape("new\nline"), r"new\nline");
    }

    #[test]
    fn schema_has_correct_action_enum() {
        let tool = test_tool();
        let schema = tool.schema();
        assert_eq!(schema.name, BROWSER_TOOL_NAME);
        let actions = schema.parameters["properties"]["action"]["enum"]
            .as_array()
            .expect("action should have enum");
        let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        assert!(action_strs.contains(&"navigate"));
        assert!(action_strs.contains(&"snapshot"));
        assert!(action_strs.contains(&"act"));
        assert!(action_strs.contains(&"text"));
        assert!(action_strs.contains(&"screenshot"));
        assert!(action_strs.contains(&"tabs"));
        assert!(action_strs.contains(&"health"));
    }

    #[test]
    fn auto_acquire_script_checks_tab_count_before_creating() {
        let script = BrowserTool::tab_acquisition_and_cleanup(None);
        assert!(
            script.contains("20"),
            "auto-acquire should check max tab count"
        );
        assert!(
            script.contains("jq"),
            "auto-acquire should use jq for JSON parsing"
        );
    }

    // ── Mock session (only used for script-generation tests) ────────────

    struct MockShellSession;

    #[async_trait]
    impl ShellSession for MockShellSession {
        fn status(&self) -> &crate::sandbox::SessionStatus {
            static STATUS: std::sync::LazyLock<crate::sandbox::SessionStatus> =
                std::sync::LazyLock::new(|| {
                    crate::sandbox::SessionStatus::Ready(
                        crate::sandbox::SessionConnection::LocalProcess,
                    )
                });
            &STATUS
        }

        fn session_id(&self) -> Option<&str> {
            None
        }

        async fn exec_command(
            &mut self,
            _command: &str,
            _timeout_secs: Option<u64>,
        ) -> Result<types::ExecCommandAck, crate::sandbox::SandboxError> {
            Ok(types::ExecCommandAck {
                request_id: String::new(),
                accepted: true,
            })
        }

        async fn stream_output(
            &mut self,
            _max_bytes: Option<usize>,
        ) -> Result<types::StreamOutputChunk, crate::sandbox::SandboxError> {
            Ok(types::StreamOutputChunk {
                request_id: String::new(),
                session_id: String::new(),
                stream: types::ShellOutputStream::Stdout,
                data: String::new(),
                eof: true,
            })
        }

        async fn kill_session(
            &mut self,
        ) -> Result<types::KillSessionAck, crate::sandbox::SandboxError> {
            Ok(types::KillSessionAck {
                request_id: String::new(),
                killed: true,
            })
        }
    }
}
