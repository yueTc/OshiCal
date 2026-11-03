use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::Extension;
use oshical_core::AppError;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::api::{ok, ApiError, ApiResult, AppState};
use crate::secrets::{clear_persisted_bili_secret, save_persisted_bili_secret};

const MAX_QR_SESSIONS: usize = 32;
const MAX_COOKIE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
struct QrSession {
    qrcode_key: String,
    created_at: Instant,
    expires_at: Instant,
}

#[derive(Clone, Default)]
pub struct BiliQrManager {
    sessions: Arc<Mutex<HashMap<String, QrSession>>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QrStartResponse {
    session_id: String,
    url: String,
    expires_in_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QrPollResponse {
    status: String,
    message: String,
    configured: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BiliStatusResponse {
    configured: bool,
}

pub(crate) async fn start_qr_login(
    State(state): State<AppState>,
    Extension(manager): Extension<BiliQrManager>,
) -> ApiResult {
    let login = state.service.create_bili_qr_login().await?;
    if !bili_login_url_allowed(&login.url) {
        return Err(ApiError::from(AppError::External {
            code: "BILI_QR_URL".to_owned(),
            message: "B 站返回了不可信的二维码地址".to_owned(),
        }));
    }

    let now = Instant::now();
    let expires_in_seconds = login.expires_in_seconds.clamp(30, 600);
    let session_id = Uuid::new_v4().to_string();
    let session = QrSession {
        qrcode_key: login.qrcode_key,
        created_at: now,
        expires_at: now + Duration::from_secs(expires_in_seconds),
    };

    let mut sessions = manager.sessions.lock().await;
    sessions.retain(|_, value| value.expires_at > now);
    while sessions.len() >= MAX_QR_SESSIONS {
        let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, value)| value.created_at)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        sessions.remove(&oldest);
    }
    sessions.insert(session_id.clone(), session);
    drop(sessions);

    ok(QrStartResponse {
        session_id,
        url: login.url,
        expires_in_seconds,
    })
}

pub(crate) async fn poll_qr_login(
    State(state): State<AppState>,
    Extension(manager): Extension<BiliQrManager>,
    Path(session_id): Path<String>,
) -> ApiResult {
    let session = {
        let mut sessions = manager.sessions.lock().await;
        match sessions.get(&session_id).cloned() {
            Some(session) if session.expires_at > Instant::now() => session,
            Some(_) => {
                sessions.remove(&session_id);
                return ok(QrPollResponse {
                    status: "expired".to_owned(),
                    message: "二维码已过期，请重新生成".to_owned(),
                    configured: state.service.bili_cookie_configured().await,
                });
            }
            None => {
                return ok(QrPollResponse {
                    status: "expired".to_owned(),
                    message: "登录会话已失效，请重新生成二维码".to_owned(),
                    configured: state.service.bili_cookie_configured().await,
                });
            }
        }
    };

    let poll = state
        .service
        .poll_bili_qr_login(&session.qrcode_key)
        .await?;
    if let Some(cookie) = poll.cookie {
        let cookie = cookie.trim().to_owned();
        if !valid_bili_cookie(&cookie) {
            manager.sessions.lock().await.remove(&session_id);
            return Err(ApiError::from(AppError::External {
                code: "BILI_COOKIE_INVALID".to_owned(),
                message: "B 站登录成功，但返回的 Cookie 不完整".to_owned(),
            }));
        }
        save_persisted_bili_secret(state.service.store(), &state.secret_vault, &cookie).await?;
        state.service.update_bili_cookie(Some(cookie)).await?;
        manager.sessions.lock().await.remove(&session_id);
        let refresh_service = state.service.clone();
        tokio::spawn(async move {
            let streamers = match refresh_service.store().list_streamers().await {
                Ok(streamers) => streamers,
                Err(error) => {
                    tracing::warn!(%error, "读取待重试 412 主播失败");
                    return;
                }
            };
            let retry_ids: Vec<i64> = streamers
                .into_iter()
                .filter(|streamer| needs_bili_cookie_retry(streamer.last_error.as_deref()))
                .map(|streamer| streamer.id)
                .collect();
            if retry_ids.is_empty() {
                return;
            }
            tracing::info!(count = retry_ids.len(), "服务器 B 站登录后重试 412 主播");
            for streamer_id in retry_ids {
                if let Err(error) = refresh_service.refresh_streamer(streamer_id).await {
                    tracing::warn!(%error, streamer_id, "服务器 B 站登录后单主播刷新失败");
                }
            }
        });
        return ok(QrPollResponse {
            status: "confirmed".to_owned(),
            message: "服务器 B 站登录成功，Cookie 已加密保存".to_owned(),
            configured: true,
        });
    }

    if poll.status.as_str() == "expired" {
        manager.sessions.lock().await.remove(&session_id);
    }
    ok(QrPollResponse {
        status: poll.status.as_str().to_owned(),
        message: poll.message,
        configured: state.service.bili_cookie_configured().await,
    })
}

pub(crate) async fn status(State(state): State<AppState>) -> ApiResult {
    ok(BiliStatusResponse {
        configured: state.service.bili_cookie_configured().await,
    })
}

pub(crate) async fn logout(State(state): State<AppState>) -> ApiResult {
    clear_persisted_bili_secret(state.service.store()).await?;
    state.service.update_bili_cookie(None).await?;
    ok(BiliStatusResponse { configured: false })
}

fn needs_bili_cookie_retry(last_error: Option<&str>) -> bool {
    last_error.is_some_and(|message| message.contains("412"))
}

fn valid_bili_cookie(cookie: &str) -> bool {
    !cookie.is_empty()
        && cookie.len() <= MAX_COOKIE_BYTES
        && cookie.contains("SESSDATA=")
        && !cookie.chars().any(char::is_control)
}

fn bili_login_url_allowed(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && (host == "bilibili.com"
            || host.ends_with(".bilibili.com")
            || host == "biligame.com"
            || host.ends_with(".biligame.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_https_bili_login_urls_are_accepted() {
        assert!(bili_login_url_allowed(
            "https://passport.bilibili.com/h5-app/passport/login/scan?navhide=1"
        ));
        assert!(!bili_login_url_allowed(
            "http://passport.bilibili.com/h5-app/passport/login/scan?navhide=1"
        ));
        assert!(!bili_login_url_allowed(
            "https://bilibili.com.evil.example/login"
        ));
    }

    #[test]
    fn only_previous_412_errors_are_retried_after_login() {
        assert!(needs_bili_cookie_retry(Some(
            "外部接口错误 [412]: B 站风控拒绝了请求"
        )));
        assert!(!needs_bili_cookie_retry(None));
        assert!(!needs_bili_cookie_retry(Some("模型请求超时")));
    }

    #[test]
    fn only_complete_cookie_headers_are_accepted() {
        assert!(valid_bili_cookie("SESSDATA=abc; bili_jct=def"));
        assert!(!valid_bili_cookie("bili_jct=def"));
        assert!(!valid_bili_cookie("SESSDATA=abc\r\nInjected: true"));
    }
}
