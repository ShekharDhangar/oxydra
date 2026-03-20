mod catalog_api;
mod config_read;
mod config_write;
mod control;
mod logs;
mod masking;
mod middleware;
mod onboarding;
mod response;
mod schema;
mod state;
mod static_files;
mod status;

use std::path::Path;
use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    middleware::from_fn_with_state,
    routing::{get, post},
};
use serde::Serialize;
use thiserror::Error;
use tokio::net::TcpListener;

use crate::load_runner_global_config;
pub use state::WebState;

const LOW_MEMORY_WARNING_THRESHOLD_MIB: u64 = 2048;
#[cfg(any(test, target_os = "linux"))]
const LOW_POWER_ARM_HINT_MAX_MEMORY_MIB: u64 = 4096;

/// Errors that can occur when running the web server.
#[derive(Debug, Error)]
pub enum WebServerError {
    #[error("failed to load runner config: {0}")]
    Config(#[from] crate::RunnerError),
    #[error("failed to bind web server to `{bind}`: {source}")]
    Bind {
        bind: String,
        #[source]
        source: std::io::Error,
    },
    #[error("web server error: {0}")]
    Serve(#[from] std::io::Error),
}

/// Entry point for `oxydra web`. Loads config, builds the router, and runs
/// the HTTP server until interrupted.
pub async fn run_web_server(
    config_path: &Path,
    bind_override: Option<String>,
) -> Result<(), WebServerError> {
    let config_path = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_owned());
    let global_config = load_runner_global_config(&config_path)?;

    let bind = bind_override
        .as_deref()
        .unwrap_or(&global_config.web.bind)
        .to_owned();

    let web_state = Arc::new(WebState::new(global_config, config_path, bind.clone()));
    let app = build_router(web_state.clone());

    let listener = TcpListener::bind(&bind)
        .await
        .map_err(|source| WebServerError::Bind {
            bind: bind.clone(),
            source,
        })?;

    tracing::info!(bind = %bind, "web configurator started");
    eprintln!("Oxydra Web Configurator running at http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(web_state))
        .await?;

    tracing::info!("web configurator shut down");
    Ok(())
}

pub fn build_router(state: Arc<WebState>) -> Router {
    let api = Router::new()
        .route("/meta", get(meta_handler))
        // Schema metadata endpoint
        .route("/meta/schema", get(schema::get_config_schema))
        // Model catalog endpoints
        .route("/catalog", get(catalog_api::get_catalog))
        .route("/catalog/status", get(catalog_api::get_catalog_status))
        .route("/catalog/refresh", post(catalog_api::refresh_catalog))
        // Phase 2 + 3: config read/write endpoints
        .route(
            "/config/runner",
            get(config_read::get_runner_config).patch(config_write::patch_runner_config),
        )
        .route(
            "/config/runner/validate",
            post(config_write::validate_runner_config),
        )
        .route(
            "/config/agent",
            get(config_read::get_agent_config).patch(config_write::patch_agent_config),
        )
        .route(
            "/config/agent/effective",
            get(config_read::get_agent_config_effective),
        )
        .route(
            "/config/agent/validate",
            post(config_write::validate_agent_config),
        )
        .route(
            "/config/users",
            get(config_read::list_users).post(config_write::create_user),
        )
        .route("/config/users/rename", post(config_write::rename_user))
        .route(
            "/config/users/{user_id}",
            get(config_read::get_user_config)
                .patch(config_write::patch_user_config)
                .delete(config_write::delete_user),
        )
        .route(
            "/config/users/{user_id}/validate",
            post(config_write::validate_user_config),
        )
        // Phase 2: status endpoints
        .route("/status", get(status::get_status))
        .route("/status/{user_id}", get(status::get_user_status))
        // Phase 4: lifecycle control + logs
        .route("/control/{user_id}/start", post(control::start_user_daemon))
        .route("/control/{user_id}/stop", post(control::stop_user_daemon))
        .route(
            "/control/{user_id}/restart",
            post(control::restart_user_daemon),
        )
        .route("/logs/{user_id}", get(logs::get_logs))
        // Phase 2: onboarding
        .route("/onboarding/status", get(onboarding::get_onboarding_status));

    Router::new()
        .nest("/api/v1", api)
        .route("/", get(static_files::serve_index))
        .route("/{*path}", get(static_files::serve_static))
        .layer(from_fn_with_state(state.clone(), middleware::auth_layer))
        .layer(from_fn_with_state(
            state.clone(),
            middleware::content_type_enforcement,
        ))
        .layer(from_fn_with_state(
            state.clone(),
            middleware::host_validation_layer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct MetaResponse {
    version: &'static str,
    config_path: String,
    workspace_root: String,
    host_memory_mib: Option<u64>,
    low_memory_warning: bool,
    low_power_device_warning: bool,
}

async fn meta_handler(State(state): State<Arc<WebState>>) -> response::ApiResponse<MetaResponse> {
    let host_memory_mib = detect_host_memory_mib();
    let low_power_device_warning = detect_low_power_device_hint(host_memory_mib);
    response::ok_response(MetaResponse {
        version: crate::VERSION,
        config_path: state.config_path.display().to_string(),
        workspace_root: state.workspace_root.display().to_string(),
        host_memory_mib,
        low_memory_warning: host_memory_mib
            .is_some_and(|memory_mib| memory_mib < LOW_MEMORY_WARNING_THRESHOLD_MIB),
        low_power_device_warning,
    })
}

#[cfg(target_os = "linux")]
fn detect_host_memory_mib() -> Option<u64> {
    detect_linux_memory_mib()
}

#[cfg(target_os = "macos")]
fn detect_host_memory_mib() -> Option<u64> {
    detect_macos_memory_mib()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_host_memory_mib() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn detect_linux_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_linux_memtotal_mib(&meminfo)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_memtotal_mib(meminfo: &str) -> Option<u64> {
    let mem_total_kib = meminfo
        .lines()
        .find(|line| line.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    Some(mem_total_kib / 1024)
}

#[cfg(target_os = "macos")]
fn detect_macos_memory_mib() -> Option<u64> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let total_bytes = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(total_bytes / (1024 * 1024))
}

#[cfg(target_os = "linux")]
fn detect_low_power_device_hint(host_memory_mib: Option<u64>) -> bool {
    detect_linux_low_power_device_hint(host_memory_mib)
}

#[cfg(not(target_os = "linux"))]
fn detect_low_power_device_hint(_host_memory_mib: Option<u64>) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn detect_linux_low_power_device_hint(host_memory_mib: Option<u64>) -> bool {
    if detect_linux_raspberry_pi_marker() {
        return true;
    }
    arm_low_power_heuristic(std::env::consts::ARCH, host_memory_mib)
}

#[cfg(target_os = "linux")]
fn detect_linux_raspberry_pi_marker() -> bool {
    for path in [
        "/proc/device-tree/model",
        "/sys/firmware/devicetree/base/model",
    ] {
        if let Ok(raw) = std::fs::read(path) {
            let model = String::from_utf8_lossy(&raw);
            if text_contains_raspberry_pi(&model) {
                return true;
            }
        }
    }

    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        return text_contains_raspberry_pi(&cpuinfo);
    }

    false
}

#[cfg(any(test, target_os = "linux"))]
fn text_contains_raspberry_pi(text: &str) -> bool {
    text.to_ascii_lowercase().contains("raspberry pi")
}

#[cfg(any(test, target_os = "linux"))]
fn arm_low_power_heuristic(arch: &str, host_memory_mib: Option<u64>) -> bool {
    matches!(arch, "arm" | "aarch64")
        && host_memory_mib.is_some_and(|memory| memory <= LOW_POWER_ARM_HINT_MAX_MEMORY_MIB)
}

async fn shutdown_signal(state: Arc<WebState>) {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    let tracked_daemons = state.spawned_daemon_pids_snapshot();
    if tracked_daemons.is_empty() {
        tracing::info!("received shutdown signal");
    } else {
        tracing::warn!(
            tracked_daemons = ?tracked_daemons,
            "received shutdown signal; leaving tracked daemons running"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;
    use types::RunnerGlobalConfig;

    fn test_state() -> Arc<WebState> {
        let config = RunnerGlobalConfig::default();
        let config_path = std::path::PathBuf::from("/tmp/test-runner.toml");
        Arc::new(WebState::new(
            config,
            config_path,
            "127.0.0.1:9400".to_owned(),
        ))
    }

    fn token_auth_state() -> Arc<WebState> {
        let config: RunnerGlobalConfig = toml::from_str(
            r#"
config_version = "1.0.1"
workspace_root = "workspaces"

[web]
enabled = true
bind = "127.0.0.1:9401"
auth_mode = "token"
auth_token = "test-token"
"#,
        )
        .expect("token-auth test config should parse");
        let config_path = std::path::PathBuf::from("/tmp/test-runner-token.toml");
        Arc::new(WebState::new(
            config,
            config_path,
            "127.0.0.1:9401".to_owned(),
        ))
    }

    #[tokio::test]
    async fn meta_endpoint_returns_version() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
                    .header("host", "127.0.0.1:9400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["version"], crate::VERSION);
        assert!(json["data"]["low_memory_warning"].is_boolean());
        assert!(json["data"]["low_power_device_warning"].is_boolean());
        assert!(
            json["data"]["host_memory_mib"].is_null() || json["data"]["host_memory_mib"].is_u64()
        );
        assert!(json["meta"]["request_id"].is_string());
    }

    #[test]
    fn parse_linux_memtotal_mib_parses_value() {
        let sample = "MemTotal:       2048000 kB\nMemFree:         123456 kB\n";
        assert_eq!(parse_linux_memtotal_mib(sample), Some(2000));
    }

    #[test]
    fn text_contains_raspberry_pi_detects_marker() {
        assert!(text_contains_raspberry_pi(
            "Model\t: Raspberry Pi 5 Model B"
        ));
        assert!(!text_contains_raspberry_pi("Model\t: Generic ARM SBC"));
    }

    #[test]
    fn arm_low_power_heuristic_requires_arm_and_small_memory() {
        assert!(arm_low_power_heuristic("aarch64", Some(2048)));
        assert!(!arm_low_power_heuristic("x86_64", Some(2048)));
        assert!(!arm_low_power_heuristic("aarch64", Some(8192)));
    }

    #[tokio::test]
    async fn index_serves_html() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("host", "127.0.0.1:9400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/html"),
            "expected text/html, got {content_type}"
        );
    }

    #[tokio::test]
    async fn unknown_api_path_returns_404_or_fallback() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/nonexistent")
                    .header("host", "127.0.0.1:9400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Unknown API routes fall through to SPA fallback, which serves index.html
        // This is expected behavior for SPA routing
        assert!(response.status() == StatusCode::OK || response.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn host_validation_rejects_mismatch() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
                    .header("host", "malicious.example:9400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn content_type_layer_rejects_non_json_mutation() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/config/runner")
                    .header("host", "127.0.0.1:9400")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn auth_layer_blocks_missing_token_and_accepts_valid_token() {
        let app = build_router(token_auth_state());

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
                    .header("host", "127.0.0.1:9401")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
                    .header("host", "127.0.0.1:9401")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }
}
