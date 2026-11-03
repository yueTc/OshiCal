use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use md5::{Digest, Md5};
use regex::Regex;
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, REFERER, SET_COOKIE};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::RwLock;
use url::form_urlencoded::Serializer;

use crate::error::{AppError, AppResult};
use crate::models::{AccountSearchResult, DynamicPost, LiveRoomOwner, LiveSnapshot, ReserveCard};

const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];
const DYNAMIC_FEATURES: &str = "itemOpusStyle,listOnlyfans,opusBigCover,onlyfansVote,forwardListHidden,decorationCard,commentsNewVersion,onlyfansAssetsV2,ugcDelete,onlyfansQaCard";
const LOGIN_COOKIE_NAMES: [&str; 5] = [
    "SESSDATA",
    "bili_jct",
    "DedeUserID",
    "DedeUserID__ckMd5",
    "sid",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrLoginStatus {
    Waiting,
    Scanned,
    Confirmed,
    Expired,
}

impl QrLoginStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Scanned => "scanned",
            Self::Confirmed => "confirmed",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QrLoginStart {
    pub qrcode_key: String,
    pub url: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct QrLoginPoll {
    pub status: QrLoginStatus,
    pub message: String,
    pub cookie: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionCookies {
    buvid3: String,
    buvid4: String,
}

#[derive(Debug, Clone)]
struct WbiKeys {
    mixin_key: String,
    fetched_at: Instant,
}

pub struct BilibiliClient {
    http: Client,
    user_cookie: RwLock<Option<String>>,
    session: RwLock<Option<SessionCookies>>,
    wbi: RwLock<Option<WbiKeys>>,
}

impl BilibiliClient {
    pub fn new(user_cookie: Option<String>) -> AppResult<Self> {
        Self::new_with_proxy(user_cookie, None)
    }

    pub fn new_with_proxy(user_cookie: Option<String>, proxy_port: Option<u16>) -> AppResult<Self> {
        let http = crate::network::client_builder(proxy_port)?
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT_VALUE)
            .build()?;
        let user_cookie = user_cookie
            .map(|cookie| cookie.replace(['\r', '\n'], "").trim().to_owned())
            .filter(|cookie| !cookie.is_empty());
        Ok(Self {
            http,
            user_cookie: RwLock::new(user_cookie),
            session: RwLock::new(None),
            wbi: RwLock::new(None),
        })
    }

    pub async fn set_cookie(&self, user_cookie: Option<String>) {
        let value = user_cookie
            .map(|cookie| cookie.replace(['\r', '\n'], "").trim().to_owned())
            .filter(|cookie| !cookie.is_empty());
        *self.user_cookie.write().await = value;
        *self.session.write().await = None;
    }

    async fn ensure_session(&self) -> AppResult<SessionCookies> {
        if let Some(session) = self.session.read().await.clone() {
            return Ok(session);
        }
        let response = send_with_retry(|| {
            self.http
                .get("https://api.bilibili.com/x/frontend/finger/spi")
                .header(ACCEPT, "application/json")
                .header(REFERER, "https://www.bilibili.com/")
        })
        .await?;
        let data = self.response_json(response).await?;
        let session = SessionCookies {
            buvid3: data
                .pointer("/data/b_3")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::External {
                    code: "BILI_BUVID".to_owned(),
                    message: "无法获取 buvid3".to_owned(),
                })?
                .to_owned(),
            buvid4: data
                .pointer("/data/b_4")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::External {
                    code: "BILI_BUVID".to_owned(),
                    message: "无法获取 buvid4".to_owned(),
                })?
                .to_owned(),
        };
        *self.session.write().await = Some(session.clone());
        Ok(session)
    }

    async fn ensure_wbi(&self) -> AppResult<String> {
        if let Some(keys) = self.wbi.read().await.as_ref() {
            if keys.fetched_at.elapsed() < Duration::from_secs(30 * 60) {
                return Ok(keys.mixin_key.clone());
            }
        }
        let cookie = self.cookie_header().await?;
        let response = send_with_retry(|| {
            self.http
                .get("https://api.bilibili.com/x/web-interface/nav")
                .header(ACCEPT, "application/json")
                .header(REFERER, "https://www.bilibili.com/")
                .header(COOKIE, cookie.clone())
        })
        .await?;
        let data = self.response_json_unchecked(response).await?;
        let img_url = data
            .pointer("/data/wbi_img/img_url")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::External {
                code: "BILI_WBI".to_owned(),
                message: "无法获取 WBI 图片密钥".to_owned(),
            })?;
        let sub_url = data
            .pointer("/data/wbi_img/sub_url")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::External {
                code: "BILI_WBI".to_owned(),
                message: "无法获取 WBI 子密钥".to_owned(),
            })?;
        let img_key = filename_key(img_url)?;
        let sub_key = filename_key(sub_url)?;
        let mixin_key = mixin_key(&img_key, &sub_key)?;
        *self.wbi.write().await = Some(WbiKeys {
            mixin_key: mixin_key.clone(),
            fetched_at: Instant::now(),
        });
        Ok(mixin_key)
    }

    async fn cookie_header(&self) -> AppResult<String> {
        let session = self.ensure_session().await?;
        let mut parts = vec![
            format!("buvid3={}", session.buvid3),
            format!("buvid4={}", session.buvid4),
        ];
        if let Some(cookie) = self.user_cookie.read().await.clone() {
            parts.push(cookie);
        }
        Ok(parts.join("; "))
    }

    async fn response_json(&self, response: reqwest::Response) -> AppResult<Value> {
        let value = self.response_json_unchecked(response).await?;
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("B 站接口请求失败")
                .to_owned();
            return Err(AppError::External {
                code: code.to_string(),
                message,
            });
        }
        Ok(value)
    }

    async fn response_json_unchecked(&self, response: reqwest::Response) -> AppResult<Value> {
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(AppError::External {
                code: status.as_str().to_owned(),
                message: if status.as_u16() == 412 {
                    "B 站风控拒绝了请求，可在设置中配置 SESSDATA Cookie 后重试".to_owned()
                } else {
                    format!("B 站返回 HTTP {status}")
                },
            });
        }
        serde_json::from_str(&text).map_err(|_| AppError::External {
            code: "BILI_NON_JSON".to_owned(),
            message: "B 站返回了非 JSON 内容，可能触发风控".to_owned(),
        })
    }
    async fn get_signed(&self, path: &str, params: BTreeMap<String, String>) -> AppResult<Value> {
        self.get_signed_host(
            "https://api.bilibili.com",
            path,
            params,
            "https://www.bilibili.com/",
        )
        .await
    }

    async fn get_signed_host(
        &self,
        base_url: &str,
        path: &str,
        params: BTreeMap<String, String>,
        referer: &str,
    ) -> AppResult<Value> {
        let mixin = self.ensure_wbi().await?;
        let cookie = self.cookie_header().await?;
        let mut signed = params;
        signed.insert("wts".to_owned(), Utc::now().timestamp().to_string());
        let query = signed_query(&signed, &mixin);
        let url = format!("{base_url}{path}?{query}");
        let response = send_with_retry(|| {
            self.http
                .get(&url)
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(REFERER, referer)
                .header(COOKIE, cookie.clone())
        })
        .await?;
        self.response_json(response).await
    }

    pub async fn fetch_live_danmu_info(&self, room_id: &str) -> AppResult<Value> {
        if room_id.trim().is_empty() || !room_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(AppError::validation("直播间号必须是数字"));
        }
        let mut params = BTreeMap::new();
        params.insert("id".to_owned(), room_id.to_owned());
        params.insert("type".to_owned(), "0".to_owned());
        self.get_signed_host(
            "https://api.live.bilibili.com",
            "/xlive/web-room/v1/index/getDanmuInfo",
            params,
            &format!("https://live.bilibili.com/{room_id}"),
        )
        .await
    }

    async fn get_public(&self, url: &str, referer: &str) -> AppResult<Value> {
        let cookie = self.cookie_header().await?;
        let response = send_with_retry(|| {
            self.http
                .get(url)
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(REFERER, referer)
                .header(COOKIE, cookie.clone())
        })
        .await?;
        self.response_json(response).await
    }
    pub async fn search_accounts(&self, keyword: &str) -> AppResult<Vec<AccountSearchResult>> {
        let keyword = keyword.trim();
        if keyword.is_empty() || keyword.chars().count() > 100 {
            return Err(AppError::validation("搜索关键词长度必须为 1 到 100 字"));
        }
        if let Some(mid) = extract_search_mid(keyword) {
            return self.fetch_account(&mid).await.map(|account| vec![account]);
        }
        let keyword = keyword.trim_start_matches('@').trim();
        if keyword.is_empty() {
            return Err(AppError::validation("搜索关键词不能只有 @"));
        }
        let mut params = BTreeMap::new();
        params.insert("keyword".to_owned(), keyword.to_owned());
        params.insert("search_type".to_owned(), "bili_user".to_owned());
        let value = self
            .get_signed("/x/web-interface/wbi/search/type", params)
            .await?;
        let results = value
            .pointer("/data/result")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| AppError::External {
                code: "BILI_SEARCH_FORMAT".to_owned(),
                message: "B 站搜索返回数据格式异常，请稍后重试".to_owned(),
            })?;
        Ok(results
            .into_iter()
            .filter_map(|item| {
                let mid = value_to_string(item.get("mid")?)?;
                let name = strip_html(item.get("uname")?.as_str()?);
                let avatar_url = normalize_image_url(item.get("upic")?.as_str()?)?;
                let room_id = item
                    .get("room_id")
                    .and_then(value_to_string)
                    .filter(|value| value != "0");
                Some(AccountSearchResult {
                    mid,
                    name,
                    avatar_url,
                    sign: item
                        .get("usign")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    room_id,
                    fans: item.get("fans").and_then(Value::as_u64).unwrap_or(0),
                    is_live: item.get("is_live").and_then(Value::as_i64).unwrap_or(0) == 1,
                })
            })
            .collect())
    }

    pub async fn create_qr_login(&self) -> AppResult<QrLoginStart> {
        let response = send_with_retry(|| {
            self.http
                .get("https://passport.bilibili.com/x/passport-login/web/qrcode/generate")
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(REFERER, "https://www.bilibili.com/")
        })
        .await?;
        let value = self.response_json(response).await?;
        let data = value.get("data").ok_or_else(|| AppError::External {
            code: "BILI_QR_GENERATE".to_owned(),
            message: "B 站二维码接口未返回数据".to_owned(),
        })?;
        let qrcode_key = data
            .get("qrcode_key")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or_else(|| AppError::External {
                code: "BILI_QR_GENERATE".to_owned(),
                message: "B 站二维码接口缺少有效 qrcode_key".to_owned(),
            })?;
        let url = data
            .get("url")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("https://"))
            .ok_or_else(|| AppError::External {
                code: "BILI_QR_GENERATE".to_owned(),
                message: "B 站二维码接口缺少有效登录 URL".to_owned(),
            })?;
        Ok(QrLoginStart {
            qrcode_key: qrcode_key.to_owned(),
            url: url.to_owned(),
            expires_in_seconds: data
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(180)
                .clamp(30, 600),
        })
    }

    pub async fn poll_qr_login(&self, qrcode_key: &str) -> AppResult<QrLoginPoll> {
        let qrcode_key = qrcode_key.trim();
        if qrcode_key.is_empty() || qrcode_key.len() > 256 {
            return Err(AppError::validation("二维码登录会话无效"));
        }
        let query = Serializer::new(String::new())
            .append_pair("qrcode_key", qrcode_key)
            .finish();
        let url = format!("https://passport.bilibili.com/x/passport-login/web/qrcode/poll?{query}");
        let response = send_with_retry(|| {
            self.http
                .get(&url)
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(REFERER, "https://www.bilibili.com/")
        })
        .await?;
        let set_cookie_headers: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .collect();
        let value = self.response_json(response).await?;
        let data = value.get("data").ok_or_else(|| AppError::External {
            code: "BILI_QR_POLL".to_owned(),
            message: "B 站二维码状态接口未返回数据".to_owned(),
        })?;
        let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
        match code {
            0 => {
                let callback_url = data.get("url").and_then(Value::as_str);
                let cookie = extract_login_cookies(callback_url, &set_cookie_headers)?;
                Ok(QrLoginPoll {
                    status: QrLoginStatus::Confirmed,
                    message: "扫码登录成功，Cookie 已保存".to_owned(),
                    cookie: Some(cookie),
                })
            }
            86101 => Ok(QrLoginPoll {
                status: QrLoginStatus::Waiting,
                message: "等待手机扫码".to_owned(),
                cookie: None,
            }),
            86090 => Ok(QrLoginPoll {
                status: QrLoginStatus::Scanned,
                message: "已扫码，请在手机上确认登录".to_owned(),
                cookie: None,
            }),
            86038 => Ok(QrLoginPoll {
                status: QrLoginStatus::Expired,
                message: "二维码已过期，请重新生成".to_owned(),
                cookie: None,
            }),
            _ => Err(AppError::External {
                code: format!("BILI_QR_{code}"),
                message: data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("二维码登录状态异常")
                    .to_owned(),
            }),
        }
    }

    pub async fn fetch_account(&self, mid: &str) -> AppResult<AccountSearchResult> {
        validate_mid(mid)?;
        let url = format!("https://api.bilibili.com/x/web-interface/card?mid={mid}");
        let value = self.get_public(&url, "https://space.bilibili.com/").await?;
        let card = value
            .get("data")
            .and_then(|data| data.get("card"))
            .ok_or_else(|| AppError::External {
                code: "BILI_CARD".to_owned(),
                message: "账号卡片为空".to_owned(),
            })?;
        let name = card
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::External {
                code: "BILI_CARD".to_owned(),
                message: "账号昵称为空".to_owned(),
            })?
            .to_owned();
        let avatar_url = card
            .get("face")
            .and_then(Value::as_str)
            .and_then(normalize_image_url)
            .unwrap_or_default();
        let sign = card
            .get("sign")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let fans = card.get("fans").and_then(Value::as_u64).unwrap_or(0);
        let room_id = self.resolve_room_id(mid).await.ok().flatten();
        Ok(AccountSearchResult {
            mid: mid.to_owned(),
            name,
            avatar_url,
            sign,
            room_id,
            fans,
            is_live: false,
        })
    }

    pub async fn fetch_fan_count(&self, mid: &str) -> AppResult<u64> {
        validate_mid(mid)?;
        let url = format!("https://api.bilibili.com/x/web-interface/card?mid={mid}");
        let value = self.get_public(&url, "https://space.bilibili.com/").await?;
        parse_fan_count(&value)
    }

    pub async fn fetch_avatar_data_url(&self, avatar_url: &str) -> AppResult<String> {
        let avatar_url = normalize_image_url(avatar_url)
            .ok_or_else(|| AppError::validation("头像地址必须是受信任的 B 站图片链接"))?;
        let thumbnail_url = avatar_thumbnail_url(&avatar_url);
        let response = send_with_retry(|| {
            self.http
                .get(&thumbnail_url)
                .header(ACCEPT, "image/avif,image/webp,image/apng,image/*,*/*;q=0.8")
                .header(REFERER, "https://www.bilibili.com/")
        })
        .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::External {
                code: format!("BILI_AVATAR_{}", status.as_u16()),
                message: format!("B 站头像返回 HTTP {status}"),
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !content_type.starts_with("image/") {
            return Err(AppError::External {
                code: "BILI_AVATAR_TYPE".to_owned(),
                message: "B 站头像响应不是图片".to_owned(),
            });
        }
        let bytes = response.bytes().await?;
        if bytes.is_empty() || bytes.len() > 512 * 1024 {
            return Err(AppError::External {
                code: "BILI_AVATAR_SIZE".to_owned(),
                message: "B 站头像为空或超过 512 KiB".to_owned(),
            });
        }
        Ok(format!(
            "data:{content_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }

    pub async fn resolve_room_id(&self, mid: &str) -> AppResult<Option<String>> {
        validate_mid(mid)?;
        let url = format!("https://api.live.bilibili.com/room/v1/Room/getRoomInfoOld?mid={mid}");
        let value = self.get_public(&url, "https://live.bilibili.com/").await?;
        Ok(value
            .pointer("/data/roomid")
            .and_then(value_to_string)
            .filter(|room_id| room_id != "0"))
    }

    pub async fn fetch_live_status(&self, room_id: &str) -> AppResult<LiveSnapshot> {
        if room_id.trim().is_empty() || !room_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(AppError::validation("直播间号必须是数字"));
        }
        let url = format!("https://api.live.bilibili.com/room/v1/Room/get_info?room_id={room_id}");
        let value = self.get_public(&url, "https://live.bilibili.com/").await?;
        let data = value.get("data").ok_or_else(|| AppError::External {
            code: "BILI_LIVE".to_owned(),
            message: "直播间信息为空".to_owned(),
        })?;
        let live_status = data.get("live_status").and_then(Value::as_i64).unwrap_or(0);
        let started_at = data
            .get("live_time")
            .and_then(Value::as_str)
            .and_then(parse_bili_time);
        let cover = data
            .get("user_cover")
            .or_else(|| data.get("keyframe"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .and_then(normalize_image_url);
        Ok(LiveSnapshot {
            is_live: live_status == 1,
            title: data
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            cover_url: cover,
            live_url: Some(format!("https://live.bilibili.com/{room_id}")),
            online: data.get("online").and_then(Value::as_u64).unwrap_or(0),
            started_at,
            checked_at: Utc::now(),
        })
    }

    pub async fn fetch_room_owner(&self, room_id: &str) -> AppResult<Option<LiveRoomOwner>> {
        if room_id.trim().is_empty() || !room_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(AppError::validation("直播间号必须是数字"));
        }
        let room = self
            .get_public(
                &format!("https://api.live.bilibili.com/room/v1/Room/get_info?room_id={room_id}"),
                "https://live.bilibili.com/",
            )
            .await?;
        let Some(uid) = room
            .pointer("/data/uid")
            .and_then(value_to_string)
            .filter(|value| value != "0")
        else {
            return Ok(None);
        };
        let master = self
            .get_public(
                &format!("https://api.live.bilibili.com/live_user/v1/Master/info?uid={uid}"),
                "https://live.bilibili.com/",
            )
            .await?;
        let name = master
            .pointer("/data/info/uname")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.chars().count() <= 80);
        Ok(name.map(|name| LiveRoomOwner {
            mid: uid,
            name: name.to_owned(),
            room_id: room_id.to_owned(),
        }))
    }

    pub async fn fetch_live_status_by_mid(&self, mid: &str) -> AppResult<Option<LiveSnapshot>> {
        let Some(room_id) = self.resolve_room_id(mid).await? else {
            return Ok(None);
        };
        self.fetch_live_status(&room_id).await.map(Some)
    }

    pub async fn fetch_dynamics(&self, mid: &str, limit: usize) -> AppResult<Vec<DynamicPost>> {
        validate_mid(mid)?;
        let limit = limit.clamp(1, 40);
        let mut posts = Vec::new();
        let mut offset = String::new();
        for _ in 0..2 {
            let mut params = BTreeMap::new();
            params.insert("host_mid".to_owned(), mid.to_owned());
            params.insert("offset".to_owned(), offset.clone());
            params.insert("platform".to_owned(), "web".to_owned());
            params.insert("features".to_owned(), DYNAMIC_FEATURES.to_owned());
            let value = self
                .get_signed("/x/polymer/web-dynamic/v1/feed/space", params)
                .await?;
            let data = value.get("data").ok_or_else(|| AppError::External {
                code: "BILI_DYNAMIC".to_owned(),
                message: "动态列表为空".to_owned(),
            })?;
            let items = data
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                break;
            }
            for item in items {
                if let Some(post) = parse_dynamic_item(&item) {
                    posts.push(post);
                    if posts.len() >= limit {
                        return Ok(posts);
                    }
                }
            }
            offset = data
                .get("offset")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if offset.is_empty()
                || !data
                    .get("has_more")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                break;
            }
        }
        Ok(posts)
    }
}

fn extract_search_mid(keyword: &str) -> Option<String> {
    let value = keyword.trim();
    let lower = value.to_ascii_lowercase();
    let value = lower
        .strip_prefix("uid:")
        .map(str::trim)
        .unwrap_or(value)
        .trim_start_matches('@')
        .trim();
    if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(value.to_owned());
    }
    if value.contains("space.bilibili.com") {
        let normalized = if value.starts_with("http://") || value.starts_with("https://") {
            value.to_owned()
        } else {
            format!("https://{value}")
        };
        if let Ok(url) = url::Url::parse(&normalized) {
            if url.host_str() == Some("space.bilibili.com") {
                return url
                    .path_segments()?
                    .find(|segment| {
                        !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit())
                    })
                    .map(ToOwned::to_owned);
            }
        }
    }
    None
}

async fn send_with_retry<F>(mut builder: F) -> AppResult<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut last_error = None;
    let mut attempts = 0;
    for attempt in 0..3 {
        attempts = attempt + 1;
        match builder().send().await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                last_error = Some(error);
                if !retryable || attempt == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
            }
        }
    }
    let error = last_error.expect("retry loop always records an error");
    Err(AppError::External {
        code: "BILI_NETWORK".to_owned(),
        message: format!("B 站网络请求失败（已尝试 {attempts} 次）: {error}"),
    })
}

fn extract_login_cookies(
    callback_url: Option<&str>,
    set_cookie_headers: &[String],
) -> AppResult<String> {
    let mut values = BTreeMap::new();
    let mut observed = std::collections::BTreeSet::new();
    if let Some(value) = callback_url {
        let parsed = url::Url::parse(value)?;
        let host = parsed.host_str().unwrap_or_default();
        if parsed.scheme() != "https" || !trusted_bili_host(host) {
            return Err(AppError::External {
                code: "BILI_QR_COOKIE".to_owned(),
                message: format!("B 站登录回调地址不可信: {host}"),
            });
        }
        collect_url_cookie_pairs(value, &mut values, &mut observed, 0);
    }
    for header in set_cookie_headers {
        let pair = header.split(';').next().unwrap_or_default().trim();
        if let Some((name, value)) = pair.split_once('=') {
            let name = name.trim();
            observed.insert(name.to_owned());
            insert_login_cookie(&mut values, name, value.trim().trim_matches('"'));
        }
    }
    if !values.contains_key("SESSDATA") {
        return Err(AppError::External {
            code: "BILI_QR_COOKIE".to_owned(),
            message: format!(
                "登录成功但未获取到 SESSDATA（回调/Cookie 参数: {}）",
                if observed.is_empty() {
                    "无".to_owned()
                } else {
                    observed.into_iter().collect::<Vec<_>>().join(", ")
                }
            ),
        });
    }
    Ok(LOGIN_COOKIE_NAMES
        .iter()
        .filter_map(|name| values.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; "))
}

fn trusted_bili_host(host: &str) -> bool {
    host == "bilibili.com"
        || host.ends_with(".bilibili.com")
        || host == "biligame.com"
        || host.ends_with(".biligame.com")
}

fn collect_url_cookie_pairs(
    value: &str,
    cookies: &mut BTreeMap<String, String>,
    observed: &mut std::collections::BTreeSet<String>,
    depth: u8,
) {
    if depth > 2 || value.len() > 16_384 {
        return;
    }
    let Ok(parsed) = url::Url::parse(value) else {
        return;
    };
    let host = parsed.host_str().unwrap_or_default();
    if parsed.scheme() != "https" || !trusted_bili_host(host) {
        return;
    }
    for (name, item_value) in parsed.query_pairs() {
        observed.insert(name.to_string());
        insert_login_cookie(cookies, &name, &item_value);
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "gourl" | "url" | "redirect" | "redirect_url" | "callback"
        ) {
            collect_url_cookie_pairs(&item_value, cookies, observed, depth + 1);
        }
    }
    if let Some(fragment) = parsed.fragment() {
        for (name, item_value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            observed.insert(name.to_string());
            insert_login_cookie(cookies, &name, &item_value);
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "gourl" | "url" | "redirect" | "redirect_url" | "callback"
            ) {
                collect_url_cookie_pairs(&item_value, cookies, observed, depth + 1);
            }
        }
    }
}

fn insert_login_cookie(cookies: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let Some(canonical) = LOGIN_COOKIE_NAMES
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
    else {
        return;
    };
    if value.is_empty()
        || value.len() > 4096
        || value
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | ';'))
    {
        return;
    }
    cookies.insert((*canonical).to_owned(), value.to_owned());
}
fn filename_key(url: &str) -> AppResult<String> {
    let filename = url
        .split('/')
        .next_back()
        .ok_or_else(|| AppError::validation("WBI 密钥 URL 无效"))?;
    let stem = filename
        .split('.')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::validation("WBI 密钥文件名为空"))?;
    Ok(stem.to_owned())
}

fn mixin_key(img_key: &str, sub_key: &str) -> AppResult<String> {
    let raw = format!("{img_key}{sub_key}");
    if raw.chars().count() < 64 {
        return Err(AppError::validation("WBI 原始密钥长度不足"));
    }
    Ok(MIXIN_KEY_ENC_TAB
        .iter()
        .filter_map(|index| raw.chars().nth(*index))
        .take(32)
        .collect())
}

fn signed_query(params: &BTreeMap<String, String>, mixin_key: &str) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, &sanitize_wbi_value(value));
    }
    let query = serializer.finish();
    let mut hasher = Md5::new();
    hasher.update(format!("{query}{mixin_key}").as_bytes());
    let signature = format!("{:x}", hasher.finalize());
    format!("{query}&w_rid={signature}")
}

fn sanitize_wbi_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !matches!(ch, '!' | '\'' | '(' | ')' | '*'))
        .collect()
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_fan_count(value: &Value) -> AppResult<u64> {
    value
        .get("data")
        .and_then(|data| data.get("card"))
        .and_then(|card| card.get("fans"))
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::External {
            code: "BILI_FANS".to_owned(),
            message: "账号卡片缺少粉丝数".to_owned(),
        })
}

fn validate_mid(mid: &str) -> AppResult<()> {
    let mid = mid.trim();
    if mid.is_empty() || mid.len() > 32 || !mid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(AppError::validation("B 站 MID 必须是数字"));
    }
    Ok(())
}

pub(crate) fn normalize_image_url(url: &str) -> Option<String> {
    let url = url.trim();
    let url = if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_owned()
    };
    let parsed = url::Url::parse(&url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if parsed.scheme() != "https" || !(host == "hdslb.com" || host.ends_with(".hdslb.com")) {
        return None;
    }
    Some(parsed.to_string())
}

fn avatar_thumbnail_url(url: &str) -> String {
    let base = url.split('@').next().unwrap_or(url);
    format!("{base}@128w_128h_1c.webp")
}

fn strip_html(input: &str) -> String {
    static TAG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = TAG_RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("valid html regex"));
    regex.replace_all(input, "").to_string()
}

fn parse_bili_time(value: &str) -> Option<DateTime<Utc>> {
    if value.starts_with("0000-00-00") || value.trim().is_empty() {
        return None;
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = FixedOffset::east_opt(8 * 3600)?;
    offset
        .from_local_datetime(&naive)
        .single()
        .map(|value| value.with_timezone(&Utc))
}

fn parse_dynamic_item(item: &Value) -> Option<DynamicPost> {
    let id = item.get("id_str").and_then(Value::as_str)?.to_owned();
    let modules = item.get("modules")?;
    let author = modules.get("module_author")?;
    let author_mid = author.get("mid").and_then(value_to_string)?;
    let author_name = author
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let published_at = author
        .get("pub_ts")
        .and_then(|value| match value {
            Value::String(value) => value.parse::<i64>().ok(),
            Value::Number(value) => value.as_i64(),
            _ => None,
        })
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))?;
    let module_dynamic = modules
        .get("module_dynamic")
        .cloned()
        .unwrap_or(Value::Null);
    let mut text = module_dynamic
        .pointer("/desc/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty() {
        if let Some(opus_text) = module_dynamic
            .pointer("/major/opus/summary/text")
            .and_then(Value::as_str)
        {
            text = opus_text.to_owned();
        }
    }
    text = text.chars().take(65_536).collect();

    let mut images = Vec::new();
    if let Some(items) = module_dynamic
        .pointer("/major/draw/items")
        .and_then(Value::as_array)
    {
        for item in items {
            if let Some(url) = item.get("src").and_then(Value::as_str) {
                if let Some(url) = normalize_image_url(url) {
                    images.push(url);
                }
            }
        }
    }
    if let Some(items) = module_dynamic
        .pointer("/major/opus/pics")
        .and_then(Value::as_array)
    {
        for item in items {
            let url = item
                .get("url")
                .or_else(|| item.get("src"))
                .and_then(Value::as_str);
            if let Some(url) = url.and_then(normalize_image_url) {
                images.push(url);
            }
        }
    }
    images.sort();
    images.dedup();
    images.truncate(9);

    let reserve = module_dynamic
        .pointer("/additional/reserve")
        .filter(|value| {
            module_dynamic
                .pointer("/additional/type")
                .and_then(Value::as_str)
                == Some("ADDITIONAL_TYPE_RESERVE")
                || value.is_object()
        })
        .map(|reserve| ReserveCard {
            title: reserve
                .get("title")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("预约")
                .to_owned(),
            description: reserve
                .pointer("/desc1/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            room_id: reserve.get("rid").and_then(value_to_string),
            jump_url: reserve
                .get("jump_url")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            stype: reserve.get("stype").and_then(Value::as_i64),
        });

    if text.is_empty() && images.is_empty() && reserve.is_none() {
        return None;
    }
    let source_url = item
        .pointer("/basic/jump_url")
        .and_then(Value::as_str)
        .map(|value| {
            if let Some(rest) = value.strip_prefix("//") {
                format!("https://{rest}")
            } else {
                value.to_owned()
            }
        })
        .unwrap_or_else(|| format!("https://t.bilibili.com/{id}"));
    Some(DynamicPost {
        id,
        author_mid,
        author_name,
        published_at,
        text,
        images,
        source_url,
        raw_kind: item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("DYNAMIC_TYPE_UNKNOWN")
            .to_owned(),
        reserve,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mixin_key_matches_known_vector() {
        let value = mixin_key(
            "7cd084941338484aae1ad9425b84077c",
            "4932caff0ff746eab6f01bf08b70ac45",
        )
        .expect("mixin");
        assert_eq!(value, "ea1db124af3c7062474693fa704f4ff8");
    }

    #[test]
    fn image_urls_are_https_and_host_limited() {
        assert_eq!(
            normalize_image_url("//i0.hdslb.com/bfs/test.jpg"),
            Some("https://i0.hdslb.com/bfs/test.jpg".to_owned())
        );
        assert_eq!(normalize_image_url("https://example.com/test.jpg"), None);
        assert_eq!(normalize_image_url("http://127.0.0.1/test.jpg"), None);
    }

    #[test]
    fn avatar_urls_use_a_small_cdn_variant() {
        assert_eq!(
            avatar_thumbnail_url("https://i0.hdslb.com/bfs/face/test.jpg"),
            "https://i0.hdslb.com/bfs/face/test.jpg@128w_128h_1c.webp"
        );
        assert_eq!(
            avatar_thumbnail_url("https://i0.hdslb.com/bfs/face/test.jpg@64w_64h_1c.webp"),
            "https://i0.hdslb.com/bfs/face/test.jpg@128w_128h_1c.webp"
        );
    }

    #[test]
    fn dynamic_item_extracts_text_images_and_reservation() {
        let item = json!({
            "id_str": "123",
            "type": "DYNAMIC_TYPE_DRAW",
            "basic": {"jump_url": "//www.bilibili.com/opus/123"},
            "modules": {
                "module_author": {
                    "mid": 100,
                    "name": "测试主播",
                    "pub_ts": "1761390000"
                },
                "module_dynamic": {
                    "desc": {"text": "10月30日 20:00 直播"},
                    "major": {
                        "type": "MAJOR_TYPE_DRAW",
                        "draw": {"items": [{"src": "//i0.hdslb.com/bfs/a.jpg"}]}
                    },
                    "additional": {
                        "type": "ADDITIONAL_TYPE_RESERVE",
                        "reserve": {
                            "title": "直播预约：周五杂谈",
                            "desc1": {"text": "10月30日 20:00 直播"},
                            "rid": 12345,
                            "jump_url": "https://live.bilibili.com/12345",
                            "stype": 2
                        }
                    }
                }
            }
        });
        let post = parse_dynamic_item(&item).expect("post");
        assert_eq!(post.id, "123");
        assert_eq!(post.author_mid, "100");
        assert_eq!(post.images.len(), 1);
        let reserve = post.reserve.expect("reserve");
        assert_eq!(reserve.title, "直播预约：周五杂谈");
        assert_eq!(reserve.room_id.as_deref(), Some("12345"));
        assert_eq!(reserve.stype, Some(2));
        assert!(reserve.is_live());
    }

    #[test]
    fn parses_fan_count_from_account_card() {
        let value = serde_json::json!({"data": {"card": {"fans": 123456}}});
        assert_eq!(parse_fan_count(&value).expect("fans"), 123456);
        assert!(parse_fan_count(&serde_json::json!({"data": {"card": {}}})).is_err());
    }

    #[test]
    fn search_normalizes_uid_mentions_and_space_urls() {
        for value in [
            "672328094",
            "UID:672328094",
            "@672328094",
            "https://space.bilibili.com/672328094?spm_id_from=333.1368.0.0",
            "space.bilibili.com/672328094",
        ] {
            assert_eq!(
                extract_search_mid(value).as_deref(),
                Some("672328094"),
                "{value}"
            );
        }
        assert!(extract_search_mid("嘉然").is_none());
    }

    #[test]
    fn wbi_query_is_deterministic() {
        let mut params = BTreeMap::new();
        params.insert("keyword".to_owned(), "嘉然".to_owned());
        params.insert("wts".to_owned(), "123".to_owned());
        let query = signed_query(&params, "ea1db124af3c7062474693fa704f4ff8");
        assert!(query.contains("keyword=%E5%98%89%E7%84%B6"));
        assert!(query.contains("w_rid="));
    }

    #[test]
    fn login_callback_extracts_only_allowed_cookies() {
        let cookie = extract_login_cookies(
            Some("https://passport.bilibili.com/login?SESSDATA=abc%2C123&bili_jct=csrf&DedeUserID=42&DedeUserID__ckMd5=check&sid=xyz&gourl=https%3A%2F%2Fwww.bilibili.com"),
            &[],
        )
        .expect("cookie");
        assert_eq!(
            cookie,
            "SESSDATA=abc,123; bili_jct=csrf; DedeUserID=42; DedeUserID__ckMd5=check; sid=xyz"
        );
        assert!(extract_login_cookies(
            Some("https://passport.bilibili.com/login?bili_jct=csrf&DedeUserID=42"),
            &[],
        )
        .is_err());
        assert_eq!(
            extract_login_cookies(
                Some("https://passport.biligame.com/crossDomain?SESSDATA=game&bili_jct=csrf&DedeUserID=42"),
                &[],
            )
            .expect("biligame cookie"),
            "SESSDATA=game; bili_jct=csrf; DedeUserID=42"
        );
        assert!(extract_login_cookies(
            Some("https://example.com/login?SESSDATA=abc&bili_jct=csrf"),
            &[],
        )
        .is_err());
    }

    #[test]
    fn login_cookie_uses_set_cookie_and_nested_urls() {
        let from_headers = extract_login_cookies(
            Some(
                "https://passport.biligame.com/crossDomain?DedeUserID=42&gourl=https%3A%2F%2Fwww.bilibili.com%2F",
            ),
            &[
                "SESSDATA=header-session; Path=/; Domain=.bilibili.com; HttpOnly".to_owned(),
                "bili_jct=header-csrf; Path=/".to_owned(),
            ],
        )
        .expect("header cookies");
        assert_eq!(
            from_headers,
            "SESSDATA=header-session; bili_jct=header-csrf; DedeUserID=42"
        );
        let nested = extract_login_cookies(
            Some(
                "https://passport.biligame.com/crossDomain?gourl=https%3A%2F%2Fpassport.bilibili.com%2Flogin%3FSESSDATA%3Dnested%26bili_jct%3Dnested-csrf",
            ),
            &[],
        )
        .expect("nested cookies");
        assert_eq!(nested, "SESSDATA=nested; bili_jct=nested-csrf");
    }

    #[tokio::test]
    #[ignore = "requires live Bilibili network"]
    async fn real_bilibili_qr_generate_smoke() {
        let client = BilibiliClient::new(None).expect("client");
        let login = client.create_qr_login().await.expect("qr generate");
        assert!(login.url.starts_with("https://"));
        assert!(!login.qrcode_key.is_empty());
        let poll = client
            .poll_qr_login(&login.qrcode_key)
            .await
            .expect("qr poll");
        assert!(matches!(
            poll.status,
            QrLoginStatus::Waiting | QrLoginStatus::Scanned | QrLoginStatus::Expired
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Bilibili network"]
    async fn real_bilibili_avatar_capture_smoke() {
        let client = BilibiliClient::new(None).expect("client");
        let account = client.fetch_account("672328094").await.expect("account");
        assert!(!account.avatar_url.is_empty());
        let data_url = client
            .fetch_avatar_data_url(&account.avatar_url)
            .await
            .expect("avatar");
        assert!(data_url.starts_with("data:image/webp;base64,"));
        assert!(data_url.len() < 50_000);
    }

    #[tokio::test]
    #[ignore = "requires live Bilibili network"]
    async fn real_bilibili_smoke() {
        let client =
            BilibiliClient::new(std::env::var("OSHICAL_BILI_COOKIE").ok()).expect("client");
        let results = client.search_accounts("嘉然").await.expect("search");
        assert!(!results.is_empty());
        let direct = client
            .search_accounts("672328094")
            .await
            .expect("uid search");
        assert_eq!(
            direct.first().map(|account| account.mid.as_str()),
            Some("672328094")
        );
        let live = client.fetch_live_status("22637261").await.expect("live");
        assert_eq!(
            live.live_url.as_deref(),
            Some("https://live.bilibili.com/22637261")
        );
        let owner = client
            .fetch_room_owner("22637261")
            .await
            .expect("room owner")
            .expect("owner info");
        assert_eq!(owner.mid, "672328094");
        assert_eq!(owner.name, "嘉然今天吃什么");
    }
}
