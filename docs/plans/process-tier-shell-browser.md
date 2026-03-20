# Plan: Process-Tier Host-Local Shell and Browser Support

Status: Proposed
Created: 2026-03-06
Updated: 2026-03-11

## Executive summary

Today, `process` tier hard-disables both shell and browser in multiple places. The current implementation assumes privileged tools are only available when the runner also launches a sidecar guest, and `process` tier does not do that.

This plan describes the **pure host-local** approach: enable shell and browser in process tier using direct host execution and host-local browser/CDP plumbing, with **no sidecar container dependency**.

- **Shell** uses the existing `LocalProcessShellSession` and `run_shell_command()` primitives that already exist in the codebase.
- **Browser** uses a host-local Pinchtab bridge process managing a locally-installed Chromium/Chrome binary, with `BrowserTool` executing commands through a `LocalProcessShellSession` instead of a sidecar shell session.

Shell is shipped first because the primitives already exist. Browser follows as a second phase because it requires host-local binary management, bridge lifecycle, and more OS-specific behavior.

Existing container and microVM tiers are **not affected** by this work. They continue to use the sidecar model unchanged.

## What "restrictive-only overrides" means

The current config model after the recent cleanup is:

- `agent.toml` decides the global defaults:
  - `tools.shell.enabled`
  - `tools.browser.enabled`
- `runner-user.toml` can only apply per-user restrictions:
  - `behavior.shell_enabled`
  - `behavior.browser_enabled`

Effective tool access is conceptually:

```text
effective_shell =
  tier_supports_shell
  AND agent.tools.shell.enabled
  AND user.behavior.shell_enabled.unwrap_or(true)

effective_browser =
  tier_supports_browser
  AND agent.tools.browser.enabled
  AND user.behavior.browser_enabled.unwrap_or(true)
```

So:

- if the agent/global config disables shell/browser, a user cannot force-enable it
- if the sandbox tier disables shell/browser, a user cannot force-enable it
- if the user sets `behavior.shell_enabled = false`, that user loses shell even if it is globally allowed
- if the user sets `behavior.shell_enabled = true`, that only says "do not restrict me further"; it does **not** grant access by itself

That is why these fields are "restrictive-only overrides".

## Current state

### Capability resolution

`crates/runner/src/lib.rs` currently resolves requested capabilities and forces both shell and browser off in `SandboxTier::Process`. This is the first hard gate.

Specifically:
- Line ~66: `PROCESS_TIER_WARNING` constant says "shell/browser tools are disabled"
- Lines ~1642-1657: `resolve_requested_capabilities()` hard-codes Process tier exclusion:
  ```rust
  let mut shell = !matches!(sandbox_tier, SandboxTier::Process) && agent_tools.shell_enabled();
  let mut browser = !matches!(sandbox_tier, SandboxTier::Process) && agent_tools.browser_enabled();
  ```
- Lines ~161-168: `pre_compute_sidecar_endpoint()` is called when capabilities request shell/browser — this pre-computes a sidecar endpoint path that does not apply to host-local shell
- Lines ~171-181: `build_startup_status_report()` is called twice: once pre-launch (optimistic) and once post-launch (actual). It derives `sidecar_available` from whether a sidecar endpoint exists.
- Lines ~202-215: `extra_env` and `shell_env` are set to empty vecs for Process tier; `copy_agent_config_to_workspace_with_paths()` is also skipped
- Line ~230: Browser provisioning (Pinchtab) is skipped with `sandbox_tier != SandboxTier::Process` guard
- Lines ~429-430: `launch_process()` returns `shell_available: false`, `browser_available: false`

### Bootstrap envelope validation (BLOCKING constraint)

`RunnerBootstrapEnvelope::validate()` (`crates/types/src/runner.rs` lines ~712-727) enforces two hard constraints:

1. If `sidecar_endpoint` is `None`, then `sidecar_available`, `shell_available`, and `browser_available` must all be `false`.
2. If `shell_available` or `browser_available` is `true`, then `sidecar_available` must also be `true`.

This means process tier (which always has `sidecar_endpoint: None`) **cannot** report shell or browser as available without this validation failing. This validation must be relaxed for the host-local path. See Phase 1 code changes.

### Backend launch behavior

`crates/runner/src/backend.rs` `launch_process()` (lines ~386-434):

- launches only the host `oxydra-vm` process via `spawn_process_guest_with_startup_stdin()`
- calls `attempt_process_tier_hardening()`
- always adds `InsecureProcessTier` degraded reason
- returns `sidecar_endpoint: None`, `shell_available: false`, `browser_available: false`

### Tool bootstrap behavior

`crates/tools/src/lib.rs` `bootstrap_bash_tool()` (lines ~975-1032) is the actual session creation site:

- if no bootstrap envelope → shell/browser disabled
- if no `sidecar_endpoint` → **early return with all disabled** (including for Process tier: "runner bootstrap indicates process tier; shell/browser tools are disabled")
- if sidecar endpoint exists → connects via `connect_sidecar_bash_tool()` and creates `BashTool` with sidecar-backed `ShellSession`
- `bootstrap_runtime_tools()` in `registry.rs` (lines ~202-285) calls `bootstrap_bash_tool()`, then conditionally registers shell/browser tools based on the returned session statuses

`crates/tools/src/registry.rs` `ToolAvailability::startup_status()` (lines ~178-191):

- derives `sidecar_available` from `shell_available || browser_available` (semantic issue: this would be `true` for host-local shell even though there is no sidecar)
- for Process tier, unconditionally adds `InsecureProcessTier` degraded reason with "shell/browser tools are disabled"

### Runtime registration and UX copy

Several surfaces assume process tier means "no shell/browser":

- `crates/tools/src/registry.rs` startup/degraded reporting
- `crates/runner/src/bootstrap.rs` system prompt: appends "Note: Shell and browser tools are disabled in the current environment."
- README/docs/web copy

### Browser architecture today

Current browser support is a stack, not an isolated CDP client:

- `BrowserTool` struct holds a `pinchtab_url: String` and a `session: Arc<Mutex<Box<dyn ShellSession>>>`
- browser commands execute curl/jq scripts against the Pinchtab HTTP API **through the shell session**
- Pinchtab manages Chromium lifecycle inside the shell-vm image
- `apply_browser_shell_overlay()` adds `curl`, `jq`, `sleep` to the shell allowlist and enables `allow_operators`
- `write_shell_overlay()` mutates the workspace agent config file on disk
- `build_browser_env()` produces env vars like `BRIDGE_PORT`, `BRIDGE_TOKEN`, `CHROME_BINARY`, etc.

Key insight: **`BrowserTool` does not care whether the `ShellSession` is a sidecar RPC session or a `LocalProcessShellSession`**. It just calls `exec_command()` on the trait. This means browser can work with a local shell session if Pinchtab is running locally.

### Process-tier primitives that already exist

`crates/tools/src/sandbox/mod.rs`:

- `LocalProcessShellSession` (lines ~736-835): implements `ShellSession` trait with `exec_command()`, `stream_output()`, `kill_session()`
- `run_shell_command()` (lines ~909-935): direct host shell execution with cwd/env/timeout support, uses `tokio::process::Command`
- `ShellSessionConfig`: holds shell binary, env, cwd, timeouts

`crates/tools/src/sandbox/mod.rs`:

- `attempt_process_tier_hardening()` (lines ~1046-1067): probes OS capabilities (Landlock on Linux, Seatbelt on macOS)
- `ProcessHardeningMechanism` / `ProcessHardeningOutcome` enums

Important limitations in the current code:

- `spawn_process_guest()` / `spawn_process_guest_with_startup_stdin()` do **not** inject `request.extra_env` / `request.shell_env` into the host `oxydra-vm` process
- `write_shell_overlay()` mutates a workspace config file, which is awkward for process tier where there is no container bootstrap path to pick it up
- `LocalProcessShellSession` is not currently wired into the process tier launch path

## Goals

1. Enable shell and browser in process tier using host-local execution only, no container/Docker dependency.
2. Preserve the global-vs-user config model:
   - operator/global defaults in `agent.toml`
   - per-user restrictions in `runner-user.toml`
3. Keep process-tier enablement behind an explicit deployment/operator decision.
4. Preserve stable tool contracts: `BrowserTool` and shell tool keep their existing interfaces.
5. Make status, warnings, onboarding, and web configuration explain the real security/runtime implications.
6. Avoid silent fallbacks or ambiguous partial enablement.
7. Ship shell first, browser second — do not block shell on browser readiness.
8. Do not change behavior of container or microVM tiers.

## Non-goals

1. Making process tier as strong as microVM isolation.
2. Hiding the fact that process-tier shell/browser are materially less isolated.
3. Forcing all users of process tier to adopt shell/browser.
4. Building a new browser automation stack — reuse Pinchtab and `BrowserTool` as-is.
5. Full OS-level sandboxing of host shell commands (best-effort hardening is acceptable for first rollout).

## Chosen architecture: Pure host-local process tier

### Shell design

Shell commands execute directly on the host via `LocalProcessShellSession`, which already implements the `ShellSession` trait.

The integration path:

1. `launch_process()` returns `shell_available: true` when process-tier shell is enabled
2. Tool bootstrap creates a `LocalProcessShellSession` instead of connecting to a sidecar
3. Shell policy (allowlist, denylist, operators, timeout) is applied from the effective config, computed in memory
4. The shell tool registers and works identically to how it works with a sidecar session — the `ShellSession` trait abstraction handles this

What already works:
- `LocalProcessShellSession.exec_command()` runs commands via `run_shell_command()`
- `run_shell_command()` supports cwd, env, timeout, kill-on-drop
- `stream_output()` returns buffered stdout/stderr chunks
- Session lifecycle (ready/unavailable states)

What needs to be added:
- Wiring `LocalProcessShellSession` into the tool bootstrap path when process tier shell is enabled
- Env injection into the host process path
- In-memory effective shell config composition (replacing file-mutation overlay)
- Shell policy enforcement parity with the sidecar path (allowlist/denylist checking)

### Browser design

Browser uses a host-local Pinchtab bridge process managing a locally-installed Chromium/Chrome binary.

The integration path:

1. At startup, if process-tier browser is enabled, launch Pinchtab as a supervised child process on the host
2. Pinchtab manages Chrome/Chromium lifecycle locally (same as it does inside shell-vm, but on the host)
3. Create a `LocalProcessShellSession` with browser shell overlay applied (curl, jq, sleep allowed; operators enabled)
4. Construct `BrowserTool` with the local Pinchtab URL and the local shell session
5. `BrowserTool` works unchanged — it executes curl/jq commands against Pinchtab through the shell session

Pinchtab is an external Go binary (not a Rust crate in the workspace), downloaded from `github.com/pinchtab/pinchtab` releases during the shell-vm Docker image build. It manages Chrome lifecycle itself: it reads the `CHROME_BINARY` env var, spawns that as a subprocess, and connects via CDP. A `chromium-wrapper.sh` script wraps the actual Chrome detection and launch with container-stability flags.

For process tier, the same Pinchtab binary runs on the host. Chrome/Chromium detection is handled by the wrapper script (or Pinchtab itself via `CHROME_BINARY`), not by oxydra.

Browser sub-options:

1. **External CDP only** — require `tools.browser.cdp_url`, Pinchtab connects to an existing external browser. Lowest cost, weakest out-of-box UX.
2. **Host-local Pinchtab + host Chrome/Chromium** — Pinchtab launches and manages a locally-installed Chrome/Chromium via `CHROME_BINARY`. Good out-of-box UX on systems where Chrome is installed.

**Recommendation**: support both. If `cdp_url` is set, use external CDP mode. Otherwise, Pinchtab detects and launches a local Chrome/Chromium (via the wrapper script or `CHROME_BINARY` env var). Fail clearly if neither Pinchtab nor Chrome are available.

### Why this works with the existing tool contract

The key architectural insight is that `BrowserTool` and the shell tool both operate through the `ShellSession` trait (defined in `crates/tools/src/sandbox/mod.rs`):

```rust
pub trait ShellSession: Send {
    async fn exec_command(&mut self, command: &str, timeout_secs: Option<u64>) -> Result<ExecCommandAck, SandboxError>;
    async fn stream_output(&mut self) -> Result<Option<OutputChunk>, SandboxError>;
    async fn kill_session(&mut self) -> Result<(), SandboxError>;
    // ...
}
```

`LocalProcessShellSession` already implements this trait. `BrowserTool` holds an `Arc<Mutex<Box<dyn ShellSession>>>` and calls `exec_command()` to run curl/jq scripts. The tools do not care whether the session is backed by a sidecar RPC connection or a local process — the interface is identical. This means the tool layer requires minimal changes.

## Configuration model

### Runner-global scope

Add to runner global config:

```toml
[process_tier]
shell_enabled = false      # default: disabled
browser_enabled = false    # default: disabled
```

Why runner config (not agent config or user config):

- it is a deployment-wide decision about runtime topology and security posture
- it is not user-scoped
- it decides whether process tier is allowed to realize these tools at all
- it is separate from agent tool defaults (which control policy) and user overrides (which can only restrict)

### Agent scope

Keep global tool defaults in `agent.toml` unchanged:

```toml
[tools.shell]
enabled = true
allow = ["ls", "cat", "grep"]
deny = ["rm -rf /"]
allow_operators = false
command_timeout_secs = 30

[tools.browser]
enabled = true
cdp_url = "..."  # optional: external CDP endpoint
```

### User scope

Keep per-user restrictive overrides in `runner-user.toml` unchanged:

```toml
[behavior]
shell_enabled = false
browser_enabled = false
```

### Effective resolution

```text
effective_shell =
  ((tier == Process AND runner.process_tier.shell_enabled)
    OR (tier != Process AND sidecar_shell_available))
  AND agent.tools.shell.enabled
  AND user.behavior.shell_enabled.unwrap_or(true)

effective_browser =
  ((tier == Process AND runner.process_tier.browser_enabled)
    OR (tier != Process AND sidecar_browser_available))
  AND agent.tools.browser.enabled
  AND user.behavior.browser_enabled.unwrap_or(true)
```

Note the outer parentheses: agent/user config restrictions apply to **all** tiers, including Process.

For non-Process tiers, nothing changes. For Process tier, the runner config gates are the new `tier_supports_shell` / `tier_supports_browser` inputs.

### Why this split is correct

- runner config decides whether process tier is even allowed to realize privileged tools
- agent config decides global tool defaults/policy (allowlist, denylist, operators, timeout, cdp_url)
- user config can only narrow access for that user

This preserves ownership boundaries cleanly.

## Detailed code changes

### Phase 1: Config schema and capability resolution refactor

#### `crates/types/src/runner.rs`

Add process-tier config section:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProcessTierConfig {
    /// Enable shell tool in process tier (host-local execution).
    /// Default: false.
    #[serde(default)]
    pub shell_enabled: bool,
    /// Enable browser tool in process tier (host-local Pinchtab + Chrome).
    /// Default: false.
    #[serde(default)]
    pub browser_enabled: bool,
}
```

Add a `process_tier` field to `RunnerGlobalConfig` (line ~26). Since this struct uses `#[serde(default)]` on fields, existing config files without `[process_tier]` will correctly default to `ProcessTierConfig { shell_enabled: false, browser_enabled: false }`.

**Relax `RunnerBootstrapEnvelope::validate()`** (lines ~712-727):

The current validation rejects `shell_available: true` when `sidecar_endpoint: None`. This must be relaxed for process tier with host-local tools:

```rust
// OLD: blanket rejection when no sidecar endpoint
if self.sidecar_endpoint.is_none()
    && (startup_status.sidecar_available
        || startup_status.shell_available
        || startup_status.browser_available)
{
    return Err(BootstrapEnvelopeError::InvalidField { ... });
}

// NEW: allow shell/browser without sidecar on Process tier
if self.sidecar_endpoint.is_none() && startup_status.sidecar_available {
    return Err(BootstrapEnvelopeError::InvalidField {
        field: "startup_status.sidecar_available",
    });
}
if self.sidecar_endpoint.is_none()
    && self.sandbox_tier != SandboxTier::Process
    && (startup_status.shell_available || startup_status.browser_available)
{
    return Err(BootstrapEnvelopeError::InvalidField {
        field: "startup_status.shell_available",
    });
}
```

Also relax the second constraint (shell/browser requires sidecar_available) for Process tier:

```rust
// OLD: shell/browser requires sidecar
if (startup_status.shell_available || startup_status.browser_available)
    && !startup_status.sidecar_available
{
    return Err(...);
}

// NEW: only enforce on non-Process tiers
if self.sandbox_tier != SandboxTier::Process
    && (startup_status.shell_available || startup_status.browser_available)
    && !startup_status.sidecar_available
{
    return Err(...);
}
```

Add new degraded reason codes:

```rust
pub enum StartupDegradedReasonCode {
    // ... existing variants ...
    ProcessTierShellEnabled,           // shell enabled in process tier (warning, not error)
    ProcessTierBrowserEnabled,         // browser enabled in process tier (warning)
    ProcessTierBrowserUnavailable,     // browser requested but Pinchtab not found or health check failed
}
```

#### `crates/runner/src/lib.rs`

**`resolve_requested_capabilities()` refactor** (lines ~1642-1657):

This function hard-codes Process tier exclusion. It must be updated to consult `ProcessTierConfig`:

```rust
fn resolve_requested_capabilities(
    sandbox_tier: SandboxTier,
    agent_tools: &types::ToolsConfig,
    user_behavior: &types::RunnerBehaviorOverrides,
    process_tier_config: &ProcessTierConfig,  // NEW parameter
) -> RequestedCapabilities {
    let mut shell = match sandbox_tier {
        SandboxTier::Process => process_tier_config.shell_enabled && agent_tools.shell_enabled(),
        _ => agent_tools.shell_enabled(),
    };
    let mut browser = match sandbox_tier {
        SandboxTier::Process => process_tier_config.browser_enabled && agent_tools.browser_enabled(),
        _ => agent_tools.browser_enabled(),
    };

    // User restrictive overrides still apply
    if let Some(enabled) = user_behavior.shell_enabled {
        shell &= enabled;
    }
    if let Some(enabled) = user_behavior.browser_enabled {
        browser &= enabled;
    }
    // ...
}
```

**`pre_compute_sidecar_endpoint()` guard** (lines ~161-168):

Currently, when capabilities request shell/browser, a sidecar endpoint is pre-computed. For Process tier with host-local tools, no sidecar endpoint should be pre-computed:

```rust
let pre_sidecar_endpoint = if (capabilities.shell || capabilities.browser)
    && sandbox_tier != SandboxTier::Process  // NEW: no sidecar for process tier
{
    Some(pre_compute_sidecar_endpoint(sandbox_tier, host_os, &workspace))
} else {
    None
};
```

**`build_startup_status_report()` update** (lines ~1607-1640):

This function needs to handle the case where process tier has shell/browser available without a sidecar:

- For Process tier: `sidecar_available` should be `false` even if shell/browser are available (there is no sidecar)
- Add process-tier-specific degraded reasons when shell/browser are enabled (warnings about reduced isolation)
- The existing `SidecarUnavailable` degraded reason should only be added when sidecar is expected (non-Process tiers)

**Env collection and config copying refactor** (lines ~202-230):

The current guard `if sandbox_tier != SandboxTier::Process` also skips `copy_agent_config_to_workspace_with_paths()`. For process tier with host-local shell, the effective shell config still needs to be available, but the config-file-copy approach may not be needed since we are moving to in-memory config composition. Ensure the effective `ShellConfig` (allowlist, denylist, operators, timeout, env_keys) is passed through to the tool bootstrap path without requiring a file copy.

Remove the blanket guard and replace with:

```rust
let process_tier_shell = sandbox_tier == SandboxTier::Process
    && runner_config.process_tier.shell_enabled
    && capabilities.shell;

let process_tier_browser = sandbox_tier == SandboxTier::Process
    && runner_config.process_tier.browser_enabled
    && capabilities.browser;

// Collect env for all tiers that need it (not just non-Process)
let (mut extra_env, mut shell_env) = if sandbox_tier != SandboxTier::Process
    || process_tier_shell
    || process_tier_browser
{
    // ... existing config env collection logic ...
} else {
    (Vec::new(), Vec::new())
};
```

**Browser provisioning** (line ~230):

Change from:
```rust
if capabilities.browser && sandbox_tier != SandboxTier::Process {
```
to:
```rust
if capabilities.browser && (sandbox_tier != SandboxTier::Process || process_tier_browser) {
```

For process tier browser, the provisioning path differs: instead of injecting env into a sidecar container, it launches Pinchtab as a local child process (see Phase 3).

**Shell overlay refactor**:

Replace `write_shell_overlay()` file mutation with in-memory config composition:

```rust
fn compute_effective_shell_config(
    base: &ShellConfig,
    browser_enabled: bool,
) -> ShellConfig {
    let mut effective = base.clone();
    if browser_enabled {
        apply_browser_shell_overlay(&mut effective);
    }
    effective
}
```

Pass the effective config directly to tool bootstrap instead of writing it to disk and having another component re-read it.

**Update `PROCESS_TIER_WARNING`**:

Make process-tier warning text conditional on what is actually enabled:

```rust
fn process_tier_warning(shell: bool, browser: bool) -> String {
    let tools_note = match (shell, browser) {
        (true, true) => "shell and browser tools run directly on the host with reduced isolation",
        (true, false) => "shell tool runs directly on the host with reduced isolation; browser is disabled",
        (false, true) => "browser tool runs directly on the host with reduced isolation; shell is disabled",
        (false, false) => "shell and browser tools are disabled",
    };
    format!("Process tier: {tools_note}.")
}
```

#### `crates/runner/src/backend.rs`

**`launch_process()` changes** (lines ~386-434):

1. Accept process-tier config to know if shell/browser are enabled
2. Inject `request.extra_env` into the host `oxydra-vm` process spawn (currently missing)
3. Return `shell_available` / `browser_available` based on actual config, not hardcoded `false`
4. Add process-tier-specific degraded reasons/warnings

```rust
pub async fn launch_process(
    request: &SandboxLaunchRequest,
    process_tier_config: &ProcessTierConfig,
) -> Result<SandboxLaunch, RunnerError> {
    // ... existing oxydra-vm spawn ...

    // NEW: inject extra_env into the host process
    // (currently spawn_process_guest_with_startup_stdin does not do this)

    attempt_process_tier_hardening();

    let shell_available = process_tier_config.shell_enabled && request.requested_shell;
    let browser_available = process_tier_config.browser_enabled && request.requested_browser;

    let mut warnings = vec![];
    let mut degraded_reasons = vec![];

    degraded_reasons.push(/* InsecureProcessTier - always */);

    if shell_available {
        degraded_reasons.push(/* ProcessTierShellEnabled warning */);
        warnings.push("Shell commands execute directly on the host without container isolation.");
    }

    if browser_available {
        // Browser availability will be confirmed after Pinchtab launch (Phase 3)
        degraded_reasons.push(/* ProcessTierBrowserEnabled warning */);
        warnings.push("Browser runs directly on the host without container isolation.");
    }

    Ok(SandboxLaunch {
        launch: handle,
        sidecar_endpoint: None,  // Still no sidecar - shell is local
        shell_available,
        browser_available,
        degraded_reasons,
        warnings,
    })
}
```

**Env injection into host process spawn**:

`spawn_process_guest_with_startup_stdin()` needs to merge `request.extra_env` into the child process environment. Currently it does not do this. The fix:

```rust
// In spawn_process_guest_with_startup_stdin or its caller:
for env_pair in &request.extra_env {
    if let Some((key, value)) = env_pair.split_once('=') {
        cmd.env(key, value);
    }
}
```

**Env routing for process tier**:

In the current architecture, `extra_env` goes to `oxydra-vm` and `shell_env` goes to `shell-vm` (sidecar). For process tier:
- `extra_env` → injected into the host `oxydra-vm` process (as above)
- `shell_env` → injected into the `LocalProcessShellSession`'s environment (passed via `ShellSessionConfig.env`)
- For browser, `build_browser_env()` output → injected into the browser-specific `LocalProcessShellSession`'s environment (separate from the main shell session)

This routing must be explicit so that env vars like `BRIDGE_TOKEN`, API keys from `credential_refs`, and `ShellConfig.env_keys` all reach the correct session.

### Phase 2: Host-local shell integration

#### `crates/tools/src/lib.rs`

**`bootstrap_bash_tool()` changes** (lines ~975-1032):

This is the **actual session creation site**. Currently it has an early return when `sidecar_endpoint` is `None` that disables everything for process tier. This must be changed to create a `LocalProcessShellSession` when process tier shell is enabled:

```rust
async fn bootstrap_bash_tool(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> (BashTool, SessionStatus, SessionStatus) {
    let Some(bootstrap) = bootstrap else {
        // ... existing: no bootstrap → disabled
    };

    let Some(endpoint) = bootstrap.sidecar_endpoint.clone() else {
        // Process tier with host-local shell: create LocalProcessShellSession
        if bootstrap.sandbox_tier == SandboxTier::Process {
            let shell_available = bootstrap
                .startup_status.as_ref()
                .map(|s| s.shell_available)
                .unwrap_or(false);
            let browser_available = bootstrap
                .startup_status.as_ref()
                .map(|s| s.browser_available)
                .unwrap_or(false);

            if shell_available || browser_available {
                let config = ShellSessionConfig::default()
                    .with_cwd(PathBuf::from(&bootstrap.workspace_root));
                let session = LocalProcessShellSession::new(config)?;
                // Note: new() returns Result<Self, SandboxError>
                let status = session.status().clone();
                let bash_tool = BashTool::from_shell_session(Box::new(session));

                let shell_status = if shell_available { status.clone() } else {
                    unavailable_status(SessionUnavailableReason::Disabled, "shell disabled")
                };
                let browser_status = if browser_available { status } else {
                    unavailable_status(SessionUnavailableReason::Disabled, "browser disabled")
                };
                return (bash_tool, shell_status, browser_status);
            }
        }

        // Existing: no sidecar and no host-local → disabled
        let detail = if bootstrap.sandbox_tier == SandboxTier::Process {
            "runner bootstrap indicates process tier; shell/browser tools are disabled"
        } else {
            "runner bootstrap did not provide a sidecar endpoint; shell/browser tools are disabled"
        };
        let status = unavailable_status(SessionUnavailableReason::Disabled, detail);
        return (BashTool::from_status(status.clone()), status.clone(), status);
    };

    // ... existing sidecar connection path unchanged ...
}
```

Note: `LocalProcessShellSession::new()` returns `Result<Self, SandboxError>`, not `Self`. Error handling is needed.

#### `crates/tools/src/registry.rs`

**`ToolAvailability::startup_status()` update** (lines ~178-191):

- The `sidecar_available` derivation (`shell_available || browser_available`) must not set `sidecar_available: true` for process tier. Either:
  - Guard it with `sandbox_tier != SandboxTier::Process`, or
  - Introduce a new field `privileged_tools_available` separate from `sidecar_available`
- The unconditional "shell/browser tools are disabled" message for Process tier must be made conditional on actual availability

**Shell policy enforcement**:

`LocalProcessShellSession` needs to enforce the same allowlist/denylist/operator policy that the sidecar oxydra-shelld enforces. This can be done in `exec_command()` before delegating to `run_shell_command()`:

```rust
impl ShellSession for LocalProcessShellSession {
    async fn exec_command(
        &mut self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError> {
        // NEW: validate command against shell policy
        self.policy.validate_command(command)?;

        // Existing: run the command
        let output = run_shell_command(&self.config, command, timeout_secs).await?;
        // ... build response ...
    }
}
```

Note: shell policy data (allowlist, denylist, `allow_operators`) lives in `ShellConfig` from `crates/types/src/config.rs`, **not** in `ShellSessionConfig` (which only holds shell binary, env, cwd, timeouts). Either:
- Add a `policy: ShellPolicy` field to `LocalProcessShellSession`, or
- Extend `ShellSessionConfig` to include policy data, or
- Create a new `ShellPolicy` struct extracted from `ShellConfig`

The `validate_command()` method checks:
- command matches at least one allowlist pattern (if allowlist is non-empty)
- command does not match any denylist pattern
- command does not use shell operators (`&&`, `||`, `|`, `;`) unless `allow_operators` is true

This validation logic may already exist in the sidecar oxydra-shelld. If so, extract it into a shared utility in `crates/tools/src/sandbox/` or `crates/types/`.

#### `crates/tools/src/lib.rs`

Update process-tier disabled messages to be conditional on actual state:

- if process tier + shell enabled → do not show "shell disabled in process tier"
- if process tier + shell disabled → show existing disabled message
- same for browser

#### `crates/runner/src/bootstrap.rs`

**System prompt** (lines ~900-905):

Replace the blanket "Shell and browser tools are disabled" note with backend-aware text:

```rust
let shell_note = match (sandbox_tier, shell_available, browser_available) {
    (SandboxTier::Process, true, true) => {
        "\n\nNote: Shell and browser tools are available but run directly on the host \
         with reduced isolation compared to container/microVM modes. Exercise caution \
         with destructive commands."
    }
    (SandboxTier::Process, true, false) => {
        "\n\nNote: Shell tool is available but runs directly on the host with reduced \
         isolation. Browser tool is disabled in the current environment."
    }
    (SandboxTier::Process, false, true) => {
        "\n\nNote: Browser tool is available but runs directly on the host with reduced \
         isolation. Shell tool is disabled in the current environment."
    }
    (SandboxTier::Process, false, false) => {
        "\n\nNote: Shell and browser tools are disabled in the current environment."
    }
    _ => "",  // Non-process tiers: no special note needed
};
```

### Phase 3: Host-local browser integration

#### Browser lifecycle manager

New module: `crates/tools/src/browser_local.rs` (or extend `crates/tools/src/browser.rs`)

Responsibilities:

1. **Pinchtab binary detection**: find the `pinchtab` binary on the host (or bundled with oxydra)
2. **Pinchtab launch**: start Pinchtab as a supervised child process with the right env vars
3. **Health checking**: poll Pinchtab's `/health` endpoint until responsive (same as `shell-vm-entrypoint.sh` does today)
4. **Cleanup**: kill Pinchtab (and its Chrome subprocess) on shutdown

Chrome/Chromium detection is **not** this module's job — Pinchtab handles that itself via `CHROME_BINARY` env var. On Linux, `chromium-wrapper.sh` does `command -v chromium-browser || command -v chromium`. For process tier, the same wrapper script can be used, or `CHROME_BINARY` can be set directly to the host Chrome path by the operator.

```rust
pub struct LocalBrowserManager {
    pinchtab_process: Option<Child>,
    pinchtab_url: String,
    bridge_token: String,
    state_dir: PathBuf,
}

impl LocalBrowserManager {
    /// Find the Pinchtab binary on the host.
    /// Checks: PINCHTAB_BINARY env var, then PATH, then well-known install paths.
    pub fn detect_pinchtab_binary() -> Result<PathBuf, BrowserSetupError> {
        // 1. PINCHTAB_BINARY env var (explicit override)
        // 2. `which pinchtab` / PATH lookup
        // 3. Known paths: /usr/local/bin/pinchtab, alongside oxydra binary, etc.
    }

    /// Launch Pinchtab as a local child process.
    /// Pinchtab will detect and launch Chrome/Chromium itself via CHROME_BINARY.
    pub async fn launch(
        pinchtab_binary: PathBuf,
        workspace_root: &Path,
        cdp_url: Option<&str>,
    ) -> Result<Self, BrowserSetupError> {
        // Allocate a free port for Pinchtab HTTP API
        // Create state directory: <workspace_root>/.oxydra/pinchtab/
        // Generate BRIDGE_TOKEN (32-byte random hex, same as build_browser_env)
        // Build env vars (same as build_browser_env + shell-vm-entrypoint.sh):
        //   BRIDGE_PORT, BRIDGE_BIND, BRIDGE_TOKEN, BRIDGE_HEADLESS,
        //   BRIDGE_STEALTH, BRIDGE_STATE_DIR, BRIDGE_MAX_TABS, BRIDGE_NO_RESTORE,
        //   CHROME_BINARY (host chromium-wrapper or operator-configured path),
        //   BROWSER_EXTERNAL_CDP_URL (if cdp_url is set)
        // Spawn pinchtab process with kill_on_drop(true)
        // Health-check: poll GET /health with Authorization header until ready or timeout
        // Return manager with pinchtab_url and bridge_token
    }

    /// Create a BrowserToolConfig for constructing BrowserTool.
    pub fn tool_config(&self) -> BrowserToolConfig {
        BrowserToolConfig {
            pinchtab_base_url: self.pinchtab_url.clone(),
            bridge_token: Some(self.bridge_token.clone()),
        }
    }

    /// Shutdown: kill Pinchtab and clean up state.
    pub async fn shutdown(&mut self) {
        if let Some(mut proc) = self.pinchtab_process.take() {
            let _ = proc.kill().await;
        }
        // Clean up state_dir if desired
    }
}
```

**External CDP path**: if `tools.browser.cdp_url` is configured, it is passed to Pinchtab via `BROWSER_EXTERNAL_CDP_URL` env var (same as the existing container path). The `chromium-wrapper.sh` script handles this mode by fetching the DevTools URL from the external browser's HTTP API instead of launching a local Chrome.

#### Integration into launch path

In `crates/runner/src/lib.rs`, after `launch_process()` returns with `browser_available: true`:

```rust
if process_tier_browser {
    let pinchtab_binary = match LocalBrowserManager::detect_pinchtab_binary() {
        Ok(path) => path,
        Err(e) => {
            // Degrade: browser requested but Pinchtab not found
            degraded_reasons.push(ProcessTierBrowserUnavailable(e.to_string()));
            browser_available = false;
            // Continue without browser
        }
    };

    if browser_available {
        let browser_manager = LocalBrowserManager::launch(
            pinchtab_binary,
            &workspace_root,
            agent_tools.browser_cdp_url(),
        ).await?;
        // Pinchtab detects/launches Chrome itself via CHROME_BINARY env var.
        // If Chrome is missing, Pinchtab health check will fail and we degrade.

        // Create browser shell session with overlay applied
        let browser_shell_config = compute_effective_shell_config(
            &shell_config,
            true, // browser_enabled
        );
        // Inject BRIDGE_TOKEN and other browser env vars into the session
        let mut browser_session_config = ShellSessionConfig::from(browser_shell_config);
        browser_session_config.env.insert(
            "BRIDGE_TOKEN".into(), browser_manager.bridge_token.clone()
        );
        let browser_session = LocalProcessShellSession::new(browser_session_config)?;

        // BrowserTool uses the local shell session + local Pinchtab
        browser_tool = Some(BrowserTool::new(
            browser_manager.tool_config(),
            Arc::new(Mutex::new(Box::new(browser_session))),
        ));
    }
}
```

#### Pinchtab binary availability

Pinchtab is a Go binary from `github.com/pinchtab/pinchtab`. Today it is downloaded during the shell-vm Docker image build (`docker/Dockerfile` lines ~46-70) and installed to `/usr/local/bin/pinchtab` inside the container. It is **not** built from source in this repo.

For process tier, Pinchtab must be available on the host. Options:

1. **Bundle with oxydra distribution**: download the Pinchtab release binary during oxydra's build/packaging and ship it alongside the runner binary. Cleanest out-of-box UX.
2. **Separate install**: require operators to install Pinchtab separately (e.g., `curl -L .../pinchtab -o /usr/local/bin/pinchtab`). Document the requirement.
3. **Download on demand**: download at startup if missing. Adds network dependency and versioning complexity.

**Recommendation**: option 1 (bundle) for the best UX. The `chromium-wrapper.sh` script should also be bundled or adapted for host use (it handles Chrome detection, stability flags, and external CDP mode).

#### Chrome/Chromium availability

Chrome/Chromium detection is Pinchtab's responsibility (via `CHROME_BINARY` env var → wrapper script → `command -v chromium-browser || chromium`). For process tier:

- If Chrome is installed on the host, Pinchtab will find and launch it
- If Chrome is missing, Pinchtab will fail to start and the health check will timeout → browser degrades gracefully with a clear error message
- Operators can set `CHROME_BINARY` env var to point to a specific Chrome installation
- The `chromium-wrapper.sh` script may need minor adaptation for host use (e.g., container-specific flags like `--no-sandbox` may not be appropriate on the host)

#### Browser state directory

Current browser uses `/shared/.pinchtab` inside the container (`/shared` is a bind mount to the real workspace `shared/` directory). For process tier:

- use the real resolved `shared` path: `<workspace>/{user_id}/shared/.pinchtab/`
- this is the **same physical directory** that the container tier uses — just referenced by real path instead of container alias
- screenshots go to `<workspace>/{user_id}/shared/screenshot.png`
- Chrome profile, downloads, state stored under `<workspace>/{user_id}/shared/.pinchtab/`
- no new directory structure needed — the workspace `shared/` directory already exists for all tiers

#### Container-virtual paths in `BrowserTool`

`BrowserTool` (`crates/tools/src/browser.rs` line ~405) hardcodes `/shared/screenshot.png`:

```rust
curl -sf "$BASE/screenshot?tabId=$TAB&raw=true" \
  -H "$AUTH" -o /shared/screenshot.png
echo '{"saved":"/shared/screenshot.png"}'
```

In the sidecar shell-vm, `/shared` is a Docker bind mount alias for the real workspace `shared/` directory. In process tier, there is no container mount aliasing — the shell session operates on real host paths.

The runner creates real `shared/`, `tmp/`, `vault/` directories under `<workspace>/{user_id}/` for all tiers. The bootstrap envelope provides the real absolute paths via `RunnerResolvedMountPaths`. For process tier, the screenshot path must use the real resolved `shared` path (e.g., `/path/to/workspace/{user_id}/shared/screenshot.png`) instead of the container alias `/shared/screenshot.png`.

Options:
- Make `BrowserTool` accept a configurable shared directory path and use it in generated scripts
- For sidecar tiers: keep `/shared` (container alias)
- For process tier: use the real resolved `shared` path from `RunnerResolvedMountPaths`

#### `$BRIDGE_TOKEN` env var routing

`BrowserTool` generates shell scripts that reference `$BRIDGE_TOKEN` (line ~186 of browser.rs). For the sidecar path, this env var is set in the sidecar container's environment. For process tier:

- The `LocalProcessShellSession` used by `BrowserTool` must have `BRIDGE_TOKEN` in its env map
- The env for the browser shell session should include all vars from `build_browser_env()` — specifically `BRIDGE_TOKEN`, plus `BRIDGE_PORT`, `PINCHTAB_URL`, etc.
- This is **separate** from the main shell session's env. The browser gets its own `LocalProcessShellSession` with browser-specific env vars injected.

#### Shutdown and cleanup

`LocalBrowserManager::shutdown()` must be called on:
- Normal runner shutdown
- SIGTERM/SIGINT signal handling
- Runner process panic (best-effort via `Drop` impl as safety net)

The `Drop` impl should attempt synchronous process kill:

```rust
impl Drop for LocalBrowserManager {
    fn drop(&mut self) {
        if let Some(ref mut proc) = self.pinchtab_process {
            let _ = proc.start_kill(); // best-effort
        }
    }
}
```

Chrome processes spawned by Pinchtab should be killed when Pinchtab exits (Pinchtab already handles this via process group management). If Pinchtab is killed with `kill_on_drop(true)`, its child Chrome processes should also terminate.

### Phase 4: Hardening — jailed shells and OS-level isolation

#### Current state

`attempt_process_tier_hardening()` (`crates/tools/src/sandbox/mod.rs` lines ~1046-1110) **only probes** whether Landlock (Linux) or Seatbelt (macOS) support exists. It does not apply any actual restrictions. The probe checks:
- Linux: existence of `/sys/kernel/security/landlock`
- macOS: existence of `/usr/bin/sandbox-exec`

The existing WASM sandbox (`WasmWasiToolRunner`) already provides hardware-enforced filesystem boundaries for file/web tools via wasmtime + WASI preopened directories. Shell commands bypass this entirely — they run as the host user with full access. This is the main gap.

#### Strategy: jailed shell execution via `nono`

The goal is to run shell commands inside a restricted execution environment that limits filesystem access, network access, and available syscalls. Rather than implementing Landlock (Linux) and Seatbelt (macOS) wrappers separately, use the **[nono](https://nono.sh/)** crate — a Rust library that provides a unified cross-platform sandbox API backed by OS-native kernel enforcement.

#### Why nono

nono (`nono` crate on crates.io, Apache 2.0 license) provides:

- **Linux**: Landlock filesystem restrictions (kernel 5.13+) + seccomp BPF for syscall interception
- **macOS**: Seatbelt kernel isolation
- **Unified Rust API**: `CapabilitySet` + `Sandbox::apply()` abstracts away platform differences
- **Library, not just CLI**: can be embedded directly in `run_shell_command()` via `pre_exec` hook
- **Irreversible once applied**: sandbox restrictions cannot be removed by the sandboxed process
- **Inherited by children**: shell commands and anything they spawn are also restricted
- **Supervisor-mediated expansion**: seccomp BPF on Linux allows a supervisor to mediate capability requests — when an agent needs access outside its sandbox, the supervisor can prompt the user and inject file descriptors without the agent ever executing its own `open()`

This is exactly the Landlock + Seatbelt logic we would otherwise build from scratch, packaged as a well-structured Rust crate with proper platform abstraction.

**Maturity caveat**: nono is early alpha (~910 GitHub stars, "not recommended for production until 1.0", no security audit). However:
- The underlying mechanisms (Landlock, Seatbelt) are mature kernel features
- nono is a thin wrapper, not a complex runtime — the attack surface of the wrapper itself is small
- We were going to implement the same Landlock/Seatbelt logic ourselves, with the same kernel-level trust model
- If nono proves problematic, the migration path to direct `landlock-rs` + manual Seatbelt is straightforward since the concepts are identical

#### How it works

```rust
use nono::{CapabilitySet, Sandbox};

// Build a capability set for shell command execution
let mut caps = CapabilitySet::new();

// Read-write: workspace shared/ and tmp/ directories
caps.allow_read_write(&workspace.shared)?;
caps.allow_read_write(&workspace.tmp)?;

// Read-only: system directories needed for shell commands to work
for path in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
    caps.allow_read(path)?;
}

// Optionally block network access
caps.block_network();

// Apply sandbox — irreversible, inherited by child processes
Sandbox::apply(&caps)?;

// Now exec the shell command — it runs within the sandbox
```

For `run_shell_command()`, apply the sandbox in the `pre_exec` hook of `tokio::process::Command`:

```rust
unsafe {
    cmd.pre_exec(move || {
        let caps = build_shell_capabilities(&workspace_mounts);
        nono::Sandbox::apply(&caps)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });
}
```

This ensures every shell command and its child processes are sandboxed before execution begins.

#### Platform-specific behavior

**Linux** (Landlock + seccomp):
- Filesystem: only explicitly allowed paths are accessible; everything else is denied
- Network: can be blocked entirely or restricted to specific ports (Landlock ABI v4, kernel 6.7+)
- Syscalls: seccomp BPF can intercept and mediate unauthorized access attempts
- No root required, no container runtime required

**macOS** (Seatbelt):
- Filesystem: restricted via Seatbelt kernel profiles
- Network: can be restricted via profile rules
- Process: execution restrictions via profile

**Unsupported platforms** (Windows, older kernels):
- nono will fail to apply sandbox if the kernel doesn't support the required mechanisms
- Graceful fallback: log a warning, proceed without sandbox restrictions, report in startup status

#### Optional: bubblewrap for stronger isolation

For operators who want namespace-level isolation (mount namespace, network namespace) without moving to container tier, bubblewrap (`bwrap`) remains an option as a stronger complement to nono:

```bash
bwrap \
  --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /bin /bin --ro-bind /etc /etc \
  --bind /path/to/shared /path/to/shared \
  --bind /path/to/tmp /path/to/tmp \
  --unshare-net --die-with-parent \
  -- /bin/sh -c "command here"
```

This is heavier than nono (requires bwrap installed, Linux-only) but provides PID/mount/network namespace isolation. Consider as a Phase 4+ enhancement, configurable via:

```toml
[process_tier]
shell_jail = "nono"  # nono (default) | bwrap | none
```

#### Implementation approach

**Phase 4a: Env scrubbing** (simplest, highest impact-to-effort ratio):

- `LocalProcessShellSession` starts with a **clean environment**, not inheriting from the runner process
- Only includes: explicitly configured env keys from `ShellConfig.env_keys`, plus standard system vars (`PATH`, `HOME`, `SHELL`, `TERM`, `USER`, `LANG`, `LC_ALL`)
- For browser session: also includes `BRIDGE_TOKEN` and other browser env vars
- Uses `Command::env_clear()` before setting specific vars
- This prevents accidental exposure of API keys, tokens, and other sensitive vars

**Phase 4b: nono sandbox integration**:

- Add `nono` crate as a dependency
- Build `CapabilitySet` from workspace mount paths at session creation time:
  - read-write: resolved `shared/` and `tmp/` paths
  - read-only: `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/etc`
  - optionally: `vault/` read-only (or deny entirely if vault access should go through the WASM tool)
- Apply sandbox in `pre_exec` hook of `run_shell_command()`
- Graceful fallback: if nono fails to apply (unsupported platform/kernel), log a warning and proceed without sandbox

**Different capability sets for shell vs browser sessions**:

The main shell session and the browser's shell session are separate `LocalProcessShellSession` instances and need different nono sandboxes:

| Capability | Main shell session | Browser shell session |
| --- | --- | --- |
| Filesystem read-write | workspace `shared/`, `tmp/` | workspace `shared/`, `tmp/` (screenshots, state) |
| Filesystem read-only | `/usr`, `/lib`, `/bin`, `/etc` | `/usr`, `/lib`, `/bin`, `/etc` (curl, jq, sleep) |
| Network | **blocked** (or per shell policy) | **allowed** — must reach Pinchtab on localhost |
| Env | clean + `ShellConfig.env_keys` | clean + `BRIDGE_TOKEN` + browser env vars |

The browser tool executes curl/jq scripts against Pinchtab's HTTP API on localhost (e.g., `curl -sf http://127.0.0.1:9867/navigate ...`). If network is blocked, the browser tool cannot function. The browser session's nono `CapabilitySet` must **not** call `block_network()`, or must specifically allow localhost / the Pinchtab port if nono supports port-level granularity (Landlock ABI v4+ supports `NetPort` rules for specific ports).

Build the capability set via a shared helper parameterized on whether network is needed:

```rust
fn build_shell_sandbox_caps(
    mounts: &EffectiveMountPaths,
    allow_network: bool,
) -> nono::CapabilitySet {
    let mut caps = nono::CapabilitySet::new();
    caps.allow_read_write(&mounts.shared).ok();
    caps.allow_read_write(&mounts.tmp).ok();
    for path in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        caps.allow_read(path).ok();
    }
    if !allow_network {
        caps.block_network();
    }
    caps
}

// Main shell: block network
let shell_caps = build_shell_sandbox_caps(&mounts, false);

// Browser shell: allow network (needs localhost access to Pinchtab)
let browser_caps = build_shell_sandbox_caps(&mounts, true);
```

**Phase 4c: Hardening status reporting**:

- Report which sandbox mechanism is active (Landlock, Seatbelt, none) in startup status
- Show in web configurator and CLI output
- Differentiate: "nono/Landlock active" vs "nono/Seatbelt active" vs "sandbox unavailable (kernel too old)" vs "sandbox disabled by config"
- Add `ProcessTierSandboxActive` / `ProcessTierSandboxUnavailable` degraded reason codes

**Phase 4d (optional): bubblewrap enhancement**:

- For operators who want namespace-level isolation without container tier
- Wrap shell commands in `bwrap` when configured
- Configurable: `[process_tier] shell_jail = "nono" | "bwrap" | "none"`
- Linux-only; requires bwrap installed on host

### Phase 5: UX, status, and web configurator

#### `crates/runner/src/web/schema.rs`

Add process-tier configuration section to the web schema:

```rust
pub struct ProcessTierSchema {
    pub shell_enabled: BoolField,   // with help text about security implications
    pub browser_enabled: BoolField, // with help text about Chrome requirement
}
```

Do **not** put this under user behavior or agent tools. It is a runner-global deployment decision.

#### Effective availability display

The web configurator should show the computed effective state for each tool:

```
Shell availability:
  Process tier shell enabled: yes/no (runner config)
  Agent shell enabled: yes/no (agent.toml)
  User shell restriction: yes/no (runner-user.toml)
  → Effective: AVAILABLE / DISABLED (reason)

Browser availability:
  Process tier browser enabled: yes/no (runner config)
  Agent browser enabled: yes/no (agent.toml)
  User browser restriction: yes/no (runner-user.toml)
  Chrome detected: yes/no (path)
  Pinchtab status: running/stopped/unavailable
  → Effective: AVAILABLE / DISABLED (reason)
```

#### Startup status fields

Add to `StartupStatusReport` or a parallel status struct:

```rust
pub process_tier_shell_enabled: bool,
pub process_tier_browser_enabled: bool,
pub chrome_binary_path: Option<String>,
pub pinchtab_status: Option<String>,    // "running", "failed", "not_configured"
pub shell_unavailable_reason: Option<String>,
pub browser_unavailable_reason: Option<String>,
```

#### Warnings at enablement

When enabling process-tier tools via the web configurator, show:

- "Shell commands will execute directly on the host without container isolation. This is suitable for development but not recommended for production."
- "Browser requires Chrome or Chromium installed on the host. The browser process runs with the same permissions as the oxydra runner."

#### `crates/runner/static/js/*`

Update:

- onboarding copy to offer process-tier shell/browser as an option
- review summary to show effective tool state
- degraded status display with process-tier-specific reasons

### Docs and examples

Update:

- README: document that process tier can optionally enable shell/browser
- guidebook: add a chapter on process-tier shell/browser setup
- example configs: show `[process_tier]` section
- onboarding docs: explain security differences across tiers

Especially important:

- be explicit that process-tier shell/browser have NO container isolation
- explain what hardening is applied and what it does NOT protect against
- document Pinchtab and Chrome/Chromium requirements for browser
- explain the difference between `cdp_url` (external) and local Chrome management
- document how to install Pinchtab on the host

## Phased rollout

### Phase 1: Config schema and capability resolution refactor

Deliverables:

- `ProcessTierConfig` type with `shell_enabled` and `browser_enabled` fields
- `process_tier` field added to `RunnerGlobalConfig`
- runner config parsing for `[process_tier]` section
- relax `RunnerBootstrapEnvelope::validate()` to allow `shell_available`/`browser_available` without sidecar on Process tier (BLOCKING prerequisite)
- refactor `resolve_requested_capabilities()` to consult `ProcessTierConfig` instead of hard-coding Process tier exclusion
- update `pre_compute_sidecar_endpoint()` guard to skip sidecar pre-computation for Process tier
- update `build_startup_status_report()` to handle process-tier shell/browser without sidecar, and not emit spurious `SidecarUnavailable` degraded reasons
- inject env into host process launch path (`spawn_process_guest_with_startup_stdin`)
- refactor `write_shell_overlay()` to in-memory config composition (`compute_effective_shell_config`)
- update `PROCESS_TIER_WARNING` to be conditional on actual tool availability
- update system prompt in `bootstrap.rs` to be backend-aware
- new degraded reason codes (`ProcessTierShellEnabled`, `ProcessTierBrowserEnabled`, `ProcessTierBrowserUnavailable`)
- platform check: fail clearly on unsupported platforms (Windows) when process-tier tools are enabled

Gate:

- no tool behavior change yet, but the config model can represent process-tier shell/browser cleanly
- `RunnerBootstrapEnvelope::validate()` accepts process-tier shell/browser without sidecar
- existing container/microVM tiers are unchanged
- process tier with default config (both disabled) behaves identically to today
- all existing tests pass

### Phase 2: Host-local shell MVP

Deliverables:

- `bootstrap_bash_tool()` in `crates/tools/src/lib.rs` creates `LocalProcessShellSession` when process tier with no sidecar and `shell_available: true`
- `shell_env` from the launch request routed into `LocalProcessShellSession`'s `ShellSessionConfig.env`
- shell policy enforcement added to `LocalProcessShellSession` (new `ShellPolicy` struct or field, `validate_command()` method checking allowlist, denylist, operators)
- `ToolAvailability::startup_status()` in `registry.rs` updated: `sidecar_available` not set to `true` for process tier; degraded message conditional on actual tool availability
- effective shell config composition (in-memory, not file mutation)
- shell tool registered and functional in process tier
- startup status correctly reports shell availability

Gate:

- shell works end-to-end in process tier: `exec_command()` runs host commands
- shell policy (allow/deny/operators/timeout) is enforced
- env vars are correctly routed to the local shell session
- failures are surfaced as explicit degraded reasons
- existing sidecar shell path is unchanged for container/microVM tiers

### Phase 3: Host-local browser

Deliverables:

- `LocalBrowserManager`: Pinchtab binary detection, Pinchtab launch, health check, shutdown/cleanup
- `Drop` impl on `LocalBrowserManager` for best-effort cleanup on panic
- Pinchtab binary available on host (bundled with oxydra distribution or documented install)
- `chromium-wrapper.sh` adapted for host use (or documented that `CHROME_BINARY` must be set)
- `BrowserTool` constructed with local Pinchtab URL + `LocalProcessShellSession`
- browser's `LocalProcessShellSession` has `BRIDGE_TOKEN` and other browser env vars injected
- browser shell overlay applied in-memory to the browser's shell session
- screenshot path made configurable in `BrowserTool` (replace hardcoded `/shared/screenshot.png` with workspace-relative path for process tier)
- external CDP path (`cdp_url`) works for process tier
- browser tool registered and functional in process tier
- startup status reports browser availability with reasons

Gate:

- browser open/navigate/screenshot/evaluate flows work in process tier
- screenshots saved to real resolved `shared` path (not container alias `/shared/`)
- Pinchtab detects and launches Chrome on Linux and macOS
- Pinchtab health check catches failures (missing Chrome, port conflicts, etc.) and reports clearly
- external CDP via `cdp_url` works as alternative
- BrowserAutomation skill works with local browser (if applicable)
- Pinchtab and Chrome processes are cleaned up on normal shutdown and SIGTERM

### Phase 4: Hardening — jailed shells via nono

Deliverables:

- **4a**: env scrubbing — `LocalProcessShellSession` uses clean environment via `Command::env_clear()` (only allowed vars)
- **4b**: `nono` crate integration — sandbox applied in `pre_exec` hook of `run_shell_command()`, workspace-scoped read-write, system paths read-only, optional network blocking
- **4c**: hardening status reporting — startup report shows which sandbox mechanism is active (Landlock/Seatbelt/none)
- **4d (optional)**: bubblewrap/namespace enhancement for operators who want stronger isolation
- documentation of security posture and limitations for each hardening level

Gate:

- env scrubbing prevents accidental exposure of sensitive vars (no runner API keys in shell environment)
- on Linux with Landlock: shell commands cannot read/write outside workspace `shared/` and `tmp/` (verified by test attempting to read `~/.ssh/`)
- on macOS with Seatbelt: shell commands are restricted by nono's Seatbelt sandbox
- graceful fallback: nono sandbox unavailable (old kernel, unsupported platform) → logged warning, shell still works, status shows "sandbox unavailable"
- documentation clearly states what each hardening level does and does not protect against

### Phase 5: UX polish and documentation

Deliverables:

- web configurator section for process-tier config
- effective availability display in web UI
- structured startup status fields
- onboarding flow offers process-tier shell/browser
- updated docs, README, guidebook, example configs
- preflight diagnostics (Chrome missing, Pinchtab failed, etc.) in CLI and web

Gate:

- operators can understand and configure process-tier shell/browser without reading source code
- failures produce actionable error messages
- web configurator shows the three-layer effective config view

## Testing and validation

### Unit/integration coverage

- capability resolution matrix: all combinations of (tier, runner config, agent config, user override)
- `ProcessTierConfig` parsing and validation (including default values for missing `[process_tier]` section)
- `RunnerBootstrapEnvelope::validate()` accepts process-tier shell/browser without sidecar
- `RunnerBootstrapEnvelope::validate()` still rejects non-process-tier shell/browser without sidecar
- `build_startup_status_report()` produces correct `sidecar_available: false` for process tier even when shell/browser are available
- `bootstrap_bash_tool()` creates `LocalProcessShellSession` for process tier
- `bootstrap_bash_tool()` still connects to sidecar for container/microVM tiers
- `LocalProcessShellSession` command execution
- shell policy enforcement (allowlist, denylist, operators)
- effective shell config composition with browser overlay
- env routing: `shell_env` reaches `LocalProcessShellSession`, `BRIDGE_TOKEN` reaches browser session
- startup status/degraded reasons for all process-tier states
- system prompt text for all (tier, shell, browser) combinations
- web schema generation including process-tier section
- `LocalBrowserManager` Pinchtab binary detection (mock filesystem/PATH)
- `LocalBrowserManager` Pinchtab health check (mock HTTP)
- screenshot path uses real resolved `shared` path for process tier, `/shared/` alias for sidecar tiers
- nono sandbox: verify shell cannot access paths outside workspace on Linux (Landlock) and macOS (Seatbelt)
- nono graceful fallback: verify shell works without sandbox on unsupported platforms
- env scrubbing: verify runner env vars (API keys, tokens) are not inherited by shell session

### End-to-end coverage

Process tier scenarios:

- process tier + shell disabled + browser disabled (default — matches current behavior)
- process tier + shell enabled + browser disabled
- process tier + shell enabled + browser enabled + Chrome installed
- process tier + shell enabled + browser enabled + Pinchtab missing (graceful degradation)
- process tier + shell enabled + browser enabled + Chrome missing (Pinchtab health check fails, graceful degradation)
- process tier + shell enabled + browser enabled + external `cdp_url`
- process tier + shell enabled + agent shell disabled (agent config overrides)
- process tier + shell enabled + user shell disabled (user override restricts)
- process tier + shell enabled + allowlist/denylist enforcement
- process tier + browser enabled + Pinchtab launch failure (graceful degradation)
- process tier + browser enabled + Pinchtab health check timeout

Non-process tier regression:

- container tier behavior unchanged
- microVM tier behavior unchanged
- sidecar shell/browser path unchanged

### Manual validation matrix

At minimum:

- Linux x86_64 with Chrome/Chromium installed
- Linux x86_64 without Chrome (Pinchtab health check fails, browser degrades, shell works)
- Linux x86_64 without Pinchtab (browser degrades at preflight, shell works)
- macOS with Chrome installed
- macOS without Chrome (Pinchtab health check fails, browser degrades, shell works)
- Windows (should fail clearly with "platform not supported" for process-tier tools)
- existing container tier unchanged
- existing microVM tier unchanged

## Main risks

### Risk 1: Security overclaim

It is easy to overstate the safety of host-local shell/browser. Process tier shell runs as the same OS user as the runner process. With Phase 4 hardening via nono:
- **With nono/Landlock (Linux)**: filesystem access is restricted to workspace directories and read-only system paths. This is a real security boundary enforced by the kernel. Network can also be blocked. Seccomp BPF provides syscall-level interception.
- **With nono/Seatbelt (macOS)**: similar filesystem/network restrictions via kernel sandbox.
- **Without nono** (unsupported platform or old kernel): no OS-level restrictions — commands have full host access.

Process tier is meaningfully weaker than container/microVM isolation regardless of hardening level. It does not provide process namespace isolation, UID separation, or resource limits (unless bubblewrap is used as an optional enhancement).

nono itself is early alpha and has not undergone security audits, but the underlying mechanisms (Landlock, Seatbelt) are mature kernel features. The wrapper is thin and the trust model is the same as implementing these directly.

Mitigation: explicit warnings at every enablement point; documentation that says exactly what each hardening level does and does not do. Status reporting distinguishes "nono/Landlock active" from "sandbox unavailable".

### Risk 2: Browser complexity hides inside "just enable browser"

Browser is not just another boolean. It carries:
- Pinchtab binary availability (external Go binary, not part of this workspace)
- Pinchtab bridge lifecycle (launch, health check, crash recovery)
- Chrome/Chromium must be installed on the host (Pinchtab detects and launches it)
- Shell policy overlay (curl, jq, sleep, operators)
- Port allocation
- State directory management
- `chromium-wrapper.sh` adaptation for host use
- Headless vs headed mode

Mitigation: ship shell first (Phase 2) without waiting for browser. Browser is Phase 3 with its own gate criteria.

### Risk 3: Shell policy enforcement parity

The sidecar oxydra-shelld enforces allowlist/denylist/operator policy. `LocalProcessShellSession` does not currently enforce any policy — it runs commands directly. If policy enforcement is incomplete, process-tier shell would be more permissive than sidecar shell.

Mitigation: extract shell policy validation into a shared utility and use it in both sidecar and local paths. Test the validation logic thoroughly.

### Risk 4: Env leakage in host-local shell

Host-local shell could inherit the runner process's environment, exposing API keys, tokens, and other sensitive vars that the sidecar model isolates by only forwarding explicitly configured env vars.

Mitigation (Phase 4a): `LocalProcessShellSession` starts with a **clean environment** — only explicitly configured env keys from `ShellConfig.env_keys` plus standard system vars (`PATH`, `HOME`, `SHELL`, `TERM`, `USER`, `LANG`, `LC_ALL`). The existing `run_shell_command()` already supports custom env via `ShellSessionConfig.env` — the key change is to not inherit the parent process environment by default (use `Command::env_clear()` before setting specific vars).

### Risk 5: Chrome/Chromium version and Pinchtab compatibility

Different hosts will have different Chrome versions. Pinchtab may not work with all versions. Additionally, Pinchtab itself must be installed on the host (it is a Go binary, not part of the Rust workspace).

Mitigation: document minimum Chrome version requirement and Pinchtab installation instructions. Support `cdp_url` as an escape hatch for environments with non-standard Chrome setups. Pinchtab's own health check will catch incompatibilities — if it fails to connect to Chrome, the health check times out and browser degrades gracefully.

### Risk 6: Windows and platform-specific behavior

The current code has platform-specific handling:
- `run_shell_command()` uses `/bin/sh -lc` on Unix and `cmd /C` on Windows
- `attempt_process_tier_hardening()` returns `Unsupported` on non-Linux/macOS
- Shell operator validation (`&&`, `||`, `|`, `;`) is Unix-centric; Windows `cmd` uses different syntax

For the first rollout, process-tier shell/browser targets **Linux and macOS only**. Windows is out of scope. If process tier is enabled on Windows, it should fail clearly with a "platform not supported" message rather than silently misbehaving.

Pinchtab binary availability and `chromium-wrapper.sh` would need Windows equivalents if Windows support is added later.

Mitigation: add a platform check at process-tier tool enablement. Document Linux and macOS as supported platforms for process-tier shell/browser.

### Risk 7: Hidden config coupling from shell overlay

`write_shell_overlay()` mutates a workspace config file so that browser-related shell commands are allowed. This file-mutation coupling is fragile and confusing. Moving to in-memory composition (Phase 1) eliminates this risk and is valuable cleanup independent of process tier.

Mitigation: refactor to in-memory effective config composition before wiring up host-local shell.
