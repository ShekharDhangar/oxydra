# OAuth-based LLM Provider Authentication

**Status:** Proposed  
**Created:** 2026-03-19  
**Updated:** 2026-03-19

---

## Executive Summary

Add OAuth-based authentication for three subscription-backed LLM providers — **GitHub Copilot**, **OpenAI Codex** (ChatGPT Plus/Pro), and **Google Gemini CLI** (Cloud Code Assist) — so users can authenticate with their existing subscriptions instead of provisioning API keys. This complements the existing API key flow and is modeled after proven patterns in Pi (`badlogic/pi-mono`) and OpenCode (`opencode-ai/opencode`).

---

## Current State

- **Provider auth is API-key-only.** Each `ProviderRegistryEntry` resolves a static `api_key: String` via a 4-tier chain: explicit key → `api_key_env` → provider-type default env → generic `API_KEY` fallback. (`crates/provider/src/lib.rs:59-81`)
- **Providers thread `api_key: String` into their constructors** and use it in `authenticated_request()` as `Bearer`, `x-api-key`, or `x-goog-api-key` depending on provider type. (`openai.rs:75`, `anthropic.rs:78`, `gemini.rs:90`, `responses.rs:81`)
- **No credential storage exists.** Keys come from environment variables or inline TOML config — no persistent token files.
- **No CLI subcommand for auth.** The runner dispatches `Start`, `Stop`, `Status`, `Restart`, `Logs`, `CheckUpdate`, `Web`, and `Catalog`. (`crates/runner/src/main.rs:25-86`)
- **No OAuth dependencies** in any Cargo.toml across the workspace.

### Reference: How the target providers authenticate

| Provider | OAuth Flow | Auth server | Token endpoint | API endpoint | Token lifetime |
|---|---|---|---|---|---|
| **GitHub Copilot** | Device code | `github.com/login/device/code` | `api.github.com/copilot_internal/v2/token` | `api.githubcopilot.com` (multi-API: Anthropic Messages, OpenAI Completions, OpenAI Responses depending on model) | ~30 min (Copilot token), GitHub token persists |
| **OpenAI Codex** | Authorization code + PKCE | `auth.openai.com/oauth/authorize` | `auth.openai.com/oauth/token` | `api.openai.com` (standard OpenAI) | ~1 hr access, refresh token persists |
| **Google Gemini CLI** | Authorization code + PKCE | `accounts.google.com/o/oauth2/v2/auth` | `oauth2.googleapis.com/token` | `generativelanguage.googleapis.com` (same as current, but Bearer instead of x-goog-api-key) | ~1 hr access, refresh token persists |

All three produce a Bearer token that slots into the existing `authenticated_request()` pattern. The core addition is **token lifecycle management** (obtain → cache → refresh → retry on 401).

---

## Goals

1. Users can run `oxydra auth login <provider>` to interactively authenticate with GitHub Copilot, OpenAI Codex, or Google Gemini CLI.
2. OAuth tokens are persisted locally (`~/.config/oxydra/oauth_tokens.json`) with `0600` permissions and auto-refreshed transparently before expiry.
3. Existing API key authentication is fully preserved — OAuth is opt-in via config or login command.
4. Provider registry entries gain an `auth` field to declare authentication method; defaults to `api_key` for backward compatibility.
5. OAuth credentials feed into the existing provider construction pipeline with zero changes to the `Provider` trait.
6. Token refresh happens automatically — if a token is expired at request time, it is refreshed before the request is sent.
7. `oxydra auth logout <provider>` clears stored credentials.
8. `oxydra auth status` shows which providers have valid/expired OAuth credentials.
9. Web configurator displays OAuth connection status for all providers and supports Copilot device code login directly. PKCE-based providers (Codex, Gemini) remain CLI-only for login, with status visible in the web UI.
10. CLI OAuth login works over SSH/headless sessions via auto-detected remote mode with manual URL paste for PKCE flows.

## Non-Goals

1. Anthropic OAuth — their ToS discourages third-party OAuth usage for API access.
2. Native OIDC / "Sign in with ChatGPT" identity provider flows — those are for user identity in web apps, not API access.
3. Full PKCE login in the web configurator — the hardcoded client IDs have fixed `redirect_uri` allowlists (localhost:1455, localhost:8085) that cannot be changed to the web configurator's port. Registering Oxydra-specific OAuth apps with custom redirect URIs is a future option if web-first OAuth becomes a priority.
4. OAuth for the runner's own web API — this is about LLM provider auth, not oxydra's web auth.
5. Keychain/keyring integration — tokens stored as a JSON file initially; OS keychain is a future enhancement.

---

## Chosen Architecture

### Design Principles

- **`Provider` trait gains `Arc<dyn TokenSource>` instead of `String` for credentials.** Each provider stores an `Arc<dyn TokenSource>` and calls `token_source.token().await` in `authenticated_request()`. For API keys this is a `StaticToken`; for OAuth it's a refreshing implementation. This is necessary because Copilot tokens expire every ~30 min, so snapshotting a token at construction time would cause mid-session 401 failures with no recovery path.
- **New `oauth` module in `provider` crate.** Owns OAuth flow implementations (device code, PKCE), token storage, and refresh logic. No new crate — the provider crate already has `reqwest` and `tokio`. **Exception:** the localhost callback server for PKCE flows lives in `crates/runner` since it's a CLI concern, not a provider concern.
- **`TokenSource` trait for refreshable credentials.** A trait that produces a current valid token string, with implementations for static keys and each OAuth provider. Implementations use an internal `tokio::sync::Mutex` to deduplicate concurrent refresh requests (prevents thundering herd when multiple requests hit an expired token simultaneously).
- **Config-driven auth method selection.** `ProviderRegistryEntry` gains an `auth` field (`api_key` | `copilot` | `codex` | `gemini_cli`). When set to an OAuth variant, `build_provider()` constructs the appropriate `TokenSource` from stored OAuth tokens instead of wrapping an API key in `StaticToken`.
- **CLI-driven login.** A new `Auth` subcommand handles login/logout/status. Login flows are interactive (device code prints URL+code; PKCE opens browser and starts localhost callback server).
- **401 retry in `ReliableProvider`.** Add `ProviderError::AuthExpired` variant. `is_retriable_provider_error()` treats 401 as retriable (up to 1 retry). Before retrying, the `TokenSource` is asked to force-refresh. This is critical because the current retry logic only retries on 429 and 5xx — 401 is silently non-retriable (`crates/provider/src/retry.rs:212-217`).

### Credential Flow

```
User runs: oxydra auth login copilot
    │
    ▼
OAuth flow (device code / PKCE)
    │
    ▼
Tokens stored: ~/.config/oxydra/oauth_tokens.json
    {
      "copilot": {
        "github_token": "gho_...",
        "copilot_token": "tid=...;exp=...",
        "copilot_expires": 1711234567000
      }
    }
    │
    ▼
Config references it:
    [providers.registry.copilot]
    provider_type = "copilot"
    auth = "copilot"
    catalog_provider = "github-copilot"
    │
    ▼
build_provider() sees provider_type = "copilot" + auth = "copilot"
    → loads token from oauth_tokens.json
    → if expired, refreshes (exchange github_token → new copilot_token)
    → builds CopilotProvider with model-aware dispatch:
       - Claude models → AnthropicProvider (Messages API)
       - GPT-4/4o, Gemini → OpenAIProvider (chat/completions)
       - GPT-5+ → OpenAIResponsesProvider (responses API)
    → all inner providers share the same CopilotTokenSource + base_url
    → sets base_url from copilot token's proxy-ep field
    → sets extra_headers for Copilot compat
```

### Token Storage Format

File: `~/.config/oxydra/oauth_tokens.json` (mode `0600`, parent directory `0700`)

**Security requirements:**
- Create parent directory with `0700` if it doesn't exist
- On read, verify file permissions are no wider than `0600` — warn if not
- Use `fs2::FileExt::lock_exclusive()` for file locking to prevent corruption from concurrent access (e.g., two `oxydra auth login` commands, or login during a running session)
- The file contains long-lived credentials (GitHub tokens, refresh tokens) — treat as highly sensitive
- `oxydra auth logout` must overwrite file content with zeros before unlinking (secure delete)

```json
{
  "copilot": {
    "provider": "copilot",
    "github_token": "gho_xxxx",
    "copilot_token": "tid=...;exp=...;proxy-ep=...",
    "copilot_expires_at": 1711234567000,
    "enterprise_domain": null
  },
  "codex": {
    "provider": "codex",
    "access_token": "eyJ...",
    "refresh_token": "v1.xxx",
    "expires_at": 1711234567000,
    "account_id": "org-xxx"
  },
  "gemini_cli": {
    "provider": "gemini_cli",
    "access_token": "ya29.xxx",
    "refresh_token": "1//0xxx",
    "expires_at": 1711234567000,
    "project_id": "cloud-code-assist-xxx"
  }
}
```

### Provider-Specific Auth Details

#### GitHub Copilot (Device Code Flow)

Based on Pi's `github-copilot.ts` and OpenCode's `copilot.go`:

1. **Login:** POST `github.com/login/device/code` with hardcoded client ID (`Iv1.b507a08c87ecfe98` — same as VS Code Copilot Chat) and `scope=read:user`
2. **User action:** Visit URL, enter code displayed in terminal
3. **Poll:** POST `github.com/login/oauth/access_token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code` until user completes
4. **Token exchange:** GET `api.github.com/copilot_internal/v2/token` with `Authorization: Bearer <github_token>` → returns short-lived Copilot token
5. **API calls:** Use Copilot token as Bearer against `api.githubcopilot.com` with model-family-aware routing (see "Copilot Multi-API Routing" below)
6. **Refresh:** Copilot tokens expire ~30 min. Re-exchange the persistent GitHub token for a fresh Copilot token.
7. **Extra headers:** `Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id`, `X-Initiator`, `Openai-Intent` (required by Copilot API)

#### Copilot Multi-API Routing

Research into Pi (`badlogic/pi-mono`), OpenCode (`opencode-ai/opencode`), and the official GitHub Copilot SDK (`github/copilot-sdk`) reveals that the Copilot API supports **three different wire formats** depending on the model family. All requests go to the same `api.githubcopilot.com` (or `api.individual.githubcopilot.com`) base URL, but the API path and request/response format differ:

| Model family | Wire API | API path | Examples |
|---|---|---|---|
| Claude | Anthropic Messages API | `/anthropic/v1/messages` (via Anthropic SDK) | claude-sonnet-4.6, claude-opus-4.5 |
| GPT-4/4o, Gemini | OpenAI Chat Completions | `/v1/chat/completions` | gpt-4.1, gpt-4o, gemini-2.5-pro |
| GPT-5+ | OpenAI Responses API | `/v1/responses` | gpt-5, gpt-5.1, gpt-5.2, o4-mini |

The official Copilot SDK's `ProviderConfig` type confirms this with `type: "openai" | "azure" | "anthropic"` and `wireApi: "completions" | "responses"`.

**How others handle this:**
- **Pi (badlogic/pi-mono):** Model-family-aware dispatch. Claude models use the Anthropic SDK (native Messages API), GPT-4/Gemini use OpenAI chat completions, GPT-5+ use OpenAI Responses API. Each model in Pi's catalog specifies its `api` field.
- **OpenCode (opencode-ai/opencode):** Simple approach — routes ALL models through `/v1/chat/completions` with a monkeypatch for Claude streaming quirks. Loses Anthropic-native features (extended thinking, proper multi-tool streaming).
- **Official Copilot SDK (`github/copilot-sdk`):** JSON-RPC thin client that delegates to the Copilot CLI binary, which handles routing internally. SDK exposes `type` and `wireApi` in its provider config type.

**Our approach:** Follow Pi's model-family-aware dispatch since Oxydra already has separate `Openai`, `OpenaiResponses`, and `Anthropic` provider implementations. A new `CopilotProvider` wraps and dispatches to the correct inner provider based on the model name, ensuring Claude models get native Anthropic features and GPT-5+ models get Responses API support.

#### OpenAI Codex (PKCE Auth Code Flow)

Based on Pi's `openai-codex.ts` and OpenAI Codex CLI source:

1. **Login:** Build authorization URL with `auth.openai.com/oauth/authorize`, client ID `app_EMoamEEZ73f0CkXaXp7hrann`, PKCE challenge, redirect to `localhost:1455/auth/callback`
2. **User action:** Browser opens, user logs in with ChatGPT account
3. **Callback:** Local HTTP server on port 1455 captures authorization code
4. **Token exchange:** POST `auth.openai.com/oauth/token` with code + PKCE verifier → access_token + refresh_token
5. **API calls:** Use access_token as Bearer against standard `api.openai.com` endpoints
6. **Refresh:** POST token endpoint with `grant_type=refresh_token`
7. **Account ID:** Extracted from JWT claim at `https://api.openai.com/auth` → `chatgpt_account_id`
8. **Fallback:** If localhost callback fails, prompt user to paste the redirect URL manually

#### Google Gemini CLI (PKCE Auth Code Flow)

Based on Pi's `google-gemini-cli.ts` and Gemini CLI source:

1. **Login:** Build authorization URL with `accounts.google.com/o/oauth2/v2/auth`, client ID from Gemini CLI (same public installed-app credentials used by Google's Gemini CLI — see [Gemini CLI source](https://github.com/google-gemini/gemini-cli)), scopes for `cloud-platform` + `userinfo`, redirect to `localhost:8085/oauth2callback`
2. **User action:** Browser opens, user authenticates with Google account
3. **Callback:** Local HTTP server on port 8085 captures authorization code
4. **Token exchange:** POST `oauth2.googleapis.com/token` with code + PKCE verifier + client secret → access_token + refresh_token
5. **Project discovery:** POST `cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` to discover/provision Cloud Code Assist project
6. **API calls:** Use access_token as `Authorization: Bearer` header against `generativelanguage.googleapis.com` (replacing `x-goog-api-key`)
7. **Refresh:** POST token endpoint with `grant_type=refresh_token`
8. **Free tier:** Any Google account gets a free tier; paid Cloud Code Assist supported via `GOOGLE_CLOUD_PROJECT` env var

---

## Configuration Model

### Provider Registry Entry (types/src/config.rs)

```toml
# GitHub Copilot via OAuth (multi-API routing)
[providers.registry.copilot]
provider_type = "copilot"
auth = "copilot"                      # NEW field
catalog_provider = "github-copilot"   # Copilot-specific model namespace
# base_url auto-set from copilot token's proxy-ep field
# extra_headers auto-set for Copilot compatibility
# model-aware dispatch: Claude→Anthropic API, GPT-4→completions, GPT-5→responses

# OpenAI via Codex OAuth (ChatGPT Plus/Pro subscription)
[providers.registry.codex]
provider_type = "openai_responses"
auth = "codex"                        # NEW field
# api_key resolved from OAuth token store

# Google Gemini via OAuth (free Cloud Code Assist)
[providers.registry.gemini-oauth]
provider_type = "gemini"
auth = "gemini_cli"                   # NEW field
# api_key resolved from OAuth token store
# base_url unchanged (same Gemini API, different auth header)
```

### Auth Method Enum

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    ApiKey,       // Current behavior — resolve from api_key/api_key_env/env
    Copilot,      // GitHub Copilot device code flow
    Codex,        // OpenAI Codex PKCE flow
    GeminiCli,    // Google Gemini CLI PKCE flow
}
```

### Default Registry Update

The built-in registry remains unchanged (openai + anthropic with api_key auth). Users opt into OAuth by adding new entries like the examples above, or by running `oxydra auth login <provider>` which can auto-create entries.

---

## Detailed Code Changes

### Phase 1: Token Source Abstraction & Storage

**Goal:** Introduce `TokenSource` trait, token storage, and `AuthMethod` config — no OAuth flows yet.

#### 1a. Add `AuthMethod` to config types

**File:** `crates/types/src/config.rs`

- Add `AuthMethod` enum (see above)
- Add `auth: AuthMethod` field to `ProviderRegistryEntry` with `#[serde(default)]`
- Update `default_provider_registry()` — no change needed since `AuthMethod::ApiKey` is the default

#### 1b. Add `TokenSource` trait and token store

**File:** `crates/provider/src/oauth/mod.rs` (new module)

```rust
pub mod store;    // Token persistence
pub mod types;    // TokenSource trait, OAuthCredentials

// Phase 2 additions:
// pub mod copilot;
// pub mod codex;
// pub mod gemini_cli;
```

**File:** `crates/provider/src/oauth/types.rs`

```rust
/// Provides a valid access token on demand.
/// Implementations handle caching and refresh internally.
/// All implementations must use an internal tokio::sync::Mutex to deduplicate
/// concurrent refresh requests (prevents thundering herd).
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// Returns a valid token, refreshing if expired (with a configurable buffer,
    /// e.g. 5 min before expiry for Copilot, 5 min for Codex/Gemini).
    async fn token(&self) -> Result<String, ProviderError>;

    /// Force-refresh the token, ignoring any cached value.
    /// Called by ReliableProvider after a 401 response.
    async fn force_refresh(&self) -> Result<String, ProviderError>;
}

/// Static token (for API keys or tests). Never expires.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String, ProviderError> {
        Ok(self.0.clone())
    }
    async fn force_refresh(&self) -> Result<String, ProviderError> {
        // Static tokens can't be refreshed — return the same value.
        // ReliableProvider will propagate the 401 as non-retriable.
        Ok(self.0.clone())
    }
}

/// Stored OAuth credentials per provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider")]
pub enum StoredCredentials {
    #[serde(rename = "copilot")]
    Copilot(CopilotCredentials),
    #[serde(rename = "codex")]
    Codex(CodexCredentials),
    #[serde(rename = "gemini_cli")]
    GeminiCli(GeminiCliCredentials),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotCredentials {
    pub github_token: String,
    pub copilot_token: String,
    pub copilot_expires_at: i64,
    pub enterprise_domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCliCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub project_id: String,
}
```

**File:** `crates/provider/src/oauth/store.rs`

```rust
/// Reads/writes ~/.config/oxydra/oauth_tokens.json with 0600 perms.
/// Uses fs2 file locking to prevent concurrent access corruption.
pub struct TokenStore { ... }

impl TokenStore {
    pub fn default_path() -> PathBuf;                                   // ~/.config/oxydra/oauth_tokens.json
    pub fn load(path: &Path) -> Result<Self, ...>;                      // Acquires shared lock, verifies perms
    pub fn save(&self) -> Result<(), ...>;                              // Acquires exclusive lock, writes, sets 0600
    pub fn get(&self, provider: &str) -> Option<&StoredCredentials>;
    pub fn set(&mut self, provider: &str, creds: StoredCredentials);
    pub fn remove(&mut self, provider: &str);                           // Secure zero before remove
    pub fn providers(&self) -> Vec<String>;
}
```

#### OAuthError Type

**File:** `crates/provider/src/oauth/error.rs`

```rust
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("Network error during OAuth: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Invalid grant: {message}")]
    InvalidGrant { message: String },

    #[error("Token revoked by provider")]
    TokenRevoked,

    #[error("User cancelled authentication")]
    UserCancelled,

    #[error("Callback server port {port} already in use")]
    PortInUse { port: u16 },

    #[error("Timeout waiting for OAuth callback ({timeout_secs}s)")]
    CallbackTimeout { timeout_secs: u64 },

    #[error("Token store error: {0}")]
    Store(String),

    #[error("Device code expired — please try again")]
    DeviceCodeExpired,

    #[error("Provider returned unexpected response: {0}")]
    UnexpectedResponse(String),
}

impl From<OAuthError> for ProviderError {
    fn from(err: OAuthError) -> Self {
        match err {
            OAuthError::TokenRevoked | OAuthError::InvalidGrant { .. } => {
                ProviderError::AuthExpired { message: err.to_string() }
            }
            _ => ProviderError::Transport { message: err.to_string() },
        }
    }
}
```

#### 1c. Update provider constructors to accept `Arc<dyn TokenSource>`

**Files:** `crates/provider/src/openai.rs`, `anthropic.rs`, `gemini.rs`, `responses.rs`

- Change `api_key: String` field to `token_source: Arc<dyn TokenSource>` in each provider struct
- In `authenticated_request()`, call `self.token_source.token().await?` instead of using `self.api_key`
- Update `new()` constructors to accept `Arc<dyn TokenSource>` instead of `String`

#### 1d. Update `build_provider()` to construct `TokenSource` based on `AuthMethod`

**File:** `crates/provider/src/lib.rs`

- Keep `resolve_api_key_for_entry()` for backward compatibility
- Add `build_token_source()` that returns `Arc<dyn TokenSource>`:
  - `AuthMethod::ApiKey` → `Arc::new(StaticToken(resolve_api_key_for_entry(...)?))` — current behavior
  - `AuthMethod::Copilot` → `Arc::new(CopilotTokenSource::from_store(...))`
  - `AuthMethod::Codex` → `Arc::new(CodexTokenSource::from_store(...))`
  - `AuthMethod::GeminiCli` → `Arc::new(GeminiCliTokenSource::from_store(...))`
- `build_provider()` calls `build_token_source()` and passes the result to provider constructors
- For `ProviderType::Copilot`: build a `CopilotProvider` (see 1d-copilot below)
- For Gemini CLI: no `base_url` change, but switch from `x-goog-api-key` to `Bearer` auth header

#### 1d-copilot. Add `ProviderType::Copilot` and `CopilotProvider`

**File:** `crates/types/src/config.rs`

- Add `Copilot` variant to `ProviderType` enum

**File:** `crates/provider/src/copilot.rs` (new)

The `CopilotProvider` implements `Provider` and dispatches to the correct inner provider based on the model name:

```rust
pub struct CopilotProvider {
    token_source: Arc<dyn TokenSource>,
    base_url: String,
    extra_headers: HashMap<String, String>,
    // Lazily-constructed inner providers, all sharing the same token_source + base_url
    anthropic: AnthropicProvider,       // For Claude models
    openai: OpenAIProvider,             // For GPT-4/4o, Gemini models
    responses: OpenAIResponsesProvider, // For GPT-5+ models
}

impl CopilotProvider {
    /// Determine which wire API to use based on the model name.
    fn wire_api_for_model(model: &str) -> CopilotWireApi {
        if model.starts_with("claude-") {
            CopilotWireApi::AnthropicMessages
        } else if model.starts_with("gpt-5") || model.starts_with("o4-") || model.starts_with("o3-") {
            CopilotWireApi::OpenAIResponses
        } else {
            // Default: GPT-4/4o, Gemini, and anything unrecognized
            CopilotWireApi::OpenAICompletions
        }
    }
}

enum CopilotWireApi {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
}
```

Key implementation details:
- All three inner providers share the same `Arc<dyn TokenSource>` (CopilotTokenSource) and base URL
- The Anthropic inner provider uses `authToken` (Bearer) instead of `apiKey` — Copilot's Anthropic endpoint uses Bearer auth, not `x-api-key`
- Extra headers are injected on all requests: `Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id` (static), `X-Initiator`, `Openai-Intent` (dynamic per-request, based on Pi's `buildCopilotDynamicHeaders`)
- Model name prefix matching (`claude-`, `gpt-5`, `o4-`, `o3-`) follows Pi's model catalog. The mapping should be a simple function, not a hardcoded list, so new models work without code changes.
- If the model prefix is unrecognized, default to OpenAI Chat Completions (safest common denominator)

**File:** `crates/provider/src/lib.rs`

- `build_provider()` handles `ProviderType::Copilot`:
  - Extracts `base_url` from the Copilot token's `proxy-ep` field (e.g., `proxy.individual.githubcopilot.com` → `https://api.individual.githubcopilot.com`)
  - Constructs all three inner providers with the shared token source and base URL
  - Wraps in `CopilotProvider`

#### 1e. Add `ProviderError::AuthExpired` and update `ReliableProvider`

**File:** `crates/types/src/provider.rs`

- Add `ProviderError::AuthExpired { message: String }` variant

**File:** `crates/provider/src/retry.rs`

- `ReliableProvider` must store the `Arc<dyn TokenSource>` (shared with the inner provider)
- `is_retriable_provider_error()` now treats `HttpStatus { status: 401, .. }` as retriable (max 1 retry)
- Before retrying a 401, call `token_source.force_refresh().await` to obtain a new token
- If `force_refresh()` returns the same token (e.g. `StaticToken`), don't retry — propagate the 401 immediately
- This is a **BLOCKING constraint**: without this change, Copilot sessions will fail after ~30 min

#### 1f. Update Gemini provider to support Bearer auth

**File:** `crates/provider/src/gemini.rs`

- `authenticated_request()` currently always uses `x-goog-api-key` header
- Add a `use_bearer: bool` field to `GeminiProvider`
- When `use_bearer` is true, use `.bearer_auth(&self.api_key)` instead of the `x-goog-api-key` header
- `GeminiProvider::new()` gains a `use_bearer` parameter
- `build_provider()` passes `use_bearer = true` when `auth` is `GeminiCli`

#### 1g. Add `AuthMethod` to secret masking

**File:** `crates/runner/src/web/masking.rs`

- No change needed — the `api_key` field is already masked, and OAuth tokens live outside the config file

### Phase 2: OAuth Flow Implementations

**Goal:** Implement the three interactive OAuth flows. Uses the `oauth2` crate for RFC-compliant PKCE and device code handling.

#### 2a. GitHub Copilot — Device Code Flow

**File:** `crates/provider/src/oauth/copilot.rs`

Implements the device code flow:

```rust
pub async fn login_copilot(
    enterprise_domain: Option<&str>,
) -> Result<CopilotCredentials, OAuthError> { ... }

pub async fn refresh_copilot_token(
    creds: &CopilotCredentials,
) -> Result<CopilotCredentials, OAuthError> { ... }

pub struct CopilotTokenSource {
    store_path: PathBuf,
    // Internal Mutex prevents thundering herd on concurrent refresh
    inner: tokio::sync::Mutex<CopilotCredentials>,
}

impl TokenSource for CopilotTokenSource {
    async fn token(&self) -> Result<String, ProviderError> {
        // Check expiry with 5-min buffer → refresh if needed → return copilot_token
    }
    async fn force_refresh(&self) -> Result<String, ProviderError> {
        // Always re-exchange github_token → new copilot_token
    }
}
```

Key details:
- Client ID: `Iv1.b507a08c87ecfe98` (VS Code Copilot Chat) — **overridable** via config `oauth.copilot.client_id` for users who register their own GitHub OAuth app
- Device code endpoint: `https://{domain}/login/device/code`
- Token exchange: `https://{domain}/login/oauth/access_token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`
- Copilot token: `https://api.{domain}/copilot_internal/v2/token`
- Uses `oauth2` crate's `DeviceAuthorizationRequest` for RFC-compliant device code flow
- Polling with exponential backoff and `slow_down` handling
- Copilot-specific extra headers (injected at provider construction, not in token source):
  - `Editor-Version: vscode/1.95.0` (must look like a real editor)
  - `Editor-Plugin-Version: copilot-chat/0.23.0`
  - `Copilot-Integration-Id: vscode-chat`
  - **Risk:** These are spoofed values. If GitHub validates them more strictly, this breaks. Document prominently.
- Base URL extracted from token's `proxy-ep` field (e.g., `proxy.individual.githubcopilot.com` → `api.individual.githubcopilot.com`). This is a semicolon-delimited custom format (`tid=...;exp=...;proxy-ep=...`), not JSON — parsing must be explicit and tested.
- GitHub Enterprise Server support via custom domain

#### 2b. OpenAI Codex — PKCE Authorization Code Flow

**File:** `crates/provider/src/oauth/codex.rs`

Implements PKCE auth code flow. The token exchange logic lives here; the localhost callback server is provided by the caller (runner crate).

```rust
/// Build the authorization URL. Caller opens it in a browser.
pub fn build_auth_url() -> (String, PkceVerifier, CsrfToken) { ... }

/// Exchange authorization code for tokens.
pub async fn exchange_code(
    code: &str,
    verifier: PkceVerifier,
) -> Result<CodexCredentials, OAuthError> { ... }

pub async fn refresh_codex_token(
    creds: &CodexCredentials,
) -> Result<CodexCredentials, OAuthError> { ... }

pub struct CodexTokenSource {
    store_path: PathBuf,
    inner: tokio::sync::Mutex<CodexCredentials>,
}

impl TokenSource for CodexTokenSource { ... }
```

Key details:
- Client ID: `app_EMoamEEZ73f0CkXaXp7hrann` — **overridable** via config `oauth.codex.client_id`
- Auth URL: `https://auth.openai.com/oauth/authorize`
- Token URL: `https://auth.openai.com/oauth/token`
- Redirect URI: `http://localhost:1455/auth/callback`
- Scopes: `openid profile email offline_access`
- Uses `oauth2` crate's `AuthorizationCode` flow with PKCE (S256)
- Fallback: if port binding fails, prompt user to paste redirect URL
- Account ID extracted from JWT payload at `https://api.openai.com/auth` → `chatgpt_account_id`

#### 2c. Google Gemini CLI — PKCE Authorization Code Flow

**File:** `crates/provider/src/oauth/gemini_cli.rs`

Implements Google OAuth with Cloud Code Assist project discovery. Like Codex, the callback server is provided by the runner.

```rust
pub fn build_auth_url() -> (String, PkceVerifier, CsrfToken) { ... }

pub async fn exchange_code(
    code: &str,
    verifier: PkceVerifier,
) -> Result<GeminiCliCredentials, OAuthError> { ... }

pub async fn refresh_gemini_cli_token(
    creds: &GeminiCliCredentials,
) -> Result<GeminiCliCredentials, OAuthError> { ... }

pub struct GeminiCliTokenSource {
    store_path: PathBuf,
    inner: tokio::sync::Mutex<GeminiCliCredentials>,
}

impl TokenSource for GeminiCliTokenSource { ... }
```

Key details:
- Client ID: Same public installed-app client ID used by Google's Gemini CLI — **overridable** via config `oauth.gemini_cli.client_id`. See [Gemini CLI source](https://github.com/google-gemini/gemini-cli) for the value.
- Client secret: Public installed-app client secret per RFC 8252 (same as Gemini CLI); **overridable** via config `oauth.gemini_cli.client_secret`. Not a real secret — installed-app OAuth clients are treated as public clients.
- Auth URL: `https://accounts.google.com/o/oauth2/v2/auth`
- Token URL: `https://oauth2.googleapis.com/token`
- Redirect URI: `http://localhost:8085/oauth2callback`
- Scopes: `cloud-platform`, `userinfo.email`, `userinfo.profile`
- Uses `oauth2` crate's `AuthorizationCode` flow with PKCE (S256)
- Project discovery via `cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` and `onboardUser`
- Supports free tier (auto-provisioned) and paid tier (via `GOOGLE_CLOUD_PROJECT` env var)

#### 2d. PKCE Utility

The `oauth2` crate handles PKCE generation natively via `PkceCodeChallenge::new_random_sha256()`. No custom PKCE module needed.

#### 2e. Localhost Callback Server

**File:** `crates/runner/src/oauth_callback.rs` (new — in runner crate, not provider)

The callback server is a CLI concern (it binds localhost ports, interacts with the user's browser). It lives in the runner crate, which already has `hyper` 1.8 as a dependency.

A minimal server that:
- Binds to a specified localhost port
- Waits for a single GET request with `code` and `state` query params
- Validates `state` matches the expected CSRF token
- Returns an HTML success/error page
- Shuts down after receiving the callback or timeout
- Falls back gracefully if port is already in use (returns error, caller prompts for manual URL paste)

```rust
pub async fn wait_for_callback(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, OAuthError> { ... }
```

**Note:** This is ~80 lines using `tokio::net::TcpListener` + minimal HTTP parsing. No need for a full `hyper` service — a raw TCP listener with HTTP/1.1 response is sufficient.

### Phase 3: CLI Integration + Remote Mode

**Goal:** Add `oxydra auth` subcommand with browser launch and auto-detected remote mode for SSH/headless environments.

#### 3a. Add `Auth` subcommand

**File:** `crates/runner/src/main.rs`

```rust
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
enum CliCommand {
    // ... existing commands ...
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
enum AuthAction {
    /// Authenticate with an OAuth provider
    Login {
        /// Provider: copilot, codex, gemini-cli
        provider: String,
        /// Skip browser auto-open; always use manual/paste mode
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove stored OAuth credentials
    Logout {
        /// Provider to log out from
        provider: String,
    },
    /// Show OAuth credential status
    Status,
}
```

**CLI naming convention:** CLI accepts hyphenated names (`gemini-cli`), mapped to underscore internally (`gemini_cli`) at the CLI boundary in `auth.rs`. All internal code, config keys, and storage keys use underscores.

#### 3b. Remote session detection

**File:** `crates/runner/src/auth.rs` (new)

```rust
/// Detect if we're in a remote/headless session where opening a local browser
/// is impossible or pointless.
fn is_remote_session() -> bool {
    // SSH session — browser would open on remote host, not user's machine
    std::env::var("SSH_TTY").is_ok()
        || std::env::var("SSH_CONNECTION").is_ok()
        // Headless Linux — no display server
        || (cfg!(target_os = "linux")
            && std::env::var("DISPLAY").is_err()
            && std::env::var("WAYLAND_DISPLAY").is_err())
}
```

The `--no-browser` CLI flag forces remote mode even in local sessions (useful for tmux-over-SSH and similar edge cases).

#### 3c. Auth command handler with remote mode

**File:** `crates/runner/src/auth.rs`

```rust
/// Map CLI provider name (hyphenated) to internal name (underscored).
fn normalize_provider_name(name: &str) -> &str {
    match name {
        "gemini-cli" => "gemini_cli",
        other => other,
    }
}

pub async fn handle_auth_login(provider: &str, no_browser: bool) -> Result<(), CliError> {
    let provider = normalize_provider_name(provider);
    let remote = no_browser || is_remote_session();

    match provider {
        "copilot" => {
            // Device code flow — works identically in local and remote mode.
            // The only difference: skip open::that() when remote.
            eprintln!("Logging in to GitHub Copilot...");
            let device_info = oauth::copilot::start_device_code(None).await?;
            if !remote {
                let _ = open_url(&device_info.verification_uri);
            }
            eprintln!("\nOpen this URL in any browser:");
            eprintln!("  {}", device_info.verification_uri);
            eprintln!("\nEnter code: {}\n", device_info.user_code);
            eprintln!("Waiting for authorization...");
            let creds = oauth::copilot::poll_device_code(device_info).await?;
            let mut store = TokenStore::load_or_default()?;
            store.set("copilot", StoredCredentials::Copilot(creds));
            store.save()?;
            eprintln!("✓ Logged in to GitHub Copilot");
        }
        "codex" => {
            eprintln!("Logging in to OpenAI Codex...");
            let (url, verifier, state) = oauth::codex::build_auth_url();
            let code = obtain_pkce_code(
                &url, state.secret(), 1455, remote, "OpenAI"
            ).await?;
            let creds = oauth::codex::exchange_code(&code, verifier).await?;
            let mut store = TokenStore::load_or_default()?;
            store.set("codex", StoredCredentials::Codex(creds));
            store.save()?;
            eprintln!("✓ Logged in to OpenAI Codex");
        }
        "gemini_cli" => {
            eprintln!("Logging in to Google Gemini CLI...");
            let (url, verifier, state) = oauth::gemini_cli::build_auth_url();
            let code = obtain_pkce_code(
                &url, state.secret(), 8085, remote, "Google"
            ).await?;
            let creds = oauth::gemini_cli::exchange_code(&code, verifier).await?;
            let mut store = TokenStore::load_or_default()?;
            store.set("gemini_cli", StoredCredentials::GeminiCli(creds));
            store.save()?;
            eprintln!("✓ Logged in to Google Gemini CLI");
        }
        _ => return Err(CliError::UnknownProvider(provider.to_string())),
    }
    Ok(())
}

/// Obtain an authorization code via PKCE — either callback server or manual paste.
async fn obtain_pkce_code(
    auth_url: &str,
    expected_state: &str,
    port: u16,
    remote: bool,
    provider_label: &str,
) -> Result<String, OAuthError> {
    if remote {
        // Remote mode: no callback server, manual URL paste
        eprintln!("\n⚠  Remote session detected. Manual authorization required.\n");
        eprintln!("Step 1: Open this URL in your local browser:");
        eprintln!("  {auth_url}\n");
        eprintln!("Step 2: After logging in with {provider_label}, your browser will redirect to:");
        eprintln!("  http://localhost:{port}/...");
        eprintln!("  The page may show a connection error — that's expected.\n");
        eprintln!("Step 3: Copy the FULL URL from your browser's address bar and paste it here:");
        eprint!("> ");
        let pasted = read_line_from_stdin()?;
        parse_code_from_redirect_url(&pasted, expected_state)
    } else {
        // Local mode: try callback server, fall back to manual paste
        let _ = open_url(auth_url);
        eprintln!("Opening browser... If it doesn't open, visit:\n  {auth_url}");
        match wait_for_callback(port, expected_state, Duration::from_secs(120)).await {
            Ok(code) => Ok(code),
            Err(OAuthError::PortInUse { .. } | OAuthError::CallbackTimeout { .. }) => {
                eprintln!("\nCallback server unavailable. Paste the redirect URL instead:");
                eprint!("> ");
                let pasted = read_line_from_stdin()?;
                parse_code_from_redirect_url(&pasted, expected_state)
            }
            Err(e) => Err(e),
        }
    }
}

/// Parse authorization code from a pasted redirect URL.
fn parse_code_from_redirect_url(url: &str, expected_state: &str) -> Result<String, OAuthError> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|_| OAuthError::UnexpectedResponse("invalid URL".into()))?;
    let code = parsed.query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| OAuthError::UnexpectedResponse("no 'code' parameter in URL".into()))?;
    if let Some((_, state)) = parsed.query_pairs().find(|(k, _)| k == "state") {
        if state != expected_state {
            return Err(OAuthError::UnexpectedResponse("state mismatch — possible CSRF".into()));
        }
    }
    Ok(code)
}

pub async fn handle_auth_logout(provider: &str) -> Result<(), CliError> {
    let provider = normalize_provider_name(provider);
    let mut store = TokenStore::load_or_default()?;
    
    // Revoke token server-side before removing locally
    if let Some(creds) = store.get(provider) {
        match creds {
            StoredCredentials::Copilot(_) => {
                // GitHub tokens: DELETE https://api.github.com/applications/{client_id}/token
                // Best-effort; don't fail logout if revocation fails
            }
            StoredCredentials::GeminiCli(g) => {
                // Google: POST https://oauth2.googleapis.com/revoke?token={refresh_token}
                let _ = revoke_google_token(&g.refresh_token).await;
            }
            StoredCredentials::Codex(_) => {
                // OpenAI: no public revocation endpoint
            }
        }
    }
    
    store.remove(provider);  // Secure zero + remove
    store.save()?;
    eprintln!("✓ Logged out of {provider}");
    Ok(())
}

pub async fn handle_auth_status() -> Result<(), CliError> { ... }
```

**SSH port forwarding (documented alternative for power users):**

Users who prefer the normal browser callback flow over SSH can use port forwarding:
```sh
# Forward callback ports from remote to local machine
ssh -L 1455:localhost:1455 -L 8085:localhost:8085 user@pi.local
# Then run login normally — callbacks tunnel through
oxydra auth login codex
```

This is documented in help text and README but not the primary flow.

#### 3d. Browser launch utility

Use the `open` crate (v5) for cross-platform browser launch. It handles Linux (xdg-open), macOS (open), WSL, and Wayland correctly.

```rust
fn open_url(url: &str) -> Result<(), OAuthError> {
    open::that(url).map_err(|e| {
        tracing::warn!("Failed to open browser: {e}");
        OAuthError::BrowserOpenFailed
    })
}
```

### Phase 3.5: Web Configurator OAuth Integration

**Goal:** Show OAuth status in the web UI, support Copilot device code login from the web, and update onboarding to recognize OAuth providers.

#### Why different providers get different web UI treatment

The core constraint is that hardcoded client IDs have fixed `redirect_uri` allowlists (`localhost:1455`, `localhost:8085`). The web configurator runs on a different port (typically `:8080`), so it **cannot** host PKCE OAuth callbacks. However, the Copilot **device code flow** has no redirect URI — it's designed for browserless devices and works from any context.

| Provider | Web UI login? | Why |
|---|---|---|
| Copilot | ✅ Full device code flow | No redirect_uri needed — user opens URL on any device |
| Codex | ❌ CLI instructions only | PKCE redirect must go to localhost:1455 |
| Gemini | ❌ CLI instructions only | PKCE redirect must go to localhost:8085 |

#### 3.5a. OAuth status API

**File:** `crates/runner/src/web/auth.rs` (new)

```rust
/// GET /api/v1/auth/status
/// Returns connection status for all OAuth providers.
pub async fn get_auth_status(
    State(state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let store = TokenStore::load(&TokenStore::default_path()).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    
    let providers: Vec<AuthProviderStatus> = ["copilot", "codex", "gemini_cli"]
        .iter()
        .map(|name| {
            match store.get(name) {
                Some(creds) => {
                    let (connected, expires_at) = match creds {
                        StoredCredentials::Copilot(c) => (true, Some(c.copilot_expires_at)),
                        StoredCredentials::Codex(c) => (true, Some(c.expires_at)),
                        StoredCredentials::GeminiCli(c) => (true, Some(c.expires_at)),
                    };
                    AuthProviderStatus {
                        name: name.to_string(),
                        connected,
                        expires_at,
                        login_method: if *name == "copilot" { "web" } else { "cli" },
                    }
                }
                None => AuthProviderStatus {
                    name: name.to_string(),
                    connected: false,
                    expires_at: None,
                    login_method: if *name == "copilot" { "web" } else { "cli" },
                },
            }
        })
        .collect();
    
    Json(AuthStatusResponse { providers })
}

#[derive(Serialize)]
struct AuthStatusResponse {
    providers: Vec<AuthProviderStatus>,
}

#[derive(Serialize)]
struct AuthProviderStatus {
    name: String,
    connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    /// "web" if the provider supports login from the web UI, "cli" if CLI-only
    login_method: &'static str,
}
```

#### 3.5b. Copilot device code flow via web API

**File:** `crates/runner/src/web/auth.rs`

The Copilot device code flow is decomposed into start + poll steps for the web UI:

```rust
/// POST /api/v1/auth/login/copilot
/// Initiates device code flow. Returns device code info for the UI to display.
pub async fn start_copilot_login(
    State(state): State<Arc<WebState>>,
) -> impl IntoResponse {
    let device_info = oauth::copilot::start_device_code(None).await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    // Store device_info in a short-lived session map for polling
    state.pending_device_codes.insert(session_id.clone(), device_info.clone());
    Json(DeviceCodeResponse {
        session_id,
        verification_uri: device_info.verification_uri,
        user_code: device_info.user_code,
        expires_in: device_info.expires_in,
    })
}

/// GET /api/v1/auth/login/copilot/poll?session_id=xxx
/// Polls for device code completion. Frontend calls this every ~5 seconds.
pub async fn poll_copilot_login(
    State(state): State<Arc<WebState>>,
    Query(params): Query<PollParams>,
) -> impl IntoResponse {
    let device_info = state.pending_device_codes.get(&params.session_id)?;
    match oauth::copilot::poll_device_code_once(&device_info).await {
        Ok(Some(creds)) => {
            let mut store = TokenStore::load_or_default()?;
            store.set("copilot", StoredCredentials::Copilot(creds));
            store.save()?;
            state.pending_device_codes.remove(&params.session_id);
            Json(PollResponse { status: "complete" })
        }
        Ok(None) => Json(PollResponse { status: "pending" }),
        Err(OAuthError::DeviceCodeExpired) => {
            state.pending_device_codes.remove(&params.session_id);
            Json(PollResponse { status: "expired" })
        }
        Err(e) => /* error response */,
    }
}

/// POST /api/v1/auth/logout/{provider}
/// Revokes and removes stored OAuth credentials.
pub async fn web_auth_logout(
    Path(provider): Path<String>,
) -> impl IntoResponse { ... }
```

**Security:** These endpoints must be protected by the web configurator's existing auth layer (token auth mode). Otherwise anyone who can reach the web port could initiate a Copilot login or view token status.

#### 3.5c. Copilot login refactoring

To support both CLI and web flows, the Copilot login is decomposed from a single `login_copilot()` function into:

**File:** `crates/provider/src/oauth/copilot.rs`

```rust
/// Start device code flow — returns info for display (URL, user code).
pub async fn start_device_code(
    enterprise_domain: Option<&str>,
) -> Result<DeviceCodeInfo, OAuthError> { ... }

/// Poll once — returns Some(creds) if the user completed auth, None if still pending.
pub async fn poll_device_code_once(
    info: &DeviceCodeInfo,
) -> Result<Option<CopilotCredentials>, OAuthError> { ... }

/// Blocking poll loop — for CLI usage. Calls poll_device_code_once in a loop.
pub async fn poll_device_code(
    info: DeviceCodeInfo,
) -> Result<CopilotCredentials, OAuthError> {
    loop {
        match poll_device_code_once(&info).await? {
            Some(creds) => return Ok(creds),
            None => tokio::time::sleep(Duration::from_secs(info.interval)).await,
        }
    }
}
```

#### 3.5d. Frontend changes

**File:** `crates/runner/static/js/app.js` and `crates/runner/static/index.html`

**Onboarding wizard Step 4 (Provider Setup) — add OAuth section above API key fields:**

```
┌─────────────────────────────────────────────────────┐
│  Provider Setup                                      │
│                                                      │
│  ── OAuth Connections ────────────────────────────── │
│  🟢 Copilot: Connected     [Disconnect]              │
│  🔴 Codex: Not connected                            │
│  🔴 Gemini: Not connected                           │
│                                                      │
│  [Connect GitHub Copilot]                            │
│                                                      │
│  💡 For Codex/Gemini, run in your terminal:          │
│     oxydra auth login codex                          │
│     oxydra auth login gemini-cli                     │
│                                                      │
│  ── API Key Provider ────────────────────────────── │
│  Provider Type: [OpenAI ▼]                           │
│  API Key Env: [OPENAI_API_KEY]                       │
│  API Key: [••••••••]                                 │
│                                                      │
│  ✓ At least one provider must be configured          │
└─────────────────────────────────────────────────────┘
```

**Copilot device code modal (shown when user clicks "Connect GitHub Copilot"):**

```
┌─────────────────────────────────────────────────────┐
│  Connect GitHub Copilot                    [✕]       │
│                                                      │
│  1. Open this URL:                                   │
│     🔗 https://github.com/login/device               │
│                                                      │
│  2. Enter this code:                                 │
│     ┌──────────────┐                                 │
│     │  ABCD-1234   │  [📋 Copy]                      │
│     └──────────────┘                                 │
│                                                      │
│  ⏳ Waiting for authorization...                     │
│                                                      │
│  [Cancel]                                            │
└─────────────────────────────────────────────────────┘
```

Frontend polls `GET /api/v1/auth/login/copilot/poll` every 5 seconds. On `"complete"`, the modal closes and the status badge updates to green.

**Dashboard — add OAuth status card:**

The main dashboard should show an "OAuth Connections" card alongside existing provider status, so users can see at a glance which OAuth providers are connected.

#### 3.5e. Update `check_provider_configured`

**File:** `crates/runner/src/web/` (onboarding logic)

The existing onboarding check (`check_provider_configured`) only looks for `api_key` / `api_key_env` fields. Update it to also check the OAuth token store:

- If any `ProviderRegistryEntry` has `auth` set to an OAuth variant AND the token store has valid credentials for that provider → consider it configured.
- If a user has logged in via `oxydra auth login copilot` but hasn't added a `[providers.registry.copilot]` entry → still show as connected in the OAuth section, but note that a registry entry is needed to use it.

### Phase 4: Model Catalog for Copilot

**Goal:** Support Copilot's model namespace and wire API mapping.

**Why only Copilot needs catalog changes:**
- **Codex OAuth** uses `provider_type = "openai_responses"` against the standard `api.openai.com` endpoint. Same models as API key access → validates against the existing `openai` catalog namespace. No changes needed.
- **Gemini CLI OAuth** uses `provider_type = "gemini"` against the standard `generativelanguage.googleapis.com` endpoint. Same models as API key access → validates against the existing `google` catalog namespace. No changes needed.
- **Copilot** uses `provider_type = "copilot"` against `api.githubcopilot.com`, which exposes models from multiple providers (GPT-4o, Claude, Gemini) under Copilot-specific model IDs. These don't exist in any single existing catalog namespace.

#### 4a. Copilot catalog provider

The GitHub Copilot API serves models from multiple vendors, each requiring a different wire API format (see "Copilot Multi-API Routing" above). The catalog must capture both the model IDs and their wire API type.

Options:

- **Option A (simple):** Use `catalog.skip_catalog_validation = true` with capability overrides on the registry entry. Wire API routing is handled by prefix matching in `CopilotProvider::wire_api_for_model()` (see Phase 1d-copilot). This works today with zero catalog changes.
- **Option B (proper):** Add a `github-copilot` namespace to the model catalog with known Copilot models and their wire API type as metadata. This enables model discovery via `oxydra catalog list --provider copilot` and makes the wire API mapping explicit rather than inferred from prefixes.

Recommend **Option A** for initial release, with Option B as a follow-up. The prefix-based routing in `CopilotProvider` is sufficient and matches how Pi handles it (Pi's model catalog tags each model with its `api` field, but the actual dispatch logic is prefix-based).

#### 4b. Copilot model discovery (future enhancement)

The Copilot token exchange response (`api.github.com/copilot_internal/v2/token`) includes model capability information. A future `oxydra auth status --models` command could query this to show which models the user's subscription grants access to, along with their wire API type. This would also feed into Option B's catalog if implemented.

### Phase 5: Guest VM Token Forwarding

**Goal:** Make OAuth tokens available inside the guest VM.

The runner currently passes credentials to the guest VM via environment variables and the bootstrap envelope. For OAuth:

#### Process Tier (no forwarding needed)

In process tier, `build_provider()` runs in the same process as the agent. OAuth `TokenSource` implementations work directly — no special handling.

#### Container/VM Tier (config rewriting required)

The guest VM cannot perform OAuth flows or refresh tokens. The runner must rewrite the agent config before passing it to the guest:

1. **At session start:** The runner resolves the current OAuth token via `token_source.token().await`
2. **Config rewriting:** For each `ProviderRegistryEntry` with an OAuth `auth` method:
   - Set `api_key` to the resolved token string
   - Set `auth` to `api_key` (the guest sees a plain API key, not OAuth)
   - For Copilot: also set `base_url` and `extra_headers` explicitly. The `provider_type` stays as `copilot` — the `CopilotProvider` works the same with a `StaticToken` wrapping the resolved Copilot bearer token. The model-aware dispatch is purely local logic.
3. **Write the rewritten config** to the workspace config dir that gets mounted into the guest
4. **Token refresh:** The guest can't refresh tokens. Two strategies:
   - **Phase 5a (simple):** Refresh at session start with a generous buffer. Copilot sessions are limited to ~25 min before the token expires. Acceptable for initial release.
   - **Phase 5b (proper):** The runner exposes a token refresh endpoint on the control socket. The guest's `ReliableProvider` calls back to the runner on 401 to get a fresh token. This requires adding a `ControlSocket`-aware `TokenSource` implementation that lives in the guest.

**BLOCKING constraint:** Without the config rewriting step, OAuth providers won't work in container/VM tier. The rewriting logic should live in the bootstrap pipeline alongside `collect_config_env_vars_with_paths()`.

---

## Phased Rollout

### Phase 1: Foundation (Token Source + Storage + Config + 401 Retry + CopilotProvider)
**Deliverables:**
- `AuthMethod` enum in types crate
- `ProviderType::Copilot` variant in types crate
- `ProviderError::AuthExpired` variant in types crate
- `TokenSource` trait (with `force_refresh`) and `TokenStore` (with file locking) in provider crate
- `OAuthError` type in provider crate
- Provider constructors accept `Arc<dyn TokenSource>` instead of `String`
- `CopilotProvider` with model-family-aware dispatch (Claude→Anthropic, GPT-4→Completions, GPT-5→Responses)
- `ReliableProvider` retries on 401 with `force_refresh()` (max 1 retry)
- `build_provider()` dispatches on `AuthMethod` and handles `ProviderType::Copilot`
- Gemini Bearer auth support
- Unit tests for token store serialization/deserialization, 401 retry behavior, Copilot wire API routing

**Gate:** `cargo test -p types -p provider` passes, existing API key auth unaffected, clippy -D warnings clean.

**BLOCKING constraint:** The 401 retry change must land in Phase 1, not Phase 4. Without it, Copilot sessions break after ~30 min with no recovery.

### Phase 2: OAuth Flows
**Deliverables:**
- Copilot device code flow (using `oauth2` crate)
- Codex PKCE flow (token exchange in provider, callback in runner)
- Gemini CLI PKCE flow with project discovery
- Integration tests with mock HTTP servers
- Copilot token format parsing (semicolon-delimited `tid=;exp=;proxy-ep=`) with tests

**Gate:** Each flow can complete end-to-end against the real provider (manual verification). Mock-server integration tests pass in CI.

### Phase 3: CLI Integration + Remote Mode
**Deliverables:**
- `oxydra auth login|logout|status` subcommands with `--no-browser` flag
- Remote session auto-detection (`SSH_TTY`, `SSH_CONNECTION`, headless `DISPLAY` check)
- Manual URL paste mode for PKCE flows over SSH/headless
- `obtain_pkce_code()` helper with local/remote dispatch
- `parse_code_from_redirect_url()` with CSRF state validation
- Localhost callback server in runner crate (local mode)
- Browser launch via `open` crate (local mode)
- CLI naming normalization (`gemini-cli` → `gemini_cli`)
- Server-side token revocation on logout (best-effort)
- CLI parsing tests, remote mode tests
- User-facing documentation in README (including SSH port forwarding as power-user alternative)

**Gate:** Full login → use → logout cycle works for all three providers, both locally and over SSH. `cargo test -p runner` passes.

### Phase 3.5: Web Configurator OAuth Integration
**Deliverables:**
- `GET /api/v1/auth/status` — OAuth connection status for all providers
- `POST /api/v1/auth/login/copilot` + `GET .../poll` — Copilot device code flow via web API
- `POST /api/v1/auth/logout/{provider}` — web-initiated logout
- Copilot login decomposed into `start_device_code()` + `poll_device_code_once()` for web consumption
- Frontend: OAuth status badges on dashboard and onboarding wizard Step 4
- Frontend: Copilot device code flow modal with URL, code display, and polling
- Frontend: CLI instructions for Codex/Gemini login
- `check_provider_configured` updated to recognize OAuth token store
- Web auth endpoints protected by existing token auth layer

**Gate:** Web UI shows accurate OAuth status. Copilot device code login works from web UI. Onboarding recognizes OAuth-configured providers. `cargo test -p runner web::` passes.

### Phase 4: Catalog & Polish
**Deliverables:**
- Copilot model catalog (or skip_catalog_validation guidance) + wire API mapping documentation
- Example config snippets in `examples/config/agent.toml`
- Guidebook chapter update

**Gate:** Existing test suite passes, clippy clean, documentation updated.

### Phase 5: Guest VM Token Forwarding
**Deliverables:**
- Config rewriting in bootstrap: OAuth entries → api_key entries with resolved tokens
- Token injection alongside `collect_config_env_vars_with_paths()` pipeline
- Phase 5a: pre-session refresh with buffer (simple)
- Phase 5b (follow-up): control socket token refresh endpoint
- E2E test with container tier

**Gate:** OAuth-authenticated provider works in container/VM tier sessions. Existing container-tier tests still pass.

---

## New Dependencies

| Crate | Version | Purpose | Added to |
|---|---|---|---|
| `oauth2` | `5` | RFC-compliant PKCE, device code flow, token exchange/refresh | `crates/provider` |
| `open` | `5` | Cross-platform browser launch (xdg-open, WSL, Wayland) | `crates/runner` |
| `fs2` | `0.4` | File locking for token store (`lock_exclusive`) | `crates/provider` |
| `url` | `2` | Parse redirect URLs in manual paste mode | `crates/runner` |
| `base64` | `0.22` | Already a dependency — used for JWT parsing | — |
| `reqwest` | `0.13` | Already a dependency — used for token exchange HTTP calls | — |

**Note:** The `oauth2` crate with the `reqwest` backend subsumes what would otherwise require `sha2` + `rand` as direct dependencies. It provides RFC-compliant PKCE generation, device code flow handling, and token exchange — reducing hand-rolled security-sensitive code. The `hyper` localhost callback server stays in the runner crate (which already depends on `hyper` 1.8) — it's ~80 lines using `tokio::net::TcpListener` with minimal HTTP parsing, no need for a full `hyper` service.

---

## Testing and Validation

| Area | Test Type | Location |
|---|---|---|
| Token store read/write/permissions/locking | Unit | `crates/provider/src/oauth/store.rs` |
| Credential serialization round-trip | Unit | `crates/provider/src/oauth/types.rs` |
| `OAuthError` → `ProviderError` mapping | Unit | `crates/provider/src/oauth/error.rs` |
| `AuthMethod` serde (default, all variants) | Unit | `crates/types/src/config.rs` |
| `build_provider()` with OAuth auth method | Unit | `crates/provider/src/tests.rs` |
| `ReliableProvider` 401 retry + force_refresh | Unit | `crates/provider/src/retry.rs` |
| `TokenSource` concurrent refresh dedup | Unit | `crates/provider/src/oauth/types.rs` |
| Copilot token format parsing (proxy-ep) | Unit | `crates/provider/src/oauth/copilot.rs` |
| Device code polling with mock server | Integration | `crates/provider/src/oauth/copilot.rs` |
| PKCE callback flow with mock server | Integration | `crates/provider/src/oauth/codex.rs` |
| Localhost callback server | Integration | `crates/runner/src/oauth_callback.rs` |
| CLI `auth` subcommand parsing | Unit | `crates/runner/src/main.rs` |
| CLI provider name normalization | Unit | `crates/runner/src/auth.rs` |
| `is_remote_session()` detection | Unit | `crates/runner/src/auth.rs` |
| `parse_code_from_redirect_url()` + state validation | Unit | `crates/runner/src/auth.rs` |
| Web `GET /api/v1/auth/status` endpoint | Integration | `crates/runner/src/web/auth.rs` |
| Web Copilot device code start/poll | Integration | `crates/runner/src/web/auth.rs` |
| `check_provider_configured` with OAuth tokens | Unit | `crates/runner/src/web/` |
| Existing API key auth unchanged | Regression | `crates/provider/src/tests.rs` |
| Gemini Bearer vs x-goog-api-key dispatch | Unit | `crates/provider/src/gemini.rs` |
| CopilotProvider wire API routing (model→provider dispatch) | Unit | `crates/provider/src/copilot.rs` |
| CopilotProvider Anthropic path for Claude models | Integration | `crates/provider/src/copilot.rs` |
| Guest VM config rewriting (OAuth→api_key) | Unit | `crates/runner/src/bootstrap.rs` |

---

## Blocking Constraints

These must be addressed in the stated phase — deferring them will cause silent failures:

1. **`is_retriable_provider_error()` must handle 401 (Phase 1).** Currently `crates/provider/src/retry.rs:212-217` only retries on 429 and 5xx. Without this, the "retry on 401 with re-exchange" strategy silently doesn't work, and Copilot sessions break after ~30 min.
2. **Provider constructors must accept `Arc<dyn TokenSource>` (Phase 1).** Snapshotting a token as `String` at construction time makes `TokenSource` dead code — the `force_refresh()` path has no way to inject a new token into the provider.
3. **Guest VM config rewriting must happen before container-tier OAuth works (Phase 5).** The guest cannot do OAuth flows or call `token_source.token()`. The runner must rewrite OAuth entries to plain api_key entries with resolved tokens before mounting config into the guest.
4. **Token refresh concurrency guard must exist before multi-turn sessions are safe (Phase 1).** Without an internal `Mutex` in `TokenSource` implementations, concurrent requests hitting an expired token will trigger a thundering herd of refresh requests.

---

## Main Risks

| Risk | Impact | Mitigation |
|---|---|---|
| **Hardcoded client IDs may be revoked** | Login flows break | These are the same IDs used by VS Code Copilot, Codex CLI, and Gemini CLI — revoking them would break those tools too. Add config overrides (`oauth.<provider>.client_id`) so users can substitute their own registered OAuth apps. Monitor for changes. |
| **Copilot spoofed editor headers** | API rejects requests | Copilot requires `Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id` headers that look like a real editor. If GitHub validates these more strictly, this breaks. No mitigation except monitoring — same risk as Pi/OpenCode. |
| **Copilot wire API mismatch for new models** | Wrong API format used | If GitHub adds new model families (e.g., Mistral) that need a different wire API, prefix-based routing may pick the wrong format. Mitigation: default to OpenAI Completions (safest common denominator) and add config override for wire API per model. Monitor Copilot SDK and Pi for new model additions. |
| **Copilot token ~30 min expiry** | Mid-session auth failures | `TokenSource.force_refresh()` called by `ReliableProvider` on 401. Proactive refresh with 5-min buffer before expiry. |
| **Localhost port conflicts** (1455, 8085) | PKCE callback fails | Fallback to manual URL paste (same as Pi does). Ports are fixed by the provider's OAuth app redirect_uri allowlist — can't change them. |
| **Google project discovery API changes** | Gemini CLI login breaks | The `v1internal` API is used by Gemini CLI itself — changes would break them too. Pin to known-working behavior. |
| **Guest VM can't refresh tokens** | Long sessions fail | Phase 5a: pre-session refresh with buffer. Phase 5b: control socket refresh endpoint. |
| **Rate limits differ for OAuth vs API key** | Unexpected throttling | Document that Copilot/Codex have subscription-based limits, not usage-based API limits. |
| **Concurrent token refresh thundering herd** | Redundant refresh calls | Internal `tokio::sync::Mutex` in each `TokenSource` implementation ensures only one refresh at a time. |
| **Google client secret in source code** | Secret scanner false positives | This is a public installed-app client secret per RFC 8252 (same pattern as gcloud, Gemini CLI). Store as a const, make overridable via config. |
| **Remote/SSH PKCE flows** | Callback server unreachable from user's browser | Auto-detect SSH via `SSH_TTY`/`SSH_CONNECTION`; fall back to manual URL paste. Document SSH port forwarding as alternative. Authorization code in pasted URL is single-use and short-lived (~60s), so terminal history exposure is acceptable. |
| **`is_remote_session()` false positives** | User prompted for manual paste unnecessarily | `--no-browser` flag provides explicit override. False positive only means more manual steps, never a broken flow. |
| **Web auth endpoints exposed** | Unauthorized Copilot login initiation | Protect all `/api/v1/auth/*` endpoints behind existing token auth layer. |

---

## Open Questions

1. **Should `oxydra auth login` auto-create a provider registry entry?** Or require the user to configure it manually in agent.toml? Auto-creation is friendlier but more magic.
2. **Token storage location:** `~/.config/oxydra/oauth_tokens.json` follows XDG, but the runner also supports `~/.oxydra/`. Should we check both?
3. **Copilot model catalog:** Should we maintain a Copilot-specific model list with explicit wire API tags, or rely on prefix-based routing in `CopilotProvider`? Prefix matching is simpler and handles new models automatically, but explicit catalog entries give users visibility into available models and their capabilities. Consider adding model discovery to `oxydra auth status` that queries available models from the Copilot token exchange response.
4. **`oxydra auth status` scope:** Should it also query and list available models per authenticated provider? This would help users discover what Copilot models their subscription grants access to.

## Resolved Decisions

- **Use the `oauth2` crate** — it subsumes `sha2`+`rand`, provides RFC-compliant PKCE/device-code handling, and reduces hand-rolled security-sensitive code. The callback server remains manual (~80 lines in runner).
- **Callback server lives in `crates/runner`**, not `crates/provider` — it's a CLI concern that binds localhost ports and interacts with the browser. Provider crate only handles token exchange.
- **Providers accept `Arc<dyn TokenSource>`** instead of `String` — necessary for mid-session token refresh. `StaticToken` wraps API keys for backward compatibility.
- **CLI uses hyphens, internals use underscores** — `gemini-cli` at CLI boundary, `gemini_cli` everywhere else. Normalization in `auth.rs`.
- **Client IDs are overridable** via config `oauth.<provider>.client_id` — mitigates revocation risk.
- **Logout revokes server-side** (best-effort) for Google and GitHub tokens.
- **Copilot uses model-family-aware API dispatch** (Pi approach, not OpenCode's single-endpoint approach). A `CopilotProvider` wraps `AnthropicProvider`, `OpenAIProvider`, and `OpenAIResponsesProvider`, dispatching based on model name prefix. This leverages Oxydra's existing separate provider implementations and ensures Claude models get native Anthropic features (extended thinking, proper tool use streaming) and GPT-5+ models get Responses API support. Research basis: Pi (`badlogic/pi-mono`) uses per-model-family dispatch; OpenCode routes everything through chat/completions (loses features); official Copilot SDK (`github/copilot-sdk`) confirms three wire formats via `ProviderConfig.type` and `wireApi` fields.
- **Web configurator gets Copilot device code login + status display for all.** PKCE providers (Codex, Gemini) are CLI-only for login due to fixed `redirect_uri` constraints. This is a pragmatic split — device code was designed for exactly this use case.
- **Remote/SSH uses auto-detected manual URL paste mode.** `SSH_TTY`/`SSH_CONNECTION` env vars trigger remote mode. `--no-browser` flag for explicit control. SSH port forwarding documented as power-user alternative.

## Future Considerations

- **Register Oxydra-specific OAuth apps** with OpenAI and Google to control `redirect_uri` allowlists. Would unlock full web UI PKCE flows. Trigger: if web-first users become >30% of the user base and OAuth adoption is high.
- **Token proxy service** — a hosted service that receives OAuth callbacks and relays them to Oxydra instances. Only needed if manual URL paste proves to be a major adoption blocker (unlikely — Codex CLI has the same limitation).
