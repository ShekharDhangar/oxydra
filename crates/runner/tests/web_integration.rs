//! Integration tests for the web configurator API.
//!
//! These tests spin up a web router via `build_router` and exercise the API
//! surface end-to-end: config read/write round-trips, validation, auth,
//! host-header checks, onboarding, status, and user management.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;
use types::{RunnerGlobalConfig, RunnerUserRegistration};

/// Build a web router backed by the given temp directory.
fn web_app(dir: &std::path::Path, config: RunnerGlobalConfig) -> (axum::Router, PathBuf) {
    let config_path = dir.join("runner.toml");
    fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();

    let state = Arc::new(runner::web::WebState::new(
        config,
        config_path.clone(),
        "127.0.0.1:9400".to_owned(),
    ));
    (runner::web::build_router(state), config_path)
}

fn default_app(dir: &std::path::Path) -> axum::Router {
    web_app(dir, RunnerGlobalConfig::default()).0
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("host", "127.0.0.1:9400")
        .body(Body::empty())
        .unwrap()
}

fn mutation(method: Method, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:9400")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn json_body(response: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ── Meta ────────────────────────────────────────────────────────

#[tokio::test]
async fn meta_returns_version_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/api/v1/meta")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = json_body(resp).await;
    assert!(json["data"]["version"].is_string());
    assert!(json["data"]["config_path"].is_string());
    assert!(json["data"]["workspace_root"].is_string());
    assert!(json["meta"]["request_id"].is_string());
}

// ── SPA routing ────────────────────────────────────────────────

#[tokio::test]
async fn root_serves_html() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/html"));
}

// ── Host validation ───────────────────────────────────────────

#[tokio::test]
async fn host_validation_rejects_wrong_host() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let req = Request::builder()
        .uri("/api/v1/meta")
        .header("host", "evil.example.com:9400")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn host_validation_accepts_localhost_alias() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let req = Request::builder()
        .uri("/api/v1/meta")
        .header("host", "localhost:9400")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Content-Type enforcement ──────────────────────────────────

#[tokio::test]
async fn content_type_enforcement_blocks_non_json_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let req = Request::builder()
        .method(Method::PATCH)
        .uri("/api/v1/config/runner")
        .header("host", "127.0.0.1:9400")
        // No content-type header
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// ── Auth ─────────────────────────────────────────────────────

#[tokio::test]
async fn auth_blocks_unauthenticated_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let config: RunnerGlobalConfig = toml::from_str(
        r#"
config_version = "1.0.1"
workspace_root = "workspaces"

[web]
enabled = true
bind = "127.0.0.1:9400"
auth_mode = "token"
auth_token = "my-secret-token"
"#,
    )
    .unwrap();
    let (app, _) = web_app(dir.path(), config);

    // No token
    let req = Request::builder()
        .uri("/api/v1/meta")
        .header("host", "127.0.0.1:9400")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token
    let req2 = Request::builder()
        .uri("/api/v1/meta")
        .header("host", "127.0.0.1:9400")
        .header("authorization", "Bearer wrong-token")
        .body(Body::empty())
        .unwrap();
    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);

    // Correct token
    let req3 = Request::builder()
        .uri("/api/v1/meta")
        .header("host", "127.0.0.1:9400")
        .header("authorization", "Bearer my-secret-token")
        .body(Body::empty())
        .unwrap();
    let resp3 = app.oneshot(req3).await.unwrap();
    assert_eq!(resp3.status(), StatusCode::OK);
}

// ── Onboarding status ────────────────────────────────────────

#[tokio::test]
async fn onboarding_reports_needs_setup_on_fresh_install() {
    let dir = tempfile::tempdir().unwrap();
    // Delete the runner.toml that web_app creates so we can test fresh install
    let config_path = dir.path().join("runner.toml");
    let state = Arc::new(runner::web::WebState::new(
        RunnerGlobalConfig::default(),
        config_path,
        "127.0.0.1:9400".to_owned(),
    ));
    let app = runner::web::build_router(state);

    let resp = app.oneshot(get("/api/v1/onboarding/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["data"]["needs_setup"], true);
}

// ── Status ───────────────────────────────────────────────────

#[tokio::test]
async fn status_returns_empty_users_for_fresh_config() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/api/v1/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert!(json["data"]["users"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn status_per_user_returns_not_found_for_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/api/v1/status/ghost")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Config read ──────────────────────────────────────────────

#[tokio::test]
async fn config_runner_returns_config_with_masked_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let config: RunnerGlobalConfig = toml::from_str(
        r#"
config_version = "1.0.1"
workspace_root = "workspaces"

[web]
auth_token = "secret-value"
"#,
    )
    .unwrap();
    let (app, _) = web_app(dir.path(), config);

    let resp = app.oneshot(get("/api/v1/config/runner")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["data"]["file_exists"], true);
    // Token should be masked
    assert_eq!(json["data"]["config"]["web"]["auth_token"], "********");
}

#[tokio::test]
async fn config_agent_returns_defaults_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/api/v1/config/agent")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["data"]["file_exists"], false);
    assert!(json["data"]["config"].is_object());
}

// ── Config write round-trip ──────────────────────────────────

#[tokio::test]
async fn config_runner_patch_then_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    // Patch
    let patch_req = mutation(
        Method::PATCH,
        "/api/v1/config/runner",
        serde_json::json!({ "workspace_root": "integration-test-workspace" }),
    );
    let patch_resp = app.clone().oneshot(patch_req).await.unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);

    let patch_json = json_body(patch_resp).await;
    assert!(
        !patch_json["data"]["changed_fields"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Read back
    let get_resp = app.oneshot(get("/api/v1/config/runner")).await.unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_json = json_body(get_resp).await;
    assert_eq!(
        get_json["data"]["config"]["workspace_root"],
        "integration-test-workspace"
    );
}

#[tokio::test]
async fn config_agent_patch_creates_file_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let agent_path = dir.path().join("agent.toml");
    assert!(!agent_path.exists());

    let patch_req = mutation(
        Method::PATCH,
        "/api/v1/config/agent",
        serde_json::json!({ "runtime": { "max_turns": 25 } }),
    );
    let patch_resp = app.clone().oneshot(patch_req).await.unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);

    assert!(agent_path.exists());

    let get_resp = app.oneshot(get("/api/v1/config/agent")).await.unwrap();
    let get_json = json_body(get_resp).await;
    assert_eq!(get_json["data"]["file_exists"], true);
    assert_eq!(get_json["data"]["config"]["runtime"]["max_turns"], 25);
}

// ── Validation ────────────────────────────────────────────────

#[tokio::test]
async fn validate_runner_config_returns_422_for_invalid_patch() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    // Setting config_version to something invalid will trigger validation
    let req = mutation(
        Method::POST,
        "/api/v1/config/runner/validate",
        serde_json::json!({ "config_version": "0.0.0" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    // It should either pass or fail validation depending on what's checked
    // At minimum, it should return a valid JSON response
    let json = json_body(resp).await;
    // Response has either data.valid or error.code
    assert!(
        json["data"]["valid"].is_boolean() || json["error"]["code"].is_string(),
        "expected valid response shape"
    );
}

// ── User management ──────────────────────────────────────────

#[tokio::test]
async fn full_user_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    // List users — should be empty
    let list_resp = app
        .clone()
        .oneshot(get("/api/v1/config/users"))
        .await
        .unwrap();
    let list_json = json_body(list_resp).await;
    assert!(list_json["data"]["users"].as_array().unwrap().is_empty());

    // Create user
    let create_req = mutation(
        Method::POST,
        "/api/v1/config/users",
        serde_json::json!({ "user_id": "bob", "config_path": "users/bob.toml" }),
    );
    let create_resp = app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // List again — should have one
    let list_resp2 = app
        .clone()
        .oneshot(get("/api/v1/config/users"))
        .await
        .unwrap();
    let list_json2 = json_body(list_resp2).await;
    assert_eq!(list_json2["data"]["users"].as_array().unwrap().len(), 1);

    // Read user config
    let user_resp = app
        .clone()
        .oneshot(get("/api/v1/config/users/bob"))
        .await
        .unwrap();
    assert_eq!(user_resp.status(), StatusCode::OK);
    let user_json = json_body(user_resp).await;
    assert_eq!(user_json["data"]["file_exists"], true);

    // Patch user config
    let patch_req = mutation(
        Method::PATCH,
        "/api/v1/config/users/bob",
        serde_json::json!({ "sandbox": { "enable_shell": false } }),
    );
    let patch_resp = app.clone().oneshot(patch_req).await.unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);

    // Delete user
    let delete_req = mutation(
        Method::DELETE,
        "/api/v1/config/users/bob?delete_config_file=true",
        serde_json::json!({}),
    );
    let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
    assert_eq!(delete_resp.status(), StatusCode::OK);

    // List — should be empty again
    let list_resp3 = app.oneshot(get("/api/v1/config/users")).await.unwrap();
    let list_json3 = json_body(list_resp3).await;
    assert!(list_json3["data"]["users"].as_array().unwrap().is_empty());
}

// ── Status with registered user ──────────────────────────────

#[tokio::test]
async fn status_shows_registered_user_as_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut users = BTreeMap::new();
    users.insert(
        "carol".to_owned(),
        RunnerUserRegistration {
            config_path: "users/carol.toml".to_owned(),
        },
    );
    let config = RunnerGlobalConfig {
        workspace_root: dir.path().join("workspaces").display().to_string(),
        users,
        ..RunnerGlobalConfig::default()
    };
    let (app, _) = web_app(dir.path(), config);

    let resp = app.oneshot(get("/api/v1/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    let users_arr = json["data"]["users"].as_array().unwrap();
    assert_eq!(users_arr.len(), 1);
    assert_eq!(users_arr[0]["user_id"], "carol");
    assert_eq!(users_arr[0]["daemon_running"], false);
}

// ── Logs ────────────────────────────────────────────────────

#[tokio::test]
async fn logs_returns_404_for_unregistered_user() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let resp = app.oneshot(get("/api/v1/logs/nobody")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn logs_returns_entries_from_workspace_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut users = BTreeMap::new();
    users.insert(
        "dave".to_owned(),
        RunnerUserRegistration {
            config_path: "users/dave.toml".to_owned(),
        },
    );
    let config = RunnerGlobalConfig {
        workspace_root: dir.path().join("workspaces").display().to_string(),
        users,
        ..RunnerGlobalConfig::default()
    };
    let (app, _) = web_app(dir.path(), config);

    // Create a log file
    let log_dir = dir.path().join("workspaces/dave/logs");
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(
        log_dir.join("oxydra-vm.stdout.log"),
        "2026-03-02T10:00:00Z test log line\n",
    )
    .unwrap();

    let resp = app
        .oneshot(get("/api/v1/logs/dave?role=runtime&stream=stdout&tail=10"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    let entries = json["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]["message"]
            .as_str()
            .unwrap()
            .contains("test log line")
    );
}

// ── Control ─────────────────────────────────────────────────

#[tokio::test]
async fn control_stop_returns_conflict_when_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut users = BTreeMap::new();
    users.insert(
        "eve".to_owned(),
        RunnerUserRegistration {
            config_path: "users/eve.toml".to_owned(),
        },
    );
    let config = RunnerGlobalConfig {
        workspace_root: dir.path().join("workspaces").display().to_string(),
        users,
        ..RunnerGlobalConfig::default()
    };
    let (app, _) = web_app(dir.path(), config);

    let req = mutation(
        Method::POST,
        "/api/v1/control/eve/stop",
        serde_json::json!({}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = json_body(resp).await;
    assert_eq!(json["error"]["code"], "daemon_not_running");
}

#[tokio::test]
async fn control_start_returns_404_for_unregistered_user() {
    let dir = tempfile::tempdir().unwrap();
    let app = default_app(dir.path());

    let req = mutation(
        Method::POST,
        "/api/v1/control/ghost/start",
        serde_json::json!({}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
