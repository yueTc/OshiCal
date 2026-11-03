use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use oshical_core::{AddSubscription, AppService, RefreshReport, RuntimeSecrets, SqliteStore};
use oshical_server::{build_router, AppState, RefreshJobManager, SecretVault};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

type TestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn state() -> AppState {
    let store = SqliteStore::in_memory().await.expect("db");
    state_with_store(store)
}

fn state_with_store(store: SqliteStore) -> AppState {
    let service = AppService::new(store, RuntimeSecrets::default()).expect("service");
    let (hub, _) = broadcast::channel(64);
    AppState {
        service: Arc::new(service),
        api_token: Some(Arc::new("test-token".to_owned())),
        secret_vault: SecretVault::from_material("test-master-key"),
        hub,
        jobs: RefreshJobManager::new(),
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

async fn add_streamer(state: &AppState, mid: &str) -> i64 {
    state
        .service
        .store()
        .add_subscription(&AddSubscription {
            mid: mid.to_owned(),
            name: Some(format!("主播{mid}")),
            avatar_url: None,
            room_id: Some(mid.to_owned()),
            aliases: None,
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        })
        .await
        .expect("streamer")
        .id
}

async fn serve(app: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    address
}

async fn connect_v2(address: std::net::SocketAddr, after: i64, device_id: &str) -> TestSocket {
    let (mut socket, _) = connect_async(format!("ws://{address}/api/ws?after={after}&protocol=2"))
        .await
        .expect("connect");
    socket
        .send(Message::Text(
            json!({
                "type": "auth",
                "token": "test-token",
                "deviceId": device_id,
                "protocolVersion": 2
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("auth");
    socket
}

async fn next_json(socket: &mut TestSocket) -> Option<Value> {
    loop {
        let frame = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => return None,
        };
        if let Message::Text(text) = frame {
            return serde_json::from_str(&text).ok();
        }
    }
}

async fn next_message_type(socket: &mut TestSocket, message_type: &str) -> Value {
    for _ in 0..20 {
        if let Some(value) = next_json(socket).await {
            if value.get("type").and_then(Value::as_str) == Some(message_type) {
                return value;
            }
        }
    }
    panic!("did not receive {message_type}");
}

#[tokio::test]
async fn bili_login_routes_are_authenticated_and_do_not_expose_cookies() {
    let app = build_router(state().await);
    let (status, _) = call_json(
        &app,
        Method::POST,
        "/api/bilibili/login/qr",
        Some(json!({})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call_json(&app, Method::GET, "/api/bilibili/status", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["configured"], false);
    assert!(body["data"].get("cookie").is_none());

    let (status, body) = call_json(
        &app,
        Method::POST,
        "/api/bilibili/logout",
        Some(json!({})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["configured"], false);
}

#[tokio::test]
async fn sync_routes_require_authentication() {
    let app = build_router(state().await);
    for uri in ["/api/sync/bootstrap", "/api/sync/changes", "/api/devices"] {
        let (status, _) = call_json(&app, Method::GET, uri, None, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, _) = call_json(
        &app,
        Method::POST,
        "/api/sync/ack",
        Some(json!({"deviceId":"dev","seq":1})),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call_json(&app, Method::DELETE, "/api/devices/device-a", None, false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bootstrap_and_changes_expose_sync_feed() {
    let state = state().await;
    add_streamer(&state, "880001").await;
    let app = build_router(state);

    let (status, bootstrap) = call_json(&app, Method::GET, "/api/sync/bootstrap", None, true).await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    assert_eq!(bootstrap["data"]["schemaVersion"], 2);
    assert!(bootstrap["data"]["snapshotRevision"].is_i64());
    assert!(bootstrap["data"]["subscriptions"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));
    assert!(bootstrap["data"]["weeklyExceptions"].is_array());
    assert!(bootstrap["data"]["scheduleSources"].is_array());

    let (status, changes) = call_json(
        &app,
        Method::GET,
        "/api/sync/changes?after=0&limit=999",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changes}");
    assert_eq!(changes["data"]["schemaVersion"], 2);
    assert!(changes["data"]["latestSeq"].as_i64().unwrap_or(0) > 0);
    assert!(changes["data"]["changes"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));
}

#[tokio::test]
async fn register_ack_and_list_devices() {
    let app = build_router(state().await);
    let (status, registered) = call_json(
        &app,
        Method::POST,
        "/api/devices/register",
        Some(json!({
            "deviceId": "device-a",
            "name": "客厅电脑",
            "platform": "windows",
            "appVersion": "0.2.0"
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    assert_eq!(registered["data"]["deviceId"], "device-a");

    let (status, acknowledged) = call_json(
        &app,
        Method::POST,
        "/api/sync/ack",
        Some(json!({"deviceId":"device-a","seq":7})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{acknowledged}");
    assert_eq!(acknowledged["data"]["lastAckSeq"], 7);

    let (status, devices) = call_json(&app, Method::GET, "/api/devices", None, true).await;
    assert_eq!(status, StatusCode::OK, "{devices}");
    assert!(devices["data"].as_array().is_some_and(|items| items
        .iter()
        .any(|item| item["deviceId"] == "device-a" && item["lastAckSeq"] == 7)));

    let (status, rejected) =
        call_json(&app, Method::DELETE, "/api/devices/device-a", None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");
    let (status, devices) = call_json(&app, Method::GET, "/api/devices", None, true).await;
    assert_eq!(status, StatusCode::OK, "{devices}");
    assert!(devices["data"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["deviceId"] == "device-a")));
}

#[tokio::test]
async fn websocket_v2_auth_catchup_ack_and_two_clients() {
    let state = state().await;
    let streamer_id = add_streamer(&state, "880002").await;
    let latest = state
        .service
        .store()
        .latest_sync_seq()
        .await
        .expect("latest sync seq");
    let app = build_router(state);
    for device_id in ["device-one", "device-two"] {
        let (status, registered) = call_json(
            &app,
            Method::POST,
            "/api/devices/register",
            Some(json!({
                "deviceId": device_id,
                "name": device_id,
                "platform": "windows",
                "appVersion": "0.2.0"
            })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{registered}");
    }
    let address = serve(app.clone()).await;

    let mut first = connect_v2(address, latest, "device-one").await;
    let first_auth = next_json(&mut first).await.expect("first auth");
    assert_eq!(first_auth["type"], "auth_ok");
    assert_eq!(first_auth["protocolVersion"], 2);
    assert_eq!(first_auth["deviceId"], "device-one");
    assert!(first_auth["latestSeq"].is_i64());
    assert!(first_auth["minRetainedSeq"].is_i64());
    assert_eq!(first_auth["resetRequired"], false);

    let mut second = connect_v2(address, latest, "device-two").await;
    let second_auth = next_json(&mut second).await.expect("second auth");
    assert_eq!(second_auth["deviceId"], "device-two");

    let start = Utc::now() + ChronoDuration::days(1);
    let (status, created) = call_json(
        &app,
        Method::POST,
        "/api/schedules",
        Some(json!({
            "streamerId": streamer_id,
            "title": "多设备同步测试",
            "startAt": start,
            "sourceKind": "manual",
            "sourceIdentity": "manual:sync-ws-880002",
            "confidence": 1.0
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");

    let first_batch = next_message_type(&mut first, "sync_batch").await;
    let second_batch = next_message_type(&mut second, "sync_batch").await;
    assert_eq!(first_batch["toSeq"], second_batch["toSeq"]);
    assert!(first_batch["changes"].as_array().is_some_and(|items| items
        .iter()
        .any(|item| { item["entityType"] == "schedule" && item["operation"] == "upsert" })));

    let ack_seq = first_batch["toSeq"].as_i64().expect("ack seq");
    first
        .send(Message::Text(
            json!({"type":"ack","seq":ack_seq}).to_string().into(),
        ))
        .await
        .expect("ack");
    let mut acknowledged = false;
    let mut last_devices = Value::Null;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, devices) = call_json(&app, Method::GET, "/api/devices", None, true).await;
        assert_eq!(status, StatusCode::OK, "{devices}");
        acknowledged = devices["data"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["deviceId"] == "device-one" && item["lastAckSeq"] == ack_seq)
        });
        last_devices = devices;
        if acknowledged {
            break;
        }
    }
    assert!(acknowledged, "ACK was not persisted: {last_devices}");
}

#[tokio::test]
async fn refresh_job_endpoint_returns_background_job_id() {
    let state = state().await;
    let app = build_router(state);
    let (status, accepted) = call_json(&app, Method::POST, "/api/jobs/refresh", None, true).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["data"]["status"], "queued");
    assert!(accepted["data"]["jobId"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
}

#[tokio::test]
async fn refresh_job_status_endpoint_reports_lifecycle() {
    let state = state().await;
    let registration = state.jobs.create_all();
    let job_id = registration.job.job_id;
    state.jobs.begin(&job_id).expect("running job");
    state
        .jobs
        .update_progress(
            &job_id,
            oshical_server::RefreshJobProgress {
                stage: "解析动态".to_owned(),
                completed: 1,
                total: 2,
                percent: 50,
                done: false,
                failed: false,
            },
        )
        .expect("progress");

    let app = build_router(state.clone());
    let (status, running) = call_json(
        &app,
        Method::GET,
        &format!("/api/jobs/{job_id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{running}");
    assert_eq!(running["data"]["jobId"], job_id);
    assert_eq!(running["data"]["status"], "running");
    assert_eq!(running["data"]["progress"]["percent"], 50);

    state
        .jobs
        .finish(
            &job_id,
            RefreshReport {
                refreshed_streamers: 1,
                failed_streamers: Vec::new(),
                new_schedules: 2,
                cancelled_schedules: 1,
                llm_calls: 0,
                llm_cache_hits: 0,
            },
            "刷新完成",
        )
        .expect("completed job");
    let (status, completed) = call_json(
        &app,
        Method::GET,
        &format!("/api/jobs/{job_id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["data"]["status"], "completed");
    assert_eq!(completed["data"]["report"]["newSchedules"], 2);

    let (status, _) = call_json(&app, Method::GET, "/api/jobs/missing-job", None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refresh_job_progress_is_broadcast_on_creation() {
    let state = state().await;
    let mut receiver = state.hub.subscribe();
    let app = build_router(state);
    let (status, accepted) = call_json(&app, Method::POST, "/api/jobs/refresh", None, true).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let job_id = accepted["data"]["jobId"]
        .as_str()
        .expect("job id")
        .to_owned();

    let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("job progress timeout")
        .expect("job progress event");
    assert_eq!(event["type"], "job_progress");
    assert_eq!(event["jobId"], job_id);
    assert_eq!(event["kind"], "refresh_all");
}

#[tokio::test]
async fn refresh_endpoints_reuse_active_jobs() {
    let state = state().await;
    let streamer_id = add_streamer(&state, "880099").await;
    let all_job = state.jobs.create_all();
    let streamer_job = state.jobs.create_streamer(streamer_id, "主播880099");
    let app = build_router(state);

    let (status, accepted) = call_json(&app, Method::POST, "/api/jobs/refresh", None, true).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["data"]["jobId"], all_job.job.job_id);
    assert_eq!(accepted["data"]["deduplicated"], true);

    let (status, accepted) = call_json(
        &app,
        Method::POST,
        &format!("/api/jobs/refresh/{streamer_id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["data"]["jobId"], streamer_job.job.job_id);
    assert_eq!(accepted["data"]["deduplicated"], true);
}

#[tokio::test]
async fn migration_preview_apply_and_event_round_trip() {
    let state = state().await;
    add_streamer(&state, "123456").await;
    let app = build_router(state.clone());
    let (status, exported) = call_json(&app, Method::GET, "/api/sync/snapshot", None, true).await;
    assert_eq!(status, StatusCode::OK, "{exported}");
    let snapshot = exported["data"].clone();
    let (status, preview) = call_json(
        &app,
        Method::POST,
        "/api/migration/preview",
        Some(snapshot.clone()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["data"]["conflicts"], json!([]));

    let (status, applied) = call_json(
        &app,
        Method::POST,
        "/api/migration/apply",
        Some(json!({
            "clientId": "device-a",
            "migrationId": "migration:device-a:1",
            "strategy": "server",
            "snapshot": snapshot.clone()
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["data"]["subscriptions"], 1);

    let (status, retried) = call_json(
        &app,
        Method::POST,
        "/api/migration/apply",
        Some(json!({
            "clientId": "device-a",
            "migrationId": "migration:device-a:1",
            "strategy": "server",
            "snapshot": snapshot.clone()
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["data"], applied["data"]);
    assert_eq!(state.service.list_subscriptions().await.unwrap().len(), 1);

    let (status, events) = call_json(&app, Method::GET, "/api/events?after=0", None, true).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let imported_events = events["data"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["type"] == "data_imported")
        .collect::<Vec<_>>();
    assert_eq!(imported_events.len(), 1);
    assert_eq!(
        imported_events[0]["payload"]["migrationId"],
        "migration:device-a:1"
    );

    let changes = state
        .service
        .store()
        .sync_changes_after(0, 100)
        .await
        .unwrap()
        .changes;
    let resets = changes
        .iter()
        .filter(|change| change.entity_type == "sync_reset")
        .collect::<Vec<_>>();
    assert_eq!(resets.len(), 1);
    assert_eq!(resets[0].entity_id, "migration:device-a:1");
    assert!(!resets[0]
        .payload
        .as_ref()
        .unwrap()
        .to_string()
        .contains("123456"));

    let mut changed_snapshot = snapshot.clone();
    changed_snapshot["latestEventSeq"] = json!(
        changed_snapshot["latestEventSeq"]
            .as_i64()
            .unwrap_or_default()
            + 1
    );
    let (status, conflict) = call_json(
        &app,
        Method::POST,
        "/api/migration/apply",
        Some(json!({
            "clientId": "device-a",
            "migrationId": "migration:device-a:1",
            "strategy": "server",
            "snapshot": changed_snapshot
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{conflict}");
    assert_eq!(conflict["error"]["code"], "MIGRATION_ID_CONFLICT");

    let (status, invalid) = call_json(
        &app,
        Method::POST,
        "/api/migration/apply",
        Some(json!({
            "clientId": "device-a",
            "strategy": "server",
            "snapshot": snapshot
        })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    assert_eq!(invalid["error"]["code"], "VALIDATION_ERROR");

    let (status, events) = call_json(&app, Method::GET, "/api/events?after=0", None, true).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(
        events["data"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["type"] == "data_imported")
            .count(),
        1
    );
}

#[tokio::test]
async fn migration_receipt_survives_store_reopen() {
    let directory = std::env::temp_dir().join(format!(
        "oshical-migration-receipt-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).expect("temp directory");
    let database_path = directory.join("server.db");
    let snapshot = json!({
        "subscriptions": [],
        "schedules": [],
        "weeklyRules": [],
        "latestEventSeq": 0,
        "exportedAt": Utc::now()
    });
    let payload = json!({
        "clientId": "device-persistent",
        "migrationId": "migration:persistent:1",
        "strategy": "server",
        "snapshot": snapshot
    });

    let first_report;
    {
        let store = SqliteStore::connect_path(database_path.clone())
            .await
            .expect("first db");
        let app = build_router(state_with_store(store));
        let (status, applied) = call_json(
            &app,
            Method::POST,
            "/api/migration/apply",
            Some(payload.clone()),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{applied}");
        first_report = applied["data"].clone();
    }

    {
        let store = SqliteStore::connect_path(database_path)
            .await
            .expect("reopened db");
        let app = build_router(state_with_store(store));
        let (status, replayed) = call_json(
            &app,
            Method::POST,
            "/api/migration/apply",
            Some(payload),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replayed}");
        assert_eq!(replayed["data"], first_report);

        let (status, events) =
            call_json(&app, Method::GET, "/api/events?after=0", None, true).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        assert_eq!(
            events["data"]
                .as_array()
                .expect("events")
                .iter()
                .filter(|event| event["type"] == "data_imported")
                .count(),
            1
        );
    }

    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_migration_retries_apply_once() {
    let state = state().await;
    let app = build_router(state);
    let payload = json!({
        "clientId": "device-concurrent",
        "migrationId": "migration:concurrent:1",
        "strategy": "server",
        "snapshot": {
            "subscriptions": [],
            "schedules": [],
            "weeklyRules": [],
            "latestEventSeq": 0,
            "exportedAt": Utc::now()
        }
    });

    let (first, second) = tokio::join!(
        call_json(
            &app,
            Method::POST,
            "/api/migration/apply",
            Some(payload.clone()),
            true
        ),
        call_json(
            &app,
            Method::POST,
            "/api/migration/apply",
            Some(payload),
            true
        )
    );
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(second.0, StatusCode::OK, "{}", second.1);
    assert_eq!(first.1["data"], second.1["data"]);

    let (status, events) = call_json(&app, Method::GET, "/api/events?after=0", None, true).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(
        events["data"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["type"] == "data_imported")
            .count(),
        1
    );
}

#[tokio::test]
async fn llm_secret_round_trip_is_encrypted_and_never_enters_sync_payload() {
    let state = state().await;
    let store = state.service.store().clone();
    let app = build_router(state);

    let (status, saved) = call_json(
        &app,
        Method::PUT,
        "/api/secrets/llm",
        Some(json!({"apiKey": "sk-fixture-super-secret"})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["data"]["configured"], true);
    assert_eq!(saved["data"]["revision"], 1);

    let (status, loaded) = call_json(&app, Method::GET, "/api/secrets/llm", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(loaded["data"]["apiKey"], "sk-fixture-super-secret");
    assert_eq!(loaded["data"]["revision"], 1);

    let raw = store
        .get_client_sync_state("server:secret:llm_api_key:v1")
        .await
        .unwrap()
        .expect("persisted secret");
    assert!(!raw.contains("sk-fixture-super-secret"));

    let changes = store.sync_changes_after(0, 20).await.unwrap().changes;
    let secret_change = changes
        .iter()
        .find(|change| change.entity_type == "server_secret")
        .expect("secret sync change");
    assert_eq!(secret_change.entity_id, "llm_api_key");
    assert!(!secret_change
        .payload
        .as_ref()
        .unwrap()
        .to_string()
        .contains("sk-fixture-super-secret"));
}
