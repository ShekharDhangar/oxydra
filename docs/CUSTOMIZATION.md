## Customizing Your Oxydra

After initial setup, the most impactful customizations are listed below. Everything is controlled through `agent.toml` and `users/alice.toml` (or the web configurator). Restart the runner for changes to take effect.

### Enabling Browser Automation

Oxydra can control a headless Chrome browser (via [Pinchtab](https://github.com/pinchtab/pinchtab)) from within its sandboxed environment. When enabled, the agent can navigate web pages, interact with forms, take screenshots, read page content, download files, and deliver results directly to you — guided by an injected skill document and backed by the dedicated `browser` tool.

**Requires:** `container` or `micro_vm` isolation. Browser is not available in `process` (`--insecure`) mode.

Enable it workspace-wide in `.oxydra/agent.toml`:

```toml
[tools.browser]
enabled = true
# cdp_url = "http://127.0.0.1:9222"   # Optional external Chrome/Chromium CDP endpoint
```

If you need to restrict browser access for one user, add an override in `.oxydra/users/alice.toml`:

```toml
[behavior]
browser_enabled = false
```

Or use the web configurator under **Agent Config → Tools → Browser** for workspace defaults and **User Config → Behavior** for per-user restrictions.

**How it works:** The Browser Automation skill is a markdown document embedded in the Oxydra binary. When browser is enabled and Pinchtab starts successfully, the skill is automatically injected into the agent's system prompt with the Pinchtab API URL pre-filled. The agent uses the built-in `browser` tool for common browser actions, and the shell sandbox is also extended to allow `curl`, `jq`, `sleep`, and shell operators when file-oriented Pinchtab flows are needed. Browser activity stays inside the sandboxed guest environment.

The guest containers now run unprivileged by default. When launched by the runner, they are mapped to your host UID/GID when possible so bind-mounted workspaces stay writable.

If you need container-local time to match your region for shell commands or browser automation, set it per user in `.oxydra/users/<user>.toml`:

```toml
[behavior]
timezone = "America/New_York"
```

The default is `UTC`. You can still override it for a single launch:

```bash
oxydra --config .oxydra/runner.toml --user alice -e TZ=America/New_York start
```

`TZ` is forwarded to both guest containers. Use an IANA timezone such as `America/New_York` or `Asia/Kolkata`.

### Custom Specialist Agents

Specialist agents let you configure separate personas, tool scopes, and model choices for different tasks. The main agent can delegate work to specialists using the `delegate_to_agent` tool.

Define specialists in `.oxydra/agent.toml`:

```toml
[agents.researcher]
system_prompt = "You are a research specialist focused on evidence gathering."
tools         = ["web_search", "web_fetch", "file_read"]
max_turns     = 12
max_cost      = 0.50

[agents.researcher.selection]
provider = "anthropic"
model    = "claude-3-5-haiku-latest"

[agents.coder]
system_prompt = "You are a coding specialist for implementation and debugging."
tools         = ["file_read", "file_edit", "file_write", "shell_exec"]
# No [agents.coder.selection]: inherits the caller's current provider/model.
```

The `tools` list restricts which tools a specialist can use. Omit it entirely to inherit the default tool set. See [`examples/config/agent.toml`](../examples/config/agent.toml) for more examples including multimodal and image-generation agents.

### Turn Limits and Cost Budgets

The default configuration allows up to **100 turns** per interactive session with no cost cap. Tighten or loosen these in `.oxydra/agent.toml`:

```toml
[runtime]
max_turns         = 15     # Reduce for more focused tasks (default: 100)
turn_timeout_secs = 60     # Per-LLM-call timeout in seconds
# max_cost        = 0.50   # Optional cost cap (provider-reported units). Uncomment to enable.
```

For scheduled tasks, set stricter per-run budgets under `[scheduler]`:

```toml
[scheduler]
enabled  = true
max_turns = 8     # Per-run turn limit for scheduled tasks
max_cost  = 0.25  # Per-run cost cap
```

The gateway also has session-level knobs for multi-session environments:

```toml
[gateway]
max_sessions_per_user         = 50   # How many sessions a user can have open
max_concurrent_turns_per_user = 10   # How many turns can run in parallel
session_idle_ttl_hours        = 48   # When idle sessions are archived
```

### Shell Command Allowlist

By default, the shell tool is enabled with `allow = ["*"]` and shell operators enabled. To restrict it explicitly in `.oxydra/agent.toml`:

```toml
[tools.shell]
enabled          = true                               # Workspace-wide default for shell access
replace_defaults = true                               # Replace the permissive default
allow            = ["npm", "make", "docker", "rg"]   # Explicit allowlist
deny             = ["rm"]                             # Block specific commands
allow_operators  = true                               # Enable &&, ||, |, $() chaining
env_keys         = ["NPM_TOKEN", "GH_TOKEN"]          # Forward specific env vars into the shell
command_timeout_secs = 60                             # Max seconds per shell command
```

To replace the default list entirely and have full control over allowed commands:

```toml
[tools.shell]
replace_defaults = true
allow            = ["git", "python3", "pip", "npm"]
```

If you need to disable shell for a specific user while keeping it on globally, set `behavior.shell_enabled = false` in that user's config.

### Skills: Custom Workflows and Overrides

Skills are markdown files that teach the agent domain-specific workflows by extending its system prompt. Oxydra ships with a built-in **Browser Automation** skill; you can author your own or override the built-ins.

#### Writing a custom skill

A skill is a folder containing a `SKILL.md` file with YAML frontmatter followed by markdown content:

```markdown
---
name: my-git-workflow
description: Git conventions and PR format for this project
activation: always
priority: 80
---

## Git Workflow

Always create a branch before making changes: `git checkout -b feat/<name>`

Commit messages must follow Conventional Commits: `feat:`, `fix:`, `docs:`, etc.
PRs should reference the issue with `Closes #<issue-number>`.
```

**Frontmatter fields:**

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | Yes | — | Unique identifier (kebab-case). Used for deduplication across locations. |
| `description` | Yes | — | One-line summary shown in diagnostic logs. |
| `activation` | No | `auto` | `auto` — inject when all conditions are met; `always` — always inject; `manual` — never auto-inject. |
| `requires` | No | `[]` | Tool names that must be **ready** for this skill to activate (e.g. `["shell_exec"]`). |
| `env` | No | `[]` | Environment variable names that must be set. Their values are available as `{{VAR}}` placeholders in the skill body (for non-sensitive values like URLs). |
| `priority` | No | `100` | Ordering when multiple skills are active; lower numbers appear earlier in the prompt. |

Skills are capped at approximately **3,000 tokens** (~12 KB) to avoid prompt bloat. For large reference material (full API docs, parameter tables), keep your `SKILL.md` concise and place supplementary files in a `references/` subfolder — then mention in the skill body that the agent can `cat` those files on demand.

#### Where to place skills

| Location | Scope | Path |
|---|---|---|
| Workspace skill | Project-specific (highest priority) | `.oxydra/skills/<SkillName>/SKILL.md` |
| User skill (home) | Shared across all projects for one user | `~/.oxydra/skills/<SkillName>/SKILL.md` |
| User skill (XDG) | Shared across all projects for one user | `~/.config/oxydra/skills/<SkillName>/SKILL.md` |
| System skill | Machine-wide shared defaults | `/etc/oxydra/skills/<SkillName>/SKILL.md` |

The **same `name` field** determines which skill wins when multiple tiers define the same skill: workspace overrides `~/.oxydra`, which overrides `~/.config/oxydra`, which overrides `/etc/oxydra`, which overrides the embedded built-ins.

#### Overriding or disabling a built-in skill

To override the built-in browser automation skill with a customized version, create a `SKILL.md` with the **exact same `name`** at workspace or user level:

```markdown
---
name: browser-automation     # Must match the built-in name exactly
description: My custom browser skill
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

## Browser Automation (Custom)

...your customized instructions...
```

To effectively **disable** a built-in skill, place a version with `activation: manual` and the matching `name` at workspace or user level — `manual` skills are never auto-injected.
