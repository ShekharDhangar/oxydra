# Chapter 12: External Channels and Identity

> **Status:** Implemented
> **Implemented:** Telegram adapter (long-polling, edit-message streaming, command interception), sender auth/audit, durable channel session mapping, forum-topic threading, multi-modal input/output, `send_media` tool, proactive notifications
> **Remaining:** Discord, Slack, WhatsApp adapters (deferred)
> **Last verified against code:** 2026-02-28

## Overview

Oxydra supports external channel adapters (Telegram, and future Discord/Slack/WhatsApp) that run as in-process components inside the VM alongside the gateway. Each adapter calls the gateway's internal API directly — no WebSocket overhead.

The foundation for external channels is built in layers:
- **Config types** (`ChannelsConfig`, `TelegramChannelConfig`, `SenderBinding`) in the `types` crate define per-user channel configuration
- **Sender authentication** (`SenderAuthPolicy`) in the `channels` crate implements default-deny authorization
- **Audit logging** (`AuditLogger`) in the `channels` crate records rejected sender events
- **Bootstrap propagation** — the runner includes channel config in the `RunnerBootstrapEnvelope` and forwards bot token env vars to the VM
- The `Channel` trait (defined in `types`) is for WebSocket-based client adapters (TUI); in-process adapters use the gateway's internal API directly and do not implement `Channel`

## Architecture

### Two Adapter Patterns

Oxydra has two distinct adapter patterns for connecting to the gateway:

1. **WebSocket client adapters** (TUI): Implement the `Channel` trait, connect over WebSocket, and communicate via the gateway protocol frames. The TUI is the primary example.

2. **In-process adapters** (Telegram, future Discord/Slack): Run inside the VM alongside the gateway. They call the gateway's internal API methods directly (`create_or_get_session()`, `submit_turn()`, `subscribe_events()`, etc.). This avoids WebSocket overhead and provides identical behavior to WebSocket clients since both call the same underlying methods.

### Channel Adapters Run Inside the VM

Channel adapters run inside the VM (same process as the gateway), not in the runner:
- Gateway is in the same process — direct function calls, no WebSocket overhead
- Adapter lifecycle matches VM lifecycle automatically — no separate management
- Each VM handles only its own user's bot — no multi-user routing complexity
- Follows the same pattern as provider, memory, scheduler — everything runs in the VM
- Bot tokens are same trust level as LLM API keys, which already enter the VM

### What Remains Outside the VM (Host-Side, in Runner)

- `RunnerUserConfig` with `channels` section — config source of truth
- `RunnerBootstrapEnvelope` carries channels config into the VM
- Bot token env var forwarding (runner reads `bot_token_env`, forwards the value)
- Everything else about channels (auth, adapters, session mapping, audit) runs inside the VM

## Per-User Channel Configuration

Channel configuration lives in `RunnerUserConfig` (per-user, host-side config at `users/<user>/config.toml`). It is delivered to the VM via the `RunnerBootstrapEnvelope`.

### Configuration Types

```rust
// types/src/runner.rs

/// Per-user channel configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramChannelConfig>,
    // Future: discord, whatsapp, etc.
}

/// Telegram channel adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    pub enabled: bool,                        // default: false
    pub bot_token_env: Option<String>,        // env var name holding the bot token
    pub polling_timeout_secs: u64,            // default: 30
    pub senders: Vec<SenderBinding>,          // authorized sender identities
    pub max_message_length: usize,            // default: 4096
}

/// A sender identity binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderBinding {
    pub platform_ids: Vec<String>,            // platform-specific sender IDs
    pub display_name: Option<String>,         // human-readable name for audit
}
```

### Example Configuration

```toml
# users/alice/config.toml (RunnerUserConfig — per-user, host-side)

[channels.telegram]
enabled = true
bot_token_env = "ALICE_TELEGRAM_BOT_TOKEN"
polling_timeout_secs = 30
max_message_length = 4096

# Authorized senders — only these platform IDs can interact
[[channels.telegram.senders]]
platform_ids = ["12345678"]         # Alice's Telegram user ID
display_name = "Alice"

[[channels.telegram.senders]]
platform_ids = ["87654321", "11223344"]  # Bob has two Telegram accounts
display_name = "Bob"
```

Telegram requires `[memory] enabled = true` in `agent.toml` because channel session mappings and resumed sessions use the shared session store.

### Bootstrap Propagation

The `RunnerBootstrapEnvelope` includes an optional `channels: Option<ChannelsConfig>` field. The runner populates it from the user's config and also forwards bot token environment variables to the VM container alongside existing API key env vars.

```rust
// In runner's start_user_for_host():
let bootstrap = RunnerBootstrapEnvelope {
    // ... existing fields ...
    channels: if user_config.channels.is_empty() {
        None
    } else {
        Some(user_config.channels.clone())
    },
};
```

### Config Design Principles

- All new config sections use `#[serde(default)]` so existing configs work without modification
- `RunnerUserConfig.channels` defaults to empty (no channels enabled)
- `TelegramChannelConfig.enabled` defaults to `false`
- `TelegramChannelConfig.senders` defaults to empty vec (nobody can interact)
- Telegram adapters require the agent memory backend to be enabled because session mappings are stored in the shared session store
- Channels config is per-user because bot tokens and sender bindings differ per user
- Agent behavior config (`agent.toml`) remains separate — channels config doesn't belong there

## Sender Authentication

### Default-Deny Ingress

Every inbound message must pass sender authentication before it reaches the agent runtime. The policy is **default-deny**: only platform IDs explicitly listed in the configuration are allowed to interact.

### SenderAuthPolicy

Implemented in `channels/src/sender_auth.rs`:

```rust
pub struct SenderAuthPolicy {
    authorized: HashSet<String>,  // flattened set of all platform IDs
}

impl SenderAuthPolicy {
    pub fn from_bindings(bindings: &[SenderBinding]) -> Self;
    pub fn is_authorized(&self, platform_id: &str) -> bool;
    pub fn authorized_count(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

The policy is built from the user's configured `SenderBinding` list. All `platform_ids` from all bindings are flattened into a single `HashSet` for O(1) lookup. Empty bindings produce a policy that rejects everyone.

### Authorization Model

Binary decision: a sender is either **authorized** or **rejected**.

- **Authorized senders** (listed in `channels.*.senders`): Messages are processed as normal user turns. The agent sees them as `MessageRole::User`, identical to TUI input.
- **Unknown senders** (not in the list): Rejected silently. Audit log entry created. No response sent (prevents enumeration).

All authorized senders are treated identically as the owning user — there is no role hierarchy or permission differentiation. If alice authorizes Bob's Telegram ID, Bob's messages are processed exactly as if alice typed them in the TUI.

### Validation Flow

```
Platform message arrives
        │
        ▼
Extract platform sender ID
(Telegram: message.from.id)
        │
        ▼
sender_auth.is_authorized(sender_id)
        │
        ├── true → route to gateway (submit_turn)
        │
        └── false → audit_logger.log_rejected_sender() + silent drop
```

## Audit Logging

Implemented in `channels/src/audit.rs`:

```rust
pub struct AuditEntry {
    pub timestamp: String,          // ISO 8601 UTC
    pub channel: String,            // e.g., "telegram"
    pub sender_id: String,          // rejected platform ID
    pub reason: String,             // brief rejection reason
    pub context: Option<String>,    // optional context (chat_id, etc.)
}

pub struct AuditLogger {
    log_path: PathBuf,
}
```

### Behavior

- Writes JSON-lines to `<workspace>/.oxydra/sender_audit.log`
- Each line is a self-contained JSON object
- Parent directories created automatically on first write
- Append-only (no rotation — simple for v1)
- Failures to write are logged via `tracing::warn` but **never propagated** — audit logging must not break message processing

### Example Audit Line

```json
{"timestamp":"2026-02-25T12:00:00Z","channel":"telegram","sender_id":"99999999","reason":"sender not in authorized list","context":"chat_id=12345"}
```

## Session Identity Mapping

### The Problem

A single user may connect through multiple channels (TUI, Telegram) and each channel has its own session semantics. Without explicit mapping, each channel creates isolated sessions with fragmented context.

### Canonical Session Identity

Each unique `(channel_id, channel_context_id)` maps to one gateway session:

- `channel_id` — the channel adapter identifier (e.g., "telegram")
- `channel_context_id` — the platform-specific session context, derived per platform (D14 in the plan):
  - **Telegram (forum groups):** `"{chat_id}:{message_thread_id}"` — each topic is a separate session
  - **Telegram (regular chats/DMs):** `"{chat_id}"` — single session per chat
  - **Discord:** `"{guild_id}:{channel_id}:{thread_id?}"` — threads are separate sessions

This means each topic/thread gets its own session with its own `active_turn` — enabling true concurrency within a single chat. Within a single topic, "turn already active" still applies naturally.

### Database-Backed Mapping

Channel session mappings are persisted in the `channel_session_mappings` table (migration 0021):

```sql
CREATE TABLE channel_session_mappings (
    channel_id          TEXT NOT NULL,
    channel_context_id  TEXT NOT NULL,
    session_id          TEXT NOT NULL REFERENCES gateway_sessions(session_id) ON DELETE CASCADE,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (channel_id, channel_context_id)
);
```

The `SessionStore` trait (in `types`) provides `get_channel_session()` and `set_channel_session()` methods. The `ChannelSessionMap` wrapper (in `channels/src/session_map.rs`) provides a thin adapter-friendly API:

```rust
pub struct ChannelSessionMap {
    store: Arc<dyn SessionStore>,
}

impl ChannelSessionMap {
    pub async fn get_session_id(&self, channel_id: &str, channel_context_id: &str) -> Result<Option<String>, MemoryError>;
    pub async fn set_session_id(&self, channel_id: &str, channel_context_id: &str, session_id: &str) -> Result<(), MemoryError>;
}
```

When a mapped session is evicted from gateway memory due idle TTL, the mapping remains durable. The next inbound message resolves the same `session_id` from `channel_session_mappings`, and the gateway resumes that session from the store before handling the turn.

### Cross-Channel Continuity

Different channels for the same user share the same workspace and memory namespace (keyed by `user_id`). Conversation threads are independent per channel — a user can start a task in the TUI and check on workspace state from Telegram, but the conversation histories are separate.

## Why Not Dynamic Onboarding?

For the initial implementation, we deliberately avoid invite-code or OAuth flows because:
1. They add attack surface (invite code leakage, phishing)
2. They require state management for pending invites
3. They're unnecessary for the primary use case (personal agent)
4. Pre-configured binding is zero-trust: only the operator with file system access can authorize senders

Dynamic onboarding can be added later as an enhancement on top of the static binding model.

## Implementation Status

| Component | Status | Location |
|-----------|--------|----------|
| `ChannelsConfig`, `TelegramChannelConfig`, `SenderBinding` types | ✅ Implemented | `types/src/runner.rs` |
| Bootstrap envelope propagation | ✅ Implemented | `runner/src/lib.rs` |
| Bot token env var forwarding | ✅ Implemented | `runner/src/lib.rs` |
| `SenderAuthPolicy` | ✅ Implemented | `channels/src/sender_auth.rs` |
| `AuditLogger` + `AuditEntry` | ✅ Implemented | `channels/src/audit.rs` |
| Channel session mapping (DB-backed) | ✅ Implemented | `types/src/session.rs`, `memory/src/session_store.rs`, `channels/src/session_map.rs` |
| `channel_session_mappings` DB migration | ✅ Implemented | `memory/migrations/0021_create_channel_session_mappings.sql` |
| `ChannelSessionMap` wrapper | ✅ Implemented | `channels/src/session_map.rs` |
| Telegram adapter (`TelegramAdapter`) | ✅ Implemented | `channels/src/telegram.rs` |
| Edit-message streaming (`ResponseStreamer`) | ✅ Implemented | `channels/src/telegram.rs` |
| Markdown → Telegram HTML conversion | ✅ Implemented | `channels/src/telegram.rs` |
| Telegram command interception (`/new`, `/sessions`, `/switch`, `/cancel`, `/cancelall`, `/status`) | ✅ Implemented | `channels/src/telegram.rs` |
| Adapter spawning in oxydra-vm | ✅ Implemented | `runner/src/bin/oxydra-vm.rs` |
| Feature-flagged `telegram` in channels + runner | ✅ Implemented | `channels/Cargo.toml`, `runner/Cargo.toml` |
| Discord/Slack/WhatsApp adapters | Deferred | — |

## Telegram Adapter

### Overview

The Telegram adapter (`channels/src/telegram.rs`, feature-gated behind `telegram`) is an in-process component that runs alongside the gateway inside the VM. It uses the `frankenstein` crate (v0.47, `client-reqwest` feature) for Telegram Bot API access.

### Architecture

```
Telegram API (long-polling)
    │
    ▼
TelegramAdapter::run() loop
    ├── bot.get_updates() → Update list
    │
    ▼ per Update
    ├── Extract sender ID (message.from.id)
    ├── SenderAuthPolicy.is_authorized() → reject + audit, or continue
    ├── derive_channel_context_id(chat_id, thread_id)
    ├── Command interception (/new, /sessions, /switch, /cancel, /cancelall, /status, /help)
    │    └── Call gateway internal API directly
    ├── ChannelSessionMap.get_session_id() → resolve or create session
    ├── gateway.subscribe_events() (before submit, to not miss frames)
    ├── gateway.submit_turn_from_channel("telegram", channel_context_id) → start the turn with origin
    │
    ▼
ResponseStreamer (edit-message streaming)
    ├── send_message("⏳ Working...") → placeholder
    ├── TurnProgress → edit with status line
    ├── AssistantDelta → accumulate + throttled edit (1.5s)
    ├── Message splitting → new message at ~3896 chars
    └── TurnCompleted → final edit with Markdown→HTML, fallback to plain text
```

### Edit-Message Streaming (D15)

The adapter uses Telegram's `editMessageText` API to stream responses live:

1. **Turn starts** → Send placeholder "⏳ Working..."
2. **Progress events** → Edit message with status ("🔍 Searching the web...")
3. **Token deltas** → Accumulate text, edit message every 1.5 seconds
4. **Near char limit** → Stop editing, send new continuation message
5. **Turn completed** → Final edit with complete response (Markdown→HTML)

The 1.5-second throttle stays safely within Telegram's ~30 edits/minute rate limit.

### Markdown → Telegram HTML

The `markdown_to_telegram_html()` utility converts common Markdown to Telegram's HTML subset:

| Markdown | Telegram HTML |
|----------|--------------|
| `**bold**` | `<b>bold</b>` |
| `*italic*` | `<i>italic</i>` |
| `` `code` `` | `<code>code</code>` |
| ```` ```code``` ```` | `<pre>code</pre>` |
| `[text](url)` | `<a href="url">text</a>` |
| `~~strike~~` | `<s>strike</s>` |
| `# Header` | `<b>Header</b>` |

HTML conversion is used only in the final edit. Interim edits use plain text for speed. If HTML parsing fails (Telegram returns an error), the adapter falls back to plain text.

### Commands

| Command | Description |
|---------|-------------|
| `/new [name]` | Create a new session (optionally named) |
| `/sessions` | List active sessions |
| `/switch <id>` | Switch to a different session |
| `/cancel` | Cancel the active turn |
| `/cancelall` | Cancel active turns across all sessions for the user |
| `/status` | Show current session info |
| `/start`, `/help` | Show help text |

### Feature Flag

The Telegram adapter is behind the `telegram` feature flag in both the `channels` and `runner` crates. It's included in default features for both crates.

```toml
# channels/Cargo.toml
[features]
default = ["telegram"]
telegram = ["dep:frankenstein", "dep:gateway", "dep:tokio", "dep:tokio-util", "dep:uuid"]

# runner/Cargo.toml
[features]
default = ["telegram"]
telegram = ["dep:channels", "channels/telegram"]
```

## Design Boundaries

- Channel adapters never access the runtime directly — all routing flows through the gateway's internal API
- Platform-specific SDK dependencies are fully contained within their feature-flagged adapter code — no platform types leak into `types` or `runtime`
- Sender authentication is non-negotiable: there is no "open mode" that skips allowlist validation for external channels
- The TUI remains a WebSocket client adapter, not a privileged path — it follows the same gateway protocol as always
- In-process adapters use the gateway's internal API; the existing `Channel` trait is for WebSocket-based client adapters only

## Channel Capabilities and Rich Media

### Overview

When connected via a rich channel (Telegram, Discord, etc.), the agent is aware of the channel's media capabilities and can send photos, audio, documents, videos, and voice messages to the user. This is implemented through:

1. **Channel capabilities** — A `ChannelCapabilities` struct describes what each channel supports
2. **System prompt augmentation** — The runtime injects channel-specific instructions into the system prompt per-session
3. **`send_media` tool** — A tool that reads workspace files and delivers them through the channel
4. **StreamItem::Media pipeline** — Media flows through the existing event streaming infrastructure

### Channel Capabilities

Defined in `types/src/channel.rs`:

```rust
pub struct ChannelCapabilities {
    pub channel_type: String,          // "tui", "telegram", "discord", etc.
    pub media: MediaCapabilities,
}

pub struct MediaCapabilities {
    pub photo: bool,       // images (JPEG, PNG, GIF)
    pub audio: bool,       // audio files (MP3, OGG)
    pub document: bool,    // arbitrary file attachments
    pub voice: bool,       // voice messages (OGG/OPUS)
    pub video: bool,       // video files (MP4)
}
```

Capabilities are resolved per-turn from the `TurnOrigin.channel_id` via `ChannelCapabilities::from_channel_origin()`. Known channel types (e.g. "telegram") get full media capabilities; unknown types default to text-only.

### Proactive Notifications

External channels that support sending unsolicited messages implement the `ProactiveSender` trait (defined in `types/src/proactive.rs`):

```rust
#[async_trait]
pub trait ProactiveSender: Send + Sync {
    async fn send_notification(&self, channel_context_id: &str, frame: GatewayServerFrame);
}
```

At startup, each channel adapter registers its proactive sender with `GatewayServer::register_proactive_sender()`. For example, Telegram registers a `TelegramProactiveSender` that converts the `channel_context_id` (format: `{chat_id}` or `{chat_id}:{thread_id}`) back into a Telegram chat target and sends the notification message via the Bot API.

When a scheduled task fires and its notification policy requires delivery, the `GatewayServer::notify_user()` implementation looks up the schedule's `channel_id`, finds the matching `ProactiveSender`, and calls `send_notification()` with the `channel_context_id` and notification frame.

### System Prompt Augmentation

When a session is connected via a media-capable channel, the runtime appends a "Channel Media Capabilities" section to the system prompt. This section:
- Tells the agent what media types it can send
- Explains how to use the `send_media` tool
- Encourages the agent to send actual files instead of just describing them

The augmentation happens per-session in `run_session_internal()` based on the `ToolExecutionContext.channel_capabilities`. Sessions from the TUI get no augmentation (text-only).

### The `send_media` Tool

**File:** `tools/src/media_tools.rs`

The `send_media` tool allows the agent to deliver workspace files as media attachments:

```
send_media(path: "/shared/chart.png", media_type: "photo", caption: "Monthly sales chart")
```

**Parameters:**
- `path` — Workspace file path (e.g. `/shared/output.pdf`, `/tmp/audio.mp3`)
- `media_type` — One of: `photo`, `audio`, `document`, `voice`, `video`
- `caption` — Optional description

**How it works:**
1. Validates channel supports the requested media type
2. Reads file bytes from the workspace path through the sandbox runner (`file_read_bytes` under the `FileReadOnly` capability profile)
3. Emits a `StreamItem::Media(MediaAttachment)` through the `ToolExecutionContext.event_sender`
4. Runtime scrubs `MediaAttachment.file_path` to virtual paths before forwarding to gateway/channel adapters
5. Returns a confirmation message to the agent

The tool is registered globally but validates channel capabilities at runtime — calling it from a text-only channel (TUI) returns a clear error.

### Media Pipeline

```
Agent calls send_media tool
    │
    ▼
Tool reads file, emits StreamItem::Media(MediaAttachment)
    │
    ▼ via ToolExecutionContext.event_sender
Runtime scrubs MediaAttachment.file_path
    │
    ▼
RuntimeGatewayTurnRunner forwards StreamItem::Media to gateway
    │
    ▼
Gateway publishes GatewayServerFrame::MediaAttachment
    │
    ▼ via session broadcast
Channel adapter receives frame
    │
    ├── Telegram: calls send_photo / send_document / send_audio / etc.
    ├── TUI: shows "📎 Sent photo: chart.png" system message
    └── Future channels: handle per their capabilities
```

### Telegram Media Handling

The Telegram adapter handles `GatewayServerFrame::MediaAttachment` by:
1. Writing file bytes to a temporary file (frankenstein requires file paths for upload)
2. Calling the appropriate Telegram API method (`send_photo`, `send_document`, `send_audio`, `send_voice`, `send_video`)
3. Cleaning up the temporary file
4. On failure, sending a text fallback message

## Receiving Media from Users (Multi-Modal Input)

### Overview

Users can send rich media (photos, audio, voice messages, video, documents) to the agent through Telegram. The adapter extracts media attachments, downloads them from Telegram servers, and passes them through the gateway to the LLM provider as inline multi-modal input.

### Telegram Media Extraction

The `TelegramAdapter` handles the following Telegram message types:

| Message type | Default MIME | Notes |
|-------------|-------------|-------|
| Photo | `image/jpeg` | Telegram provides multiple sizes; the largest is selected |
| Voice | `audio/ogg` | Voice messages recorded in Telegram |
| Audio | `audio/mpeg` | Audio files sent as media |
| Video | `video/mp4` | Video files |
| Video note | `video/mp4` | Round video messages |
| Document | `application/octet-stream` | Generic file attachments |

For media messages, the `caption` field is used as the text prompt. If no caption is provided, a placeholder text `"[The user sent media without a caption.]"` is used so the model knows media was sent.

### Download and Size Limits

| Limit | Value |
|-------|-------|
| Max file size | 10 MB |
| Max attachments per message | 4 |

File size is checked both via the Telegram API's reported `file_size` (before downloading) and during streaming download (to handle cases where the reported size is incorrect). Downloads use a 30-second timeout.

The download uses a streaming approach with chunk-by-chunk accumulation, aborting early if the file exceeds the size limit during download rather than loading the entire file first.

### Data Flow

```
Telegram user sends photo with caption
    │
    ▼
TelegramAdapter.extract_media_attachments()
    ├── Telegram Bot API: getFile → resolve file_path
    ├── Check file_size before download (if available)
    ├── Download from https://api.telegram.org/file/bot<token>/<path>
    ├── Streaming download with per-chunk size validation
    └── Create InlineMedia { mime_type, data }
    │
    ▼
GatewaySendTurn { prompt: caption, attachments: [InlineMedia] }
    │
    ▼
Gateway validates attachment limits
    │
    ▼
Turn runner strips older attachment bytes, appends new user message
    │
    ▼
Runtime context budget management (with media-aware handling)
    │
    ▼
Provider validates modality support, encodes in provider wire format
    │
    ▼
LLM receives multi-modal input
```

The same inbound `InlineMedia` payloads are also exposed to tools through `ToolExecutionContext.inbound_attachments` for the current turn only. This enables explicit persistence flows such as `attachment_save(index, path)` into `/shared` or `/tmp` without auto-writing files.

### Command Interception

Commands (`/new`, `/sessions`, etc.) are only intercepted for text-only messages. Media messages with a caption starting with `/` are treated as normal media turns, not commands.
