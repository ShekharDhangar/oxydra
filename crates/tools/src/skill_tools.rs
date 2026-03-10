//! Agent-authored skill tools: `skill_create` and `skill_update`.
//!
//! These tools let the agent create and update skill files in the workspace
//! skills directory (`<workspace>/.oxydra/skills/`). Skills are validated
//! through the same `validate_skill_content()` used by the skill loader.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use types::{
    FunctionDecl, SafetyTier, Skill, SkillActivation, SkillMetadata, Tool, ToolError,
    ToolExecutionContext, validate_skill_content,
};

use crate::{execution_failed, invalid_args, parse_args};

pub const SKILL_CREATE_TOOL_NAME: &str = "skill_create";
pub const SKILL_UPDATE_TOOL_NAME: &str = "skill_update";
const SKILL_FILE_NAME: &str = "SKILL.md";

/// Maximum length for a skill name.
const MAX_NAME_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SkillCreateArgs {
    name: String,
    description: String,
    content: String,
    activation: Option<String>,
    requires: Option<Vec<String>>,
    env_vars: Option<Vec<String>>,
    priority: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct SkillUpdateArgs {
    name: String,
    description: Option<String>,
    content: Option<String>,
    activation: Option<String>,
    requires: Option<Vec<String>>,
    env_vars: Option<Vec<String>>,
    priority: Option<i32>,
}

// ---------------------------------------------------------------------------
// Tool structs
// ---------------------------------------------------------------------------

pub struct SkillCreateTool {
    skills_dir: PathBuf,
}

pub struct SkillUpdateTool {
    skills_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_skill_tools(registry: &mut crate::ToolRegistry, workspace_config_dir: &Path) {
    let skills_dir = workspace_config_dir.join("skills");
    registry.register(
        SKILL_CREATE_TOOL_NAME,
        SkillCreateTool {
            skills_dir: skills_dir.clone(),
        },
    );
    registry.register(SKILL_UPDATE_TOOL_NAME, SkillUpdateTool { skills_dir });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validates a kebab-case skill name: `^[a-z0-9][a-z0-9-]*$`, max 64 chars.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill name must not be empty".to_owned());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "skill name must be at most {MAX_NAME_LEN} characters, got {}",
            name.len()
        ));
    }
    // Reject path traversal characters explicitly (belt-and-suspenders on top
    // of the regex check).
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains('\0') {
        return Err("skill name must not contain '/', '\\', '..', or null bytes".to_owned());
    }
    // Kebab-case: starts with [a-z0-9], then [a-z0-9-]*.
    let mut chars = name.chars();
    let first = chars.next().unwrap(); // safe: non-empty checked above
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "skill name must start with a lowercase letter or digit, got '{first}'. \
             Expected pattern: ^[a-z0-9][a-z0-9-]*$"
        ));
    }
    for ch in chars {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
            return Err(format!(
                "skill name contains invalid character '{ch}'. \
                 Expected pattern: ^[a-z0-9][a-z0-9-]*$"
            ));
        }
    }
    Ok(())
}

/// Parses an activation string into a [`SkillActivation`].
fn parse_activation(s: &str) -> Result<SkillActivation, String> {
    match s {
        "auto" => Ok(SkillActivation::Auto),
        "manual" => Ok(SkillActivation::Manual),
        "always" => Ok(SkillActivation::Always),
        other => Err(format!(
            "invalid activation mode '{other}'; expected 'auto', 'manual', or 'always'"
        )),
    }
}

/// Assembles YAML frontmatter + markdown body into a complete skill file string.
fn assemble_skill_file(metadata: &SkillMetadata, content: &str) -> String {
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("name: {}\n", metadata.name));
    // Quote the description to handle special YAML characters.
    fm.push_str(&format!(
        "description: \"{}\"\n",
        metadata.description.replace('"', "\\\"")
    ));

    let activation_str = match metadata.activation {
        SkillActivation::Auto => "auto",
        SkillActivation::Manual => "manual",
        SkillActivation::Always => "always",
    };
    fm.push_str(&format!("activation: {activation_str}\n"));

    if !metadata.requires.is_empty() {
        fm.push_str("requires:\n");
        for req in &metadata.requires {
            fm.push_str(&format!("  - {req}\n"));
        }
    }
    if !metadata.env_vars.is_empty() {
        fm.push_str("env_vars:\n");
        for var in &metadata.env_vars {
            fm.push_str(&format!("  - {var}\n"));
        }
    }
    fm.push_str(&format!("priority: {}\n", metadata.priority));
    fm.push_str("---\n");
    format!("{fm}\n{content}")
}

/// Resolves a new folder-based skill path (`{name}/SKILL.md`) under the
/// workspace skills directory. Creates the root directory if needed. Rejects
/// path traversal.
fn validate_new_skill_path(skills_dir: &Path, name: &str) -> Result<PathBuf, String> {
    // Ensure the skills directory exists.
    if let Err(err) = std::fs::create_dir_all(skills_dir) {
        return Err(format!(
            "failed to create skills directory {}: {err}",
            skills_dir.display()
        ));
    }

    // Canonicalize the existing directory.
    let canonical_dir = skills_dir
        .canonicalize()
        .map_err(|err| format!("failed to canonicalize {}: {err}", skills_dir.display()))?;

    // Build the target path.
    let target_dir = canonical_dir.join(name);
    let target = target_dir.join(SKILL_FILE_NAME);

    // Verify the parent directory is still inside the canonical skills root.
    let target_dir_parent = target_dir
        .parent()
        .ok_or_else(|| "target directory has no parent".to_owned())?;
    if target_dir_parent != canonical_dir {
        return Err("skill path escapes the workspace skills directory".to_owned());
    }
    let target_parent = target
        .parent()
        .ok_or_else(|| "target path has no parent".to_owned())?;
    if target_parent != target_dir {
        return Err("skill path escapes the workspace skills directory".to_owned());
    }

    Ok(target)
}

/// Reject `skill_create` when the target folder/file path already exists, or
/// when a legacy bare-file path with the same basename exists.
fn ensure_target_path_absent(
    skills_dir: &Path,
    target_path: &Path,
    name: &str,
) -> Result<(), String> {
    let target_dir = target_path
        .parent()
        .ok_or_else(|| "target path has no parent".to_owned())?;
    if target_dir.exists() || target_path.exists() {
        return Err(format!(
            "a skill path already exists at {}. Use skill_update to modify an existing skill \
             or remove the file before creating a new one.",
            target_dir.display()
        ));
    }
    let legacy_bare_path = skills_dir.join(format!("{name}.md"));
    if legacy_bare_path.exists() {
        return Err(format!(
            "a legacy skill file already exists at {}. Use skill_update to modify an existing skill \
             or remove the file before creating a new one.",
            legacy_bare_path.display()
        ));
    }
    Ok(())
}

/// Scans the workspace skills directory and returns the on-disk file path
/// whose parsed metadata `name` field matches the given name.
fn resolve_skill_path_by_metadata(
    skills_dir: &Path,
    name: &str,
) -> Result<Option<(PathBuf, Skill)>, String> {
    if !skills_dir.is_dir() {
        return Ok(None);
    }
    let entries = std::fs::read_dir(skills_dir)
        .map_err(|err| format!("failed to read {}: {err}", skills_dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();

        // Folder-based skill: subdirectory with SKILL.md inside.
        if path.is_dir() {
            let skill_file = path.join("SKILL.md");
            if skill_file.is_file() {
                if let Ok(raw) = std::fs::read_to_string(&skill_file) {
                    if let Ok(skill) = validate_skill_content(&raw, &skill_file) {
                        if skill.metadata.name == name {
                            return Ok(Some((skill_file, skill)));
                        }
                    }
                }
            }
            continue;
        }

        // Bare-file skill: a .md file directly in the skills directory.
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            && path.is_file()
        {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(skill) = validate_skill_content(&raw, &path) {
                    if skill.metadata.name == name {
                        return Ok(Some((path, skill)));
                    }
                }
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for SkillCreateTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SKILL_CREATE_TOOL_NAME,
            Some(
                "Create a new skill file in the workspace. Skills are markdown documents \
                 with YAML frontmatter that teach agents domain-specific workflows. \
                 The skill will be available in the next session."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["name", "description", "content"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Kebab-case skill identifier (e.g. 'deploy-staging'). Pattern: ^[a-z0-9][a-z0-9-]*$"
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line summary of what the skill does"
                    },
                    "content": {
                        "type": "string",
                        "description": "Markdown body (skill instructions, without frontmatter)"
                    },
                    "activation": {
                        "type": "string",
                        "description": "Activation mode: 'auto' (default), 'manual', or 'always'",
                        "enum": ["auto", "manual", "always"]
                    },
                    "requires": {
                        "type": "array",
                        "description": "Tool names that must be ready (e.g. ['shell_exec'])",
                        "items": { "type": "string" }
                    },
                    "env_vars": {
                        "type": "array",
                        "description": "Environment variables needed (e.g. ['DEPLOY_TOKEN'])",
                        "items": { "type": "string" }
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Sort order, lower = earlier in prompt (default: 100)"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: SkillCreateArgs = parse_args(SKILL_CREATE_TOOL_NAME, args)?;

        // 1. Validate name.
        validate_skill_name(&request.name)
            .map_err(|msg| invalid_args(SKILL_CREATE_TOOL_NAME, msg))?;

        // 2. Check for existing skill with this name.
        let existing = resolve_skill_path_by_metadata(&self.skills_dir, &request.name)
            .map_err(|msg| execution_failed(SKILL_CREATE_TOOL_NAME, msg))?;
        if existing.is_some() {
            return Err(invalid_args(
                SKILL_CREATE_TOOL_NAME,
                format!(
                    "a skill named '{}' already exists in the workspace. \
                     Use skill_update to modify it.",
                    request.name
                ),
            ));
        }

        // 3. Parse activation.
        let activation = match &request.activation {
            Some(s) => {
                parse_activation(s).map_err(|msg| invalid_args(SKILL_CREATE_TOOL_NAME, msg))?
            }
            None => SkillActivation::Auto,
        };

        // 4. Assemble metadata and full file content.
        let metadata = SkillMetadata {
            name: request.name.clone(),
            description: request.description,
            activation,
            requires: request.requires.unwrap_or_default(),
            env_vars: request.env_vars.unwrap_or_default(),
            priority: request.priority.unwrap_or(100),
        };
        let full_content = assemble_skill_file(&metadata, &request.content);

        // 5. Validate the assembled content (frontmatter + token cap).
        let target_path = validate_new_skill_path(&self.skills_dir, &request.name)
            .map_err(|msg| execution_failed(SKILL_CREATE_TOOL_NAME, msg))?;
        ensure_target_path_absent(&self.skills_dir, &target_path, &request.name)
            .map_err(|msg| invalid_args(SKILL_CREATE_TOOL_NAME, msg))?;

        validate_skill_content(&full_content, &target_path)
            .map_err(|err| invalid_args(SKILL_CREATE_TOOL_NAME, err.to_string()))?;

        // 6. Write the file.
        std::fs::create_dir_all(target_path.parent().ok_or_else(|| {
            execution_failed(SKILL_CREATE_TOOL_NAME, "target path has no parent")
        })?)
        .map_err(|err| {
            execution_failed(
                SKILL_CREATE_TOOL_NAME,
                format!("failed to create skill directory: {err}"),
            )
        })?;
        std::fs::write(&target_path, &full_content).map_err(|err| {
            execution_failed(
                SKILL_CREATE_TOOL_NAME,
                format!("failed to write skill file: {err}"),
            )
        })?;

        Ok(format!(
            "Skill '{}' created at {}. It will be active in the next session.",
            request.name,
            target_path.display()
        ))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

#[async_trait]
impl Tool for SkillUpdateTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SKILL_UPDATE_TOOL_NAME,
            Some(
                "Update an existing skill file in the workspace. Only workspace-scoped \
                 skills can be updated; to override a system/user skill, use skill_create. \
                 Omitted fields keep their existing values."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to update"
                    },
                    "description": {
                        "type": "string",
                        "description": "New description (keeps existing if omitted)"
                    },
                    "content": {
                        "type": "string",
                        "description": "New markdown body (keeps existing if omitted)"
                    },
                    "activation": {
                        "type": "string",
                        "description": "New activation mode (keeps existing if omitted)",
                        "enum": ["auto", "manual", "always"]
                    },
                    "requires": {
                        "type": "array",
                        "description": "New required tools (keeps existing if omitted)",
                        "items": { "type": "string" }
                    },
                    "env_vars": {
                        "type": "array",
                        "description": "New env vars (keeps existing if omitted)",
                        "items": { "type": "string" }
                    },
                    "priority": {
                        "type": "integer",
                        "description": "New priority (keeps existing if omitted)"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        _context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: SkillUpdateArgs = parse_args(SKILL_UPDATE_TOOL_NAME, args)?;

        // 1. Check that at least one field to update was provided.
        let has_updates = request.description.is_some()
            || request.content.is_some()
            || request.activation.is_some()
            || request.requires.is_some()
            || request.env_vars.is_some()
            || request.priority.is_some();
        if !has_updates {
            return Err(invalid_args(
                SKILL_UPDATE_TOOL_NAME,
                "at least one field (description, content, activation, requires, env_vars, \
                 priority) must be provided to update",
            ));
        }

        // 2. Find the existing skill by metadata name in the workspace directory.
        let (existing_path, existing_skill) =
            resolve_skill_path_by_metadata(&self.skills_dir, &request.name)
                .map_err(|msg| execution_failed(SKILL_UPDATE_TOOL_NAME, msg))?
                .ok_or_else(|| {
                    invalid_args(
                        SKILL_UPDATE_TOOL_NAME,
                        format!(
                            "no skill named '{}' found in the workspace skills directory. \
                             Only workspace-scoped skills can be updated. Use skill_create \
                             to create a workspace override for a system/user skill.",
                            request.name
                        ),
                    )
                })?;

        // 3. Merge provided fields over existing metadata.
        let activation = match &request.activation {
            Some(s) => {
                parse_activation(s).map_err(|msg| invalid_args(SKILL_UPDATE_TOOL_NAME, msg))?
            }
            None => existing_skill.metadata.activation,
        };

        let merged_metadata = SkillMetadata {
            name: existing_skill.metadata.name.clone(),
            description: request
                .description
                .unwrap_or(existing_skill.metadata.description),
            activation,
            requires: request.requires.unwrap_or(existing_skill.metadata.requires),
            env_vars: request.env_vars.unwrap_or(existing_skill.metadata.env_vars),
            priority: request.priority.unwrap_or(existing_skill.metadata.priority),
        };

        let merged_content = request.content.unwrap_or(existing_skill.content);

        // 4. Assemble and re-validate.
        let full_content = assemble_skill_file(&merged_metadata, &merged_content);

        validate_skill_content(&full_content, &existing_path)
            .map_err(|err| invalid_args(SKILL_UPDATE_TOOL_NAME, err.to_string()))?;

        // 5. Write back to the same path.
        std::fs::write(&existing_path, &full_content).map_err(|err| {
            execution_failed(
                SKILL_UPDATE_TOOL_NAME,
                format!("failed to write skill file: {err}"),
            )
        })?;

        Ok(format!(
            "Skill '{}' updated at {}. Changes will be active in the next session.",
            request.name,
            existing_path.display()
        ))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use types::ToolExecutionContext;

    fn temp_skills_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // --- validate_skill_name ---

    #[test]
    fn valid_kebab_case_names() {
        assert!(validate_skill_name("deploy-staging").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("db-migrations").is_ok());
        assert!(validate_skill_name("foo123").is_ok());
        assert!(validate_skill_name("0-start-with-digit").is_ok());
    }

    #[test]
    fn reject_uppercase() {
        assert!(validate_skill_name("Deploy").is_err());
    }

    #[test]
    fn reject_spaces() {
        assert!(validate_skill_name("my skill").is_err());
    }

    #[test]
    fn reject_slashes() {
        assert!(validate_skill_name("foo/bar").is_err());
    }

    #[test]
    fn reject_dot_dot() {
        assert!(validate_skill_name("..foo").is_err());
    }

    #[test]
    fn reject_empty() {
        assert!(validate_skill_name("").is_err());
    }

    #[test]
    fn reject_too_long() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_skill_name(&long_name).is_err());
    }

    // --- assemble_skill_file ---

    #[test]
    fn assemble_produces_valid_skill() {
        let metadata = SkillMetadata {
            name: "test-skill".to_owned(),
            description: "A test skill".to_owned(),
            activation: SkillActivation::Auto,
            requires: vec!["shell_exec".to_owned()],
            env_vars: vec!["MY_VAR".to_owned()],
            priority: 50,
        };
        let assembled = assemble_skill_file(&metadata, "## Instructions\n\nDo stuff.");
        let skill = validate_skill_content(&assembled, Path::new("test.md")).unwrap();
        assert_eq!(skill.metadata.name, "test-skill");
        assert_eq!(skill.metadata.description, "A test skill");
        assert_eq!(skill.metadata.requires, vec!["shell_exec"]);
        assert_eq!(skill.metadata.env_vars, vec!["MY_VAR"]);
        assert_eq!(skill.metadata.priority, 50);
        assert!(skill.content.contains("## Instructions"));
    }

    // --- SkillCreateTool ---

    #[tokio::test]
    async fn create_happy_path() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "deploy-staging",
            "description": "Deploy to staging",
            "content": "## Deploy\n\n1. Run deploy.sh",
            "requires": ["shell_exec"],
            "env_vars": ["DEPLOY_TOKEN"]
        }))
        .unwrap();

        let result = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(result.contains("deploy-staging"));
        assert!(result.contains("created"));

        // Verify the file was written and is parseable.
        let file_path = dir.path().join("deploy-staging").join("SKILL.md");
        assert!(file_path.exists());
        let raw = fs::read_to_string(&file_path).unwrap();
        let skill = validate_skill_content(&raw, &file_path).unwrap();
        assert_eq!(skill.metadata.name, "deploy-staging");
        assert_eq!(skill.metadata.requires, vec!["shell_exec"]);
    }

    #[tokio::test]
    async fn create_defaults() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "simple",
            "description": "A simple skill",
            "content": "Do something."
        }))
        .unwrap();

        let result = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(result.contains("simple"));

        let file_path = dir.path().join("simple").join("SKILL.md");
        let raw = fs::read_to_string(&file_path).unwrap();
        let skill = validate_skill_content(&raw, &file_path).unwrap();
        assert_eq!(skill.metadata.activation, SkillActivation::Auto);
        assert_eq!(skill.metadata.priority, 100);
        assert!(skill.metadata.requires.is_empty());
    }

    #[tokio::test]
    async fn create_duplicate_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-skill",
            "description": "first",
            "content": "body"
        }))
        .unwrap();

        tool.execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();

        // Second create with same name should fail.
        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(msg.contains("skill_update"), "got: {msg}");
    }

    #[tokio::test]
    async fn create_rejects_existing_malformed_target_file() {
        let dir = temp_skills_dir();
        let existing_path = dir.path().join("my-skill.md");
        fs::write(&existing_path, "not valid frontmatter").unwrap();

        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-skill",
            "description": "first",
            "content": "body"
        }))
        .unwrap();

        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(msg.contains("skill_update"), "got: {msg}");

        let raw = fs::read_to_string(&existing_path).unwrap();
        assert_eq!(raw, "not valid frontmatter");
    }

    #[tokio::test]
    async fn create_invalid_name_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "../../etc/passwd",
            "description": "evil",
            "content": "body"
        }))
        .unwrap();

        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains(".."), "got: {err}");
    }

    #[tokio::test]
    async fn create_token_cap_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        // Exceed the 3000-token (~12000 char) cap.
        let big_content = "x".repeat(13000);
        let args = serde_json::to_string(&json!({
            "name": "big-skill",
            "description": "too big",
            "content": big_content
        }))
        .unwrap();

        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("token cap"), "got: {err}");
    }

    // --- SkillUpdateTool ---

    #[tokio::test]
    async fn update_happy_path() {
        let dir = temp_skills_dir();

        // Create a skill first.
        let create_tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-skill",
            "description": "original",
            "content": "## Original body"
        }))
        .unwrap();
        create_tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();

        // Update description only.
        let update_tool = SkillUpdateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-skill",
            "description": "updated description"
        }))
        .unwrap();
        let result = update_tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(result.contains("updated"));

        // Verify only description changed.
        let file_path = dir.path().join("my-skill").join("SKILL.md");
        let raw = fs::read_to_string(&file_path).unwrap();
        let skill = validate_skill_content(&raw, &file_path).unwrap();
        assert_eq!(skill.metadata.description, "updated description");
        assert!(skill.content.contains("## Original body"));
    }

    #[tokio::test]
    async fn update_resolves_by_metadata_name() {
        let dir = temp_skills_dir();

        // Create a folder-based skill manually (directory name != metadata name).
        let skill_dir = dir.path().join("SomeFolder");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_content = "---\nname: my-actual-name\ndescription: test\n---\n\n## Body";
        fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();

        let update_tool = SkillUpdateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-actual-name",
            "description": "updated"
        }))
        .unwrap();
        let result = update_tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(result.contains("updated"));

        // Verify the file was updated at the original path.
        let raw = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        let skill = validate_skill_content(&raw, &skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.metadata.description, "updated");
    }

    #[tokio::test]
    async fn update_nonexistent_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillUpdateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "nonexistent",
            "description": "new"
        }))
        .unwrap();

        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no skill named"), "got: {err}");
    }

    #[tokio::test]
    async fn update_no_fields_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillUpdateTool {
            skills_dir: dir.path().to_path_buf(),
        };
        let args = serde_json::to_string(&json!({
            "name": "my-skill"
        }))
        .unwrap();

        let err = tool
            .execute(&args, &ToolExecutionContext::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one field"), "got: {err}");
    }

    // --- Path safety ---

    #[tokio::test]
    async fn create_path_traversal_rejected() {
        let dir = temp_skills_dir();
        let tool = SkillCreateTool {
            skills_dir: dir.path().to_path_buf(),
        };

        for bad_name in &["foo/bar", "foo\\bar", "a\0b"] {
            let args = serde_json::to_string(&json!({
                "name": bad_name,
                "description": "evil",
                "content": "body"
            }))
            .unwrap();
            let err = tool
                .execute(&args, &ToolExecutionContext::default())
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("must not contain"),
                "name '{bad_name}' was not rejected: {err}"
            );
        }
    }
}
