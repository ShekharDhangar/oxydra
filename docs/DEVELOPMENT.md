## Use Latest From Repository (and Contribute)

Use this path if you want unreleased changes, local development, or contributions.

### 1) Prerequisites

- Rust stable (`rustup toolchain install stable`)
- WASM target (`rustup target add wasm32-wasip1`)
- Docker (required for `container` and `micro_vm` tiers)
- `micro_vm` on macOS: Docker Desktop
- `micro_vm` on Linux: `firecracker` binary + Firecracker VM config files referenced in `.oxydra/runner.toml`
- Provider API key (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`)

If you want to build guest images locally via cross-compilation:

- `cargo-zigbuild` (`cargo install cargo-zigbuild`)
- `zig` (`brew install zig` on macOS)

### 2) Clone and bootstrap config

```bash
git clone https://github.com/shantanugoel/oxydra.git
cd oxydra

mkdir -p .oxydra/users
cp examples/config/agent.toml .oxydra/agent.toml
cp examples/config/runner.toml .oxydra/runner.toml
cp examples/config/runner-user.toml .oxydra/users/alice.toml
```

Edit `.oxydra/agent.toml` and `.oxydra/runner.toml` for your provider, tier, and image refs.
For most setups, set `default_tier = "container"` in `.oxydra/runner.toml`.

### 3) Build workspace

```bash
cargo build --workspace
```

### 4) Build guest images (for container/micro_vm)

Option A: cross-compile locally (supports both `amd64` and `arm64`):

```bash
./scripts/build-guest-images.sh arm64
# or
./scripts/build-guest-images.sh amd64
```

Option B: in-Docker build (defaults to the host Linux architecture; pass `amd64` or `arm64` explicitly if needed):

```bash
./scripts/build-guest-images-in-docker.sh
# or
./scripts/build-guest-images-in-docker.sh amd64
```

If you used a custom tag, set matching refs in `.oxydra/runner.toml`:

```toml
[guest_images]
oxydra_vm = "oxydra-vm:<tag>"
shell_vm  = "shell-vm:<tag>"
```

### 5) Run from source

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice start

# One-time setup for a fresh clone (skip if oxydra-tui is already in PATH)
cargo install --path crates/tui

cargo run -p runner -- --tui --config .oxydra/runner.toml --user alice
```

Process-tier fallback:

```bash
cargo run -p runner -- --config .oxydra/runner.toml --user alice --insecure start
```

### 6) Quality checks before opening a PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/test-install-release.sh
```

### 7) Build release artifacts (maintainers)

```bash
# macOS arm64 host: builds macOS + linux-amd64 + linux-arm64 artifacts
./scripts/build-release-assets.sh --tag local

# Linux host example
./scripts/build-release-assets.sh --platforms linux-amd64,linux-arm64 --tag local
```

### Documentation and code map

- Architecture guidebook: [`docs/guidebook/README.md`](guidebook/README.md)
- Example configs: [`examples/config/`](../examples/config)
- Workspace layout:

```text
crates/
  types/          Core type definitions, config, model catalog
  provider/       LLM provider implementations
  tools/          Tool trait, core tools, browser tool, and WASM sandboxing
  tools-macros/   #[tool] procedural macro
  runtime/        Agent turn-loop runtime
  memory/         Persistent memory and retrieval
  runner/         Runner lifecycle and guest orchestration
  oxydra-shelld/   Shell daemon protocol
  channels/       External channel adapters (Telegram)
  gateway/        WebSocket gateway server
  tui/            Terminal UI client
```

License: see [LICENSE](../LICENSE).
