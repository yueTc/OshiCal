use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, SecondsFormat, Utc};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use oshical_core::{
    AddSubscription, AppService, NewSchedule, NewWeeklyScheduleRule, RuntimeSecrets, SqliteStore,
};
use oshical_server::{build_router, AppState, RefreshJobManager, SecretVault, ServerUpdateManager};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tower::ServiceExt;

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
            std::env::temp_dir().join(format!("oshical-update-{}", uuid::Uuid::new_v4())),
        )
        .expect("updates"),
    }
}

fn auth_header() -> String {
    "Bearer test-token".to_owned()
}

#[tokio::test]
async fn health_is_public_and_protected_routes_require_token() {
    let app = build_router(state().await);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("health");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/subscriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("unauthorized");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn server_update_routes_require_auth_and_validate_settings() {
    let app = build_router(state().await);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/system/update")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("unauthorized update response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/system/update")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("update response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"]["currentVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["data"]["channel"], "stable");
    assert_eq!(value["data"]["latestChannel"], Value::Null);
    assert_eq!(value["data"]["autoCheckEnabled"], true);
    assert_eq!(value["data"]["autoUpdateEnabled"], false);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/system/update")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "channel": "beta",
                        "autoCheckEnabled": false,
                        "autoUpdateEnabled": true,
                        "updateWindowStart": "23:30",
                        "updateWindowEnd": "02:30",
                        "githubMirrorUrl": "https://gh-proxy.com/"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("save update settings response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"]["channel"], "beta");
    assert_eq!(value["data"]["autoCheckEnabled"], true);
    assert_eq!(value["data"]["autoUpdateEnabled"], true);
    assert_eq!(value["data"]["githubMirrorUrl"], "https://gh-proxy.com/");

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/system/update")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "autoCheckEnabled": true,
                        "autoUpdateEnabled": false,
                        "updateWindowStart": "99:99",
                        "updateWindowEnd": "05:00"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("invalid update settings response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn proxy_test_returns_google_latency_report() {
    let app = build_router(state().await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/proxy/test")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"proxyPort":1}"#))
                .unwrap(),
        )
        .await
        .expect("proxy test");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(value["data"]["latencyMs"].is_u64());
    assert!(value["data"]["target"]
        .as_str()
        .unwrap_or_default()
        .contains("Google"));
}

#[tokio::test]
async fn subscription_and_manual_schedule_round_trip_over_http() {
    let app = build_router(state().await);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/subscriptions")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&AddSubscription {
                        mid: "100".to_owned(),
                        name: Some("测试主播".to_owned()),
                        avatar_url: None,
                        room_id: Some("12345".to_owned()),
                        aliases: Some(vec!["小测".to_owned()]),
                        schedule_enabled: true,
                        live_notify_enabled: true,
                        live_stats_enabled: false,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .expect("add");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let streamer_id = value["data"]["id"].as_i64().expect("streamer id");

    let start = Utc::now() + Duration::days(1);
    let schedule = NewSchedule {
        streamer_id,
        participants: vec!["测试主播".to_owned(), "外部嘉宾".to_owned()],
        title: "手动日程".to_owned(),
        start_at: start,
        end_at: Some(start + Duration::hours(1)),
        room_url: Some("https://live.bilibili.com/12345".to_owned()),
        room_owner_mid: Some("100".to_owned()),
        room_owner_name: Some("测试主播".to_owned()),
        source_kind: "manual".to_owned(),
        source_post_id: None,
        source_image_url: None,
        source_author_mid: None,
        source_fingerprint: None,
        source_identity: Some("manual:test".to_owned()),
        source_published_at: None,
        confidence: 1.0,
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/schedules")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&schedule).unwrap()))
                .unwrap(),
        )
        .await
        .expect("schedule");
    assert_eq!(response.status(), StatusCode::OK);

    let from = (start - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let to = (start + Duration::days(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/schedules?from={from}&to={to}"))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("list");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let schedules = value["data"].as_array().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(
        schedules[0]["participants"],
        serde_json::json!(["测试主播", "外部嘉宾"])
    );
    assert_eq!(schedules[0]["roomOwnerMid"], "100");
    assert_eq!(schedules[0]["roomOwnerName"], "测试主播");
}

#[tokio::test]
async fn fan_history_route_returns_current_count_and_recent_snapshots() {
    let state = state().await;
    let streamer = state
        .service
        .add_subscription(AddSubscription {
            mid: "99101".to_owned(),
            name: Some("粉丝历史主播".to_owned()),
            avatar_url: None,
            room_id: Some("12345".to_owned()),
            aliases: None,
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        })
        .await
        .expect("streamer");
    let now = Utc::now();
    state
        .service
        .store()
        .record_fan_snapshot(streamer.id, 900, now - Duration::days(40))
        .await
        .expect("old snapshot");
    state
        .service
        .store()
        .record_fan_snapshot(streamer.id, 1_000, now)
        .await
        .expect("recent snapshot");
    let app = build_router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/subscriptions/{}/fans?days=30", streamer.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("fan history");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"]["streamerId"], streamer.id);
    assert_eq!(value["data"]["currentFans"], 1_000);
    assert_eq!(value["data"]["snapshots"].as_array().unwrap().len(), 1);
    assert_eq!(value["data"]["snapshots"][0]["fans"], 1_000);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/subscriptions/{}/fans?days=0", streamer.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("invalid range");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn weekly_rule_round_trip_generates_weekly_schedule() {
    let state = state().await;
    let streamer = state
        .service
        .add_subscription(AddSubscription {
            mid: "99100".to_owned(),
            name: Some("周表主播".to_owned()),
            avatar_url: None,
            room_id: Some("12345".to_owned()),
            aliases: None,
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        })
        .await
        .expect("streamer");
    let app = build_router(state);
    let start_date = Utc::now() + Duration::days(1);
    let input = NewWeeklyScheduleRule {
        rule_key: Some("weekly-api-rule".to_owned()),
        streamer_id: streamer.id,
        title: "固定周表".to_owned(),
        weekdays_mask: 0b0111_1111,
        start_time: "20:00".to_owned(),
        end_time: Some("21:00".to_owned()),
        timezone: "Asia/Shanghai".to_owned(),
        room_url: streamer.live_url.clone(),
        effective_from: start_date.format("%Y-%m-%d").to_string(),
        effective_until: None,
        enabled: true,
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/weekly-rules")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&input).unwrap()))
                .unwrap(),
        )
        .await
        .expect("create weekly rule");
    assert_eq!(response.status(), StatusCode::OK);

    let from = (Utc::now() - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let to = (Utc::now() + Duration::days(8)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/schedules?from={from}&to={to}"))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("list weekly schedules");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let schedules = value["data"].as_array().expect("schedule array");
    assert!(!schedules.is_empty());
    assert!(schedules
        .iter()
        .all(|schedule| schedule["sourceKind"] == "weekly"));
}

#[tokio::test]
async fn cors_allows_tauri_https_origin() {
    let app = build_router(state().await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/devices")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::ORIGIN, "https://tauri.localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://tauri.localhost"
    );
}

#[tokio::test]
async fn websocket_authenticates_and_receives_event() {
    let state = state().await;
    let hub = state.hub.clone();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let (mut socket, _) = connect_async(format!("ws://{address}/api/ws?after=0"))
        .await
        .expect("connect");
    socket
        .send(Message::Text(
            json!({"type":"auth","token":"test-token"})
                .to_string()
                .into(),
        ))
        .await
        .expect("auth");
    let auth = socket.next().await.expect("frame").expect("auth frame");
    let auth_text = auth.to_text().expect("text");
    assert!(auth_text.contains("auth_ok"));

    hub.send(json!({
        "seq": 1,
        "type": "live_status",
        "payload": {"isLive": true}
    }))
    .expect("send");
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("timeout")
        .expect("frame")
        .expect("event frame");
    let event_text = event.to_text().expect("event text");
    assert!(event_text.contains("live_status"));
}

#[tokio::test]
async fn delete_mutation_replay_is_idempotent() {
    let state = state().await;
    let store = state.service.store();
    let streamer = store
        .add_subscription(&AddSubscription {
            mid: "1001".to_owned(),
            name: Some("回归主播".to_owned()),
            avatar_url: None,
            room_id: Some("1001".to_owned()),
            aliases: None,
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        })
        .await
        .unwrap();
    let start = Utc::now() + Duration::days(1);
    let (schedule, _) = store
        .upsert_schedule(&NewSchedule {
            streamer_id: streamer.id,
            participants: Vec::new(),
            title: "重复删除".to_owned(),
            start_at: start,
            end_at: None,
            room_url: None,
            room_owner_mid: None,
            room_owner_name: None,
            source_kind: "manual".to_owned(),
            source_post_id: None,
            source_image_url: None,
            source_author_mid: None,
            source_fingerprint: None,
            source_identity: Some("manual:replay".to_owned()),
            source_published_at: None,
            confidence: 1.0,
        })
        .await
        .unwrap();
    let app = build_router(state);
    let mutation = json!([{
        "mutationId":"delete-replay-1", "operation":"delete", "entity":"schedule",
        "payload":{"id":schedule.id}, "baseUpdatedAt":schedule.updated_at,
        "status":"pending", "lastError":null, "createdAt":Utc::now(), "appliedAt":null
    }]);
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sync/mutations")
                    .header(header::AUTHORIZATION, auth_header())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(mutation.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let result: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["data"][0]["status"], "applied", "{result}");
    }
}

#[tokio::test]
async fn llm_test_endpoint_runs_fixed_calendar_fixture() {
    // The HTTP fixture is intentionally local; production defaults reject it.
    std::env::set_var("OSHICAL_LLM_ALLOWED_HOSTS", "127.0.0.1");
    use axum::{
        routing::{get, post},
        Json, Router,
    };

    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["model"], "fixture-vision");
                assert!(body
                    .pointer("/messages/1/content/0/text")
                    .and_then(Value::as_str)
                    .expect("context text")
                    .contains("A-SOUL小剧场"));
                assert!(body
                    .pointer("/messages/1/content/1/image_url/url")
                    .and_then(Value::as_str)
                    .expect("image URL")
                    .starts_with("data:image/jpeg;base64,"));
                Json(json!({"choices":[{"finish_reason":"stop", "message":{
                    "role":"assistant", "content":json!([
                        {"performer":"乃琳","participants":["乃琳"],"title":"四季歌回——秋日印象","start_at":"2026-10-28T19:30:00+08:00","confidence":0.98},
                        {"performer":"嘉然","participants":["嘉然"],"title":"乃琳飞行棋","start_at":"2026-10-28T21:00:00+08:00","confidence":0.96},
                        {"performer":"贝拉","participants":["贝拉"],"title":"逃出神秘园!","start_at":"2026-10-29T19:30:00+08:00","confidence":0.97},
                        {"performer":"心宜&思诺","participants":["心宜","思诺"],"room_owner":"思诺snow","room_owner_mid":"3537115310721781","title":"执笔入梦","start_at":"2026-10-31T20:00:00+08:00","confidence":0.95},
                        {"performer":"A-SOUL","participants":["A-SOUL"],"room_owner":"贝拉kira","room_owner_mid":"672353429","title":"我，同频，万圣节","start_at":"2026-11-01T20:00:00+08:00","confidence":0.95}
                    ]).to_string()
                }}]}))
            }),
        )
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({"data":[{"id":"fixture-vision"},{"id":"fixture-text"}]}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock listener");
    let address = listener.local_addr().expect("mock address");
    tokio::spawn(async move {
        axum::serve(listener, mock).await.expect("mock server");
    });

    let app = build_router(state().await);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/llm/test")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "baseUrl": format!("http://{address}/v1"),
                        "apiKey": "fixture-key",
                        "model": "fixture-vision",
                        "promptVersion": "fixture-v1"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("llm test response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"]["connected"], true);
    assert_eq!(value["data"]["matchedCount"], 5);
    assert_eq!(value["data"]["successRate"], 1.0);
    assert_eq!(value["data"]["expectedCount"], 5);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/llm/models")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "baseUrl": format!("http://{address}/v1"),
                        "apiKey": "fixture-key"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("model list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"], json!(["fixture-vision", "fixture-text"]));
}

#[tokio::test]
async fn fertility_endpoint_stays_disabled_until_the_switch_is_on() {
    let app = build_router(state().await);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/fertility?refresh=true")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("disabled fertility response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"]["enabled"], false);
    assert_eq!(value["data"]["matched"], 0);
    assert_eq!(value["data"]["source"], "disabled");
    assert_eq!(value["data"]["statuses"], json!([]));

    // 打开开关会把 fertilityEnabled 写进服务端设置并原样返回。
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/settings")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("settings response");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let mut settings: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(settings["data"]["fertilityEnabled"], false);
    settings["data"]["fertilityEnabled"] = json!(true);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(settings["data"].to_string()))
                .unwrap(),
        )
        .await
        .expect("settings save response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let saved: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(saved["data"]["fertilityEnabled"], true);
}
