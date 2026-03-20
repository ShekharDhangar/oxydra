## Manual Install & Configuration

Use this path for more control over the install process, direct TOML editing instead of the web configurator, or to set up optional features like Telegram.

### 0) Choose a release tag

Pick a version from the [GitHub releases page](https://github.com/shantanugoel/oxydra/releases) and export it once:

```bash
export OXYDRA_TAG=v0.2.7   # replace with the release you want
```

### 1) Install binaries and bootstrap config templates

#### Option A (recommended): one-command installer

```bash
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir "$HOME"
```

Defaults:

- Installs to `~/.local/bin`
- Installs `oxydra`, `oxydra-vm`, `oxydra-shelld`, `oxydra-tui`
- Copies example configs to `<base-dir>/.oxydra/agent.toml`, `<base-dir>/.oxydra/runner.toml`, and `<base-dir>/.oxydra/users/alice.toml`
- On upgrades, verifies release checksum (`SHA256SUMS`), backs up existing binaries/config, and updates existing `runner.toml` `[guest_images]` tags to the installed release tag (without replacing other settings)
- Leaves existing config files unchanged outside the targeted image-tag update (use `--overwrite-config` to replace templates)

Useful variants:

```bash
# Preview upgrade actions without changing anything
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --dry-run

# Install latest release (no pin)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash

# Install to /usr/local/bin (uses sudo)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --system

# Install into a different project directory
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --base-dir /path/to/project

# Install binaries only (skip config scaffolding)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --skip-config

# Non-interactive upgrade (auto-confirm prompts)
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --yes

# Skip Docker pre-pull after install
curl -fsSL https://raw.githubusercontent.com/shantanugoel/oxydra/main/scripts/install-release.sh | bash -s -- --tag "$OXYDRA_TAG" --no-pull
```

#### Option B: manual install

```bash
# Download the correct artifact for your platform:
#   oxydra-<tag>-macos-arm64.tar.gz
#   oxydra-<tag>-linux-amd64.tar.gz
#   oxydra-<tag>-linux-arm64.tar.gz

PLATFORM=linux-amd64   # change for your platform

curl -fL -o "oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz" \
  "https://github.com/shantanugoel/oxydra/releases/download/${OXYDRA_TAG}/oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz"

tar -xzf "oxydra-${OXYDRA_TAG}-${PLATFORM}.tar.gz"

mkdir -p ~/.local/bin
install -m 0755 oxydra ~/.local/bin/oxydra
install -m 0755 oxydra-vm ~/.local/bin/oxydra-vm
install -m 0755 oxydra-shelld ~/.local/bin/oxydra-shelld
install -m 0755 oxydra-tui ~/.local/bin/oxydra-tui
```

Option B installs binaries only. If you want automatic config scaffolding, use Option A.

If `~/.local/bin` is not in `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### 2) Review and configure

If you used Option A, the installer already created:

- `.oxydra/agent.toml`
- `.oxydra/runner.toml`
- `.oxydra/users/alice.toml`

#### Configure `runner.toml`

Verify `.oxydra/runner.toml` guest image tags match the release you installed:

```toml
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:${OXYDRA_TAG}"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:${OXYDRA_TAG}"
```

If you plan to use `micro_vm` instead of `container`:

- macOS: install and run Docker Desktop
- Linux: install `firecracker`, set `default_tier = "micro_vm"`, and configure `guest_images.firecracker_oxydra_vm_config` (plus `guest_images.firecracker_shell_vm_config` if you want shell/browser sidecar)

If you want a user id other than `alice`, update `[users.alice]` in `.oxydra/runner.toml` and rename `.oxydra/users/alice.toml` accordingly.

#### Configure `agent.toml`

Edit `.oxydra/agent.toml`:

- Set `[selection].provider` and `[selection].model`
- Add the matching `[providers.registry.<name>]` entry with the correct `api_key_env`:
  - OpenAI example: `api_key_env = "OPENAI_API_KEY"`
  - Anthropic example: `api_key_env = "ANTHROPIC_API_KEY"`
  - Gemini example: `api_key_env = "GEMINI_API_KEY"`

#### Optional: use the web configurator instead

If you prefer a browser UI to editing TOML directly, the web configurator provides a guided interface for the same settings:

```bash
oxydra --config .oxydra/runner.toml web
```

Open **http://127.0.0.1:9400** and navigate to **Agent Config**. See [Quick Start — step 3](../README.md#3-configure-with-the-web-configurator) for details on the Core Setup section. Once saved, stop the web configurator with `Ctrl+C`.

### 3) Ensure Docker is ready (Linux)

```bash
# Start Docker daemon and enable it on boot
sudo systemctl enable --now docker

# Add your user to the docker group so you don't need sudo
sudo usermod -aG docker $USER
newgrp docker   # apply in the current shell without logging out
```

The guest images are public on ghcr.io and pull without authentication. If you ever hit a `manifest unknown` 404, double-check that the tag in `runner.toml` includes the `v` prefix (e.g. `v0.2.7`, not `0.1.2`).

### 4) Set your provider API key

```bash
export OPENAI_API_KEY=your-key-here
# or: export ANTHROPIC_API_KEY=...
# or: export GEMINI_API_KEY=...
```

### 5) Start and connect

Run the daemon in terminal 1:

```bash
oxydra --config .oxydra/runner.toml --user alice start
```

Connect TUI in terminal 2:

```bash
oxydra --tui --config .oxydra/runner.toml --user alice
```

Lifecycle commands:

```bash
oxydra --config .oxydra/runner.toml --user alice status
oxydra --config .oxydra/runner.toml --user alice stop
oxydra --config .oxydra/runner.toml --user alice restart
```

### 6) If Docker is unavailable

Use process mode (lower safety, no shell/browser tools):

```bash
oxydra --config .oxydra/runner.toml --user alice --insecure start
oxydra --tui --config .oxydra/runner.toml --user alice
```

Even in `--insecure` mode, WASM tool policies still enforce path boundaries and web SSRF checks.

### 7) TUI session commands

| Command | Meaning |
|---|---|
| `/new` | Create a fresh session |
| `/new <name>` | Create a named session |
| `/sessions` | List sessions |
| `/switch <id>` | Switch by session id (prefix supported) |
| `/cancel` | Cancel active turn in current session |
| `/cancelall` | Cancel active turns across sessions |

### 8) (Optional) Enable Telegram

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the bot token.
2. Find your Telegram user ID (for example with [@userinfobot](https://t.me/userinfobot)).
3. Export the bot token before starting the runner:

```bash
export ALICE_TELEGRAM_BOT_TOKEN=your-bot-token
```

4. Ensure `.oxydra/agent.toml` has `[memory] enabled = true`. Current templates enable memory by default; only change this if you disabled it manually.

5. Edit `.oxydra/users/alice.toml` and add/uncomment:

```toml
[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"

[[channels.telegram.senders]]
platform_ids = ["12345678"]
```

6. Restart the runner and message your bot in Telegram.

Only IDs listed in `[[channels.telegram.senders]]` are allowed to interact with your agent.
Telegram supports the same session commands (`/new`, `/sessions`, `/switch`, `/cancel`, `/cancelall`, `/status`).

### 9) (Optional) Web Configurator for guided setup and ongoing management

The web configurator provides a browser-based dashboard for managing Oxydra without editing TOML files directly — useful both for first-run onboarding and day-to-day config changes later.

```bash
# Start the web configurator
oxydra --config .oxydra/runner.toml web

# Custom bind address
oxydra --config .oxydra/runner.toml web --bind 0.0.0.0:8080
```

Then open `http://127.0.0.1:9400` in your browser. The dashboard offers:
- **Config editors** for runner, agent, and user settings (with validation and backups)
- **Guided onboarding** for runner, first-user, provider, tool defaults, and optional Telegram setup
- **Control panel** to start/stop/restart daemons
- **Log viewer** with filtering and auto-refresh
- **Status dashboard** showing registered users and daemon health

The web server binds to localhost only by default. To enable token auth for remote access, add to `runner.toml`:

```toml
[web]
auth_mode = "token"
auth_token_env = "OXYDRA_WEB_TOKEN"
```