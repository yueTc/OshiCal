use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use oshical_core::youtube::{parse_youtube_account_input, YouTubeAccountInput};
use oshical_core::{
    AddSubscription, AppError, AppService, Platform, RuntimeSecrets, SqliteStore,
    SYNC_SCHEMA_VERSION,
};
use oshical_server::secrets::{save_persisted_youtube_oauth, YoutubeOAuthToken};
use oshical_server::{
    build_router, AppState, RefreshJobManager, SecretVault, ServerConfig, ServerUpdateManager,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;

const YOUTUBE_API_KEY_STATE: &str = "server:secret:youtube_api_key:v1";
static YOUTUBE_ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

async fn state() -> AppState {
    let store = SqliteStore::in_memory().await.expect("db");
    let service = AppService::new(store, RuntimeSecrets::default()).expect("service");
    let (hub, _) = broadcast::channel(32);
    AppState {
        service: Arc::new(service),
        api_token: Some(Arc::new("test-token".to_owned())),
        secret_vault: SecretVault::from_material("test-master-key"),
        hub,
        jobs: RefreshJobManager::new(),
        updates: ServerUpdateManager::new(
            std::env::temp_dir().join(format!("oshical-youtube-update-{}", uuid::Uuid::new_v4())),
        )
        .expect("updates"),
    }
}

async fn call_json(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    authorized: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if authorized {
        request = request.header(header::AUTHORIZATION, "Bearer test-token");
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn call_text(app: &Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn enable_youtube(app: &Router) {
    let (status, mut settings) = call_json(app, Method::GET, "/api/settings", None, true).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["data"]["youtubeEnabled"], false);
    settings["data"]["youtubeEnabled"] = json!(true);
    let (status, saved) = call_json(
        app,
        Method::PUT,
        "/api/settings",
        Some(settings["data"].clone()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["data"]["youtubeEnabled"], true);
}

fn assert_feature_disabled(status: StatusCode, body: &Value) {
    assert!(
        !status.is_success(),
        "disabled YouTube route unexpectedly succeeded: {body}"
    );
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_str),
        Some("YOUTUBE_FEATURE_DISABLED"),
        "{body}"
    );
}

#[tokio::test]
async fn youtube_routes_require_auth_and_are_disabled_by_default() {
    let app = build_router(state().await);
    let probes = [
        (Method::GET, "/api/search?platform=youtube&q=fixture", None),
        (Method::GET, "/api/secrets/youtube", None),
        (
            Method::PUT,
            "/api/secrets/youtube",
            Some(json!({"apiKey": "AIza-fixture-disabled"})),
        ),
        (Method::DELETE, "/api/secrets/youtube", None),
        (Method::GET, "/api/youtube/search?q=fixture", None),
        (Method::GET, "/api/youtube/status", None),
        (
            Method::POST,
            "/api/youtube/subscriptions",
            Some(json!({
                "mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "name": "Fixture Channel",
                "avatarUrl": "https://yt3.googleusercontent.com/fixture-channel.jpg",
                "profileUrl": "https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw"
            })),
        ),
        (
            Method::POST,
            "/api/subscriptions",
            Some(json!({
                "platform": "youtube",
                "mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "name": "Fixture Channel",
                "avatarUrl": ""
            })),
        ),
        (
            Method::POST,
            "/api/youtube/subscriptions/1/links",
            Some(json!({"mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw"})),
        ),
        (
            Method::POST,
            "/api/subscriptions/1/links",
            Some(json!({
                "platform": "youtube",
                "mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw"
            })),
        ),
        (
            Method::DELETE,
            "/api/youtube/subscriptions/1/links/UC_x5XG1OV2P6uZZ5FSM9Ttw",
            None,
        ),
        (Method::POST, "/api/youtube/oauth/start", Some(json!({}))),
        (
            Method::POST,
            "/api/youtube/oauth/complete",
            Some(json!({"code": "fixture-code", "state": "fixture-state"})),
        ),
        (Method::POST, "/api/youtube/oauth/refresh", Some(json!({}))),
        (Method::POST, "/api/youtube/oauth/logout", Some(json!({}))),
        (Method::GET, "/api/youtube/session", None),
        (
            Method::PUT,
            "/api/youtube/session",
            Some(json!({"cookie": "SAPISID=fixture"})),
        ),
        (Method::DELETE, "/api/youtube/session", None),
        (Method::GET, "/api/youtube/session/status", None),
        (Method::POST, "/api/youtube/session/start", Some(json!({}))),
    ];

    for (method, uri, body) in probes {
        let (status, payload) = call_json(&app, method.clone(), uri, body.clone(), false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {payload}");

        // Every YouTube route must stop at the feature gate before credential
        // lookup, upstream client construction, or a network request.
        let (status, payload) = call_json(&app, method, uri, body, true).await;
        assert_feature_disabled(status, &payload);
    }
}

#[tokio::test]
async fn youtube_oauth_callback_checks_feature_gate_before_error_or_code() {
    let app = build_router(state().await);
    for uri in [
        "/api/youtube/oauth/callback?code=fixture-code&state=fixture-state",
        "/api/youtube/oauth/callback?error=access_denied&state=fixture-state",
    ] {
        let (status, body) = call_text(&app, uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("data-error-code=\"YOUTUBE_FEATURE_DISABLED\""),
            "disabled callback did not expose the expected error code: {body}"
        );
        assert!(
            !body.contains("YOUTUBE_OAUTH_ACCESS_DENIED"),
            "Google error was processed before the feature gate: {body}"
        );
    }
}

#[tokio::test]
async fn youtube_oauth_callback_returns_google_error_after_feature_gate() {
    let app = build_router(state().await);
    enable_youtube(&app).await;
    let (status, body) = call_text(
        &app,
        "/api/youtube/oauth/callback?error=access_denied&state=fixture-state",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("data-error-code=\"YOUTUBE_OAUTH_ACCESS_DENIED\""),
        "Google OAuth error code was not preserved: {body}"
    );
}

#[test]
fn every_youtube_error_code_is_preserved_for_api_clients() {
    const CODES: &[&str] = &[
        "YOUTUBE_API_KEY_INVALID",
        "YOUTUBE_BAD_REQUEST",
        "YOUTUBE_COMMUNITY_FORMAT",
        "YOUTUBE_FEATURE_DISABLED",
        "YOUTUBE_FORBIDDEN",
        "YOUTUBE_INVALID_INPUT",
        "YOUTUBE_LIVE_FORMAT",
        "YOUTUBE_NETWORK",
        "YOUTUBE_NON_JSON",
        "YOUTUBE_NOT_CONFIGURED",
        "YOUTUBE_NOT_FOUND",
        "YOUTUBE_OAUTH_ACCESS_DENIED",
        "YOUTUBE_OAUTH_BAD_RESPONSE",
        "YOUTUBE_OAUTH_BUSY",
        "YOUTUBE_OAUTH_EXCHANGE_FAILED",
        "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING",
        "YOUTUBE_OAUTH_STATE_INVALID",
        "YOUTUBE_QUOTA_EXCEEDED",
        "YOUTUBE_RATE_LIMITED",
        "YOUTUBE_RESPONSE_SIZE",
        "YOUTUBE_STORAGE_ERROR",
        "YOUTUBE_SUBSCRIBER_COUNT_MISSING",
        "YOUTUBE_UNAUTHORIZED",
        "YOUTUBE_UPSTREAM_ERROR",
        "YOUTUBE_HTTP_429",
    ];
    for code in CODES {
        let error = AppError::External {
            code: (*code).to_owned(),
            message: "fixture".to_owned(),
        };
        assert_eq!(error.api_code(), *code);
        assert!(
            error.code().starts_with("YOUTUBE_"),
            "{} normalized to {}",
            code,
            error.code()
        );
    }
}

#[test]
fn server_config_ignores_youtube_api_key_environment_bypass() {
    const KEY: &str = "OSHICAL_YOUTUBE_API_KEY";
    const SENTINEL: &str = "AIza-environment-bypass-must-be-ignored";
    let _guard = YOUTUBE_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _env = EnvVarGuard::set(KEY, SENTINEL);
    let config = ServerConfig::from_env().expect("server config");
    assert!(config.secrets.youtube_api_key.is_none());
    assert!(!format!("{config:?}").contains(SENTINEL));
}

#[tokio::test]
async fn youtube_api_key_is_encrypted_and_never_returned_or_synced_in_plaintext() {
    let state = state().await;
    let store = state.service.store().clone();
    assert!(!state.service.youtube_api_key_configured().await);
    let app = build_router(state.clone());
    enable_youtube(&app).await;

    let (status, initial) = call_json(&app, Method::GET, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert_eq!(initial["data"]["apiKey"]["configured"], false);
    assert!(
        initial["data"]["apiKey"].get("apiKey").is_none(),
        "{initial}"
    );

    let secret = "AIza-fixture-youtube-secret";
    let (status, saved) = call_json(
        &app,
        Method::PUT,
        "/api/secrets/youtube",
        Some(json!({"apiKey": secret})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["data"]["configured"], true);
    assert!(saved["data"].get("apiKey").is_none(), "{saved}");
    assert!(!saved.to_string().contains(secret), "{saved}");
    assert!(state.service.youtube_api_key_configured().await);

    let (status, loaded) = call_json(&app, Method::GET, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["data"]["apiKey"]["configured"], true);
    assert!(loaded["data"]["apiKey"].get("apiKey").is_none(), "{loaded}");
    assert!(!loaded.to_string().contains(secret), "{loaded}");
    for forbidden in ["refreshToken", "accessToken", "cookie", "clientSecret"] {
        assert!(
            !loaded.to_string().contains(forbidden),
            "secrets view exposed {forbidden}: {loaded}"
        );
    }

    let raw = store
        .get_client_sync_state(YOUTUBE_API_KEY_STATE)
        .await
        .unwrap()
        .expect("YouTube API key should be persisted in its dedicated encrypted slot");
    assert!(!raw.contains(secret), "plaintext secret entered SQLite");
    assert!(
        !raw.contains("apiKey"),
        "persisted secret envelope should not expose a field named apiKey"
    );

    let changes = store.sync_changes_after(0, 500).await.unwrap().changes;
    for change in changes {
        if let Some(payload) = change.payload {
            assert!(
                !payload.to_string().contains(secret),
                "YouTube secret leaked into sync payload"
            );
        }
    }

    let (status, cleared) =
        call_json(&app, Method::DELETE, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["data"]["configured"], false);
    assert!(!state.service.youtube_api_key_configured().await);

    let (status, loaded) = call_json(&app, Method::GET, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["data"]["apiKey"]["configured"], false);
    assert!(store
        .get_client_sync_state(YOUTUBE_API_KEY_STATE)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn youtube_api_key_can_be_cleared_when_its_ciphertext_is_corrupt() {
    let state = state().await;
    let store = state.service.store().clone();
    let app = build_router(state);
    enable_youtube(&app).await;
    store
        .set_client_sync_state(
            YOUTUBE_API_KEY_STATE,
            &json!({
                "ciphertext": "not-valid-base64",
                "revision": 7,
                "updatedAt": "2026-11-12T00:00:00Z"
            })
            .to_string(),
        )
        .await
        .unwrap();

    let (status, body) = call_json(&app, Method::DELETE, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["configured"], false);
    assert!(store
        .get_client_sync_state(YOUTUBE_API_KEY_STATE)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn youtube_subscription_defaults_live_stats_to_false() {
    let state = state().await;
    let input: AddSubscription = serde_json::from_value(json!({
        "platform": "youtube",
        "mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
        "name": "Fixture Channel",
        "avatarUrl": "",
        "roomId": null
    }))
    .expect("YouTube AddSubscription default");
    assert!(!input.live_stats_enabled);

    let saved = state
        .service
        .store()
        .add_subscription(&input)
        .await
        .expect("store YouTube subscription");
    assert_eq!(saved.platform, Platform::Youtube);
    assert!(!saved.live_stats_enabled);

    let app = build_router(state);
    let (status, bootstrap) = call_json(&app, Method::GET, "/api/sync/bootstrap", None, true).await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    let subscription = bootstrap["data"]["subscriptions"]
        .as_array()
        .expect("subscriptions")
        .iter()
        .find(|item| item["mid"] == "UC_x5XG1OV2P6uZZ5FSM9Ttw")
        .expect("YouTube subscription");
    assert_eq!(subscription["platform"], "youtube");
    assert_eq!(subscription["liveStatsEnabled"], false);
}

#[tokio::test]
async fn youtube_refresh_routes_are_disabled_before_network_probe() {
    let state = state().await;
    let input: AddSubscription = serde_json::from_value(json!({
        "platform": "youtube",
        "mid": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
        "name": "Fixture Channel",
        "avatarUrl": "",
        "roomId": null,
        "liveStatsEnabled": false
    }))
    .expect("YouTube AddSubscription");
    let saved = state
        .service
        .store()
        .add_subscription(&input)
        .await
        .expect("store YouTube subscription");
    let app = build_router(state);

    for uri in [
        format!("/api/subscriptions/{}/refresh", saved.id),
        format!("/api/jobs/refresh/{}", saved.id),
    ] {
        let (status, payload) = call_json(&app, Method::POST, &uri, Some(json!({})), true).await;
        assert_feature_disabled(status, &payload);
    }
}

#[tokio::test]
async fn youtube_oauth_and_web_session_status_never_expose_credentials() {
    let app = build_router(state().await);
    enable_youtube(&app).await;

    let (status, body) =
        call_json(&app, Method::GET, "/api/youtube/session/status", None, true).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    for forbidden in ["refreshToken", "accessToken", "cookie", "clientSecret"] {
        assert!(
            !body.to_string().contains(forbidden),
            "session status exposed {forbidden}: {body}"
        );
    }

    // OAuth app credentials are intentionally absent in this test process.
    // The endpoint must fail closed with an actionable error rather than start
    // a browser flow or return any stored token material.
    let (status, body) = call_json(
        &app,
        Method::POST,
        "/api/youtube/oauth/start",
        Some(json!({})),
        true,
    )
    .await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    for forbidden in ["refreshToken", "accessToken", "cookie", "clientSecret"] {
        assert!(
            !body.to_string().contains(forbidden),
            "OAuth start exposed {forbidden}: {body}"
        );
    }
}

#[tokio::test]
async fn youtube_oauth_missing_required_scope_requires_reauth_without_echoing_tokens() {
    let state = state().await;
    let access_token = "ya29.fixture-access-token";
    let refresh_token = "fixture-refresh-token";
    save_persisted_youtube_oauth(
        state.service.store(),
        &state.secret_vault,
        &YoutubeOAuthToken {
            refresh_token: refresh_token.to_owned(),
            access_token: Some(access_token.to_owned()),
            access_token_expires_at: None,
            scopes: vec!["openid".to_owned()],
            token_type: "Bearer".to_owned(),
        },
    )
    .await
    .expect("save OAuth fixture");
    let app = build_router(state);
    enable_youtube(&app).await;

    let (status, body) = call_json(&app, Method::GET, "/api/secrets/youtube", None, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["oauth"]["configured"], true);
    assert_eq!(body["data"]["oauth"]["expired"], true);
    assert!(
        !body.to_string().contains(access_token),
        "OAuth status exposed the access token: {body}"
    );
    assert!(
        !body.to_string().contains(refresh_token),
        "OAuth status exposed the refresh token: {body}"
    );
    for forbidden in ["accessToken", "refreshToken", "clientSecret"] {
        assert!(
            !body.to_string().contains(forbidden),
            "OAuth status exposed {forbidden}: {body}"
        );
    }
}

#[tokio::test]
async fn sync_schema_is_three_and_legacy_bilibili_rows_default_to_bilibili() {
    assert_eq!(SYNC_SCHEMA_VERSION, 3);

    let state = state().await;
    let store = state.service.store().clone();
    let input: AddSubscription = serde_json::from_value(json!({
        "mid": "880001",
        "name": "旧版 B 站主播",
        "avatarUrl": "",
        "roomId": "880001",
        "scheduleEnabled": true,
        "liveNotifyEnabled": true,
        "liveStatsEnabled": false
    }))
    .expect("legacy AddSubscription JSON");
    store.add_subscription(&input).await.expect("legacy row");

    let app = build_router(state);
    let (status, bootstrap) = call_json(&app, Method::GET, "/api/sync/bootstrap", None, true).await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    assert_eq!(bootstrap["data"]["schemaVersion"], 3);
    let subscription = bootstrap["data"]["subscriptions"]
        .as_array()
        .expect("subscriptions")
        .iter()
        .find(|item| item["mid"] == "880001")
        .expect("legacy Bilibili subscription");
    assert_eq!(subscription["platform"], "bilibili");
    assert_eq!(subscription["handle"], Value::Null);
    assert_eq!(subscription["profileUrl"], Value::Null);
    assert_eq!(subscription["uploadsPlaylistId"], Value::Null);

    let (status, changes) = call_json(
        &app,
        Method::GET,
        "/api/sync/changes?after=0&limit=500",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changes}");
    assert_eq!(changes["data"]["schemaVersion"], 3);
    let subscription_change = changes["data"]["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["entityType"] == "subscription")
        .expect("subscription sync change");
    assert_eq!(subscription_change["payload"]["platform"], "bilibili");
}

#[tokio::test]
async fn account_input_fixture_matches_public_parser() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../scripts/youtube-fixtures/account-inputs.json");
    let fixture: Value =
        serde_json::from_slice(&std::fs::read(&fixture_path).expect("read account input fixture"))
            .expect("parse account input fixture");

    for case in fixture["validCases"].as_array().expect("valid cases") {
        let input = case["input"].as_str().expect("input");
        let expected_kind = case["expected"]["kind"].as_str().expect("expected kind");
        let parsed = parse_youtube_account_input(input).expect(input);
        match (expected_kind, parsed) {
            ("channel", YouTubeAccountInput::ChannelId(value)) => {
                assert_eq!(value, case["expected"]["channelId"].as_str().unwrap());
            }
            ("handle", YouTubeAccountInput::Handle(value)) => {
                assert_eq!(value, case["expected"]["handle"].as_str().unwrap());
            }
            ("video", YouTubeAccountInput::VideoId(value)) => {
                assert_eq!(value, case["expected"]["videoId"].as_str().unwrap());
            }
            ("search", YouTubeAccountInput::Search(value)) => {
                assert_eq!(value, input);
            }
            (kind, parsed) => panic!("{input}: expected {kind}, got {parsed:?}"),
        }
    }

    for case in fixture["invalidCases"].as_array().expect("invalid cases") {
        let input = case["input"].as_str().expect("input");
        assert!(
            parse_youtube_account_input(input).is_err(),
            "invalid fixture input was accepted: {input}"
        );
    }
}
