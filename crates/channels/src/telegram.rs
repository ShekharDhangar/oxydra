//! Telegram channel adapter — runs in-process alongside the gateway.
//!
//! The adapter long-polls Telegram for updates, authenticates senders,
//! resolves channel-context → session mappings, and routes messages
//! through the gateway's internal API. Responses are streamed back
//! via Telegram's edit-message pattern.

use std::sync::Arc;
use std::time::{Duration, Instant};

use frankenstein::AsyncTelegramApi;
use frankenstein::ParseMode;
use frankenstein::client_reqwest::Bot;
use frankenstein::methods::{
    EditMessageTextParams, GetUpdatesParams, SendAudioParams, SendDocumentParams,
    SendMessageParams, SendPhotoParams, SendVideoParams, SendVoiceParams, SetMyCommandsParams,
};
use frankenstein::types::{AllowedUpdate, BotCommand, ChatId, Message as TgMessage};
use frankenstein::updates::UpdateContent;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use types::{
    DeliveryStreakUpdater, GatewayCancelActiveTurn, GatewaySendTurn, GatewayServerFrame,
    MediaAttachment, MediaType, RouteDeliveryOutcome, SessionStore, TelegramChannelConfig,
};

use crate::audit::{AuditEntry, AuditLogger, now_iso8601};
use crate::sender_auth::SenderAuthPolicy;
use crate::session_map::ChannelSessionMap;

const CHANNEL_ID: &str = "telegram";
/// Minimum interval between edit-message API calls (avoids rate limits).
const EDIT_THROTTLE: Duration = Duration::from_millis(1500);
/// Safety margin below the hard limit to allow for the progress status line.
const TEXT_SPLIT_MARGIN: usize = 200;

// ---------------------------------------------------------------------------
// Public adapter
// ---------------------------------------------------------------------------

/// In-process Telegram adapter that calls the gateway internal API directly.
/// Maximum size of a single media attachment we will download (10 MB).
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum number of attachments per message.
const MAX_ATTACHMENTS_PER_MESSAGE: usize = 4;

pub struct TelegramAdapter {
    bot: Bot,
    bot_token: String,
    sender_auth: SenderAuthPolicy,
    session_map: ChannelSessionMap,
    gateway: Arc<gateway::GatewayServer>,
    user_id: String,
    config: TelegramChannelConfig,
    audit_logger: AuditLogger,
    http_client: reqwest::Client,
}

impl TelegramAdapter {
    /// Construct a new adapter. All arguments are validated before calling.
    pub fn new(
        bot_token: String,
        sender_auth: SenderAuthPolicy,
        session_store: Arc<dyn SessionStore>,
        gateway: Arc<gateway::GatewayServer>,
        user_id: String,
        config: TelegramChannelConfig,
        audit_logger: AuditLogger,
    ) -> Self {
        Self {
            bot: Bot::new(&bot_token),
            bot_token,
            sender_auth,
            session_map: ChannelSessionMap::new(session_store),
            gateway,
            user_id,
            config,
            audit_logger,
            http_client: reqwest::Client::new(),
        }
    }

    /// Run the long-polling loop until `cancel` fires.
    pub async fn run(self, cancel: CancellationToken) {
        // Wrap in Arc so individual message handlers can be spawned as
        // independent tasks, allowing different sessions to run concurrently.
        let this = Arc::new(self);

        // Register slash commands with Telegram so they appear in autocomplete.
        this.register_bot_commands().await;

        info!(user_id = %this.user_id, "telegram adapter started");
        let mut offset: Option<i64> = None;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let params = GetUpdatesParams {
                offset,
                timeout: Some(this.config.polling_timeout_secs as u32),
                limit: None,
                allowed_updates: Some(vec![AllowedUpdate::Message]),
            };

            let updates = tokio::select! {
                _ = cancel.cancelled() => break,
                result = this.bot.get_updates(&params) => {
                    match result {
                        Ok(response) => response.result,
                        Err(e) => {
                            if let Some(retry_secs) = extract_retry_after(&e) {
                                warn!(retry_after_secs = retry_secs, "telegram rate limited; backing off");
                                tokio::time::sleep(Duration::from_secs(retry_secs)).await;
                            } else {
                                warn!(error = %e, "telegram get_updates failed; retrying in 5s");
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                            continue;
                        }
                    }
                }
            };

            for update in updates {
                offset = Some(i64::from(update.update_id) + 1);

                let UpdateContent::Message(message) = update.content else {
                    continue;
                };

                // Spawn each message as an independent task so different
                // sessions (topics/chats) run concurrently. Within a single
                // session the gateway's active_turn mutex serializes turns.
                let this = Arc::clone(&this);
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    this.handle_message(&message, &cancel).await;
                });
            }
        }

        info!(user_id = %this.user_id, "telegram adapter stopped");
    }

    /// Register bot commands with Telegram so they appear in the autocomplete menu.
    async fn register_bot_commands(&self) {
        let commands = vec![
            BotCommand {
                command: "new".to_owned(),
                description: "Start a new session".to_owned(),
            },
            BotCommand {
                command: "sessions".to_owned(),
                description: "List sessions".to_owned(),
            },
            BotCommand {
                command: "switch".to_owned(),
                description: "Switch to a session".to_owned(),
            },
            BotCommand {
                command: "cancel".to_owned(),
                description: "Cancel the active turn".to_owned(),
            },
            BotCommand {
                command: "cancelall".to_owned(),
                description: "Cancel active turns in all sessions".to_owned(),
            },
            BotCommand {
                command: "status".to_owned(),
                description: "Show current session info".to_owned(),
            },
            BotCommand {
                command: "help".to_owned(),
                description: "Show help".to_owned(),
            },
        ];
        let params = SetMyCommandsParams {
            commands,
            scope: None,
            language_code: None,
        };
        if let Err(e) = self.bot.set_my_commands(&params).await {
            warn!(error = %e, "failed to register telegram bot commands");
        } else {
            info!("telegram bot commands registered");
        }
    }

    async fn handle_message(&self, message: &TgMessage, cancel: &CancellationToken) {
        // Extract sender ID for auth check.
        let Some(ref from) = message.from else {
            return;
        };
        let sender_id = from.id.to_string();

        if !self.sender_auth.is_authorized(&sender_id) {
            self.audit_logger.log_rejected_sender(&AuditEntry {
                timestamp: now_iso8601(),
                channel: CHANNEL_ID.to_owned(),
                sender_id: sender_id.clone(),
                reason: "sender not in authorized list".to_owned(),
                context: Some(format!("chat_id={}", message.chat.id)),
            });
            debug!(sender_id = %sender_id, chat_id = message.chat.id, "rejected unauthorized telegram sender");
            return;
        }

        // Extract text: either `text` (for plain text messages) or `caption`
        // (for media messages that include a caption).
        let text = message
            .text
            .as_deref()
            .or(message.caption.as_deref())
            .map(|t| t.trim())
            .unwrap_or("");

        // Extract inline media attachments from the message.
        let attachments = self.extract_media_attachments(message).await;

        // A message must have either text or media to be actionable.
        if text.is_empty() && attachments.is_empty() {
            return;
        }

        let chat_id = message.chat.id;
        let thread_id = message.message_thread_id;
        let channel_context_id = derive_channel_context_id(chat_id, thread_id);

        // Command interception (only for text-only messages).
        if attachments.is_empty()
            && let Some(cmd) = text.strip_prefix('/')
        {
            let handled = self
                .handle_command(cmd, chat_id, thread_id, &channel_context_id)
                .await;
            if handled {
                return;
            }
            // Unknown /command — fall through and treat as a normal message.
        }

        // Resolve or create session for this context.
        let session = match self.resolve_session(&channel_context_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to resolve session for telegram message");
                self.send_reply(chat_id, thread_id, &format!("❌ {e}"))
                    .await;
                return;
            }
        };

        // Build the prompt: use the text if present, otherwise a placeholder
        // so the model knows media was sent.
        let prompt = if text.is_empty() {
            "[The user sent media without a caption.]".to_owned()
        } else {
            text.to_owned()
        };

        // Submit the turn.
        let turn_id = uuid::Uuid::new_v4().to_string();
        let send_turn = GatewaySendTurn {
            request_id: format!("tg-{turn_id}"),
            session_id: session.session_id.clone(),
            turn_id: turn_id.clone(),
            prompt,
            attachments,
        };

        // Subscribe BEFORE submit so we don't miss early frames.
        let events_rx = self.gateway.subscribe_events(&session);

        if let Some(error_frame) = self
            .gateway
            .submit_turn_from_channel(&session, send_turn, CHANNEL_ID, Some(&channel_context_id))
            .await
        {
            if let GatewayServerFrame::Error(ref err) = error_frame {
                if err.message.contains("active turn is already running") {
                    self.send_reply(
                        chat_id,
                        thread_id,
                        "⏳ I'm still working on your previous request. Send /cancel to stop it, or wait for me to finish.",
                    )
                    .await;
                } else {
                    self.send_reply(chat_id, thread_id, &format!("❌ {}", err.message))
                        .await;
                }
            }
            return;
        }

        // Stream the response back using edit-message pattern.
        self.stream_response(chat_id, thread_id, events_rx, cancel)
            .await;
    }

    /// Extract media attachments from a Telegram message.
    ///
    /// Handles photo, audio, voice, video, and document messages by downloading
    /// the file from Telegram servers.
    async fn extract_media_attachments(&self, message: &TgMessage) -> Vec<types::InlineMedia> {
        let mut file_ids: Vec<(String, String)> = Vec::new(); // (file_id, mime_type)

        // Photo: Telegram provides multiple sizes; pick the largest.
        if let Some(ref photos) = message.photo
            && let Some(largest) = photos.last()
        {
            file_ids.push((largest.file_id.clone(), "image/jpeg".to_owned()));
        }

        // Voice message (OGG Opus).
        if let Some(ref voice) = message.voice {
            let mime = voice
                .mime_type
                .clone()
                .unwrap_or_else(|| "audio/ogg".to_owned());
            file_ids.push((voice.file_id.clone(), mime));
        }

        // Audio file.
        if let Some(ref audio) = message.audio {
            let mime = audio
                .mime_type
                .clone()
                .unwrap_or_else(|| "audio/mpeg".to_owned());
            file_ids.push((audio.file_id.clone(), mime));
        }

        // Video.
        if let Some(ref video) = message.video {
            let mime = video
                .mime_type
                .clone()
                .unwrap_or_else(|| "video/mp4".to_owned());
            file_ids.push((video.file_id.clone(), mime));
        }

        // Document (generic file).
        if let Some(ref document) = message.document {
            let mime = document
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            file_ids.push((document.file_id.clone(), mime));
        }

        // Video note (round video messages).
        if let Some(ref video_note) = message.video_note {
            file_ids.push((video_note.file_id.clone(), "video/mp4".to_owned()));
        }

        // Limit the number of attachments.
        file_ids.truncate(MAX_ATTACHMENTS_PER_MESSAGE);

        let mut attachments = Vec::with_capacity(file_ids.len());
        for (file_id, mime_type) in file_ids {
            match self.download_telegram_file(&file_id).await {
                Ok(data) => {
                    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
                        warn!(
                            file_id = %file_id,
                            size = data.len(),
                            "telegram attachment exceeds size limit, skipping"
                        );
                        continue;
                    }
                    attachments.push(types::InlineMedia { mime_type, data });
                }
                Err(e) => {
                    warn!(file_id = %file_id, error = %e, "failed to download telegram file");
                }
            }
        }

        attachments
    }

    /// Download a file from Telegram by its `file_id`.
    ///
    /// Uses the Bot API `getFile` to resolve the file path, then downloads
    /// the content from `https://api.telegram.org/file/bot<token>/<path>`.
    async fn download_telegram_file(&self, file_id: &str) -> Result<Vec<u8>, String> {
        use frankenstein::methods::GetFileParams;

        let params = GetFileParams {
            file_id: file_id.to_owned(),
        };
        let file_info = self
            .bot
            .get_file(&params)
            .await
            .map_err(|e| format!("getFile failed: {e}"))?;

        let file_path = file_info
            .result
            .file_path
            .ok_or_else(|| "Telegram file has no file_path".to_owned())?;

        // Check file_size before downloading if available.
        if let Some(size) = file_info.result.file_size
            && size > MAX_ATTACHMENT_BYTES
        {
            return Err(format!(
                "file too large ({size} bytes, limit {MAX_ATTACHMENT_BYTES})"
            ));
        }

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot_token, file_path
        );

        let mut response = self
            .http_client
            .get(&url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("download returned status {}", response.status()));
        }

        let mut data = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("failed to read body chunk: {e}"))?
        {
            let next_len = data.len().saturating_add(chunk.len());
            if next_len as u64 > MAX_ATTACHMENT_BYTES {
                return Err(format!(
                    "downloaded file too large (limit {MAX_ATTACHMENT_BYTES} bytes)"
                ));
            }
            data.extend_from_slice(&chunk);
        }

        Ok(data)
    }

    /// Handle slash commands. Returns `true` if the command was handled.
    async fn handle_command(
        &self,
        cmd: &str,
        chat_id: i64,
        thread_id: Option<i32>,
        channel_context_id: &str,
    ) -> bool {
        let (command, args) = cmd
            .split_once(|c: char| c.is_whitespace())
            .map(|(c, a)| (c, a.trim()))
            .unwrap_or((cmd, ""));

        // Strip bot username suffix (e.g., /new@MyBot → new).
        let command = command.split('@').next().unwrap_or(command);

        match command {
            "new" => {
                let display_name = if args.is_empty() {
                    None
                } else {
                    Some(args.to_owned())
                };
                match self
                    .gateway
                    .create_or_get_session(&self.user_id, None, "default", CHANNEL_ID)
                    .await
                {
                    Ok(session) => {
                        // Persist the new mapping.
                        if let Err(e) = self
                            .session_map
                            .set_session_id(CHANNEL_ID, channel_context_id, &session.session_id)
                            .await
                        {
                            warn!(error = %e, "failed to update channel session mapping");
                        }
                        let name_display = display_name
                            .as_deref()
                            .unwrap_or(&session.session_id[..8.min(session.session_id.len())]);
                        self.send_reply(
                            chat_id,
                            thread_id,
                            &format!("✅ New session created: {name_display}"),
                        )
                        .await;
                    }
                    Err(e) => {
                        self.send_reply(chat_id, thread_id, &format!("❌ {e}"))
                            .await;
                    }
                }
                true
            }
            "sessions" => {
                match self.gateway.list_user_sessions(&self.user_id, false).await {
                    Ok(sessions) => {
                        if sessions.is_empty() {
                            self.send_reply(chat_id, thread_id, "No active sessions.")
                                .await;
                        } else {
                            let mut lines = Vec::with_capacity(sessions.len() + 1);
                            lines.push("📋 Sessions:".to_owned());
                            for s in sessions.iter().filter(|s| s.parent_session_id.is_none()) {
                                let id_short = &s.session_id[..8.min(s.session_id.len())];
                                let name = s.display_name.as_deref().unwrap_or("(unnamed)");
                                lines.push(format!(
                                    "  {id_short} — {name} [{origin}]",
                                    origin = s.channel_origin
                                ));
                            }
                            self.send_reply(chat_id, thread_id, &lines.join("\n")).await;
                        }
                    }
                    Err(e) => {
                        self.send_reply(chat_id, thread_id, &format!("❌ {e}"))
                            .await;
                    }
                }
                true
            }
            "switch" => {
                if args.is_empty() {
                    self.send_reply(chat_id, thread_id, "Usage: /switch <session_id>")
                        .await;
                    return true;
                }
                // Support prefix matching: the user may supply a short prefix
                // (e.g. the 8-char ID shown by /sessions) instead of the full UUID.
                let resolved_id = match self.resolve_session_id_by_prefix(args).await {
                    Ok(id) => id,
                    Err(msg) => {
                        self.send_reply(chat_id, thread_id, &format!("❌ {msg}"))
                            .await;
                        return true;
                    }
                };
                match self
                    .gateway
                    .create_or_get_session(&self.user_id, Some(&resolved_id), "default", CHANNEL_ID)
                    .await
                {
                    Ok(session) => {
                        if let Err(e) = self
                            .session_map
                            .set_session_id(CHANNEL_ID, channel_context_id, &session.session_id)
                            .await
                        {
                            warn!(error = %e, "failed to update channel session mapping on switch");
                        }
                        let id_short = &session.session_id[..8.min(session.session_id.len())];
                        self.send_reply(
                            chat_id,
                            thread_id,
                            &format!("🔄 Switched to session {id_short}"),
                        )
                        .await;
                    }
                    Err(e) => {
                        self.send_reply(chat_id, thread_id, &format!("❌ {e}"))
                            .await;
                    }
                }
                true
            }
            "cancel" => {
                let Ok(Some(session_id)) = self
                    .session_map
                    .get_session_id(CHANNEL_ID, channel_context_id)
                    .await
                else {
                    self.send_reply(chat_id, thread_id, "No active session.")
                        .await;
                    return true;
                };
                match self
                    .gateway
                    .create_or_get_session(&self.user_id, Some(&session_id), "default", CHANNEL_ID)
                    .await
                {
                    Ok(session) => {
                        let cancel_turn = GatewayCancelActiveTurn {
                            request_id: format!("tg-cancel-{}", uuid::Uuid::new_v4()),
                            session_id: session.session_id.clone(),
                            turn_id: String::new(),
                        };
                        if let Some(error_frame) = self
                            .gateway
                            .cancel_session_turn(&session, cancel_turn)
                            .await
                        {
                            if let GatewayServerFrame::Error(ref err) = error_frame {
                                self.send_reply(chat_id, thread_id, &format!("ℹ️ {}", err.message))
                                    .await;
                            }
                        } else {
                            self.send_reply(chat_id, thread_id, "🛑 Turn cancelled.")
                                .await;
                        }
                    }
                    Err(e) => {
                        self.send_reply(chat_id, thread_id, &format!("❌ {e}"))
                            .await;
                    }
                }
                true
            }
            "cancelall" => {
                let cancelled = self.gateway.cancel_all_user_turns(&self.user_id).await;
                if cancelled == 0 {
                    self.send_reply(chat_id, thread_id, "ℹ️ No active turns to cancel.")
                        .await;
                } else {
                    let suffix = if cancelled == 1 { "" } else { "s" };
                    self.send_reply(
                        chat_id,
                        thread_id,
                        &format!("🛑 Cancelled {cancelled} active turn{suffix}."),
                    )
                    .await;
                }
                true
            }
            "status" => {
                let session_id = self
                    .session_map
                    .get_session_id(CHANNEL_ID, channel_context_id)
                    .await
                    .ok()
                    .flatten();
                let status = match session_id {
                    Some(ref id) => {
                        let id_short = &id[..8.min(id.len())];
                        format!(
                            "Session: {id_short}\nChannel: {CHANNEL_ID}\nContext: {channel_context_id}"
                        )
                    }
                    None => "No session mapped to this chat.".to_owned(),
                };
                self.send_reply(chat_id, thread_id, &status).await;
                true
            }
            "start" | "help" => {
                let help = "🤖 Oxydra Bot\n\n\
                    Send any message to start a conversation.\n\n\
                    Commands:\n\
                    /new [name] — start a new session\n\
                    /sessions — list sessions\n\
                    /switch <id> — switch to a session\n\
                    /cancel — cancel active turn\n\
                    /cancelall — cancel active turns in all sessions\n\
                    /status — show current session info";
                self.send_reply(chat_id, thread_id, help).await;
                true
            }
            _ => false,
        }
    }

    /// Resolve a (possibly prefix-shortened) session ID to the full session ID.
    ///
    /// If the argument is an exact match, returns it directly. Otherwise,
    /// searches the user's sessions for a unique prefix match.
    async fn resolve_session_id_by_prefix(&self, prefix: &str) -> Result<String, String> {
        // First, try exact match via the gateway (fast path).
        if self
            .gateway
            .create_or_get_session(&self.user_id, Some(prefix), "default", CHANNEL_ID)
            .await
            .is_ok()
        {
            return Ok(prefix.to_owned());
        }

        // Fall back to prefix matching against listed sessions.
        let sessions = self
            .gateway
            .list_user_sessions(&self.user_id, false)
            .await
            .map_err(|e| format!("failed to list sessions: {e}"))?;

        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.parent_session_id.is_none() && s.session_id.starts_with(prefix))
            .collect();

        match matches.len() {
            0 => Err(format!("no session found matching `{prefix}`")),
            1 => Ok(matches[0].session_id.clone()),
            n => {
                let previews: Vec<String> = matches
                    .iter()
                    .take(5)
                    .map(|s| {
                        let short = &s.session_id[..8.min(s.session_id.len())];
                        let name = s.display_name.as_deref().unwrap_or("(unnamed)");
                        format!("  {short} — {name}")
                    })
                    .collect();
                Err(format!(
                    "ambiguous prefix `{prefix}` matches {n} sessions:\n{}",
                    previews.join("\n")
                ))
            }
        }
    }

    /// Resolve or create a gateway session for the given channel context.
    async fn resolve_session(
        &self,
        channel_context_id: &str,
    ) -> Result<Arc<gateway::GatewaySessionState>, String> {
        // Check for existing mapping and resolve the session in one go.
        if let Ok(Some(session_id)) = self
            .session_map
            .get_session_id(CHANNEL_ID, channel_context_id)
            .await
            && let Ok(session) = self
                .gateway
                .create_or_get_session(&self.user_id, Some(&session_id), "default", CHANNEL_ID)
                .await
        {
            return Ok(session);
        }

        // No mapping or stale mapping — create a fresh session.
        let session = self
            .gateway
            .create_or_get_session(&self.user_id, None, "default", CHANNEL_ID)
            .await?;

        // Persist the mapping.
        if let Err(e) = self
            .session_map
            .set_session_id(CHANNEL_ID, channel_context_id, &session.session_id)
            .await
        {
            warn!(error = %e, "failed to persist channel session mapping");
        }

        Ok(session)
    }

    /// Stream a turn's response back to Telegram using edit-message.
    async fn stream_response(
        &self,
        chat_id: i64,
        thread_id: Option<i32>,
        mut events_rx: broadcast::Receiver<GatewayServerFrame>,
        cancel: &CancellationToken,
    ) {
        let mut streamer = ResponseStreamer::new(
            &self.bot,
            chat_id,
            thread_id,
            self.config.max_message_length,
        );

        // Send placeholder.
        streamer.send_placeholder().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = events_rx.recv() => {
                    match event {
                        Ok(GatewayServerFrame::TurnStarted(_)) => {
                            // Already sent placeholder.
                        }
                        Ok(GatewayServerFrame::TurnProgress(progress)) => {
                            streamer.set_progress(&progress.progress.message).await;
                        }
                        Ok(GatewayServerFrame::AssistantDelta(delta)) => {
                            streamer.append_text(&delta.delta).await;
                        }
                        Ok(GatewayServerFrame::MediaAttachment(media)) => {
                            self.send_media_attachment(chat_id, thread_id, &media.attachment).await;
                        }
                        Ok(GatewayServerFrame::TurnCompleted(completed)) => {
                            let final_text = completed
                                .response
                                .message
                                .content
                                .as_deref()
                                .unwrap_or("");
                            streamer.finalize(final_text).await;
                            break;
                        }
                        Ok(GatewayServerFrame::TurnCancelled(_)) => {
                            streamer.finalize("🛑 Turn cancelled.").await;
                            break;
                        }
                        Ok(GatewayServerFrame::Error(err)) => {
                            streamer.finalize(&format!("❌ {}", err.message)).await;
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug!(skipped = n, "telegram event subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Ok(_) => {}
                    }
                }
            }
        }
    }

    /// Send a simple text reply to a chat.
    async fn send_reply(&self, chat_id: i64, thread_id: Option<i32>, text: &str) {
        let params = SendMessageParams {
            chat_id: ChatId::Integer(chat_id),
            text: text.to_owned(),
            message_thread_id: thread_id,
            business_connection_id: None,
            direct_messages_topic_id: None,
            parse_mode: None,
            entities: None,
            link_preview_options: None,
            disable_notification: None,
            protect_content: None,
            allow_paid_broadcast: None,
            message_effect_id: None,
            suggested_post_parameters: None,
            reply_parameters: None,
            reply_markup: None,
        };
        if let Err(e) = self.bot.send_message(&params).await {
            warn!(error = %e, chat_id, "failed to send telegram reply");
        }
    }

    /// Send a media attachment (photo, audio, document, voice, video) to a chat.
    async fn send_media_attachment(
        &self,
        chat_id: i64,
        thread_id: Option<i32>,
        attachment: &MediaAttachment,
    ) {
        match upload_media_to_chat(&self.bot, chat_id, thread_id, attachment).await {
            Ok(()) => {}
            Err(MediaUploadError::Io(e)) => {
                warn!(error = %e, "failed to write temp file for telegram media upload");
                self.send_reply(
                    chat_id,
                    thread_id,
                    &format!("📎 [Failed to send media: {}]", e),
                )
                .await;
            }
            Err(MediaUploadError::Api(e)) => {
                warn!(
                    error = %e,
                    media_type = ?attachment.media_type,
                    chat_id,
                    "failed to send telegram media"
                );
                let fallback = format!(
                    "📎 [Tried to send a {:?} but the upload failed: {}]",
                    attachment.media_type, e
                );
                self.send_reply(chat_id, thread_id, &fallback).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Response streamer — edit-message streaming for Telegram
// ---------------------------------------------------------------------------

/// Handles the edit-message streaming pattern:
/// 1. Send placeholder "⏳ Working..."
/// 2. Edit with progress status and accumulated text
/// 3. Final edit with complete response
struct ResponseStreamer<'a> {
    bot: &'a Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    /// ID of the message currently being edited.
    message_id: Option<i32>,
    /// Accumulated response text.
    accumulated_text: String,
    /// Current progress status line (shown at top during processing).
    progress_status: Option<String>,
    /// Last time we edited the message (for throttling).
    last_edit: Instant,
    /// Maximum chars per message.
    max_message_length: usize,
}

impl<'a> ResponseStreamer<'a> {
    fn new(bot: &'a Bot, chat_id: i64, thread_id: Option<i32>, max_message_length: usize) -> Self {
        Self {
            bot,
            chat_id,
            thread_id,
            message_id: None,
            accumulated_text: String::new(),
            progress_status: None,
            last_edit: Instant::now() - EDIT_THROTTLE, // allow immediate first edit
            max_message_length,
        }
    }

    async fn send_placeholder(&mut self) {
        let params = SendMessageParams {
            chat_id: ChatId::Integer(self.chat_id),
            text: "⏳ Working...".to_owned(),
            message_thread_id: self.thread_id,
            business_connection_id: None,
            direct_messages_topic_id: None,
            parse_mode: None,
            entities: None,
            link_preview_options: None,
            disable_notification: None,
            protect_content: None,
            allow_paid_broadcast: None,
            message_effect_id: None,
            suggested_post_parameters: None,
            reply_parameters: None,
            reply_markup: None,
        };
        match self.bot.send_message(&params).await {
            Ok(response) => {
                self.message_id = Some(response.result.message_id);
            }
            Err(e) => {
                warn!(error = %e, "failed to send telegram placeholder");
            }
        }
    }

    async fn set_progress(&mut self, message: &str) {
        self.progress_status = Some(message.to_owned());
        self.try_edit().await;
    }

    async fn append_text(&mut self, delta: &str) {
        self.accumulated_text.push_str(delta);

        // Check if we need to split to a new message.
        let effective_limit = self.max_message_length - TEXT_SPLIT_MARGIN;
        if self.accumulated_text.len() > effective_limit {
            self.force_edit().await;
            self.accumulated_text.clear();
            self.message_id = None;
            self.send_placeholder().await;
            return;
        }

        self.try_edit().await;
    }

    /// Edit the message if enough time has elapsed since the last edit.
    async fn try_edit(&mut self) {
        if self.last_edit.elapsed() < EDIT_THROTTLE {
            return;
        }
        self.force_edit().await;
    }

    /// Edit the message unconditionally.
    async fn force_edit(&mut self) {
        let Some(msg_id) = self.message_id else {
            return;
        };

        let display_text = self.compose_display_text();
        if display_text.is_empty() {
            return;
        }

        let params = EditMessageTextParams {
            chat_id: Some(ChatId::Integer(self.chat_id)),
            message_id: Some(msg_id),
            text: display_text,
            business_connection_id: None,
            inline_message_id: None,
            parse_mode: None,
            entities: None,
            link_preview_options: None,
            reply_markup: None,
        };

        if let Err(e) = self.bot.edit_message_text(&params).await {
            debug!(error = %e, "failed to edit telegram message");
        }
        self.last_edit = Instant::now();
    }

    /// Send the final edit with the complete response text.
    async fn finalize(&mut self, final_text: &str) {
        self.progress_status = None;

        if final_text.is_empty() {
            return;
        }

        let Some(msg_id) = self.message_id else {
            // Placeholder failed. Send a new message.
            let params = SendMessageParams {
                chat_id: ChatId::Integer(self.chat_id),
                text: final_text.to_owned(),
                message_thread_id: self.thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                entities: None,
                link_preview_options: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            let _ = self.bot.send_message(&params).await;
            return;
        };

        // Split long responses across multiple messages.
        let chunks = split_message(final_text, self.max_message_length);
        if let Some((first, rest)) = chunks.split_first() {
            // Edit existing message with the first chunk (try HTML, fallback to plain).
            let html_text = markdown_to_telegram_html(first);
            let params = EditMessageTextParams {
                chat_id: Some(ChatId::Integer(self.chat_id)),
                message_id: Some(msg_id),
                text: html_text,
                parse_mode: Some(ParseMode::Html),
                business_connection_id: None,
                inline_message_id: None,
                entities: None,
                link_preview_options: None,
                reply_markup: None,
            };
            if let Err(e) = self.bot.edit_message_text(&params).await {
                // Fallback to plain text.
                let fallback = EditMessageTextParams {
                    chat_id: Some(ChatId::Integer(self.chat_id)),
                    message_id: Some(msg_id),
                    text: first.to_string(),
                    business_connection_id: None,
                    inline_message_id: None,
                    parse_mode: None,
                    entities: None,
                    link_preview_options: None,
                    reply_markup: None,
                };
                let _ = self.bot.edit_message_text(&fallback).await;
                debug!(error = %e, "html edit failed; used plain text fallback");
            }

            // Send continuation messages.
            for chunk in rest {
                let html_chunk = markdown_to_telegram_html(chunk);
                let params = SendMessageParams {
                    chat_id: ChatId::Integer(self.chat_id),
                    text: html_chunk,
                    message_thread_id: self.thread_id,
                    parse_mode: Some(ParseMode::Html),
                    business_connection_id: None,
                    direct_messages_topic_id: None,
                    entities: None,
                    link_preview_options: None,
                    disable_notification: None,
                    protect_content: None,
                    allow_paid_broadcast: None,
                    message_effect_id: None,
                    suggested_post_parameters: None,
                    reply_parameters: None,
                    reply_markup: None,
                };
                if let Err(e) = self.bot.send_message(&params).await {
                    // Fallback to plain text.
                    let fallback = SendMessageParams {
                        chat_id: ChatId::Integer(self.chat_id),
                        text: chunk.to_string(),
                        message_thread_id: self.thread_id,
                        business_connection_id: None,
                        direct_messages_topic_id: None,
                        parse_mode: None,
                        entities: None,
                        link_preview_options: None,
                        disable_notification: None,
                        protect_content: None,
                        allow_paid_broadcast: None,
                        message_effect_id: None,
                        suggested_post_parameters: None,
                        reply_parameters: None,
                        reply_markup: None,
                    };
                    let _ = self.bot.send_message(&fallback).await;
                    debug!(error = %e, "html send failed; used plain text fallback");
                }
            }
        }
    }

    /// Compose the text to display in an interim edit (progress + accumulated text).
    fn compose_display_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref status) = self.progress_status {
            parts.push(status.clone());
        }
        if !self.accumulated_text.is_empty() {
            parts.push(self.accumulated_text.clone());
        }
        if parts.is_empty() {
            "⏳ Working...".to_owned()
        } else {
            parts.join("\n\n")
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown → Telegram HTML conversion
// ---------------------------------------------------------------------------

/// Convert common Markdown constructs to Telegram's HTML subset.
///
/// Supported: **bold**, *italic*, `code`, ```pre```, [links](url), ~~strikethrough~~
/// Unsupported constructs (tables, images, headers) pass through as plain text.
pub fn markdown_to_telegram_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len() + input.len() / 4);
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Code blocks: ```...```
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            i += 3;
            // Skip optional language tag on same line.
            while i < len && chars[i] != '\n' && chars[i] != '`' {
                i += 1;
            }
            if i < len && chars[i] == '\n' {
                i += 1;
            }
            let start = i;
            while i + 2 < len && !(chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`') {
                i += 1;
            }
            let code_content: String = chars[start..i].iter().collect();
            output.push_str("<pre>");
            output.push_str(&html_escape(&code_content));
            output.push_str("</pre>");
            if i + 2 < len {
                i += 3; // skip closing ```
            }
            continue;
        }

        // Inline code: `...`
        if chars[i] == '`' {
            i += 1;
            let start = i;
            while i < len && chars[i] != '`' {
                i += 1;
            }
            let code: String = chars[start..i].iter().collect();
            output.push_str("<code>");
            output.push_str(&html_escape(&code));
            output.push_str("</code>");
            if i < len {
                i += 1;
            }
            continue;
        }

        // Bold: **...**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            i += 2;
            let start = i;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            output.push_str("<b>");
            output.push_str(&html_escape(&content));
            output.push_str("</b>");
            if i + 1 < len {
                i += 2;
            }
            continue;
        }

        // Strikethrough: ~~...~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            i += 2;
            let start = i;
            while i + 1 < len && !(chars[i] == '~' && chars[i + 1] == '~') {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            output.push_str("<s>");
            output.push_str(&html_escape(&content));
            output.push_str("</s>");
            if i + 1 < len {
                i += 2;
            }
            continue;
        }

        // Italic: *...* (single asterisk, not double)
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            i += 1;
            let start = i;
            while i < len && chars[i] != '*' {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            output.push_str("<i>");
            output.push_str(&html_escape(&content));
            output.push_str("</i>");
            if i < len {
                i += 1;
            }
            continue;
        }

        // Links: [text](url)
        if chars[i] == '[' {
            let bracket_start = i + 1;
            let mut j = bracket_start;
            while j < len && chars[j] != ']' {
                j += 1;
            }
            if j + 1 < len && chars[j] == ']' && chars[j + 1] == '(' {
                let link_text: String = chars[bracket_start..j].iter().collect();
                let url_start = j + 2;
                let mut k = url_start;
                while k < len && chars[k] != ')' {
                    k += 1;
                }
                if k < len {
                    let url: String = chars[url_start..k].iter().collect();
                    output.push_str(&format!(
                        "<a href=\"{}\">{}</a>",
                        html_escape(&url),
                        html_escape(&link_text)
                    ));
                    i = k + 1;
                    continue;
                }
            }
        }

        // Headers: # → bold (best approximation)
        if chars[i] == '#' && (i == 0 || chars[i - 1] == '\n') {
            let mut level = 0;
            while i < len && chars[i] == '#' {
                level += 1;
                i += 1;
            }
            while i < len && chars[i] == ' ' {
                i += 1;
            }
            let start = i;
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            let header: String = chars[start..i].iter().collect();
            if level > 0 {
                output.push_str("<b>");
                output.push_str(&html_escape(&header));
                output.push_str("</b>");
            } else {
                output.push_str(&html_escape(&header));
            }
            continue;
        }

        // Default: escape and pass through.
        match chars[i] {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            ch => output.push(ch),
        }
        i += 1;
    }

    output
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Telegram Proactive Sender — for origin-only scheduler notifications
// ---------------------------------------------------------------------------

/// Outcome of a single proactive delivery batch (text or media).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProactiveDeliveryOutcome {
    /// Primary target delivery succeeded.
    PrimarySuccess,
    /// Primary failed with thread-not-found; fallback to main chat succeeded.
    ThreadNotFoundFallbackSuccess,
    /// Primary failed with thread-not-found; fallback also failed.
    ThreadNotFoundFallbackFailed,
    /// Primary failed with a non-thread error.
    OtherFailure,
}

/// Error from attempting to upload media to Telegram.
#[derive(Debug)]
enum MediaUploadError {
    /// Local I/O failure (temp file write). Not retryable via target switch.
    Io(std::io::Error),
    /// Telegram API failure. May be thread-not-found (retryable) or other.
    Api(String),
}

/// Check if a Telegram error description indicates a deleted forum topic.
fn is_thread_not_found_error(err: &str) -> bool {
    err.to_lowercase().contains("message thread not found")
}

/// Plain text notice sent when scheduled media cannot be delivered.
const MEDIA_FALLBACK_NOTICE: &str =
    "[Scheduled attachment could not be delivered — upload failed after retry]";

/// Send an HTML message to a Telegram chat. Returns `Ok(())` on success or the
/// error description string on failure.
async fn send_message_html(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    html: &str,
) -> Result<(), String> {
    let params = SendMessageParams {
        chat_id: ChatId::Integer(chat_id),
        text: html.to_owned(),
        message_thread_id: thread_id,
        parse_mode: Some(ParseMode::Html),
        business_connection_id: None,
        direct_messages_topic_id: None,
        entities: None,
        link_preview_options: None,
        disable_notification: None,
        protect_content: None,
        allow_paid_broadcast: None,
        message_effect_id: None,
        suggested_post_parameters: None,
        reply_parameters: None,
        reply_markup: None,
    };
    bot.send_message(&params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Send a plain-text message to a Telegram chat. Returns `Ok(())` on success or
/// the error description string on failure.
async fn send_message_plain(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    text: &str,
) -> Result<(), String> {
    let params = SendMessageParams {
        chat_id: ChatId::Integer(chat_id),
        text: text.to_owned(),
        message_thread_id: thread_id,
        parse_mode: None,
        business_connection_id: None,
        direct_messages_topic_id: None,
        entities: None,
        link_preview_options: None,
        disable_notification: None,
        protect_content: None,
        allow_paid_broadcast: None,
        message_effect_id: None,
        suggested_post_parameters: None,
        reply_parameters: None,
        reply_markup: None,
    };
    bot.send_message(&params)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Upload a media attachment to a Telegram chat. Shared between the interactive
/// adapter and the proactive sender.
async fn upload_media_to_chat(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    attachment: &MediaAttachment,
) -> Result<(), MediaUploadError> {
    let file_name = attachment.file_name.as_deref().unwrap_or("attachment");
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(format!("oxydra-tg-{}-{}", uuid::Uuid::new_v4(), file_name));

    tokio::fs::write(&temp_path, &attachment.data)
        .await
        .map_err(MediaUploadError::Io)?;

    let file_upload: PathBuf = temp_path.clone();
    let caption = attachment.caption.clone();
    let result = match attachment.media_type {
        MediaType::Photo => {
            let params = SendPhotoParams {
                chat_id: ChatId::Integer(chat_id),
                photo: file_upload.into(),
                caption,
                message_thread_id: thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                caption_entities: None,
                show_caption_above_media: None,
                has_spoiler: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            bot.send_photo(&params).await.map(|_| ())
        }
        MediaType::Audio => {
            let params = SendAudioParams {
                chat_id: ChatId::Integer(chat_id),
                audio: file_upload.into(),
                caption,
                message_thread_id: thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                caption_entities: None,
                duration: None,
                performer: None,
                title: None,
                thumbnail: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            bot.send_audio(&params).await.map(|_| ())
        }
        MediaType::Document => {
            let params = SendDocumentParams {
                chat_id: ChatId::Integer(chat_id),
                document: file_upload.into(),
                caption,
                message_thread_id: thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                caption_entities: None,
                thumbnail: None,
                disable_content_type_detection: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            bot.send_document(&params).await.map(|_| ())
        }
        MediaType::Voice => {
            let params = SendVoiceParams {
                chat_id: ChatId::Integer(chat_id),
                voice: file_upload.into(),
                caption,
                message_thread_id: thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                caption_entities: None,
                duration: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            bot.send_voice(&params).await.map(|_| ())
        }
        MediaType::Video => {
            let params = SendVideoParams {
                chat_id: ChatId::Integer(chat_id),
                video: file_upload.into(),
                caption,
                message_thread_id: thread_id,
                business_connection_id: None,
                direct_messages_topic_id: None,
                parse_mode: None,
                caption_entities: None,
                duration: None,
                width: None,
                height: None,
                thumbnail: None,
                cover: None,
                start_timestamp: None,
                show_caption_above_media: None,
                has_spoiler: None,
                supports_streaming: None,
                disable_notification: None,
                protect_content: None,
                allow_paid_broadcast: None,
                message_effect_id: None,
                suggested_post_parameters: None,
                reply_parameters: None,
                reply_markup: None,
            };
            bot.send_video(&params).await.map(|_| ())
        }
    };

    // Clean up temp file (best-effort).
    let _ = tokio::fs::remove_file(&temp_path).await;

    result.map_err(|e| MediaUploadError::Api(e.to_string()))
}

/// Send a batch of text chunks to a Telegram chat with thread-not-found fallback.
async fn send_text_batch(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    message: &str,
    max_len: usize,
) -> ProactiveDeliveryOutcome {
    let chunks = split_message(message, max_len);
    let mut effective_thread_id = thread_id;
    let mut target_switched = false;
    let mut fallback_failed = false;

    for (i, chunk) in chunks.iter().enumerate() {
        let html = markdown_to_telegram_html(chunk);

        let result = send_message_html(bot, chat_id, effective_thread_id, &html).await;
        match result {
            Ok(()) => {}
            Err(e) if i == 0 && is_thread_not_found_error(&e) && thread_id.is_some() => {
                // Thread is deleted — switch to main chat for the entire batch.
                effective_thread_id = None;
                target_switched = true;
                debug!(
                    error = %e,
                    chat_id,
                    "proactive send: thread not found, retrying to main chat"
                );
                // Retry chunk 0 on fallback target.
                if send_message_html(bot, chat_id, None, &html).await.is_err() {
                    // Try plain text as last resort.
                    if send_message_plain(bot, chat_id, None, chunk).await.is_err() {
                        fallback_failed = true;
                    }
                }
            }
            Err(e) => {
                // Non-thread HTML failure → plain text fallback (same target).
                let _ = send_message_plain(bot, chat_id, effective_thread_id, chunk).await;
                debug!(error = %e, "proactive html send failed; used plain text fallback");
            }
        }
    }

    if target_switched {
        if fallback_failed {
            ProactiveDeliveryOutcome::ThreadNotFoundFallbackFailed
        } else {
            ProactiveDeliveryOutcome::ThreadNotFoundFallbackSuccess
        }
    } else {
        ProactiveDeliveryOutcome::PrimarySuccess
    }
}

/// Maximum number of transient-error retries for media uploads.
const MEDIA_UPLOAD_MAX_RETRIES: u32 = 2;

/// Initial backoff delay between transient-error retries.
const MEDIA_UPLOAD_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Attempt a media upload with retries for transient API errors.
/// Thread-not-found errors are NOT retried here — they are handled by the
/// caller with a different target.
async fn upload_media_with_retries(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    attachment: &MediaAttachment,
) -> Result<(), MediaUploadError> {
    let mut last_err = None;
    for attempt in 0..=MEDIA_UPLOAD_MAX_RETRIES {
        if attempt > 0 {
            let delay = MEDIA_UPLOAD_RETRY_DELAY * attempt;
            debug!(
                attempt,
                ?delay,
                "proactive media: retrying after transient error"
            );
            tokio::time::sleep(delay).await;
        }
        match upload_media_to_chat(bot, chat_id, thread_id, attachment).await {
            Ok(()) => return Ok(()),
            Err(MediaUploadError::Io(e)) => return Err(MediaUploadError::Io(e)),
            Err(MediaUploadError::Api(ref e)) if is_thread_not_found_error(e) => {
                return Err(MediaUploadError::Api(e.clone()));
            }
            Err(e) => {
                debug!(attempt, error = ?e, "proactive media: transient upload error");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| MediaUploadError::Api("upload failed after retries".to_owned())))
}

/// Send a media attachment proactively with transient retry and
/// thread-not-found fallback.
async fn send_media_proactive(
    bot: &Bot,
    chat_id: i64,
    thread_id: Option<i32>,
    attachment: &MediaAttachment,
) -> ProactiveDeliveryOutcome {
    match upload_media_with_retries(bot, chat_id, thread_id, attachment).await {
        Ok(()) => ProactiveDeliveryOutcome::PrimarySuccess,
        Err(MediaUploadError::Io(e)) => {
            warn!(error = %e, "proactive media: temp file write failed");
            ProactiveDeliveryOutcome::OtherFailure
        }
        Err(MediaUploadError::Api(e)) if is_thread_not_found_error(&e) && thread_id.is_some() => {
            debug!(
                error = %e,
                chat_id,
                "proactive media: thread not found, retrying to main chat"
            );
            // Retry to main chat (also with transient retries).
            match upload_media_with_retries(bot, chat_id, None, attachment).await {
                Ok(()) => ProactiveDeliveryOutcome::ThreadNotFoundFallbackSuccess,
                Err(_) => {
                    // Send text notice as last resort.
                    let _ = send_message_plain(bot, chat_id, None, MEDIA_FALLBACK_NOTICE).await;
                    ProactiveDeliveryOutcome::ThreadNotFoundFallbackFailed
                }
            }
        }
        Err(MediaUploadError::Api(e)) => {
            warn!(error = %e, "proactive media: upload failed after retries");
            // Non-thread API failure — try text notice.
            let _ = send_message_plain(bot, chat_id, thread_id, MEDIA_FALLBACK_NOTICE).await;
            ProactiveDeliveryOutcome::OtherFailure
        }
    }
}

/// Sends proactive (scheduler-originated) notifications to Telegram chats.
pub struct TelegramProactiveSender {
    bot: Bot,
    max_message_length: usize,
    streak_updater: Option<Arc<dyn DeliveryStreakUpdater>>,
}

impl TelegramProactiveSender {
    pub fn new(bot_token: &str, max_message_length: usize) -> Self {
        Self {
            bot: Bot::new(bot_token),
            max_message_length,
            streak_updater: None,
        }
    }

    pub fn new_with_streak_updater(
        bot_token: &str,
        max_message_length: usize,
        streak_updater: Arc<dyn DeliveryStreakUpdater>,
    ) -> Self {
        Self {
            bot: Bot::new(bot_token),
            max_message_length,
            streak_updater: Some(streak_updater),
        }
    }

    /// For tests: construct with a pre-built Bot (e.g. `Bot::new_url`).
    #[cfg(test)]
    pub fn new_with_bot(bot: Bot, max_message_length: usize) -> Self {
        Self {
            bot,
            max_message_length,
            streak_updater: None,
        }
    }

    #[cfg(test)]
    pub fn new_with_bot_and_updater(
        bot: Bot,
        max_message_length: usize,
        streak_updater: Arc<dyn DeliveryStreakUpdater>,
    ) -> Self {
        Self {
            bot,
            max_message_length,
            streak_updater: Some(streak_updater),
        }
    }

    /// Core async implementation. Called by both the sync trait method and tests.
    async fn send_proactive_impl(
        &self,
        channel_context_id: &str,
        frame: &GatewayServerFrame,
    ) -> Option<ProactiveDeliveryOutcome> {
        let (chat_id, thread_id) = parse_channel_context_id(channel_context_id)?;

        match frame {
            GatewayServerFrame::ScheduledNotification(notif) => {
                let outcome = send_text_batch(
                    &self.bot,
                    chat_id,
                    thread_id,
                    &notif.message,
                    self.max_message_length,
                )
                .await;

                // Report streak outcome for text notifications only (D1).
                if let Some(ref updater) = self.streak_updater {
                    let route_outcome = match &outcome {
                        ProactiveDeliveryOutcome::PrimarySuccess => {
                            Some(RouteDeliveryOutcome::PrimaryRouteSucceeded)
                        }
                        ProactiveDeliveryOutcome::ThreadNotFoundFallbackSuccess => Some(
                            RouteDeliveryOutcome::PrimaryRouteNotFoundFallbackSucceeded {
                                fallback_channel_context_id: chat_id.to_string(),
                            },
                        ),
                        ProactiveDeliveryOutcome::ThreadNotFoundFallbackFailed => {
                            Some(RouteDeliveryOutcome::PrimaryRouteNotFoundFallbackFailed)
                        }
                        ProactiveDeliveryOutcome::OtherFailure => {
                            // Non-route errors don't affect the route-health streak.
                            None
                        }
                    };

                    if let Some(ref ro) = route_outcome
                        && let Err(e) = updater
                            .report_outcome(&notif.schedule_id, channel_context_id, ro)
                            .await
                    {
                        warn!(
                            schedule_id = %notif.schedule_id,
                            error = %e,
                            "failed to update delivery streak"
                        );
                    }
                }

                Some(outcome)
            }
            GatewayServerFrame::MediaAttachment(media) if media.schedule_id.is_some() => {
                // Media: deliver with fallback, but no streak reporting (D1).
                let outcome =
                    send_media_proactive(&self.bot, chat_id, thread_id, &media.attachment).await;
                Some(outcome)
            }
            _ => None,
        }
    }
}

impl types::ProactiveSender for TelegramProactiveSender {
    fn send_proactive(&self, channel_context_id: &str, frame: &GatewayServerFrame) {
        let bot = self.bot.clone();
        let max_len = self.max_message_length;
        let streak_updater = self.streak_updater.clone();
        let ctx = channel_context_id.to_owned();
        let frame = frame.clone();
        tokio::spawn(async move {
            let sender = TelegramProactiveSender {
                bot,
                max_message_length: max_len,
                streak_updater,
            };
            if let Some(outcome) = sender.send_proactive_impl(&ctx, &frame).await {
                debug!(
                    ?outcome,
                    channel_context_id = %ctx,
                    "proactive delivery completed"
                );
            }
        });
    }
}

/// Parse a channel_context_id into (chat_id, optional thread_id).
/// Returns `None` if the format is invalid.
fn parse_channel_context_id(ctx: &str) -> Option<(i64, Option<i32>)> {
    if let Some((chat_str, thread_str)) = ctx.split_once(':') {
        let chat_id = chat_str.parse::<i64>().ok()?;
        let thread_id = thread_str.parse::<i32>().ok()?;
        Some((chat_id, Some(thread_id)))
    } else {
        let chat_id = ctx.parse::<i64>().ok()?;
        Some((chat_id, None))
    }
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Derive the `channel_context_id` for a Telegram message (D14).
///
/// - Forum groups: `"{chat_id}:{message_thread_id}"` — each topic is a separate session.
/// - Regular chats/DMs: `"{chat_id}"` — single session per chat.
pub fn derive_channel_context_id(chat_id: i64, message_thread_id: Option<i32>) -> String {
    match message_thread_id {
        Some(thread_id) => format!("{chat_id}:{thread_id}"),
        None => chat_id.to_string(),
    }
}

/// Split a long message into chunks that fit within Telegram's character limit.
/// Splits at paragraph boundaries (double newlines) where possible.
fn split_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while remaining.len() > max_len {
        let search_window = &remaining[..max_len];
        let split_pos = search_window
            .rfind("\n\n")
            .or_else(|| search_window.rfind('\n'))
            .or_else(|| search_window.rfind(' '))
            .unwrap_or(max_len);

        let (chunk, rest) = remaining.split_at(split_pos);
        chunks.push(chunk);
        remaining = rest.trim_start_matches('\n');
    }

    if !remaining.is_empty() {
        chunks.push(remaining);
    }

    chunks
}

/// Extract `retry_after` seconds from a frankenstein error (429 rate limiting).
fn extract_retry_after(error: &frankenstein::Error) -> Option<u64> {
    let msg = error.to_string();
    if msg.contains("retry after") || msg.contains("Retry-After") {
        for word in msg.split_whitespace() {
            if let Ok(n) = word.parse::<u64>() {
                return Some(n);
            }
        }
        Some(5)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use types::{GatewayMediaAttachment, GatewayScheduledNotification, GatewaySession};

    // -----------------------------------------------------------------------
    // Mock Telegram server
    // -----------------------------------------------------------------------

    /// Spawn a mock Telegram Bot API server.
    /// Returns `(base_url, request_log)`.
    /// `responses` is consumed in order; once exhausted the server returns 500.
    async fn spawn_mock_tg(
        responses: Vec<(axum::http::StatusCode, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        use axum::{Router, body::Bytes, extract::State, routing::post};

        type Log = Arc<Mutex<Vec<(String, String)>>>;

        #[derive(Clone)]
        struct MockState {
            responses: Arc<Mutex<Vec<(axum::http::StatusCode, serde_json::Value)>>>,
            log: Log,
        }

        async fn handler(
            axum::extract::Path((_token, method)): axum::extract::Path<(String, String)>,
            State(state): State<MockState>,
            body: Bytes,
        ) -> axum::response::Response<String> {
            let body_str = String::from_utf8_lossy(&body).to_string();
            state.log.lock().unwrap().push((method, body_str));

            let mut q = state.responses.lock().unwrap();
            let (status, json) = if q.is_empty() {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"ok": false, "description": "no more mock responses"}),
                )
            } else {
                q.remove(0)
            };
            axum::response::Response::builder()
                .status(status)
                .header("Content-Type", "application/json")
                .body(json.to_string())
                .unwrap()
        }

        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            responses: Arc::new(Mutex::new(responses)),
            log: log.clone(),
        };

        // Use `/*method` to capture all API method paths, e.g. /botTOKEN/sendMessage
        let app = Router::new()
            .route("/{token}/{method}", post(handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://127.0.0.1:{}/botTEST_TOKEN", addr.port());
        (base_url, log)
    }

    /// A success response that looks like Telegram's `sendMessage` result.
    fn tg_ok() -> (axum::http::StatusCode, serde_json::Value) {
        (
            axum::http::StatusCode::OK,
            serde_json::json!({
                "ok": true,
                "result": {
                    "message_id": 1,
                    "date": 0,
                    "chat": {"id": 123, "type": "private"}
                }
            }),
        )
    }

    /// A thread-not-found error response.
    fn tg_thread_not_found() -> (axum::http::StatusCode, serde_json::Value) {
        (
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::json!({
                "ok": false,
                "description": "Bad Request: message thread not found"
            }),
        )
    }

    /// A generic error (non-thread) response.
    fn tg_bad_request(msg: &str) -> (axum::http::StatusCode, serde_json::Value) {
        (
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::json!({
                "ok": false,
                "description": msg
            }),
        )
    }

    /// A server error (500) response.
    fn tg_server_error() -> (axum::http::StatusCode, serde_json::Value) {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "ok": false,
                "description": "Internal Server Error"
            }),
        )
    }

    fn make_text_frame(msg: &str) -> GatewayServerFrame {
        GatewayServerFrame::ScheduledNotification(GatewayScheduledNotification {
            schedule_id: "sched-1".to_owned(),
            schedule_name: Some("test".to_owned()),
            message: msg.to_owned(),
        })
    }

    fn make_scheduled_media_frame() -> GatewayServerFrame {
        GatewayServerFrame::MediaAttachment(GatewayMediaAttachment {
            request_id: "req-1".to_owned(),
            session: GatewaySession {
                user_id: "user-1".to_owned(),
                session_id: "session-1".to_owned(),
            },
            attachment: MediaAttachment {
                file_path: "/shared/photo.jpg".to_owned(),
                media_type: MediaType::Photo,
                caption: Some("test photo".to_owned()),
                data: vec![0xFF, 0xD8, 0xFF],
                file_name: Some("photo.jpg".to_owned()),
            },
            schedule_id: Some("sched-1".to_owned()),
        })
    }

    fn make_interactive_media_frame() -> GatewayServerFrame {
        GatewayServerFrame::MediaAttachment(GatewayMediaAttachment {
            request_id: "req-2".to_owned(),
            session: GatewaySession {
                user_id: "user-1".to_owned(),
                session_id: "session-1".to_owned(),
            },
            attachment: MediaAttachment {
                file_path: "/shared/photo.jpg".to_owned(),
                media_type: MediaType::Photo,
                caption: None,
                data: vec![0xFF, 0xD8, 0xFF],
                file_name: None,
            },
            schedule_id: None,
        })
    }

    // -----------------------------------------------------------------------
    // Proactive sender tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proactive_text_primary_success() {
        let (url, log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].0.contains("sendMessage"));
        // Should contain parse_mode (HTML)
        assert!(log[0].1.contains("parse_mode"));
    }

    #[tokio::test]
    async fn proactive_text_html_failure_falls_back_to_plain() {
        // First call: HTML fails (non-thread error). Second call: plain text succeeds.
        let (url, log) = spawn_mock_tg(vec![
            tg_bad_request("Bad Request: can't parse entities"),
            tg_ok(),
        ])
        .await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        // Second call should NOT have parse_mode (plain text)
        assert!(!log[1].1.contains("parse_mode"));
    }

    #[tokio::test]
    async fn proactive_text_thread_not_found_retries_main_chat() {
        // First: thread-not-found. Second: success on main chat.
        let (url, log) = spawn_mock_tg(vec![tg_thread_not_found(), tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(
            outcome,
            Some(ProactiveDeliveryOutcome::ThreadNotFoundFallbackSuccess)
        );
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        // Second request must NOT contain message_thread_id (omitted by serde).
        assert!(
            !log[1].1.contains("message_thread_id"),
            "fallback request should omit message_thread_id, got: {}",
            log[1].1
        );
    }

    #[tokio::test]
    async fn proactive_media_success() {
        let (url, log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].0.contains("sendPhoto"));
    }

    #[tokio::test]
    async fn proactive_media_transient_failure_retries_and_succeeds() {
        // First sendPhoto: transient 500 error. Second sendPhoto (retry): success.
        let (url, log) = spawn_mock_tg(vec![tg_server_error(), tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log[0].0.contains("sendPhoto"));
        assert!(log[1].0.contains("sendPhoto"));
    }

    #[tokio::test]
    async fn proactive_media_two_transient_failures_then_success() {
        // sendPhoto fails twice with 500, then succeeds on the 3rd (final) attempt.
        let (url, log) = spawn_mock_tg(vec![tg_server_error(), tg_server_error(), tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert!(log[0].0.contains("sendPhoto"));
        assert!(log[1].0.contains("sendPhoto"));
        assert!(log[2].0.contains("sendPhoto"));
    }

    #[tokio::test]
    async fn proactive_media_thread_not_found_retries_main_chat() {
        // First sendPhoto: thread-not-found. Second sendPhoto: success.
        let (url, log) = spawn_mock_tg(vec![tg_thread_not_found(), tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(
            outcome,
            Some(ProactiveDeliveryOutcome::ThreadNotFoundFallbackSuccess)
        );
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        // Both should be sendPhoto
        assert!(log[0].0.contains("sendPhoto"));
        assert!(log[1].0.contains("sendPhoto"));
    }

    #[tokio::test]
    async fn proactive_media_total_failure_sends_text_notice() {
        // sendPhoto fails with 500 on every retry attempt, then sendMessage
        // for the fallback notice succeeds. With MEDIA_UPLOAD_MAX_RETRIES=2
        // the upload is attempted 3 times before giving up.
        let (url, log) = spawn_mock_tg(vec![
            tg_server_error(),
            tg_server_error(),
            tg_server_error(),
            tg_ok(),
        ])
        .await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::OtherFailure));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 4);
        // First three are sendPhoto retries, last is sendMessage with fallback notice.
        assert!(log[0].0.contains("sendPhoto"));
        assert!(log[1].0.contains("sendPhoto"));
        assert!(log[2].0.contains("sendPhoto"));
        assert!(log[3].0.contains("sendMessage"));
        assert!(log[3].1.contains(MEDIA_FALLBACK_NOTICE));
    }

    #[tokio::test]
    async fn proactive_non_scheduled_media_ignored() {
        let (url, log) = spawn_mock_tg(vec![]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_interactive_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, None);
        let log = log.lock().unwrap();
        assert!(log.is_empty(), "no HTTP requests should be made");
    }

    // -----------------------------------------------------------------------
    // Existing tests
    // -----------------------------------------------------------------------

    #[test]
    fn channel_context_id_regular_chat() {
        assert_eq!(derive_channel_context_id(12345, None), "12345");
    }

    #[test]
    fn channel_context_id_forum_topic() {
        assert_eq!(derive_channel_context_id(12345, Some(42)), "12345:42");
    }

    #[test]
    fn channel_context_id_negative_chat_id() {
        // Group chats have negative chat IDs.
        assert_eq!(derive_channel_context_id(-100123456, None), "-100123456");
    }

    #[test]
    fn channel_context_id_forum_topic_with_negative_chat() {
        assert_eq!(
            derive_channel_context_id(-100123456, Some(7)),
            "-100123456:7"
        );
    }

    #[test]
    fn split_message_short_text_returns_single_chunk() {
        let text = "Hello, world!";
        let chunks = split_message(text, 4096);
        assert_eq!(chunks, vec!["Hello, world!"]);
    }

    #[test]
    fn split_message_splits_at_paragraph_boundary() {
        let text = format!("{}\n\n{}", "a".repeat(100), "b".repeat(100));
        let chunks = split_message(&text, 150);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with('a'));
        assert!(chunks[1].starts_with('b'));
    }

    #[test]
    fn split_message_splits_at_newline_when_no_paragraph() {
        let text = format!("{}\n{}", "a".repeat(100), "b".repeat(100));
        let chunks = split_message(&text, 150);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn markdown_to_html_bold() {
        assert_eq!(markdown_to_telegram_html("**bold**"), "<b>bold</b>");
    }

    #[test]
    fn markdown_to_html_italic() {
        assert_eq!(markdown_to_telegram_html("*italic*"), "<i>italic</i>");
    }

    #[test]
    fn markdown_to_html_inline_code() {
        assert_eq!(markdown_to_telegram_html("`code`"), "<code>code</code>");
    }

    #[test]
    fn markdown_to_html_code_block() {
        assert_eq!(
            markdown_to_telegram_html("```\nhello\n```"),
            "<pre>hello\n</pre>"
        );
    }

    #[test]
    fn markdown_to_html_code_block_with_lang() {
        assert_eq!(
            markdown_to_telegram_html("```rust\nfn main() {}\n```"),
            "<pre>fn main() {}\n</pre>"
        );
    }

    #[test]
    fn markdown_to_html_link() {
        assert_eq!(
            markdown_to_telegram_html("[click](https://example.com)"),
            "<a href=\"https://example.com\">click</a>"
        );
    }

    #[test]
    fn markdown_to_html_strikethrough() {
        assert_eq!(markdown_to_telegram_html("~~deleted~~"), "<s>deleted</s>");
    }

    #[test]
    fn markdown_to_html_escapes_special_chars() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn markdown_to_html_header_becomes_bold() {
        assert_eq!(markdown_to_telegram_html("# Title"), "<b>Title</b>");
    }

    #[test]
    fn markdown_to_html_plain_text_unchanged() {
        assert_eq!(
            markdown_to_telegram_html("just some text"),
            "just some text"
        );
    }

    #[test]
    fn markdown_to_html_mixed_formatting() {
        let input = "**bold** and *italic* and `code`";
        let output = markdown_to_telegram_html(input);
        assert_eq!(
            output,
            "<b>bold</b> and <i>italic</i> and <code>code</code>"
        );
    }

    // -----------------------------------------------------------------------
    // Mock DeliveryStreakUpdater for streak-reporting tests
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct MockStreakUpdater {
        calls: Arc<Mutex<Vec<(String, String, RouteDeliveryOutcome)>>>,
        fail: bool,
    }

    impl MockStreakUpdater {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: true,
            }
        }

        fn calls(&self) -> Vec<(String, String, RouteDeliveryOutcome)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl DeliveryStreakUpdater for MockStreakUpdater {
        async fn report_outcome(
            &self,
            schedule_id: &str,
            attempted_channel_context_id: &str,
            outcome: &RouteDeliveryOutcome,
        ) -> Result<(), types::SchedulerError> {
            self.calls.lock().unwrap().push((
                schedule_id.to_owned(),
                attempted_channel_context_id.to_owned(),
                outcome.clone(),
            ));
            if self.fail {
                Err(types::SchedulerError::Store {
                    message: "mock error".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Streak-reporting tests (Phase 3)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn streak_text_primary_success_reports_outcome() {
        let (url, _log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_text_frame("Hello world");
        sender.send_proactive_impl("-100123:42", &frame).await;

        let calls = updater.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sched-1");
        assert_eq!(calls[0].1, "-100123:42");
        assert_eq!(calls[0].2, RouteDeliveryOutcome::PrimaryRouteSucceeded);
    }

    #[tokio::test]
    async fn streak_text_thread_not_found_reports_fallback_success() {
        let (url, _log) = spawn_mock_tg(vec![tg_thread_not_found(), tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_text_frame("Hello world");
        sender.send_proactive_impl("-100123:42", &frame).await;

        let calls = updater.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sched-1");
        assert_eq!(calls[0].1, "-100123:42");
        assert_eq!(
            calls[0].2,
            RouteDeliveryOutcome::PrimaryRouteNotFoundFallbackSucceeded {
                fallback_channel_context_id: "-100123".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn streak_text_thread_not_found_fallback_failed_reports() {
        // thread-not-found → HTML fallback fails → plain text fallback fails
        let (url, _log) = spawn_mock_tg(vec![
            tg_thread_not_found(),
            tg_server_error(),
            tg_server_error(),
        ])
        .await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_text_frame("Hello world");
        sender.send_proactive_impl("-100123:42", &frame).await;

        let calls = updater.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2,
            RouteDeliveryOutcome::PrimaryRouteNotFoundFallbackFailed
        );
    }

    #[tokio::test]
    async fn streak_text_other_failure_reports_primary_success() {
        // Non-thread error: HTML fails, plain text succeeds.
        // send_text_batch returns PrimarySuccess (target_switched is false),
        // so streak updater IS called with PrimaryRouteSucceeded — this is
        // correct because the route is healthy (Step 8 semantics).
        let (url, _log) = spawn_mock_tg(vec![
            tg_bad_request("Bad Request: can't parse entities"),
            tg_ok(),
        ])
        .await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        // Delivery should succeed (plain text fallback on same target).
        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        // Streak updater called with PrimaryRouteSucceeded (route is healthy).
        let calls = updater.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, RouteDeliveryOutcome::PrimaryRouteSucceeded);
    }

    #[tokio::test]
    async fn streak_media_does_not_report() {
        // Media delivery succeeds but should NOT trigger streak updater.
        let (url, _log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_scheduled_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        assert!(updater.calls().is_empty(), "media should not report streak");
    }

    #[tokio::test]
    async fn streak_non_scheduled_media_no_report() {
        let (url, _log) = spawn_mock_tg(vec![]).await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_interactive_media_frame();
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, None);
        assert!(updater.calls().is_empty());
    }

    #[tokio::test]
    async fn streak_no_updater_no_panic() {
        // When streak_updater is None, delivery works without errors.
        let (url, _log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let sender = TelegramProactiveSender::new_with_bot(bot, 4096);

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
    }

    #[tokio::test]
    async fn streak_updater_error_does_not_block_delivery() {
        let (url, _log) = spawn_mock_tg(vec![tg_ok()]).await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::failing());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        let frame = make_text_frame("Hello world");
        let outcome = sender.send_proactive_impl("-100123:42", &frame).await;

        // Delivery should still complete successfully despite updater error.
        assert_eq!(outcome, Some(ProactiveDeliveryOutcome::PrimarySuccess));
        // Updater was still called.
        assert_eq!(updater.calls().len(), 1);
    }

    #[tokio::test]
    async fn streak_multi_media_plus_text_single_report() {
        // Simulate a run: 2 media frames + 1 text frame.
        // Streak updater should be called exactly once (for text only).
        let (url, _log) = spawn_mock_tg(vec![
            tg_ok(), // media 1
            tg_ok(), // media 2
            tg_ok(), // text
        ])
        .await;
        let bot = Bot::new_url(&url);
        let updater = Arc::new(MockStreakUpdater::new());
        let sender = TelegramProactiveSender::new_with_bot_and_updater(bot, 4096, updater.clone());

        // Media frames
        let media1 = make_scheduled_media_frame();
        let media2 = make_scheduled_media_frame();
        let text = make_text_frame("Summary");

        sender.send_proactive_impl("-100123:42", &media1).await;
        sender.send_proactive_impl("-100123:42", &media2).await;
        sender.send_proactive_impl("-100123:42", &text).await;

        let calls = updater.calls();
        assert_eq!(
            calls.len(),
            1,
            "streak updater should be called once (text only)"
        );
        assert_eq!(calls[0].0, "sched-1");
    }
}
