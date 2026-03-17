# Plan: Reliable Scheduled-Task Notification Delivery

Status: Proposed
Created: 2026-03-17

## Executive summary

Scheduled-task notifications are silently dropped in two common scenarios:

1. **TUI — session changed.** The originating session ID is frozen in the `ScheduleDefinition` at creation time. If the user has since created a new session (`/new`), let the old session expire via TTL, or restarted the process, the notification has nowhere to go and is discarded with a `debug!` log.

2. **Telegram — topic deleted.** The originating `channel_context_id` encodes `{chat_id}:{thread_id}`. If that forum topic is deleted, the Telegram Bot API returns a 400 error inside a fire-and-forget task. The error is never observed; the notification is lost.

This plan fixes both with minimal-surface changes — no new tables, no new traits, no cross-crate dependencies. Each phase is a single-file change.

---

## Background: how notification routing works today

### Origin capture

When any user turn enters the gateway, `TurnOrigin` is populated with the ingress channel:

- **TUI / WebSocket** — `channel_id = "tui"`, `channel_context_id = <session_uuid>`
  (`gateway/lib.rs` around `start_turn_with_origin`)
- **Telegram** — `channel_id = "telegram"`, `channel_context_id = "{chat_id}:{thread_id}"` for forum topics, `"{chat_id}"` for regular chats/DMs
  (`channels/src/telegram.rs:231` — `derive_channel_context_id`)

The `schedule_create` tool captures both fields from `ToolExecutionContext` and writes them to the `ScheduleDefinition` row. They are **never updated** after creation.

### Scheduler execution

Each run executes in a dedicated session: `scheduled:{schedule_id}` (`scheduler_executor.rs:93`). The originating session is not involved in execution — only in notification delivery.

### Notification dispatch (`gateway/lib.rs:1466`)

```
channel_id == "tui"
  └─ look up channel_context_id (= session_id) in user's in-memory sessions
      ├─ found + connected  → session.publish(frame)   ✅
      └─ not found          → debug log, silent drop    ❌

channel_id == "telegram" (or other)
  └─ call registered ProactiveSender.send_proactive(channel_context_id, frame)
      └─ TelegramProactiveSender parses "{chat_id}:{thread_id}", calls Bot API
          ├─ topic exists   → delivered                 ✅
          └─ topic deleted  → Bot API 400, discarded    ❌

channel_id == None (legacy)
  └─ broadcast to all in-memory sessions for user       ✅ (but no channel specificity)
```

---

## Phase 1: TUI — fallback to most-recently-active connected session

### Problem in detail

`SessionState` (`gateway/src/session.rs`) has:
- `events: broadcast::Sender<GatewayServerFrame>` — `receiver_count()` is the number of live WebSocket subscribers
- `last_activity_epoch_secs: AtomicU64` — updated by `mark_active()` on each turn
- `channel_origin: String` — records which channel created this session ("tui", "telegram", …)
- `parent_session_id: Option<String>` — `Some(…)` for subagent sessions

The current code only tries the exact session ID stored at schedule-creation time. When that session is gone (evicted, new session created), the `else` branch at line 1481 logs at `debug!` and returns — the frame is silently dropped.

### Proposed change

**File:** `crates/gateway/src/lib.rs`
**Function:** `impl SchedulerNotifier for GatewayServer` → `notify_user`

Replace the TUI branch with a two-step approach:

1. **Primary attempt**: deliver to the exact origin session (unchanged).
2. **Fallback**: if the origin session is not present in `user.sessions`, find the most recently active TUI session that:
   - has at least one live subscriber (`receiver_count() > 0`)
   - has `channel_origin == GATEWAY_CHANNEL_ID` (avoids accidentally routing to a Telegram-origin session)
   - has `parent_session_id == None` (excludes subagent sessions)

If no fallback session is found, log at `info!` (not `debug!`) with the schedule ID and user ID so it is visible without verbose logging.

### Code change (conceptual diff)

```rust
// Before (lines ~1469–1485)
if channel_id == GATEWAY_CHANNEL_ID {
    if let Some(ref session_id) = schedule.channel_context_id {
        let users = self.users.read().await;
        if let Some(user) = users.get(&schedule.user_id) {
            let sessions = user.sessions.read().await;
            if let Some(session) = sessions.get(session_id) {
                session.publish(frame);
            } else {
                tracing::debug!(
                    schedule_id = %schedule.schedule_id,
                    session_id = %session_id,
                    "scheduler notification: TUI session not connected"
                );
            }
        }
    }
}

// After
if channel_id == GATEWAY_CHANNEL_ID {
    let users = self.users.read().await;
    if let Some(user) = users.get(&schedule.user_id) {
        let sessions = user.sessions.read().await;

        // Primary: deliver to origin session if it is still active.
        let origin_session = schedule
            .channel_context_id
            .as_deref()
            .and_then(|id| sessions.get(id));

        if let Some(session) = origin_session {
            session.publish(frame);
        } else {
            // Fallback: most recently active connected top-level TUI session.
            let fallback = sessions
                .values()
                .filter(|s| {
                    s.channel_origin == GATEWAY_CHANNEL_ID
                        && s.parent_session_id.is_none()
                        && s.events.receiver_count() > 0
                })
                .max_by_key(|s| s.last_activity_epoch_secs.load(Ordering::Relaxed));

            if let Some(session) = fallback {
                tracing::info!(
                    schedule_id = %schedule.schedule_id,
                    origin_session_id = ?schedule.channel_context_id,
                    fallback_session_id = %session.session_id,
                    "scheduler notification: origin TUI session not found, delivering to most-recent active session"
                );
                session.publish(frame);
            } else {
                tracing::info!(
                    schedule_id = %schedule.schedule_id,
                    user_id = %schedule.user_id,
                    "scheduler notification: no connected TUI session for user, notification dropped"
                );
            }
        }
    }
}
```

Note: `last_activity_epoch_secs` is currently `pub(crate)` on `SessionState` and the `Ordering` re-export is already in scope via `std::sync::atomic::Ordering` — no new imports needed.

### Tests to add (`gateway/src/tests.rs` or inline)

1. **Origin session present** — delivers to origin session, not to a second connected session.
2. **Origin session evicted, fallback session connected** — delivers to fallback; log message at `info!` contains both session IDs.
3. **Origin session evicted, no connected TUI sessions** — no panic; `info!` log says "notification dropped".
4. **Origin session evicted, only subagent sessions connected** — subagent sessions are excluded; "notification dropped" path taken.
5. **Origin session evicted, multiple fallback candidates** — delivers to the one with the highest `last_activity_epoch_secs`.

### What this does NOT change

- Telegram routing — unaffected.
- The schedule definition schema — no DB change.
- The `SchedulerNotifier` trait — unchanged.
- The `channel_context_id` value stored at creation time — unchanged; it is still used as the primary delivery target.

---

## Phase 2: Telegram — graceful thread-not-found fallback

### Problem in detail

`TelegramProactiveSender::send_proactive` (`channels/src/telegram.rs:1457`) is a synchronous trait method that spawns a detached `tokio::task` to call the Bot API. The spawned task currently discards the `Result` from every `bot.send_message(…)` call.

When a forum topic is deleted, Telegram returns:
```
HTTP 400 Bad Request
{ "ok": false, "error_code": 400, "description": "Bad Request: message thread not found" }
```

The `frankenstein` library surfaces this as `frankenstein::Error::Api(frankenstein::ErrorResponse)` where `ErrorResponse` has fields `error_code: u64` and `description: String`.

### Proposed change

**File:** `crates/channels/src/telegram.rs`
**Function:** `TelegramProactiveSender::send_proactive`

In the spawned task, after the primary `send_message` call:

1. If the result is `Ok(…)` — done.
2. If the error is a `frankenstein::Error::Api` with `error_code == 400` and `description` contains `"message thread not found"`:
   - Log at `warn!` level with `channel_context_id`, `chat_id`, `thread_id`.
   - Retry `send_message` with `message_thread_id: None` (main group chat) and `parse_mode` and `text` unchanged.
   - Log the outcome of the retry at `info!` (success) or `warn!` (failure).
3. Any other error — log at `warn!` with the full error.

The retry uses the same `chat_id` parsed from `channel_context_id`. Only `message_thread_id` changes.

### Code change (conceptual diff)

```rust
// In the spawned task inside send_proactive, replacing the current fire-and-forget:

let primary_result = bot
    .send_message(&SendMessageParams {
        chat_id: ChatId::Integer(chat_id),
        text: html_text.clone(),
        message_thread_id: thread_id,
        parse_mode: Some(ParseMode::Html),
        // … other fields …
    })
    .await;

match primary_result {
    Ok(_) => { /* delivered */ }
    Err(ref e) if is_thread_not_found(e) => {
        // Topic was deleted — retry to the main group chat.
        warn!(
            channel_context_id = %ctx_id,
            chat_id,
            thread_id = ?thread_id,
            "telegram proactive: forum topic not found, retrying to main chat"
        );
        let fallback_result = bot
            .send_message(&SendMessageParams {
                chat_id: ChatId::Integer(chat_id),
                text: html_text.clone(),
                message_thread_id: None,  // <-- main chat
                parse_mode: Some(ParseMode::Html),
                // … same other fields …
            })
            .await;
        match fallback_result {
            Ok(_) => {
                info!(chat_id, "telegram proactive: fallback to main chat delivered");
            }
            Err(e2) => {
                warn!(chat_id, error = %e2, "telegram proactive: fallback to main chat also failed");
            }
        }
    }
    Err(e) => {
        warn!(
            channel_context_id = %ctx_id,
            error = %e,
            "telegram proactive: send_message failed"
        );
    }
}
```

Where `is_thread_not_found` is a small private helper:

```rust
fn is_thread_not_found(error: &frankenstein::Error) -> bool {
    match error {
        frankenstein::Error::Api(ref resp) => {
            resp.error_code == 400
                && resp.description.contains("message thread not found")
        }
        _ => false,
    }
}
```

This helper isolates the Telegram-specific error string matching, making it easy to extend if Telegram changes the wording.

The same pattern must be applied to the other media-send paths in `send_proactive` (`SendPhotoParams`, `SendAudioParams`, etc.) — each currently has the same fire-and-forget structure. They all follow the same fix: check result, detect thread-not-found, retry without `thread_id`.

### Deduplication note

The text-send path in `send_proactive` already handles long messages by splitting into chunks (see `split_message` logic in the file). The retry must use the same chunked send so the fallback behaviour is identical to a normal delivery.

### Tests to add (`channels/src/telegram.rs` tests module or separate integration tests)

1. **`is_thread_not_found` helper** — returns `true` for API error 400 with the expected description, `false` for other codes/descriptions, `false` for non-API errors.
2. **Primary delivery succeeds** — no retry, no log warning.
3. **Thread-not-found triggers fallback** — mock bot returns error on first call (with thread_id), succeeds on second (without thread_id); both calls verified.
4. **Fallback also fails** — both errors are logged at `warn!`, no panic.
5. **Non-400 error** — logged at `warn!`, no retry attempt.

---

## Shared constraints

- **No schema changes.** Both fixes live entirely in the notification-delivery layer. The `ScheduleDefinition` rows are unchanged.
- **No new traits or crate dependencies.** Phase 1 touches one function in `gateway/src/lib.rs`; Phase 2 touches one function (and its sibling send-paths) in `channels/src/telegram.rs`. Neither phase adds imports beyond what is already in scope.
- **Logging level policy.** Silent delivery failures are upgraded from `debug!` to `info!` so they are visible in default log configurations without requiring verbose mode.
- **No behaviour change when everything works.** The primary delivery path is attempted first in both phases; the fallback only fires when the primary path fails.

---

## What this does NOT address (out of scope)

- **Persistent notification queue.** If no TUI session is connected at all when a notification fires, the notification is still lost (logged at `info!`). A durable queue (new `undelivered_notifications` table + drain-on-connect logic) is the right follow-up but is a separate, larger change.
- **Updating `channel_context_id` when sessions change.** Keeping the frozen origin and adding a best-effort fallback is simpler than tracking the "current" session — and avoids the question of which session is "current" when the user has multiple open.
- **Telegram: remembering the fallback permanently.** Phase 2 retries to the main chat on each run but does not update the stored `channel_context_id`. This means every subsequent run for this schedule will try the deleted topic first, fail, and then fall back. Updating the stored destination on confirmed fallback could be added later.
- **Discord, WhatsApp, and other future channels.** Phase 2 is Telegram-specific. Future channel adapters should implement the same pattern in their own `ProactiveSender` implementations.
