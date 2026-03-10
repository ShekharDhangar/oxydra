# Plan: Skill Authoring Via Agent Tools

> **Status:** Proposed
> **Created:** 2026-03-10
> **Issue:** [#30](https://github.com/shantanugoel/oxydra/issues/30)

---

## 1. Problem Statement

Skills today are static files authored by humans outside of agent sessions. If a
user wants the agent to learn a new workflow (e.g. "remember how to deploy to
staging"), they must manually create a `.md` file with correct YAML frontmatter
in the right directory. This breaks the conversational flow and requires
knowledge of the skill file format.

The goal is to let the agent create and update skills as part of normal tool
use, so users can say "save this as a skill" and the agent writes a valid,
immediately-usable skill file.

## 2. Design Principles

- **Simple tool surface.** Two tools: `skill_create` and `skill_update`. No
  separate validate, delete, list, or reload tools — keep the API minimal.
- **Workspace-scoped writes.** Agent-authored skills always land in the
  workspace skills directory (`.oxydra/skills/`). System and user directories
  are read-only from the agent's perspective.
- **Immediate availability.** After a successful create/update, the skill
  should be available in the next session (or after the next bootstrap). No
  hot-reload complexity — skills are loaded at bootstrap time and that is
  sufficient.
- **Validation on write.** The tools reuse the existing `parse_skill_content()`
  logic to validate frontmatter + token limits before writing. Invalid skills
  are rejected with actionable error messages.
- **Path safety.** All writes are confined to the workspace skills directory.
  Path traversal attempts (e.g. `../../etc/passwd`) are rejected.

## 3. Tool Design

### 3.1 `skill_create`

Creates a new skill file in the workspace skills directory.

**Parameters:**

| Name | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Kebab-case skill identifier (e.g. `deploy-staging`). Must match `^[a-z0-9][a-z0-9-]*$`. |
| `description` | string | yes | One-line summary of what the skill does. |
| `content` | string | yes | Markdown body (the skill instructions, without frontmatter). |
| `activation` | string | no | `auto` (default), `manual`, or `always`. |
| `requires` | string[] | no | Tool names that must be ready (e.g. `["shell_exec"]`). |
| `env_vars` | string[] | no | Environment variables needed (e.g. `["DEPLOY_TOKEN"]`). |
| `priority` | integer | no | Sort order, lower = earlier in prompt (default: 100). |

**Behavior:**

1. Validate `name` matches the kebab-case pattern.
2. Check that no skill with this name already exists in the workspace skills
   directory. If one exists, return an error telling the agent to use
   `skill_update` instead.
3. Assemble the full skill file content: YAML frontmatter + markdown body.
4. Run the assembled content through `parse_skill_content()` to validate
   frontmatter parsing and token limits.
5. Write to `.oxydra/skills/{name}.md` (bare-file format for simplicity).
6. Return success with the file path and a note that the skill will be active
   in the next session.

**Error cases:**
- Invalid name format → actionable error with the regex pattern.
- Name already exists → error suggesting `skill_update`.
- Content exceeds token cap (3000 estimated tokens) → error with counts.
- Frontmatter validation failure → error with parse details.

### 3.2 `skill_update`

Updates an existing skill file in the workspace skills directory.

**Parameters:**

| Name | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Name of the skill to update. |
| `description` | string | no | New description (keeps existing if omitted). |
| `content` | string | no | New markdown body (keeps existing if omitted). |
| `activation` | string | no | New activation mode (keeps existing if omitted). |
| `requires` | string[] | no | New required tools (keeps existing if omitted). |
| `env_vars` | string[] | no | New env vars (keeps existing if omitted). |
| `priority` | integer | no | New priority (keeps existing if omitted). |

**Behavior:**

1. Look up the existing skill file at `.oxydra/skills/{name}.md` or
   `.oxydra/skills/{Name}/SKILL.md` (supporting both formats).
2. If not found in the workspace directory, return an error. Agent-authored
   tools can only update workspace-scoped skills; system/user/embedded
   skills cannot be modified (the agent should create a workspace override
   instead).
3. Parse the existing file to get current metadata and content.
4. Merge provided fields over existing metadata (only overwrite fields that
   were explicitly provided).
5. Re-validate the merged result through `parse_skill_content()`.
6. Write back to the same path.
7. Return success with what changed.

**Error cases:**
- Skill not found in workspace → error explaining scope restriction and
  suggesting `skill_create` to override a system/user skill.
- Validation failure after merge → error with details.
- No fields provided to update → error asking for at least one field.

## 4. Implementation

### 4.1 New File: `crates/tools/src/skill_tools.rs`

Contains the two tool structs, their `Tool` trait implementations, and a
registration function. Follows the same pattern as `scratchpad_tools.rs`:
manual `#[async_trait] impl Tool` with JSON schema in `schema()`.

```rust
// Constants
pub const SKILL_CREATE_TOOL_NAME: &str = "skill_create";
pub const SKILL_UPDATE_TOOL_NAME: &str = "skill_update";

// Tool structs hold the workspace skills directory path
pub struct SkillCreateTool {
    skills_dir: PathBuf,
}

pub struct SkillUpdateTool {
    skills_dir: PathBuf,
}

// Args structs
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

// Registration
pub fn register_skill_tools(
    registry: &mut ToolRegistry,
    workspace_dir: &Path,
) {
    let skills_dir = workspace_dir.join("skills");
    registry.register(SKILL_CREATE_TOOL_NAME, SkillCreateTool { skills_dir: skills_dir.clone() });
    registry.register(SKILL_UPDATE_TOOL_NAME, SkillUpdateTool { skills_dir });
}
```

Both tools use `SafetyTier::SideEffecting` (they write files, but only to a
constrained directory).

### 4.2 Shared Helpers (in `skill_tools.rs`)

```rust
/// Validates kebab-case name: ^[a-z0-9][a-z0-9-]*$, max 64 chars.
fn validate_skill_name(name: &str) -> Result<(), String>

/// Assembles YAML frontmatter + content into a complete skill file string.
fn assemble_skill_file(metadata: &SkillMetadata, content: &str) -> String

/// Resolves the skill file path within the skills directory.
/// Checks bare file first ({name}.md), then folder ({name}/SKILL.md).
fn resolve_skill_path(skills_dir: &Path, name: &str) -> Option<PathBuf>

/// Ensures the skills directory path is safe (no traversal).
fn validate_path_safety(skills_dir: &Path, name: &str) -> Result<PathBuf, String>
```

### 4.3 Making `parse_skill_content` Reusable

`parse_skill_content()` currently lives in `crates/runner/src/skills.rs` and
is private. To reuse it from the tools crate:

**Option A (recommended): Move validation logic to `crates/types/`.**
Add a `validate_skill_content(raw: &str) -> Result<Skill, SkillValidationError>`
function in `crates/types/src/skill.rs` that performs frontmatter parsing and
token cap checks. Both `skills.rs` and `skill_tools.rs` call this. The
`gray_matter` dependency moves to `types`.

**Option B: Duplicate the validation in tools.** Simple but violates DRY.
Acceptable if the validation is small enough (it's ~30 lines).

Option A keeps the validation in one place. The `gray_matter` crate is
lightweight and has no runtime cost that would bloat `types`.

### 4.4 Registration in Bootstrap

In `crates/tools/src/registry.rs`, `register_runtime_tools()`:

```rust
// After existing tool registrations:
skill_tools::register_skill_tools(&mut registry, workspace_root);
```

The workspace root is already available in the bootstrap context via the
`RunnerBootstrapEnvelope`.

### 4.5 Path Safety

The `validate_path_safety` function:
1. Joins `skills_dir` with `{name}.md`.
2. Canonicalizes both `skills_dir` and the joined path.
3. Asserts the joined path starts with `skills_dir`.
4. Rejects names containing `/`, `\`, `..`, or null bytes.

This is defense-in-depth on top of the kebab-case regex, which already
prevents most traversal attempts.

### 4.6 Frontmatter Serialization

The `assemble_skill_file` function produces:

```markdown
---
name: deploy-staging
description: Deploy the app to the staging environment
activation: auto
requires:
  - shell_exec
env_vars:
  - DEPLOY_TOKEN
priority: 100
---

## Deploy to Staging

1. Run `deploy.sh` with the staging flag...
```

Use a simple string template rather than a YAML serializer — the frontmatter
is flat enough that `format!()` is clearer and avoids adding `serde_yaml` as a
dependency. The fields are known and controlled.

## 5. Changes Summary

| File | Change |
|---|---|
| `crates/tools/src/skill_tools.rs` | **New.** `SkillCreateTool`, `SkillUpdateTool`, registration function. |
| `crates/tools/src/lib.rs` | Add `pub mod skill_tools;`, export constants, add to `canonical_tool_names()`. |
| `crates/tools/src/registry.rs` | Call `register_skill_tools()` in `register_runtime_tools()`. |
| `crates/types/src/skill.rs` | Add `validate_skill_content()` public function. Add `Serialize` derive to `SkillMetadata`. |
| `crates/types/Cargo.toml` | Add `gray_matter` dependency. |
| `crates/runner/src/skills.rs` | Refactor `parse_skill_content()` to call `types::validate_skill_content()`. |

## 6. Testing

### Unit Tests (in `skill_tools.rs`)

1. **Happy path create:** Valid args → file written with correct frontmatter
   and content. Verify by parsing the written file.
2. **Happy path update:** Existing skill → partial update → verify only
   changed fields are modified.
3. **Name validation:** Reject uppercase, spaces, slashes, `..`, empty, too
   long.
4. **Duplicate create:** Create same name twice → second call returns error
   pointing to `skill_update`.
5. **Update non-existent:** Returns clear error.
6. **Token cap:** Content exceeding 12000 chars (~3000 tokens) → rejected.
7. **Path traversal:** Names like `../../etc/passwd` or `foo/../../bar` →
   rejected.
8. **Update with no fields:** Returns error.
9. **Default values:** Omitted `activation` defaults to `auto`, omitted
   `priority` defaults to 100.

### Integration Test

10. **Round-trip:** Create a skill via tool → bootstrap loads it → skill
    appears in system prompt.

## 7. Out of Scope

- **Skill deletion tool.** Users can delete files manually or via
  `shell_exec`. Not worth a dedicated tool.
- **Skill listing tool.** The agent already sees active skills in the system
  prompt. A list tool adds little value.
- **Hot-reload.** Skills are loaded at bootstrap. Requiring a new session to
  pick up changes is acceptable and avoids complexity.
- **Reference files.** Agent-authored skills use the bare-file format. If
  reference files are needed, the agent can write them via `file_write` and
  reference them in the skill body.
- **Skill versioning/history.** The workspace directory is typically under
  version control. Git handles history.

## 8. UX Flow

### User creates a skill conversationally

```
User: "I just showed you how to deploy to staging. Save that as a skill
       so you remember it next time."

Agent: I'll create a skill for the staging deployment workflow.
       [calls skill_create(name="deploy-staging", description="Deploy the
        app to staging via deploy.sh", content="## Deploy to Staging\n\n
        1. Run `DEPLOY_ENV=staging ./deploy.sh`\n2. ...",
        requires=["shell_exec"], env_vars=["DEPLOY_TOKEN"])]

Agent: "Done — I've saved a 'deploy-staging' skill. It'll activate
       automatically in future sessions when shell access and DEPLOY_TOKEN
       are available."
```

### User iterates on a skill

```
User: "Update the deploy skill — add a step to run smoke tests after."

Agent: [calls skill_update(name="deploy-staging",
        content="## Deploy to Staging\n\n1. Run deploy.sh...\n
        3. Run smoke tests: `./test/smoke.sh staging`")]

Agent: "Updated. The smoke test step will be included next session."
```

### Agent self-learns from experience

```
Agent (after solving a tricky task): "I discovered a reliable approach
       for database migrations with this stack. Let me save it as a skill
       for future reference."
       [calls skill_create(name="db-migrations", ...)]
```
