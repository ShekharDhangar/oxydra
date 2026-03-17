# Plan: Reliable Scheduled-Task Notification Delivery

Status: Proposed
Created: 2026-03-17
Updated: 2026-03-18

## Executive summary

Scheduled-task notifications are currently unreliable in three concrete cases:

1. **TUI origin session is stale or disconnected.** The schedule stores the session ID captured at creation time. `GatewayServer::notify_user()` only looks up that exact in-memory session. If it is missing, delivery is dropped. If it still exists in memory but has `receiver_count() == 0`, the gateway still publishes into a dead broadcast channel and the notification is effectively lost.

2. **Telegram forum topic is deleted.** The stored `channel_context_id` encodes `{chat_id}:{thread_id}` for forum topics. When the topic is deleted, Telegram returns `400 Bad Request: message thread not found` inside the detached proactive-send task. The sender logs too little, does not self-heal the stored route, and keeps retrying the dead topic forever.

3. **Scheduled Telegram media is not proactively delivered.** The scheduler already emits `GatewayServerFrame::MediaAttachment` frames before the text notification, but `TelegramProactiveSender::send_proactive()` currently handles only `ScheduledNotification`. Scheduled media for Telegram is therefore dropped today.

This revised plan fixes all three. It is no longer a single-file change. It stays incremental and test-gated, but it deliberately expands scope to cover proactive Telegram media delivery, durable route self-healing after repeated thread failures, and guidebook updates so the canonical docs match the implementation.

---

## Background: how notification routing works today

### Origin capture

When any user turn enters the gateway, `TurnOrigin` is populated with the ingress channel:

- **TUI / WebSocket** — `channel_id = "tui"`, `channel_context_id = <session_uuid>`
- **Telegram** — `channel_id = "telegram"`, `channel_context_id = "{chat_id}:{thread_id}"` for forum topics, `"{chat_id}"` for regular chats/DMs

The `schedule_create` tool copies both fields from `ToolExecutionContext` into the persisted `ScheduleDefinition`. They are not updated later.

### Scheduler execution

Each scheduled run executes in its own runtime session: `scheduled:{schedule_id}`. The originating interactive session is only relevant for notification delivery.

### Scheduler output shape

When a run should notify, the scheduler emits:

1. Zero or more `GatewayServerFrame::MediaAttachment` frames
2. One `GatewayServerFrame::ScheduledNotification` text frame

That ordering is already implemented in `runtime/src/scheduler_executor.rs`.

### Notification dispatch today

```text
channel_id == "tui"
  -> look up exact session_id from schedule.channel_context_id
     -> found: publish(frame) even if receiver_count() == 0
     -> missing: debug log, drop

channel_id == "telegram"
  -> call ProactiveSender.send_proactive(channel_context_id, frame)
     -> ScheduledNotification handled as text
     -> MediaAttachment ignored
     -> deleted topic returns Telegram 400 inside detached task

channel_id == None
  -> legacy broadcast to all in-memory sessions for user
```

---

## Phase 1: TUI fallback fan-out to all connected top-level TUI sessions

### Problem in detail

The current TUI path is too narrow:

- It only targets the exact stored origin session ID.
- It treats "session exists in memory" as equivalent to "session is connected".
- It does not log the "user has no in-memory sessions after restart" case.

This misses the most common stale-session path: the origin session still exists in `user.sessions`, but `receiver_count() == 0` until idle cleanup evicts it.

### Proposed change

**Files:**
- `crates/gateway/src/lib.rs`
- `crates/gateway/src/tests.rs`

Replace the TUI branch in `GatewayServer::notify_user()` with this policy:

1. Resolve the origin session from `schedule.channel_context_id`.
2. Treat the origin as valid only if:
   - the session exists
   - `receiver_count() > 0`
3. If the origin session is valid, deliver only to that session. This preserves current behavior when the stored route is still good.
4. If the origin session is missing or disconnected, fan out to **all connected top-level TUI sessions** for the user:
   - `channel_origin == GATEWAY_CHANNEL_ID`
   - `parent_session_id.is_none()`
   - `receiver_count() > 0`
5. If fallback delivery happens, log at `info!` with:
   - `schedule_id`
   - `user_id`
   - `origin_session_id`
   - `delivered_session_count`
6. If there is no in-memory user state or there are no connected top-level TUI sessions, log at `info!` that the notification was dropped.
7. If a TUI schedule has no `channel_context_id`, log at `warn!` because that indicates a malformed route rather than a normal offline case.

### Notes

- The chosen fallback policy is fan-out, not "most recent session". No `last_activity_epoch_secs` access is needed.
- This phase does not introduce durable queuing. If the user has no connected TUI sessions at delivery time, the notification is still lost, but now visibly and intentionally.

### Open questions

1. Are both `channel_origin == GATEWAY_CHANNEL_ID` and `parent_session_id.is_none()` necessary for the fallback filter, or does one subsume the other? If Telegram sessions always have a non-`GATEWAY_CHANNEL_ID` origin, the channel-origin check is redundant. Confirm in the gateway session model before coding the filter to avoid false confidence from an over-specified condition.

### Tests to add

1. **Origin connected** — delivers only to the origin session even when other TUI sessions are also connected.
2. **Origin present but disconnected** — fans out to all connected top-level TUI sessions.
3. **Origin evicted** — fans out to all connected top-level TUI sessions.
4. **Only subagent sessions connected** — nothing is delivered; drop path logged.
5. **Mixed channel origins** — Telegram-origin sessions are ignored; only connected top-level TUI sessions receive fallback.
6. **No in-memory user state after restart** — no panic; drop path logged at `info!`.
7. **Malformed TUI schedule with missing `channel_context_id`** — warning is logged and nothing is delivered.

### Verification gate

Run:

```sh
cargo test -p gateway
cargo clippy -p gateway --all-targets --all-features
```

---

## Phase 2: Telegram proactive delivery must handle both text and scheduled media

### Problem in detail

`TelegramProactiveSender::send_proactive()` currently has two gaps:

- It only handles `GatewayServerFrame::ScheduledNotification`.
- Its text path attempts HTML and then plain text, but it has no topic-deleted recovery path and no structured outcome for later route self-healing.

Separately, the interactive Telegram adapter already has media upload logic, but the proactive sender does not reuse it, so scheduled media frames are dropped before they ever reach Telegram.

### Proposed change

**Files:**
- `crates/types/src/channel.rs`
- `crates/runtime/src/scheduler_executor.rs`
- `crates/channels/src/telegram.rs`
- crate-local tests in `channels` and `runtime`

Make proactive Telegram delivery batch-aware and schedule-aware:

1. Extend `GatewayMediaAttachment` with an optional `schedule_id`.
   - Scheduler-emitted media sets `schedule_id = Some(schedule.schedule_id.clone())`.
   - Interactive media keeps `schedule_id = None`.
2. Refactor `TelegramProactiveSender` around small internal batch helpers:
   - `send_text_batch(...)`
   - `send_media_batch(...)`
3. `send_proactive()` must handle:
   - `ScheduledNotification`
   - `MediaAttachment` when `schedule_id.is_some()`
4. Batch delivery semantics:
   - Attempt the stored Telegram target first.
   - If Telegram returns `message thread not found` and `thread_id.is_some()`, retry the same batch to the main chat (`message_thread_id = None`).
   - For text batches, preserve the current HTML-to-plain-text fallback behavior on both the stored target and the fallback target.
   - For media batches, reuse the existing media upload logic and apply the same deleted-thread fallback to main chat.
   - If media still cannot be uploaded after retry, log a warning and send a plain text notice when possible so the user sees that an attachment failed instead of losing it silently.
5. Batch outcome is counted once per proactive frame, not once per text chunk. A long message split into N chunks is still one delivery batch for failure-streak purposes.

### Open questions

1. **`schedule_id` placement.** Embedding `schedule_id` in `GatewayMediaAttachment` adds scheduling context to a general-purpose `types` crate type. An alternative is passing the schedule ID at the proactive sender call site (e.g. as an argument to `send_proactive`) and keeping the attachment type schedule-agnostic. Which approach is preferred, and what are the downstream implications for each?
2. **Shared media upload helper.** "Reuse the existing media upload logic" is underspecified. Which specific function or struct in the interactive Telegram adapter should be extracted? If the upload path is coupled to the interactive session or bot context, a shared helper will need explicit design work before Phase 2 can proceed cleanly.
3. **Mock server crate.** The test implementation note names `Bot::new_url(...)` but does not specify a mock HTTP framework (`wiremock`, `httpmock`, a custom `axum` handler, etc.). Check existing patterns in the `channels` crate and agree on the crate before writing tests.
4. **Plain-text fallback notice wording.** What should the notice say when media cannot be uploaded after retry? A concrete template is needed to avoid divergent implementations (e.g. `"[Scheduled attachment could not be delivered]"`).

### Important regression guard

The new deleted-thread handling must **not** remove the existing plain-text fallback for formatting or parse-mode errors. The correct behavior is:

1. Try HTML to the stored target
2. If the stored target is deleted, switch targets and retry the batch
3. If HTML itself fails for a non-thread reason, keep the current plain-text fallback behavior

### Tests to add

1. **Text primary success** — no retry, no warning.
2. **Non-thread HTML failure** — still falls back to plain text on the original target.
3. **Thread-not-found text batch** — retries to main chat, and remaining chunks use the fallback target.
4. **Scheduled media success** — `MediaAttachment` is proactively uploaded.
5. **Scheduled media thread-not-found** — media retries to main chat.
6. **Scheduled media total failure** — warning logged and text fallback notice emitted when possible.
7. **Non-scheduled `MediaAttachment`** — proactive sender ignores it safely.

### Test implementation note

Use a local mock Telegram HTTP server instead of real network calls. `frankenstein::client_reqwest::Bot::new_url(...)` already supports a custom endpoint, so the `channels` tests can drive real request/response flows against a mock server and verify exact method paths and request bodies.

### Verification gate

Run:

```sh
cargo test -p channels
cargo test -p runtime
cargo clippy -p types -p runtime -p channels --all-targets --all-features
```

---

## Phase 3: Remap deleted Telegram topics after 3 consecutive failed proactive batches

### Problem in detail

Immediate fallback to the main chat restores delivery, but it does not stop repeated primary failures. Without a stored-route update, every later run will still attempt the deleted topic first.

The required behavior is:

- do not rewrite the stored route after a single transient error
- do rewrite it after **3 consecutive thread-not-found failures**
- apply the same policy to both scheduled text and scheduled media

### Proposed change

**Files:**
- `crates/types/src/proactive.rs`
- `crates/memory/migrations/0023_add_delivery_thread_not_found_streak_to_schedules.sql`
- `crates/memory/src/scheduler_store.rs`
- `crates/channels/src/telegram.rs`
- `crates/runner/src/bin/oxydra-vm.rs`
- relevant tests in `memory`, `channels`, and `runner`

Implement durable route self-healing with a small internal delivery-state field:

1. Add an internal scheduler-delivery state column to `schedules`, for example:

```sql
delivery_thread_not_found_streak INTEGER NOT NULL DEFAULT 0
```

2. Introduce a narrow trait for schedule notification route updates and streak maintenance.
   - The trait lives in a boundary-safe crate so `channels` does not need a direct storage-crate dependency.
   - The memory crate implements it with transactional SQL against the `schedules` table.
   - The runner injects it into `TelegramProactiveSender` at bootstrap.
3. `TelegramProactiveSender` must resolve the schedule ID per proactive batch:
   - `ScheduledNotification` -> `notification.schedule_id`
   - scheduled `MediaAttachment` -> `media.schedule_id`
4. Batch-level streak rules:
   - **Primary delivery to stored target succeeds:** reset streak to `0`
   - **Primary delivery fails with `message thread not found`:** increment streak by `1`
   - **Primary failure is anything else:** do not increment the thread-not-found streak
5. Remap rule:
   - If the streak reaches `3`
   - and fallback delivery to the main chat succeeded
   - then atomically update `schedules.channel_context_id` to `chat_id.to_string()`
   - and reset the streak to `0`
6. Log remaps at `info!` with:
   - `schedule_id`
   - old `channel_context_id`
   - new `channel_context_id`
   - threshold (`3`)

### Open questions

1. **Remap trigger semantics.** Does "streak reaches 3" mean `streak == 3` (fires exactly once per threshold crossing) or `streak >= 3` (fires on every subsequent attempt)? If the fallback also fails on the 3rd attempt the streak increments to 4; should the remap trigger again on the 4th? Nail down the exact comparison before writing the streak-update logic.
2. **Concurrent streak updates.** Two schedule runs for the same user firing simultaneously may race on the streak column. The SQL should use an atomic increment-and-read (e.g. `UPDATE ... SET streak = streak + 1 RETURNING streak`) rather than a read-then-write. Confirm whether the trait design assumes this or whether it must be made explicit in the store implementation.

### Why this is persistent

The streak is stored in the schedules table rather than in process memory. That means the "3 consecutive failures" rule survives process restarts and does not depend on a long-lived sender instance.

### Tests to add

1. **First and second thread-not-found batch** — streak increments, route unchanged.
2. **Third consecutive thread-not-found batch with successful fallback** — route remaps to main chat and streak resets.
3. **Successful primary delivery** — clears prior streak.
4. **Long multi-chunk text batch** — counts as one failed batch, not one failure per chunk.
5. **Scheduled media batch** — participates in the same streak/remap logic.
6. **Fallback failure** — streak increments, route does not remap automatically.
7. **Post-remap run** — proactive sender uses the main chat directly and no longer attempts the deleted thread.
8. **Migration/store tests** — default value, increment, reset, and remap semantics are all covered in `memory`.

### Verification gate

Run:

```sh
cargo test -p memory
cargo test -p channels
cargo test -p runner
cargo clippy -p types -p memory -p channels -p runner --all-targets --all-features
```

---

## Phase 4: Canonical docs and acceptance gates

### Problem in detail

The guidebook is already stale in this area. It still describes:

- TUI notification routing as `channel_id == "gateway"` instead of `"tui"`
- an async `ProactiveSender::send_notification(...)` trait instead of the current sync `send_proactive(...)`
- proactive Telegram behavior without the new media and route-remap semantics

The implementation change should not land without updating the canonical docs.

### Proposed change

**Docs to update:**
- `docs/guidebook/05-agent-runtime.md`
- `docs/guidebook/09-gateway-and-channels.md`
- `docs/guidebook/12-external-channels-and-identity.md`
- `docs/guidebook/15-progressive-build-plan.md`

Update those chapters to reflect:

- TUI fallback fan-out to connected top-level sessions when the origin session is missing or disconnected
- the correct channel ID (`"tui"`)
- the actual `ProactiveSender` naming/signature
- scheduled Telegram media delivery
- deleted-topic fallback to main chat
- 3-failure stored-route remap behavior

### Open questions

1. **Stale section pointers.** The docs list names four files but does not identify which sections within each are stale. Adding a one-line pointer per file (e.g. "§ Notification routing" in `09-gateway-and-channels.md`) would reduce lookup time during implementation and make review easier.

### Acceptance gate

Before the change is considered complete, run:

```sh
cargo fmt --check
cargo clippy -p types -p memory -p gateway -p runtime -p channels -p runner --all-targets --all-features
cargo test -p types
cargo test -p memory
cargo test -p gateway
cargo test -p runtime
cargo test -p channels
cargo test -p runner
```

---

## Shared constraints

- **No durable offline queue in this change.** If no connected TUI session exists and the schedule route is TUI, the notification is still dropped.
- **Primary behavior stays unchanged when the stored route is healthy.** TUI still delivers only to the origin session when it is connected. Telegram still delivers to the stored target first.
- **The Telegram remap threshold is fixed at 3 for now.** It is an implementation constant, not a new user-facing config knob.
- **Scheduled media is in scope.** Reliability fixes must cover both text and media proactive delivery.
- **The storage-facing route update path stays narrow.** The `channels` crate should not grow a direct dependency on the storage implementation crate.

---

## Explicitly out of scope

- Durable notification queueing for offline users
- Generic self-healing route logic for future Discord/Slack/WhatsApp adapters
- New user-facing tools or config for editing internal delivery-streak metadata
