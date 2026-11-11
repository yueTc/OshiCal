use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE, REFERER};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

use crate::error::{AppError, AppResult};

const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
const YOUTUBE_WEB_ORIGIN: &str = "https://www.youtube.com";
const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MAX_API_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_JSON_INITIAL_DATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_VIDEO_LOOKUP_IDS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeAccountInput {
    ChannelId(String),
    Handle(String),
    LegacyUsername(String),
    VideoId(String),
    Search(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum YouTubeBroadcastState {
    #[default]
    None,
    Live,
    Upcoming,
    Unknown,
}

impl YouTubeBroadcastState {
    fn from_api(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("live") => Self::Live,
            Some("upcoming") => Self::Upcoming,
            Some("none") => Self::None,
            _ => Self::Unknown,
        }
    }

    pub fn is_live(self) -> bool {
        self == Self::Live
    }

    pub fn is_upcoming(self) -> bool {
        self == Self::Upcoming
    }

    pub fn is_known_not_live(self) -> bool {
        self == Self::None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeChannel {
    pub id: String,
    pub handle: Option<String>,
    pub name: String,
    pub description: String,
    pub avatar_url: Option<String>,
    pub profile_url: String,
    pub uploads_playlist_id: Option<String>,
    pub subscriber_count: Option<u64>,
    pub video_count: Option<u64>,
    pub view_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubePlaylistItem {
    pub video_id: String,
    pub title: String,
    pub published_at: Option<DateTime<Utc>>,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeVideo {
    pub id: String,
    pub channel_id: String,
    pub channel_title: String,
    pub title: String,
    pub description: String,
    pub published_at: Option<DateTime<Utc>>,
    pub thumbnail_url: Option<String>,
    pub duration: Option<String>,
    pub live_broadcast_content: YouTubeBroadcastState,
    pub scheduled_start_at: Option<DateTime<Utc>>,
    pub actual_start_at: Option<DateTime<Utc>>,
    pub actual_end_at: Option<DateTime<Utc>>,
    pub concurrent_viewers: Option<u64>,
    pub active_live_chat_id: Option<String>,
    pub view_count: Option<u64>,
    pub like_count: Option<u64>,
    pub comment_count: Option<u64>,
}

impl YouTubeVideo {
    pub fn is_live(&self) -> bool {
        self.live_broadcast_content.is_live()
    }

    pub fn is_upcoming(&self) -> bool {
        self.live_broadcast_content.is_upcoming()
    }

    pub fn live_url(&self) -> Option<String> {
        (self.is_live() || self.is_upcoming())
            .then(|| format!("{YOUTUBE_WEB_ORIGIN}/watch?v={}", self.id))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeLiveProbe {
    pub is_live: bool,
    pub video_id: Option<String>,
    pub title: String,
    pub cover_url: Option<String>,
    pub live_url: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub scheduled_start_at: Option<DateTime<Utc>>,
    pub concurrent_viewers: Option<u64>,
    pub checked_at: DateTime<Utc>,
}

impl YouTubeLiveProbe {
    fn offline(checked_at: DateTime<Utc>) -> Self {
        Self {
            is_live: false,
            video_id: None,
            title: String::new(),
            cover_url: None,
            live_url: None,
            started_at: None,
            scheduled_start_at: None,
            concurrent_viewers: None,
            checked_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeCommunityPost {
    pub id: String,
    pub channel_id: String,
    pub author_name: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    /// True when `published_at` is derived from YouTube's relative text such as
    /// "2 days ago" rather than an absolute timestamp.
    #[serde(default)]
    pub published_at_estimated: bool,
    pub published_text: Option<String>,
    pub text: String,
    pub images: Vec<String>,
    pub source_url: String,
    pub renderer_kind: String,
    pub membership_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeCommunityFetch {
    pub page_found: bool,
    pub membership_restricted: bool,
    pub skipped_reason: Option<String>,
    pub posts: Vec<YouTubeCommunityPost>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeMonetaryEvent {
    pub kind: String,
    pub amount_micros: Option<u64>,
    pub currency: Option<String>,
    pub amount_display_string: Option<String>,
    pub tier: Option<u64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeMembershipEvent {
    pub kind: String,
    pub member_month: Option<u64>,
    pub member_level_name: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeLiveChatMessage {
    pub id: String,
    pub live_chat_id: String,
    pub kind: String,
    pub published_at: Option<DateTime<Utc>>,
    pub display_message: Option<String>,
    pub author_channel_id: Option<String>,
    pub author_display_name: Option<String>,
    pub monetary_event: Option<YouTubeMonetaryEvent>,
    pub membership_event: Option<YouTubeMembershipEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeLiveChatPage {
    pub messages: Vec<YouTubeLiveChatMessage>,
    pub next_page_token: Option<String>,
    pub polling_interval_millis: Option<u64>,
    pub offline_at: Option<DateTime<Utc>>,
    pub active_live_chat_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeSuperChatEvent {
    pub id: String,
    /// `superChatEvent` or `superStickerEvent`; preserve the upstream event kind.
    pub kind: String,
    pub live_chat_id: String,
    pub created_at: Option<DateTime<Utc>>,
    pub amount_micros: Option<u64>,
    pub currency: Option<String>,
    pub amount_display_string: Option<String>,
    pub comment: Option<String>,
    pub user_channel_id: Option<String>,
    pub user_display_name: Option<String>,
    pub tier: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeSuperChatPage {
    pub events: Vec<YouTubeSuperChatEvent>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeSuperChatBatch {
    pub events: Vec<YouTubeSuperChatEvent>,
    pub next_page_token: Option<String>,
}

pub struct YouTubeClient {
    http: Client,
    api_key: RwLock<Option<String>>,
}

impl YouTubeClient {
    pub fn new(api_key: impl Into<String>) -> AppResult<Self> {
        Self::new_optional(Some(api_key.into()), None)
    }

    pub fn new_with_proxy(
        api_key: impl Into<String>,
        proxy_port: Option<u16>,
    ) -> AppResult<Self> {
        Self::new_optional(Some(api_key.into()), proxy_port)
    }

    pub fn new_optional(api_key: Option<String>, proxy_port: Option<u16>) -> AppResult<Self> {
        let http = crate::network::client_builder(proxy_port)?
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT_VALUE)
            .build()?;
        Ok(Self {
            http,
            api_key: RwLock::new(normalize_secret(api_key)),
        })
    }

    pub async fn set_api_key(&self, api_key: Option<String>) {
        *self.api_key.write().await = normalize_secret(api_key);
    }

    pub async fn api_key_configured(&self) -> bool {
        self.api_key.read().await.is_some()
    }

    pub fn parse_account_input(input: &str) -> AppResult<YouTubeAccountInput> {
        parse_youtube_account_input(input)
    }

    async fn required_api_key(&self) -> AppResult<String> {
        self.api_key
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::NotConfigured("YouTube API Key".to_owned()))
    }

    async fn data_api_get(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> AppResult<Value> {
        let api_key = self.required_api_key().await?;
        let mut url = Url::parse(&format!("{API_BASE}/{path}"))?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
            query.append_pair("key", &api_key);
        }
        let response = send_with_retry(|| {
            self.http
                .get(url.clone())
                .header(ACCEPT, "application/json")
                .header(REFERER, "https://www.youtube.com/")
        })
        .await?;
        self.response_json(response).await
    }

    async fn oauth_get(
        &self,
        path: &str,
        params: &[(&str, String)],
        access_token: &str,
    ) -> AppResult<Value> {
        let access_token = sanitize_secret(access_token)
            .ok_or_else(|| AppError::validation("YouTube OAuth access token 不能为空"))?;
        let mut url = Url::parse(&format!("{API_BASE}/{path}"))?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        let response = send_with_retry(|| {
            self.http
                .get(url.clone())
                .header(ACCEPT, "application/json")
                .header(REFERER, "https://www.youtube.com/")
                .bearer_auth(access_token.clone())
        })
        .await?;
        self.response_json(response).await
    }

    async fn response_json(&self, response: Response) -> AppResult<Value> {
        let status = response.status();
        let body = read_response_bytes_limited(response, MAX_API_RESPONSE_BYTES).await?;
        if !status.is_success() {
            return Err(youtube_http_error(status, &body));
        }
        let value =
            serde_json::from_slice::<Value>(&body).map_err(|_| AppError::External {
                code: "YOUTUBE_NON_JSON".to_owned(),
                message: "YouTube API 返回了非 JSON 内容".to_owned(),
            })?;
        validate_api_list_response(&value)?;
        Ok(value)
    }

    async fn fetch_page(&self, url: &str, cookie: Option<&str>) -> AppResult<Response> {
        let cookie = cookie.and_then(sanitize_secret);
        send_with_retry(|| {
            let mut request = self
                .http
                .get(url)
                .header(ACCEPT, "text/html,application/xhtml+xml")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.7")
                .header(REFERER, "https://www.youtube.com/");
            if let Some(cookie) = cookie.as_deref() {
                request = request.header(COOKIE, cookie);
            }
            request
        })
        .await
    }

    pub async fn search_channels(
        &self,
        query: &str,
        max_results: u32,
    ) -> AppResult<Vec<YouTubeChannel>> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 100 {
            return Err(AppError::validation("YouTube 搜索关键词长度必须为 1 到 100 字"));
        }
        let value = self
            .data_api_get(
                "search",
                &[
                    ("part", "snippet".to_owned()),
                    ("type", "channel".to_owned()),
                    ("q", query.to_owned()),
                    ("maxResults", max_results.clamp(1, 50).to_string()),
                ],
            )
            .await?;
        let ids = channel_ids_from_search(&value);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_channels_by_ids(&ids).await
    }

    pub async fn resolve_channel(&self, input: &str) -> AppResult<YouTubeChannel> {
        match parse_youtube_account_input(input)? {
            YouTubeAccountInput::ChannelId(channel_id) => {
                self.fetch_channel_by_id(&channel_id).await
            }
            YouTubeAccountInput::Handle(handle) => {
                self.fetch_channel_by_param("forHandle", &handle).await
            }
            YouTubeAccountInput::LegacyUsername(username) => {
                self.fetch_channel_by_param("forUsername", &username).await
            }
            YouTubeAccountInput::VideoId(video_id) => {
                let videos = self.fetch_videos(std::slice::from_ref(&video_id)).await?;
                let channel_id = videos
                    .first()
                    .map(|video| video.channel_id.clone())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| AppError::not_found("YouTube 视频所属频道"))?;
                self.fetch_channel_by_id(&channel_id).await
            }
            YouTubeAccountInput::Search(query) => {
                let mut channels = self.search_channels(&query, 1).await?;
                channels
                    .drain(..)
                    .next()
                    .ok_or_else(|| AppError::not_found("YouTube 频道"))
            }
        }
    }

    pub async fn fetch_channel_metadata(
        &self,
        channel_id: &str,
    ) -> AppResult<YouTubeChannel> {
        self.fetch_channel_by_id(channel_id).await
    }

    async fn fetch_channel_by_id(&self, channel_id: &str) -> AppResult<YouTubeChannel> {
        if !is_youtube_channel_id(channel_id) {
            return Err(AppError::validation("YouTube 频道 ID 格式无效"));
        }
        self.fetch_channel_by_param("id", channel_id).await
    }

    async fn fetch_channel_by_param(
        &self,
        param: &str,
        value: &str,
    ) -> AppResult<YouTubeChannel> {
        let value = self
            .data_api_get(
                "channels",
                &[
                    ("part", "snippet,contentDetails,statistics".to_owned()),
                    (param, value.to_owned()),
                    ("maxResults", "1".to_owned()),
                ],
            )
            .await?;
        parse_channel_items(&value)
            .into_iter()
            .next()
            .ok_or_else(|| AppError::not_found("YouTube 频道"))
    }

    async fn fetch_channels_by_ids(
        &self,
        channel_ids: &[String],
    ) -> AppResult<Vec<YouTubeChannel>> {
        let mut seen = BTreeSet::new();
        let mut ids = Vec::new();
        for id in channel_ids {
            if is_youtube_channel_id(id) && seen.insert(id.clone()) {
                ids.push(id.clone());
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = ids.join(",");
        let value = self
            .data_api_get(
                "channels",
                &[
                    ("part", "snippet,contentDetails,statistics".to_owned()),
                    ("id", ids),
                    ("maxResults", "50".to_owned()),
                ],
            )
            .await?;
        Ok(parse_channel_items(&value))
    }

    pub async fn fetch_recent_uploads(
        &self,
        uploads_playlist_id: &str,
        max_results: u32,
    ) -> AppResult<Vec<YouTubePlaylistItem>> {
        if uploads_playlist_id.trim().is_empty() {
            return Err(AppError::validation("YouTube 上传列表 ID 不能为空"));
        }
        let value = self
            .data_api_get(
                "playlistItems",
                &[
                    ("part", "snippet,contentDetails".to_owned()),
                    ("playlistId", uploads_playlist_id.to_owned()),
                    ("maxResults", max_results.clamp(1, 50).to_string()),
                ],
            )
            .await?;
        Ok(parse_playlist_items(&value))
    }

    pub async fn fetch_videos(&self, video_ids: &[String]) -> AppResult<Vec<YouTubeVideo>> {
        let mut unique = BTreeSet::new();
        for id in video_ids {
            if is_youtube_video_id(id) {
                unique.insert(id.clone());
            }
            if unique.len() >= MAX_VIDEO_LOOKUP_IDS {
                break;
            }
        }
        let ids = unique.into_iter().collect::<Vec<_>>();
        let mut videos = Vec::new();
        for chunk in ids.chunks(50) {
            let value = self
                .data_api_get(
                    "videos",
                    &[
                        (
                            "part",
                            "snippet,contentDetails,liveStreamingDetails,statistics".to_owned(),
                        ),
                        ("id", chunk.join(",")),
                        ("maxResults", "50".to_owned()),
                    ],
                )
                .await?;
            videos.extend(parse_video_items(&value));
        }
        Ok(videos)
    }

    pub async fn fetch_upcoming_live(
        &self,
        channel_id: &str,
        max_results: u32,
    ) -> AppResult<Vec<YouTubeVideo>> {
        let channel = self.fetch_channel_metadata(channel_id).await?;
        let uploads_playlist_id = channel
            .uploads_playlist_id
            .ok_or_else(|| AppError::not_found("YouTube 频道上传列表"))?;
        let limit = max_results.clamp(1, 50);
        let uploads = self
            .fetch_recent_uploads(&uploads_playlist_id, limit)
            .await?;
        let ids = uploads
            .into_iter()
            .map(|item| item.video_id)
            .collect::<Vec<_>>();
        let mut videos = self.fetch_videos(&ids).await?;
        videos.retain(|video| video.is_live() || video.is_upcoming());
        videos.sort_by(|left, right| {
            left.scheduled_start_at
                .cmp(&right.scheduled_start_at)
                .then_with(|| left.actual_start_at.cmp(&right.actual_start_at))
        });
        Ok(videos)
    }

    pub async fn probe_live_page(&self, channel_id: &str) -> AppResult<YouTubeLiveProbe> {
        if !is_youtube_channel_id(channel_id) {
            return Err(AppError::validation("YouTube 频道 ID 格式无效"));
        }
        let url = format!("{YOUTUBE_WEB_ORIGIN}/channel/{channel_id}/live");
        let response = self.fetch_page(&url, None).await?;
        let status = response.status();
        let final_url = response.url().clone();
        let checked_at = Utc::now();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(YouTubeLiveProbe::offline(checked_at));
        }
        if !status.is_success() {
            let body = read_response_bytes_limited(response, MAX_PAGE_RESPONSE_BYTES).await?;
            return Err(youtube_http_error(status, &body));
        }
        let body = read_response_bytes_limited(response, MAX_PAGE_RESPONSE_BYTES).await?;
        let html = String::from_utf8_lossy(&body);
        parse_live_page(&html, &final_url, checked_at)
    }

    pub async fn fetch_community_posts(
        &self,
        channel_or_handle: &str,
        web_cookie: Option<&str>,
    ) -> AppResult<YouTubeCommunityFetch> {
        let url = community_page_url(channel_or_handle)?;
        let cookie_supplied = web_cookie.and_then(sanitize_secret).is_some();
        let response = self.fetch_page(&url, web_cookie).await?;
        let status = response.status();
        let final_url = response.url().clone();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(YouTubeCommunityFetch {
                page_found: false,
                membership_restricted: false,
                skipped_reason: Some("no_community_tab".to_owned()),
                posts: Vec::new(),
            });
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Ok(YouTubeCommunityFetch {
                page_found: false,
                membership_restricted: true,
                skipped_reason: Some("membership_session_unavailable".to_owned()),
                posts: Vec::new(),
            });
        }
        if !status.is_success() {
            let body = read_response_bytes_limited(response, MAX_PAGE_RESPONSE_BYTES).await?;
            return Err(youtube_http_error(status, &body));
        }
        if !is_youtube_host(final_url.host_str().unwrap_or_default()) {
            if cookie_supplied {
                return Ok(YouTubeCommunityFetch {
                    page_found: false,
                    membership_restricted: true,
                    skipped_reason: Some("membership_session_unavailable".to_owned()),
                    posts: Vec::new(),
                });
            }
            return Err(AppError::External {
                code: "YOUTUBE_COMMUNITY_FORMAT".to_owned(),
                message: "YouTube 社区页重定向到了非 YouTube 页面".to_owned(),
            });
        }
        if !is_channel_route_url(&final_url) {
            if cookie_supplied {
                return Ok(YouTubeCommunityFetch {
                    page_found: false,
                    membership_restricted: true,
                    skipped_reason: Some("membership_session_unavailable".to_owned()),
                    posts: Vec::new(),
                });
            }
            return Err(AppError::External {
                code: "YOUTUBE_COMMUNITY_FORMAT".to_owned(),
                message: "YouTube 社区页重定向到了无法识别的页面".to_owned(),
            });
        }
        if !final_url.path().trim_end_matches('/').ends_with("/community") {
            return Ok(YouTubeCommunityFetch {
                page_found: false,
                membership_restricted: false,
                skipped_reason: Some("no_community_tab".to_owned()),
                posts: Vec::new(),
            });
        }
        let body = read_response_bytes_limited(response, MAX_PAGE_RESPONSE_BYTES).await?;
        let html = String::from_utf8_lossy(&body);
        parse_community_page(&html, channel_or_handle, Utc::now())
    }

    pub async fn fetch_live_chat_messages(
        &self,
        live_chat_id: &str,
        access_token: &str,
        page_token: Option<&str>,
        max_results: u32,
    ) -> AppResult<YouTubeLiveChatPage> {
        if live_chat_id.trim().is_empty() {
            return Err(AppError::validation("YouTube liveChatId 不能为空"));
        }
        let mut params = vec![
            ("part", "snippet,authorDetails".to_owned()),
            ("liveChatId", live_chat_id.to_owned()),
            ("maxResults", max_results.clamp(1, 2000).to_string()),
        ];
        if let Some(page_token) = page_token.map(str::trim).filter(|value| !value.is_empty()) {
            params.push(("pageToken", page_token.to_owned()));
        }
        let value = self
            .oauth_get("liveChat/messages", &params, access_token)
            .await?;
        Ok(parse_live_chat_page(&value))
    }

    pub async fn fetch_super_chat_events(
        &self,
        live_chat_id: &str,
        access_token: &str,
        page_token: Option<&str>,
        max_results: u32,
    ) -> AppResult<Vec<YouTubeSuperChatEvent>> {
        Ok(self
            .fetch_super_chat_page(live_chat_id, access_token, page_token, max_results)
            .await?
            .events)
    }

    async fn fetch_super_chat_page(
        &self,
        live_chat_id: &str,
        access_token: &str,
        page_token: Option<&str>,
        max_results: u32,
    ) -> AppResult<YouTubeSuperChatPage> {
        if live_chat_id.trim().is_empty() {
            return Err(AppError::validation("YouTube liveChatId 不能为空"));
        }
        let mut params = vec![
            ("part", "snippet".to_owned()),
            ("liveChatId", live_chat_id.to_owned()),
            ("maxResults", max_results.clamp(1, 50).to_string()),
        ];
        if let Some(page_token) = page_token.map(str::trim).filter(|value| !value.is_empty()) {
            params.push(("pageToken", page_token.to_owned()));
        }
        let value = self
            .oauth_get("superChatEvents", &params, access_token)
            .await?;
        Ok(parse_super_chat_page(&value))
    }

    /// Fetches Super Chat pages until YouTube stops returning a continuation token.
    /// `max_pages` is a hard safety cap; a repeated continuation token terminates
    /// traversal, and duplicate event ids are discarded across pages while preserving
    /// the first occurrence order.
    pub async fn fetch_all_super_chat_events(
        &self,
        live_chat_id: &str,
        access_token: &str,
        initial_page_token: Option<&str>,
        max_results: u32,
        max_pages: u32,
    ) -> AppResult<Vec<YouTubeSuperChatEvent>> {
        Ok(self
            .fetch_super_chat_batch(
                live_chat_id,
                access_token,
                initial_page_token,
                max_results,
                max_pages,
            )
            .await?
            .events)
    }

    /// Fetches at most `max_pages` Super Chat pages from the supplied cursor and
    /// returns the cursor for the next tick. A missing cursor means the current
    /// traversal is exhausted or encountered a token cycle.
    pub(crate) async fn fetch_super_chat_batch(
        &self,
        live_chat_id: &str,
        access_token: &str,
        initial_page_token: Option<&str>,
        max_results: u32,
        max_pages: u32,
    ) -> AppResult<YouTubeSuperChatBatch> {
        let mut page_token = initial_page_token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let mut seen_page_tokens = BTreeSet::new();
        let mut seen_event_ids = BTreeSet::new();
        let mut events = Vec::new();
        for _ in 0..max_pages.clamp(1, 100) {
            if let Some(token) = page_token.as_deref() {
                if !seen_page_tokens.insert(token.to_owned()) {
                    return Ok(YouTubeSuperChatBatch {
                        events,
                        next_page_token: None,
                    });
                }
            }
            let page = self
                .fetch_super_chat_page(
                    live_chat_id,
                    access_token,
                    page_token.as_deref(),
                    max_results,
                )
                .await?;
            append_unique_super_chat_events(&mut events, &mut seen_event_ids, page.events);
            let Some(next_token) = page
                .next_page_token
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            else {
                return Ok(YouTubeSuperChatBatch {
                    events,
                    next_page_token: None,
                });
            };
            if page_token.as_deref() == Some(next_token.as_str())
                || seen_page_tokens.contains(&next_token)
            {
                return Ok(YouTubeSuperChatBatch {
                    events,
                    next_page_token: None,
                });
            }
            page_token = Some(next_token);
        }
        Ok(YouTubeSuperChatBatch {
            events,
            next_page_token: page_token,
        })
    }
}
fn normalize_secret(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.replace(['\r', '\n'], "").trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn sanitize_secret(value: &str) -> Option<String> {
    normalize_secret(Some(value.to_owned()))
}

pub fn parse_youtube_account_input(input: &str) -> AppResult<YouTubeAccountInput> {
    let input = input.trim();
    if input.is_empty() || input.chars().count() > 512 {
        return Err(AppError::validation(
            "YouTube 账号输入长度必须为 1 到 512 字",
        ));
    }
    if is_youtube_channel_id(input) {
        return Ok(YouTubeAccountInput::ChannelId(input.to_owned()));
    }
    if is_youtube_video_id(input) {
        return Ok(YouTubeAccountInput::VideoId(input.to_owned()));
    }
    if input.starts_with('@') {
        return normalize_handle(input)
            .map(YouTubeAccountInput::Handle)
            .ok_or_else(|| AppError::validation("YouTube handle 格式无效"));
    }

    let lower = input.to_ascii_lowercase();
    let looks_like_url = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("youtube.com/")
        || lower.starts_with("www.youtube.com/")
        || lower.starts_with("m.youtube.com/")
        || lower.starts_with("music.youtube.com/")
        || lower.starts_with("youtu.be/");
    if !looks_like_url {
        return Ok(YouTubeAccountInput::Search(input.to_owned()));
    }

    let normalized = if lower.starts_with("http://") || lower.starts_with("https://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url = Url::parse(&normalized)?;
    if !is_youtube_host(url.host_str().unwrap_or_default()) {
        // A pasted non-YouTube URL is safe to treat as a search phrase. It is never
        // navigated to or used as an upstream request URL.
        return Ok(YouTubeAccountInput::Search(input.to_owned()));
    }
    if url.host_str() == Some("youtu.be") {
        return url
            .path_segments()
            .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
            .filter(|segment| is_youtube_video_id(segment))
            .map(|segment| YouTubeAccountInput::VideoId(segment.to_owned()))
            .ok_or_else(|| AppError::validation("YouTube 短链接缺少有效视频 ID"));
    }

    if let Some(video_id) = url
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
        .filter(|value| is_youtube_video_id(value))
    {
        return Ok(YouTubeAccountInput::VideoId(video_id));
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    match segments.as_slice() {
        ["channel", channel_id, ..] if is_youtube_channel_id(channel_id) => {
            Ok(YouTubeAccountInput::ChannelId((*channel_id).to_owned()))
        }
        ["@", ..] => Err(AppError::validation("YouTube handle 不能为空")),
        [handle, ..] if handle.starts_with('@') => normalize_handle(handle)
            .map(YouTubeAccountInput::Handle)
            .ok_or_else(|| AppError::validation("YouTube handle 格式无效")),
        ["user", username, ..] if !username.is_empty() => {
            Ok(YouTubeAccountInput::LegacyUsername((*username).to_owned()))
        }
        ["c", name, ..] if !name.is_empty() => Ok(YouTubeAccountInput::Search((*name).to_owned())),
        [kind, video_id, ..]
            if matches!(*kind, "live" | "embed" | "shorts" | "v")
                && is_youtube_video_id(video_id) => {
            Ok(YouTubeAccountInput::VideoId((*video_id).to_owned()))
        }
        _ => Ok(YouTubeAccountInput::Search(input.to_owned())),
    }
}

fn is_youtube_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "youtu.be"
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
}

fn is_youtube_channel_id(value: &str) -> bool {
    value.len() == 24
        && value.starts_with("UC")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn is_youtube_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn normalize_handle(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('@');
    if value.is_empty()
        || value.chars().count() > 100
        || !value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return None;
    }
    Some(format!("@{value}"))
}

fn channel_profile_url(channel_id: &str, handle: Option<&str>) -> String {
    handle
        .map(|handle| format!("{YOUTUBE_WEB_ORIGIN}/{handle}"))
        .unwrap_or_else(|| format!("{YOUTUBE_WEB_ORIGIN}/channel/{channel_id}"))
}

fn channel_ids_from_search(value: &Value) -> Vec<String> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.pointer("/id/channelId")
                .and_then(Value::as_str)
                .or_else(|| item.pointer("/snippet/channelId").and_then(Value::as_str))
                .filter(|value| is_youtube_channel_id(value))
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn parse_channel_items(value: &Value) -> Vec<YouTubeChannel> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_channel_item)
        .collect()
}

fn parse_channel_item(item: &Value) -> Option<YouTubeChannel> {
    let id = item.get("id")?.as_str()?.to_owned();
    if !is_youtube_channel_id(&id) {
        return None;
    }
    let snippet = item.get("snippet");
    let name = snippet
        .and_then(|value| value.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("YouTube 频道")
        .to_owned();
    let handle = snippet
        .and_then(|value| value.get("customUrl"))
        .and_then(Value::as_str)
        .and_then(|value| {
            value
                .rsplit('/')
                .find(|segment| !segment.is_empty())
                .and_then(normalize_handle)
        });
    let statistics = item.get("statistics");
    Some(YouTubeChannel {
        profile_url: channel_profile_url(&id, handle.as_deref()),
        handle,
        id,
        name,
        description: snippet
            .and_then(|value| value.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        avatar_url: snippet
            .and_then(|value| value.get("thumbnails"))
            .and_then(best_thumbnail),
        uploads_playlist_id: item
            .pointer("/contentDetails/relatedPlaylists/uploads")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        subscriber_count: statistics
            .and_then(|value| value.get("subscriberCount"))
            .and_then(json_u64),
        video_count: statistics
            .and_then(|value| value.get("videoCount"))
            .and_then(json_u64),
        view_count: statistics
            .and_then(|value| value.get("viewCount"))
            .and_then(json_u64),
    })
}

fn parse_playlist_items(value: &Value) -> Vec<YouTubePlaylistItem> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let video_id = item
                .pointer("/contentDetails/videoId")
                .and_then(Value::as_str)
                .or_else(|| item.pointer("/snippet/resourceId/videoId").and_then(Value::as_str))
                .filter(|value| is_youtube_video_id(value))?;
            let snippet = item.get("snippet");
            Some(YouTubePlaylistItem {
                video_id: video_id.to_owned(),
                title: snippet
                    .and_then(|value| value.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                published_at: item
                    .pointer("/contentDetails/videoPublishedAt")
                    .and_then(Value::as_str)
                    .or_else(|| snippet.and_then(|value| value.get("publishedAt")).and_then(Value::as_str))
                    .and_then(parse_rfc3339),
                thumbnail_url: snippet
                    .and_then(|value| value.get("thumbnails"))
                    .and_then(best_thumbnail),
            })
        })
        .collect()
}

fn parse_video_items(value: &Value) -> Vec<YouTubeVideo> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_video_item)
        .collect()
}

fn parse_video_item(item: &Value) -> Option<YouTubeVideo> {
    let id = item.get("id")?.as_str()?.to_owned();
    if !is_youtube_video_id(&id) {
        return None;
    }
    let snippet = item.get("snippet");
    let live = item.get("liveStreamingDetails");
    let statistics = item.get("statistics");
    Some(YouTubeVideo {
        id,
        channel_id: snippet
            .and_then(|value| value.get("channelId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        channel_title: snippet
            .and_then(|value| value.get("channelTitle"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        title: snippet
            .and_then(|value| value.get("title"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        description: snippet
            .and_then(|value| value.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        published_at: snippet
            .and_then(|value| value.get("publishedAt"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        thumbnail_url: snippet
            .and_then(|value| value.get("thumbnails"))
            .and_then(best_thumbnail),
        duration: item
            .pointer("/contentDetails/duration")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        live_broadcast_content: YouTubeBroadcastState::from_api(
            snippet
                .and_then(|value| value.get("liveBroadcastContent"))
                .and_then(Value::as_str),
        ),
        scheduled_start_at: live
            .and_then(|value| value.get("scheduledStartTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        actual_start_at: live
            .and_then(|value| value.get("actualStartTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        actual_end_at: live
            .and_then(|value| value.get("actualEndTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        concurrent_viewers: live
            .and_then(|value| value.get("concurrentViewers"))
            .and_then(json_u64),
        active_live_chat_id: live
            .and_then(|value| value.get("activeLiveChatId"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        view_count: statistics
            .and_then(|value| value.get("viewCount"))
            .and_then(json_u64),
        like_count: statistics
            .and_then(|value| value.get("likeCount"))
            .and_then(json_u64),
        comment_count: statistics
            .and_then(|value| value.get("commentCount"))
            .and_then(json_u64),
    })
}

fn community_page_url(channel_or_handle: &str) -> AppResult<String> {
    let value = channel_or_handle.trim();
    if is_youtube_channel_id(value) {
        return Ok(format!("{YOUTUBE_WEB_ORIGIN}/channel/{value}/community"));
    }
    if let Some(handle) = normalize_handle(value) {
        return Ok(format!("{YOUTUBE_WEB_ORIGIN}/{handle}/community"));
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        let url = Url::parse(value)?;
        if !is_youtube_host(url.host_str().unwrap_or_default())
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(AppError::validation("不是受支持的 YouTube 社区页链接"));
        }
        let mut url = url;
        let path = url.path().trim_end_matches('/');
        if path.ends_with("/community") {
            url.set_query(None);
            url.set_fragment(None);
            return Ok(url.to_string());
        }
        if path.starts_with("/channel/") || path.starts_with("/@") {
            url.set_path(&format!("{path}/community"));
            url.set_query(None);
            url.set_fragment(None);
            return Ok(url.to_string());
        }
    }
    Err(AppError::validation(
        "YouTube 社区页目标必须是频道 ID、handle 或频道链接",
    ))
}

fn parse_community_page(
    html: &str,
    channel_hint: &str,
    observed_at: DateTime<Utc>,
) -> AppResult<YouTubeCommunityFetch> {
    let lower = html.to_ascii_lowercase();
    let html_membership_restricted = lower.contains("members only")
        || lower.contains("sponsorsonlybadge")
        || lower.contains("membersonlybadge")
        || html.contains("会员专属");
    let Some(initial) = extract_json_assignment(html, "ytInitialData") else {
        if html_membership_restricted {
            return Ok(YouTubeCommunityFetch {
                page_found: false,
                membership_restricted: true,
                skipped_reason: Some("membership_session_unavailable".to_owned()),
                posts: Vec::new(),
            });
        }
        return Err(AppError::External {
            code: "YOUTUBE_COMMUNITY_FORMAT".to_owned(),
            message: "YouTube 社区页结构已变化，未找到 ytInitialData".to_owned(),
        });
    };
    let mut renderers = Vec::new();
    collect_community_renderers(&initial, &mut renderers);
    let mut seen_ids = BTreeSet::new();
    let mut posts = Vec::new();
    for (kind, renderer) in renderers {
        let Some(post) = parse_community_post(&renderer, channel_hint, &kind, observed_at) else {
            continue;
        };
        if is_youtube_channel_id(channel_hint) && post.channel_id != channel_hint {
            // Redirects, shared feeds, or malformed markup must not attribute
            // another channel's community content to the requested streamer.
            continue;
        }
        if post.text.trim().is_empty() && post.images.is_empty() {
            continue;
        }
        if post.membership_only
            && post.images.is_empty()
            && is_membership_content_placeholder(&post.text)
        {
            continue;
        }
        if seen_ids.insert(post.id.clone()) {
            posts.push(post);
        }
    }
    let initial_membership_restricted = contains_key_recursive(&initial, "sponsorsOnlyBadge")
        || contains_key_recursive(&initial, "membersOnlyBadge")
        || contains_key_recursive(&initial, "membersOnly")
        || contains_key_recursive(&initial, "membershipOnly")
        || contains_true_for_key(&initial, "isMembersOnly");
    // Page-wide HTML phrases are only a fallback when we did not parse any post.
    // Otherwise a public post containing the words "members only" would taint
    // every other post on the page.
    let membership_restricted =
        initial_membership_restricted || (posts.is_empty() && html_membership_restricted);
    Ok(YouTubeCommunityFetch {
        page_found: true,
        membership_restricted,
        skipped_reason: (posts.is_empty() && membership_restricted)
            .then(|| "membership_session_unavailable".to_owned()),
        posts,
    })
}

fn collect_community_renderers(value: &Value, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(object) => {
            for key in ["backstagePostRenderer", "postRenderer", "sharedPostRenderer"] {
                if let Some(renderer) = object.get(key) {
                    out.push((key.to_owned(), renderer.clone()));
                }
            }
            for child in object.values() {
                collect_community_renderers(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_community_renderers(child, out);
            }
        }
        _ => {}
    }
}

fn parse_community_post(
    renderer: &Value,
    channel_hint: &str,
    renderer_kind: &str,
    observed_at: DateTime<Utc>,
) -> Option<YouTubeCommunityPost> {
    let object = renderer.as_object()?;
    let id = object.get("postId").and_then(Value::as_str)?.to_owned();
    if id.is_empty() {
        return None;
    }
    let text = text_value(object.get("contentText"));
    let author_name = text_value(object.get("authorText"));
    let published_text = text_value(object.get("publishedTimeText"));
    let (published_at, published_at_estimated) =
        parse_community_published_time(&published_text, observed_at);
    let author_channel_id = object
        .pointer("/authorEndpoint/browseEndpoint/browseId")
        .and_then(Value::as_str)
        .filter(|value| is_youtube_channel_id(value))
        .unwrap_or(channel_hint)
        .to_owned();
    let mut image_urls = Vec::new();
    let mut seen_images = BTreeSet::new();
    for key in ["backstageAttachment", "contentAttachment"] {
        if let Some(attachment) = object.get(key) {
            collect_attachment_images(attachment, true, &mut image_urls, &mut seen_images);
        }
    }
    image_urls.truncate(9);
    Some(YouTubeCommunityPost {
        id: id.clone(),
        channel_id: author_channel_id,
        author_name: (!author_name.is_empty()).then_some(author_name),
        published_at,
        published_at_estimated,
        published_text: (!published_text.is_empty()).then_some(published_text),
        text,
        images: image_urls,
        source_url: format!("{YOUTUBE_WEB_ORIGIN}/post/{id}"),
        renderer_kind: renderer_kind.to_owned(),
        membership_only: renderer_is_membership_only(renderer),
    })
}

fn renderer_is_membership_only(renderer: &Value) -> bool {
    contains_key_recursive(renderer, "sponsorsOnlyBadge")
        || contains_key_recursive(renderer, "membersOnlyBadge")
        || contains_key_recursive(renderer, "membersOnly")
        || contains_key_recursive(renderer, "membershipOnly")
        || contains_true_for_key(renderer, "isMembersOnly")
}

fn is_membership_content_placeholder(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "members only"
            | "members-only"
            | "join to view"
            | "join this channel to view"
            | "会员专属"
            | "会员专享"
            | "加入会员以查看"
            | "成为会员后查看"
    )
}

fn collect_attachment_images(
    value: &Value,
    inside_attachment: bool,
    out: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(object) => {
            let has_thumbnail_source = object
                .get("thumbnails")
                .and_then(Value::as_array)
                .is_some()
                || object.get("url").and_then(Value::as_str).is_some();
            if inside_attachment && has_thumbnail_source {
                if let Some(url) = best_thumbnail(value) {
                    if seen.insert(url.clone()) {
                        out.push(url);
                    }
                }
                return;
            }
            for (key, child) in object {
                let lower = key.to_ascii_lowercase();
                let child_inside = inside_attachment
                    || lower.contains("image")
                    || lower.contains("attachment")
                    || lower.contains("thumbnail");
                collect_attachment_images(child, child_inside, out, seen);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_attachment_images(child, inside_attachment, out, seen);
            }
        }
        _ => {}
    }
}

fn parse_live_page(
    html: &str,
    final_url: &Url,
    checked_at: DateTime<Utc>,
) -> AppResult<YouTubeLiveProbe> {
    if !is_youtube_host(final_url.host_str().unwrap_or_default()) {
        return Err(AppError::External {
            code: "YOUTUBE_LIVE_FORMAT".to_owned(),
            message: "YouTube 直播页重定向到了非 YouTube 页面".to_owned(),
        });
    }
    let player = extract_json_assignment(html, "ytInitialPlayerResponse");
    let initial = extract_json_assignment(html, "ytInitialData");
    if player.is_none() && initial.is_none() {
        return Err(AppError::External {
            code: "YOUTUBE_LIVE_FORMAT".to_owned(),
            message: "YouTube 直播页未找到可识别的 ytInitialData 或 ytInitialPlayerResponse；可能是验证页、同意页、登录页或页面结构已变化".to_owned(),
        });
    }

    let player_video_id = player
        .as_ref()
        .and_then(|value| value.pointer("/videoDetails/videoId"))
        .and_then(Value::as_str)
        .filter(|value| is_youtube_video_id(value))
        .map(ToOwned::to_owned);
    // `ytInitialData` contains recommendations and other unrelated videos. Only
    // the current player may establish that the target channel is live.
    let is_live = player_video_id.is_some()
        && player
            .as_ref()
            .is_some_and(player_has_explicit_live_signal)
        && !player
            .as_ref()
            .is_some_and(player_has_explicit_offline_signal);
    if !is_live {
        if live_page_explicitly_offline(player.as_ref(), initial.as_ref(), final_url) {
            return Ok(YouTubeLiveProbe::offline(checked_at));
        }
        return Err(AppError::External {
            code: "YOUTUBE_LIVE_FORMAT".to_owned(),
            message: "YouTube 直播页没有明确的无直播状态，不能判定为下播".to_owned(),
        });
    }

    let video_id = player_video_id;
    let title = player
        .as_ref()
        .and_then(|value| value.pointer("/videoDetails/title"))
        .and_then(Value::as_str)
        .or_else(|| {
            player
                .as_ref()
                .and_then(|value| {
                    value.pointer("/microformat/playerMicroformatRenderer/title/simpleText")
                })
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned();
    let cover_url = player
        .as_ref()
        .and_then(|value| value.pointer("/videoDetails/thumbnail"))
        .and_then(best_thumbnail);
    let started_at = player
        .as_ref()
        .and_then(|value| {
            value.pointer(
                "/microformat/playerMicroformatRenderer/liveBroadcastDetails/startTimestamp",
            )
        })
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            initial
                .as_ref()
                .and_then(|value| find_string_for_key(value, "startTimestamp"))
                .and_then(|value| parse_rfc3339(&value))
        });
    let scheduled_start_at = player
        .as_ref()
        .and_then(|value| find_string_for_key(value, "scheduledStartTime"))
        .and_then(|value| parse_rfc3339(&value))
        .or_else(|| {
            initial
                .as_ref()
                .and_then(|value| find_string_for_key(value, "scheduledStartTime"))
                .and_then(|value| parse_rfc3339(&value))
        });
    let concurrent_viewers = initial
        .as_ref()
        .and_then(|value| find_u64_for_key(value, "concurrentViewers"));
    let live_url = video_id
        .as_deref()
        .map(|id| format!("{YOUTUBE_WEB_ORIGIN}/watch?v={id}"))
        .or_else(|| Some(final_url.to_string()));
    Ok(YouTubeLiveProbe {
        is_live: true,
        video_id,
        title,
        cover_url,
        live_url,
        started_at,
        scheduled_start_at,
        concurrent_viewers,
        checked_at,
    })
}

fn live_page_explicitly_offline(
    player: Option<&Value>,
    initial: Option<&Value>,
    final_url: &Url,
) -> bool {
    let player_has_video = player
        .and_then(|value| value.pointer("/videoDetails/videoId"))
        .and_then(Value::as_str)
        .is_some_and(is_youtube_video_id);
    player.is_some_and(player_has_explicit_offline_signal)
        || (!player_has_video
            && initial.is_some_and(|value| {
                is_channel_route_url(final_url) && is_recognizable_channel_page_data(value)
            }))
}

fn player_has_explicit_live_signal(player: &Value) -> bool {
    player
        .pointer("/videoDetails/isLive")
        .and_then(Value::as_bool)
        == Some(true)
        || player
            .pointer("/microformat/playerMicroformatRenderer/liveBroadcastDetails/isLiveNow")
            .and_then(Value::as_bool)
            == Some(true)
}

fn player_has_explicit_offline_signal(player: &Value) -> bool {
    player
        .pointer("/videoDetails/isLive")
        .and_then(Value::as_bool)
        == Some(false)
        || player
            .pointer("/microformat/playerMicroformatRenderer/liveBroadcastDetails/isLiveNow")
            .and_then(Value::as_bool)
            == Some(false)
}

fn is_recognizable_channel_page_data(initial: &Value) -> bool {
    ["channelMetadataRenderer", "c4TabbedHeaderRenderer", "pageHeaderRenderer"]
        .iter()
        .any(|key| contains_key_recursive(initial, key))
}


fn is_channel_route_url(url: &Url) -> bool {
    is_youtube_host(url.host_str().unwrap_or_default())
        && (url.path().starts_with("/channel/") || url.path().starts_with("/@"))
}

fn parse_live_chat_page(value: &Value) -> YouTubeLiveChatPage {
    YouTubeLiveChatPage {
        messages: value
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_live_chat_message)
            .collect(),
        next_page_token: value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        polling_interval_millis: value.get("pollingIntervalMillis").and_then(json_u64),
        offline_at: value
            .get("offlineAt")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        active_live_chat_id: value
            .get("activeLiveChatId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    }
}

fn parse_live_chat_message(item: &Value) -> Option<YouTubeLiveChatMessage> {
    let snippet = item.get("snippet")?;
    let id = item.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let kind = snippet
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    Some(YouTubeLiveChatMessage {
        id: id.to_owned(),
        live_chat_id: snippet
            .get("liveChatId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: kind.clone(),
        published_at: snippet
            .get("publishedAt")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        display_message: snippet
            .get("displayMessage")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        author_channel_id: item
            .pointer("/authorDetails/channelId")
            .and_then(Value::as_str)
            .or_else(|| {
                snippet
                    .pointer("/authorChannelId/value")
                    .and_then(Value::as_str)
            })
            .map(ToOwned::to_owned),
        author_display_name: item
            .pointer("/authorDetails/displayName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        monetary_event: parse_chat_monetary_event(snippet, &kind),
        membership_event: parse_chat_membership_event(snippet, &kind),
    })
}

fn parse_chat_monetary_event(snippet: &Value, _kind: &str) -> Option<YouTubeMonetaryEvent> {
    let (details, event_kind) = if let Some(details) = snippet.get("superChatDetails") {
        (details, "superChatEvent")
    } else if let Some(details) = snippet.get("superStickerDetails") {
        (details, "superStickerEvent")
    } else {
        return None;
    };

    Some(YouTubeMonetaryEvent {
        kind: event_kind.to_owned(),
        amount_micros: details.get("amountMicros").and_then(json_u64),
        currency: details
            .get("currency")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        amount_display_string: details
            .get("amountDisplayString")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tier: details.get("tier").and_then(json_u64),
        comment: details
            .get("userComment")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
}

fn parse_chat_membership_event(snippet: &Value, kind: &str) -> Option<YouTubeMembershipEvent> {
    let (details, event_kind) = if let Some(details) = snippet.get("memberMilestoneChatDetails") {
        (Some(details), "memberMilestoneChatEvent")
    } else if let Some(details) = snippet.get("newSponsorDetails") {
        (Some(details), "newSponsorEvent")
    } else if let Some(details) = snippet.get("membershipGiftingDetails") {
        (Some(details), "membershipGiftingEvent")
    } else {
        (None, kind)
    };
    if details.is_none()
        && !matches!(
            kind,
            "memberMilestoneChatEvent"
                | "newSponsorEvent"
                | "membershipGiftingEvent"
        )
    {
        return None;
    }
    Some(YouTubeMembershipEvent {
        kind: event_kind.to_owned(),
        member_month: details
            .and_then(|value| value.get("memberMonth"))
            .and_then(json_u64),
        member_level_name: details
            .and_then(|value| value.get("memberLevelName"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        comment: details
            .and_then(|value| value.get("userComment"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
}

fn parse_super_chat_page(value: &Value) -> YouTubeSuperChatPage {
    YouTubeSuperChatPage {
        events: value
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_super_chat_event)
            .collect(),
        next_page_token: value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    }
}

fn parse_super_chat_events(value: &Value) -> Vec<YouTubeSuperChatEvent> {
    parse_super_chat_page(value).events
}

fn append_unique_super_chat_events(
    out: &mut Vec<YouTubeSuperChatEvent>,
    seen_event_ids: &mut BTreeSet<String>,
    events: Vec<YouTubeSuperChatEvent>,
) {
    for event in events {
        if seen_event_ids.insert(event.id.clone()) {
            out.push(event);
        }
    }
}

fn parse_super_chat_event(item: &Value) -> Option<YouTubeSuperChatEvent> {
    let snippet = item.get("snippet")?;
    let id = item.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let (details, event_kind) = if let Some(details) = snippet.get("superChatDetails") {
        (details, "superChatEvent")
    } else if let Some(details) = snippet.get("superStickerDetails") {
        (details, "superStickerEvent")
    } else {
        // Compatibility for legacy flattened superChatEvents.list payloads. Require
        // a known event type and at least one monetary field so malformed or newly
        // structured items are skipped instead of becoming zero-value events.
        let event_kind = snippet
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "superChatEvent" | "superStickerEvent"))?;
        let has_monetary_field = [
            "amountMicros",
            "currency",
            "amountDisplayString",
            "displayString",
            "commentText",
            "userComment",
            "tier",
        ]
        .iter()
        .any(|key| snippet.get(*key).is_some());
        if !has_monetary_field {
            return None;
        }
        (snippet, event_kind)
    };
    Some(YouTubeSuperChatEvent {
        id: id.to_owned(),
        kind: event_kind.to_owned(),
        live_chat_id: snippet
            .get("liveChatId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: snippet
            .get("createdAt")
            .and_then(Value::as_str)
            .or_else(|| snippet.get("publishedAt").and_then(Value::as_str))
            .and_then(parse_rfc3339),
        amount_micros: details.get("amountMicros").and_then(json_u64),
        currency: details
            .get("currency")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        amount_display_string: details
            .get("amountDisplayString")
            .and_then(Value::as_str)
            .or_else(|| details.get("displayString").and_then(Value::as_str))
            .map(ToOwned::to_owned),
        comment: details
            .get("commentText")
            .and_then(Value::as_str)
            .or_else(|| details.get("userComment").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        user_channel_id: snippet
            .pointer("/userDetails/channelId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        user_display_name: snippet
            .pointer("/userDetails/displayName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tier: details.get("tier").and_then(json_u64),
    })
}
fn text_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(value) = value.as_str() {
        return value.to_owned();
    }
    if let Some(value) = value.get("simpleText").and_then(Value::as_str) {
        return value.to_owned();
    }
    if let Some(value) = value.get("content").and_then(Value::as_str) {
        return value.to_owned();
    }
    if let Some(runs) = value.get("runs").and_then(Value::as_array) {
        return runs
            .iter()
            .filter_map(|run| run.get("text").and_then(Value::as_str))
            .collect::<String>();
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn best_thumbnail(value: &Value) -> Option<String> {
    if let Some(items) = value.as_array() {
        return best_thumbnail_array(items);
    }
    if let Some(items) = value.get("thumbnails").and_then(Value::as_array) {
        return best_thumbnail_array(items);
    }
    if let Some(url) = value.get("url").and_then(Value::as_str) {
        return normalize_youtube_asset_url(url);
    }
    if let Some(url) = value.get("image").and_then(best_thumbnail) {
        return Some(url);
    }
    for key in ["maxres", "standard", "high", "medium", "default"] {
        if let Some(url) = value.get(key).and_then(best_thumbnail) {
            return Some(url);
        }
    }
    None
}

fn best_thumbnail_array(items: &[Value]) -> Option<String> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .and_then(normalize_youtube_asset_url)
                .or_else(|| item.get("image").and_then(best_thumbnail))?;
            let width = item.get("width").and_then(json_u64).unwrap_or(0);
            let height = item.get("height").and_then(json_u64).unwrap_or(0);
            Some((url, width.saturating_mul(height), index))
        })
        .max_by_key(|(_, area, index)| (*area, *index))
        .map(|(url, _, _)| url)
}

fn normalize_youtube_asset_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let normalized = if let Some(rest) = value.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        value.to_owned()
    };
    let url = Url::parse(&normalized).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let allowed = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "ytimg.com"
        || host.ends_with(".ytimg.com")
        || host == "ggpht.com"
        || host.ends_with(".ggpht.com")
        || host == "googleusercontent.com"
        || host.ends_with(".googleusercontent.com");
    allowed.then(|| url.to_string())
}

fn contains_key_recursive(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key)
                || object
                    .values()
                    .any(|child| contains_key_recursive(child, key))
        }
        Value::Array(items) => items
            .iter()
            .any(|child| contains_key_recursive(child, key)),
        _ => false,
    }
}

fn contains_true_for_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.get(key).and_then(Value::as_bool) == Some(true)
                || object
                    .values()
                    .any(|child| contains_true_for_key(child, key))
        }
        Value::Array(items) => items
            .iter()
            .any(|child| contains_true_for_key(child, key)),
        _ => false,
    }
}

fn find_string_for_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(value) = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                return Some(value.to_owned());
            }
            object
                .values()
                .find_map(|child| find_string_for_key(child, key))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_for_key(child, key)),
        _ => None,
    }
}

fn find_u64_for_key(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(object) => {
            if let Some(value) = object.get(key).and_then(json_u64) {
                return Some(value);
            }
            object
                .values()
                .find_map(|child| find_u64_for_key(child, key))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_u64_for_key(child, key)),
        _ => None,
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .map(|value| value.trim().replace(',', ""))
            .and_then(|value| value.parse().ok())
    })
}

fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn parse_community_published_time(
    value: &str,
    observed_at: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, bool) {
    let value = value.trim();
    if value.is_empty() {
        return (None, false);
    }
    if let Some(exact) = parse_rfc3339(value) {
        return (Some(exact), false);
    }
    let Some(age) = parse_relative_published_age(value) else {
        return (None, false);
    };
    match observed_at.checked_sub_signed(age) {
        Some(value) => (Some(value), true),
        None => (None, false),
    }
}

fn parse_relative_published_age(value: &str) -> Option<ChronoDuration> {
    let normalized = value
        .trim()
        .trim_end_matches(['.', ','])
        .to_ascii_lowercase();
    if matches!(normalized.as_str(), "now" | "just now" | "today") {
        return Some(ChronoDuration::zero());
    }
    if normalized == "yesterday" {
        return Some(ChronoDuration::days(1));
    }

    let words = normalized.split_whitespace().collect::<Vec<_>>();
    if words.len() == 3 && words[2] == "ago" {
        let count = parse_relative_count(words[0])?;
        let unit = match words[1].trim_end_matches('s') {
            "second" | "sec" => "second",
            "minute" | "min" => "minute",
            "hour" | "hr" => "hour",
            "day" => "day",
            "week" | "wk" => "week",
            "month" => "month",
            "year" | "yr" => "year",
            _ => return None,
        };
        return relative_age_duration(count, unit);
    }

    let compact = normalized
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    match compact.as_str() {
        "刚刚" | "刚才" | "今天" => return Some(ChronoDuration::zero()),
        "昨天" => return Some(ChronoDuration::days(1)),
        "前天" => return Some(ChronoDuration::days(2)),
        _ => {}
    }
    for (suffix, unit) in [
        ("秒钟前", "second"),
        ("秒前", "second"),
        ("分钟前", "minute"),
        ("个小时前", "hour"),
        ("小时前", "hour"),
        ("天前", "day"),
        ("星期前", "week"),
        ("周前", "week"),
        ("个月前", "month"),
        ("月前", "month"),
        ("年前", "year"),
    ] {
        if let Some(number) = compact.strip_suffix(suffix) {
            let count = parse_relative_count(number)?;
            return relative_age_duration(count, unit);
        }
    }
    None
}

fn parse_relative_count(value: &str) -> Option<u64> {
    match value.trim() {
        "a" | "an" => Some(1),
        value => value.replace(',', "").parse().ok(),
    }
}

fn relative_age_duration(count: u64, unit: &str) -> Option<ChronoDuration> {
    let seconds_per_unit = match unit {
        "second" => 1,
        "minute" => 60,
        "hour" => 60 * 60,
        "day" => 24 * 60 * 60,
        "week" => 7 * 24 * 60 * 60,
        "month" => 30 * 24 * 60 * 60,
        "year" => 365 * 24 * 60 * 60,
        _ => return None,
    };
    let seconds = count.checked_mul(seconds_per_unit)?;
    ChronoDuration::try_seconds(i64::try_from(seconds).ok()?)
}

fn extract_json_assignment(html: &str, key: &str) -> Option<Value> {
    let mut offset = 0;
    while offset < html.len() {
        let relative = html[offset..].find(key)?;
        let key_start = offset + relative;
        let key_end = key_start + key.len();
        let tail = &html[key_end..];
        let Some(eq_relative) = tail.find('=') else {
            return None;
        };
        if eq_relative > 256 || tail[..eq_relative].contains(';') {
            offset = key_end;
            continue;
        }
        let after_equal = key_end + eq_relative + 1;
        let after = &html[after_equal..];
        let Some(brace_relative) = after.find('{') else {
            return None;
        };
        if brace_relative > 64 || after[..brace_relative].contains(';') {
            offset = key_end;
            continue;
        }
        let start = after_equal + brace_relative;
        if let Some(value) = parse_json_object_at(html, start) {
            return Some(value);
        }
        offset = key_end;
    }
    None
}

fn parse_json_object_at(html: &str, start: usize) -> Option<Value> {
    let bytes = html.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if index.saturating_sub(start) > MAX_JSON_INITIAL_DATA_BYTES {
            return None;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    let json = &html[start..=index];
                    return serde_json::from_str(json).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn validate_api_list_response(value: &Value) -> AppResult<()> {
    let Some(object) = value.as_object() else {
        return Err(AppError::External {
            code: "YOUTUBE_API_FORMAT".to_owned(),
            message: "YouTube API 响应顶层不是 JSON 对象".to_owned(),
        });
    };
    if object.get("items").is_some_and(|value| !value.is_array()) {
        return Err(AppError::External {
            code: "YOUTUBE_API_FORMAT".to_owned(),
            message: "YouTube API 响应的 items 不是数组".to_owned(),
        });
    }
    if object
        .get("nextPageToken")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(AppError::External {
            code: "YOUTUBE_API_FORMAT".to_owned(),
            message: "YouTube API 响应的 nextPageToken 格式无效".to_owned(),
        });
    }
    Ok(())
}

fn youtube_http_error(status: StatusCode, body: &[u8]) -> AppError {
    let value = serde_json::from_slice::<Value>(body).ok();
    let error = value.as_ref().and_then(|value| value.get("error"));
    let reason = error
        .and_then(|value| value.pointer("/errors/0/reason"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(|value| value.get("status")).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(|value| value.chars().take(512).collect::<String>())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("YouTube API 返回 HTTP {status}"));
    let code = if reason == "keyInvalid"
        || message
            .to_ascii_lowercase()
            .contains("api key not valid")
    {
        "YOUTUBE_API_KEY_INVALID".to_owned()
    } else {
        match (status, reason.as_str()) {
            (StatusCode::UNAUTHORIZED, _) => "YOUTUBE_UNAUTHORIZED".to_owned(),
            (StatusCode::FORBIDDEN, "quotaExceeded")
            | (StatusCode::FORBIDDEN, "dailyLimitExceeded")
            | (StatusCode::FORBIDDEN, "limitExceeded") => {
                "YOUTUBE_QUOTA_EXCEEDED".to_owned()
            }
            (StatusCode::FORBIDDEN, "rateLimitExceeded") => "YOUTUBE_RATE_LIMITED".to_owned(),
            (StatusCode::FORBIDDEN, _) => "YOUTUBE_FORBIDDEN".to_owned(),
            (StatusCode::TOO_MANY_REQUESTS, _) => "YOUTUBE_RATE_LIMITED".to_owned(),
            (StatusCode::NOT_FOUND, _) => "YOUTUBE_NOT_FOUND".to_owned(),
            (StatusCode::BAD_REQUEST, _) => "YOUTUBE_BAD_REQUEST".to_owned(),
            (status, _) if status.is_server_error() => "YOUTUBE_UPSTREAM_ERROR".to_owned(),
            (status, _) => format!("YOUTUBE_HTTP_{}", status.as_u16()),
        }
    };
    AppError::External { code, message }
}

async fn send_with_retry<F>(mut builder: F) -> AppResult<Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut last_error = None;
    let mut attempts = 0;
    for attempt in 0..3 {
        attempts = attempt + 1;
        match builder().send().await {
            Ok(response) if response.status().is_server_error() && attempt < 2 => {
                tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
            }
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
    let error = last_error.expect("retry loop always records a network error");
    let reason = if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else {
        "请求失败"
    };
    Err(AppError::External {
        code: "YOUTUBE_NETWORK".to_owned(),
        message: format!("YouTube 网络请求失败（已尝试 {attempts} 次）：{reason}"),
    })
}

async fn read_response_bytes_limited(mut response: Response, limit: usize) -> AppResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::External {
            code: "YOUTUBE_RESPONSE_SIZE".to_owned(),
            message: format!("YouTube 响应超过 {} KiB 限制", limit / 1024),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        let reason = if error.is_timeout() {
            "请求超时"
        } else if error.is_connect() {
            "连接失败"
        } else {
            "响应读取失败"
        };
        AppError::External {
            code: "YOUTUBE_NETWORK".to_owned(),
            message: format!("YouTube {reason}"),
        }
    })? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::External {
                code: "YOUTUBE_RESPONSE_SIZE".to_owned(),
                message: format!("YouTube 响应超过 {} KiB 限制", limit / 1024),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHANNEL_ID: &str = "UC1234567890123456789012";
    const VIDEO_ID: &str = "dQw4w9WgXcQ";

    #[test]
    fn account_input_recognizes_supported_forms() {
        assert_eq!(
            parse_youtube_account_input(CHANNEL_ID).expect("channel id"),
            YouTubeAccountInput::ChannelId(CHANNEL_ID.to_owned())
        );
        assert_eq!(
            parse_youtube_account_input("@Example").expect("handle"),
            YouTubeAccountInput::Handle("@Example".to_owned())
        );
        assert_eq!(
            parse_youtube_account_input("https://www.youtube.com/@Example").expect("handle url"),
            YouTubeAccountInput::Handle("@Example".to_owned())
        );
        assert_eq!(
            parse_youtube_account_input(&format!(
                "https://www.youtube.com/channel/{CHANNEL_ID}/videos"
            ))
            .expect("channel url"),
            YouTubeAccountInput::ChannelId(CHANNEL_ID.to_owned())
        );
        assert_eq!(
            parse_youtube_account_input(&format!("https://youtu.be/{VIDEO_ID}?t=30"))
                .expect("short url"),
            YouTubeAccountInput::VideoId(VIDEO_ID.to_owned())
        );
        assert_eq!(
            parse_youtube_account_input(&format!(
                "https://www.youtube.com/live/{VIDEO_ID}?feature=share"
            ))
            .expect("live url"),
            YouTubeAccountInput::VideoId(VIDEO_ID.to_owned())
        );
        assert_eq!(
            parse_youtube_account_input("https://www.youtube.com/user/legacy_name")
                .expect("legacy url"),
            YouTubeAccountInput::LegacyUsername("legacy_name".to_owned())
        );
        assert_eq!(
            parse_youtube_account_input("some search words").expect("search"),
            YouTubeAccountInput::Search("some search words".to_owned())
        );
        assert_eq!(
            parse_youtube_account_input("https://youtube.com.evil.test/@Example")
                .expect("non-youtube url falls back to search"),
            YouTubeAccountInput::Search("https://youtube.com.evil.test/@Example".to_owned())
        );
        assert!(parse_youtube_account_input("@").is_err());
    }

    #[test]
    fn channel_response_maps_metadata_and_counts() {
        let value = json!({
            "items": [{
                "id": CHANNEL_ID,
                "snippet": {
                    "title": "Example Channel",
                    "description": "channel description",
                    "customUrl": "@example",
                    "thumbnails": {
                        "default": {"url": "https://yt3.ggpht.com/default.jpg"},
                        "high": {"url": "https://yt3.ggpht.com/high.jpg"}
                    }
                },
                "contentDetails": {
                    "relatedPlaylists": {"uploads": "UU123"}
                },
                "statistics": {
                    "subscriberCount": "12345",
                    "videoCount": "67",
                    "viewCount": "890123"
                }
            }]
        });
        let channels = parse_channel_items(&value);
        assert_eq!(channels.len(), 1);
        let channel = &channels[0];
        assert_eq!(channel.id, CHANNEL_ID);
        assert_eq!(channel.handle.as_deref(), Some("@example"));
        assert_eq!(channel.name, "Example Channel");
        assert_eq!(
            channel.avatar_url.as_deref(),
            Some("https://yt3.ggpht.com/high.jpg")
        );
        assert_eq!(channel.uploads_playlist_id.as_deref(), Some("UU123"));
        assert_eq!(channel.subscriber_count, Some(12_345));
        assert_eq!(channel.video_count, Some(67));
        assert_eq!(channel.view_count, Some(890_123));
        assert_eq!(channel.profile_url, "https://www.youtube.com/@example");
    }

    #[test]
    fn playlist_and_video_mapping_preserve_live_schedule() {
        let playlist = json!({
            "items": [{
                "contentDetails": {
                    "videoId": VIDEO_ID,
                    "videoPublishedAt": "2026-11-12T10:00:00Z"
                },
                "snippet": {
                    "title": "Upcoming stream",
                    "thumbnails": {"high": {"url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hq.jpg"}}
                }
            }]
        });
        let items = parse_playlist_items(&playlist);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].video_id, VIDEO_ID);
        assert_eq!(
            items[0].published_at.as_ref().map(|value| value.to_rfc3339()),
            Some("2026-11-12T10:00:00+00:00".to_owned())
        );

        let videos = json!({
            "items": [{
                "id": VIDEO_ID,
                "snippet": {
                    "channelId": CHANNEL_ID,
                    "channelTitle": "Example Channel",
                    "title": "Upcoming stream",
                    "description": "schedule",
                    "publishedAt": "2026-11-12T10:00:00Z",
                    "liveBroadcastContent": "upcoming",
                    "thumbnails": {"high": {"url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hq.jpg"}}
                },
                "contentDetails": {"duration": "PT0S"},
                "liveStreamingDetails": {
                    "scheduledStartTime": "2026-11-12T20:00:00Z"
                },
                "statistics": {"viewCount": "42", "likeCount": "7"}
            }]
        });
        let videos = parse_video_items(&videos);
        assert_eq!(videos.len(), 1);
        assert!(videos[0].is_upcoming());
        assert_eq!(
            videos[0].live_url().as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(
            videos[0]
                .scheduled_start_at
                .as_ref()
                .map(|value| value.to_rfc3339()),
            Some("2026-11-12T20:00:00+00:00".to_owned())
        );
        assert_eq!(videos[0].view_count, Some(42));
        assert_eq!(videos[0].like_count, Some(7));
    }

    #[test]
    fn live_page_probe_maps_live_metadata() {
        let player = json!({
            "videoDetails": {
                "videoId": VIDEO_ID,
                "title": "Live now",
                "isLive": true,
                "thumbnail": {
                    "thumbnails": [{"url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/live.jpg"}]
                }
            },
            "microformat": {
                "playerMicroformatRenderer": {
                    "liveBroadcastDetails": {
                        "isLiveNow": true,
                        "startTimestamp": "2026-11-12T11:00:00Z"
                    }
                }
            }
        });
        let initial = json!({
            "contents": {
                "concurrentViewers": "4321",
                "scheduledStartTime": "2026-11-12T10:30:00Z"
            }
        });
        let html = format!(
            "<script>var ytInitialPlayerResponse = {player}; var ytInitialData = {initial};</script>"
        );
        let checked_at = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let url = Url::parse(&format!(
            "https://www.youtube.com/channel/{CHANNEL_ID}/live"
        ))
        .expect("url");
        let probe = parse_live_page(&html, &url, checked_at).expect("probe");
        assert!(probe.is_live);
        assert_eq!(probe.video_id.as_deref(), Some(VIDEO_ID));
        assert_eq!(probe.title, "Live now");
        assert_eq!(
            probe.cover_url.as_deref(),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/live.jpg")
        );
        assert_eq!(
            probe.live_url.as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(
            probe.started_at.as_ref().map(|value| value.to_rfc3339()),
            Some("2026-11-12T11:00:00+00:00".to_owned())
        );
        assert_eq!(
            probe.scheduled_start_at.as_ref().map(|value| value.to_rfc3339()),
            Some("2026-11-12T10:30:00+00:00".to_owned())
        );
        assert_eq!(probe.concurrent_viewers, Some(4321));
    }

    #[test]
    fn live_page_rejects_non_youtube_redirect() {
        let checked_at = Utc::now();
        let url = Url::parse("https://example.com/redirected/live").expect("url");
        let error = parse_live_page(
            "<script>var ytInitialPlayerResponse = {};</script>",
            &url,
            checked_at,
        )
        .expect_err("non-YouTube redirect must fail closed");
        assert!(matches!(
            error,
            AppError::External { ref code, .. } if code == "YOUTUBE_LIVE_FORMAT"
        ));
    }

    #[test]
    fn live_page_with_video_id_but_no_live_state_returns_format_error() {
        let checked_at = Utc::now();
        let player = json!({
            "videoDetails": {
                "videoId": VIDEO_ID,
                "title": "Live state missing"
            }
        });
        let initial = json!({
            "metadata": {
                "channelMetadataRenderer": {
                    "title": "Example Channel",
                    "externalId": CHANNEL_ID
                }
            },
            "recommended": {
                "videoRenderer": {
                    "videoId": "RecLive0001",
                    "isLiveNow": true
                }
            }
        });
        let html = format!(
            "<script>var ytInitialPlayerResponse = {player}; var ytInitialData = {initial};</script>"
        );
        let url = Url::parse(&format!(
            "https://www.youtube.com/channel/{CHANNEL_ID}/live"
        ))
        .expect("url");
        let error = parse_live_page(&html, &url, checked_at)
            .expect_err("recommendation live state without a target player state must remain unknown");
        assert!(matches!(
            error,
            AppError::External { ref code, .. } if code == "YOUTUBE_LIVE_FORMAT"
        ));
    }

    #[test]
    fn live_page_with_explicit_not_live_state_is_offline() {
        let checked_at = Utc::now();
        let player = json!({
            "videoDetails": {
                "videoId": VIDEO_ID,
                "title": "Ended stream",
                "isLive": false
            }
        });
        let initial = json!({
            "recommended": {
                "videoRenderer": {
                    "videoId": "RecLive0001",
                    "isLiveNow": true
                }
            }
        });
        let html = format!(
            "<script>var ytInitialPlayerResponse = {player}; var ytInitialData = {initial};</script>"
        );
        let url = Url::parse(&format!(
            "https://www.youtube.com/channel/{CHANNEL_ID}/live"
        ))
        .expect("url");
        let probe = parse_live_page(&html, &url, checked_at).expect("offline probe");
        assert!(!probe.is_live);
        assert!(probe.video_id.is_none());
    }

    #[test]
    fn channel_page_without_initial_data_returns_format_error() {
        let checked_at = Utc::now();
        let url = Url::parse(&format!("https://www.youtube.com/channel/{CHANNEL_ID}"))
            .expect("url");
        let error = parse_live_page("<html>consent or login page</html>", &url, checked_at)
            .expect_err("unrecognized page must not be treated as offline");
        assert!(matches!(
            error,
            AppError::External { ref code, .. } if code == "YOUTUBE_LIVE_FORMAT"
        ));
    }

    #[test]
    fn recognized_channel_page_without_live_is_offline() {
        let checked_at = Utc::now();
        let url = Url::parse(&format!("https://www.youtube.com/channel/{CHANNEL_ID}"))
            .expect("url");
        let initial = json!({
            "metadata": {
                "channelMetadataRenderer": {
                    "title": "Example Channel",
                    "externalId": CHANNEL_ID
                }
            }
        });
        let html = format!("<script>var ytInitialData = {initial};</script>");
        let probe = parse_live_page(&html, &url, checked_at).expect("offline probe");
        assert!(!probe.is_live);
        assert!(probe.video_id.is_none());
    }

    #[test]
    fn parsed_page_without_explicit_live_state_returns_format_error() {
        let checked_at = Utc::now();
        let url = Url::parse(&format!("https://www.youtube.com/channel/{CHANNEL_ID}"))
            .expect("url");
        let initial = json!({"alerts": [{"text": "Before you continue"}]});
        let html = format!("<script>var ytInitialData = {initial};</script>");
        let error = parse_live_page(&html, &url, checked_at)
            .expect_err("unrelated initial data must not be treated as offline");
        assert!(matches!(
            error,
            AppError::External { ref code, .. } if code == "YOUTUBE_LIVE_FORMAT"
        ));
    }

    #[test]
    fn community_page_parses_text_images_and_membership_posts() {
        let initial = json!({
            "contents": {
                "gridRenderer": {
                    "items": [{
                        "backstagePostRenderer": {
                            "postId": "Ugk_post_1",
                            "authorText": {"runs": [{"text": "Example Channel"}]},
                            "contentText": {"runs": [{"text": "周五 20:00 "}, {"text": "直播"}]},
                            "publishedTimeText": {"runs": [{"text": "2026-11-12T09:00:00Z"}]},
                            "backstageAttachment": {
                                "backstageImageRenderer": {
                                    "image": {
                                        "thumbnails": [
                                            {"url": "https://yt3.ggpht.com/small.jpg"},
                                            {"url": "https://yt3.ggpht.com/large.jpg"}
                                        ]
                                    }
                                }
                            },
                            "sponsorsOnlyBadge": {
                                "metadataBadgeRenderer": {"label": "Members only"}
                            }
                        }
                    }]
                }
            }
        });
        let html = format!("<html><script>var ytInitialData = {initial};</script></html>");
        let observed_at = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let page =
            parse_community_page(&html, CHANNEL_ID, observed_at).expect("community page");
        assert!(page.page_found);
        assert!(page.membership_restricted);
        assert_eq!(page.posts.len(), 1);
        let post = &page.posts[0];
        assert_eq!(post.id, "Ugk_post_1");
        assert_eq!(post.text, "周五 20:00 直播");
        assert_eq!(post.images, vec!["https://yt3.ggpht.com/large.jpg".to_owned()]);
        assert!(post.membership_only);
        assert_eq!(post.source_url, "https://www.youtube.com/post/Ugk_post_1");
        assert_eq!(
            post.published_at.as_ref().map(|value| value.to_rfc3339()),
            Some("2026-11-12T09:00:00+00:00".to_owned())
        );
        assert!(!post.published_at_estimated);
    }

    #[test]
    fn community_page_drops_posts_from_another_channel() {
        let initial = json!({
            "contents": {
                "gridRenderer": {
                    "items": [{
                        "backstagePostRenderer": {
                            "postId": "Ugk_other_channel",
                            "authorText": {"simpleText": "Other Channel"},
                            "authorEndpoint": {
                                "browseEndpoint": {
                                    "browseId": "UC9999999999999999999999"
                                }
                            },
                            "contentText": {"simpleText": "周五 20:00 直播"},
                            "publishedTimeText": {"simpleText": "2026-11-12"}
                        }
                    }]
                }
            }
        });
        let html = format!("<html><script>var ytInitialData = {initial};</script></html>");
        let page = parse_community_page(&html, CHANNEL_ID, Utc::now()).expect("community page");
        assert!(page.posts.is_empty());
    }

    #[test]
    fn community_relative_and_unparseable_publish_times_are_explicit() {
        let observed_at = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let initial = json!({
            "contents": {
                "items": [
                    {
                        "backstagePostRenderer": {
                            "postId": "relative-en",
                            "contentText": {"simpleText": "English relative"},
                            "publishedTimeText": {"simpleText": "2 days ago"}
                        }
                    },
                    {
                        "backstagePostRenderer": {
                            "postId": "relative-zh",
                            "contentText": {"simpleText": "Chinese relative"},
                            "publishedTimeText": {"simpleText": "2天前"}
                        }
                    },
                    {
                        "backstagePostRenderer": {
                            "postId": "relative-yesterday",
                            "contentText": {"simpleText": "Yesterday"},
                            "publishedTimeText": {"simpleText": "昨天"}
                        }
                    },
                    {
                        "backstagePostRenderer": {
                            "postId": "unparsed",
                            "contentText": {"simpleText": "Unknown time"},
                            "publishedTimeText": {"simpleText": "recently-ish"}
                        }
                    }
                ]
            }
        });
        let html = format!("<script>var ytInitialData = {initial};</script>");
        let page =
            parse_community_page(&html, CHANNEL_ID, observed_at).expect("community page");
        let post = |id: &str| page.posts.iter().find(|post| post.id == id).expect(id);

        let english = post("relative-en");
        assert_eq!(
            english.published_at.as_ref().map(|value| value.to_rfc3339()),
            Some((observed_at - chrono::Duration::days(2)).to_rfc3339())
        );
        assert!(english.published_at_estimated);
        assert_eq!(english.published_text.as_deref(), Some("2 days ago"));

        let chinese = post("relative-zh");
        assert_eq!(
            chinese.published_at.as_ref().map(|value| value.to_rfc3339()),
            Some((observed_at - chrono::Duration::days(2)).to_rfc3339())
        );
        assert!(chinese.published_at_estimated);

        let yesterday = post("relative-yesterday");
        assert_eq!(
            yesterday.published_at.as_ref().map(|value| value.to_rfc3339()),
            Some((observed_at - chrono::Duration::days(1)).to_rfc3339())
        );
        assert!(yesterday.published_at_estimated);

        let unparsed = post("unparsed");
        assert!(unparsed.published_at.is_none());
        assert!(!unparsed.published_at_estimated);
        assert_eq!(unparsed.published_text.as_deref(), Some("recently-ish"));
    }

    #[test]
    fn community_public_post_is_not_tainted_by_text_or_author_thumbnail() {
        let observed_at = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let initial = json!({
            "contents": {
                "items": [{
                    "backstagePostRenderer": {
                        "postId": "public-post",
                        "authorText": {"simpleText": "Example Channel"},
                        "authorThumbnail": {
                            "thumbnails": [
                                {"url": "https://yt3.ggpht.com/avatar.jpg"}
                            ]
                        },
                        "contentText": {"simpleText": "Members only sale starts tomorrow"},
                        "publishedTimeText": {"simpleText": "3 hours ago"}
                    }
                }]
            }
        });
        let html = format!("<script>var ytInitialData = {initial};</script>");
        let page =
            parse_community_page(&html, CHANNEL_ID, observed_at).expect("community page");
        assert_eq!(page.posts.len(), 1);
        assert!(!page.membership_restricted);
        assert!(!page.posts[0].membership_only);
        assert!(page.posts[0].images.is_empty());
    }

    #[test]
    fn super_chat_pages_preserve_continuation_and_deduplicate_event_ids() {
        let first = parse_super_chat_page(&json!({
            "nextPageToken": "page-2",
            "items": [{
                "id": "sc-1",
                "snippet": {
                    "liveChatId": "chat-id",
                    "createdAt": "2026-11-12T12:00:01Z",
                    "amountMicros": "1000000",
                    "currency": "USD"
                }
            }]
        }));
        let second = parse_super_chat_page(&json!({
            "items": [
                {
                    "id": "sc-1",
                    "snippet": {
                        "liveChatId": "chat-id",
                        "createdAt": "2026-11-12T12:00:01Z",
                        "amountMicros": "1000000",
                        "currency": "USD"
                    }
                },
                {
                    "id": "sc-2",
                    "snippet": {
                        "liveChatId": "chat-id",
                        "createdAt": "2026-11-12T12:00:02Z",
                        "amountMicros": "2000000",
                        "currency": "JPY"
                    }
                }
            ]
        }));
        assert_eq!(first.next_page_token.as_deref(), Some("page-2"));
        assert!(second.next_page_token.is_none());

        let mut events = Vec::new();
        let mut seen_event_ids = BTreeSet::new();
        append_unique_super_chat_events(&mut events, &mut seen_event_ids, first.events);
        append_unique_super_chat_events(&mut events, &mut seen_event_ids, second.events);
        assert_eq!(
            events.iter().map(|event| event.id.as_str()).collect::<Vec<_>>(),
            vec!["sc-1", "sc-2"]
        );
    }

    #[test]
    fn locked_membership_renderer_with_placeholder_is_skipped() {
        let initial = json!({
            "contents": {
                "items": [{
                    "backstagePostRenderer": {
                        "postId": "locked-members-post",
                        "contentText": {"simpleText": "Members only"},
                        "publishedTimeText": {"simpleText": "2 days ago"},
                        "sponsorsOnlyBadge": {
                            "metadataBadgeRenderer": {"label": "Members only"}
                        }
                    }
                }]
            }
        });
        let html = format!("<script>var ytInitialData = {initial};</script>");
        let page = parse_community_page(&html, CHANNEL_ID, Utc::now()).expect("community page");
        assert!(page.membership_restricted);
        assert_eq!(
            page.skipped_reason.as_deref(),
            Some("membership_session_unavailable")
        );
        assert!(page.posts.is_empty());
    }

    #[test]
    fn membership_page_without_initial_data_is_skipped() {
        let page = parse_community_page(
            "<html><body>Members only. Sign in with a channel membership.</body></html>",
            CHANNEL_ID,
            Utc::now(),
        )
        .expect("membership skip");
        assert!(!page.page_found);
        assert!(page.membership_restricted);
        assert_eq!(
            page.skipped_reason.as_deref(),
            Some("membership_session_unavailable")
        );
        assert!(page.posts.is_empty());
    }
    #[test]
    fn initial_data_extraction_handles_braces_inside_strings() {
        let initial = json!({
            "contents": {
                "note": "literal } = { in text",
                "nested": {"ok": true}
            }
        });
        let html = format!("<script>window[\"ytInitialData\"] = {initial};</script>");
        let parsed = extract_json_assignment(&html, "ytInitialData").expect("initial data");
        assert_eq!(
            parsed.pointer("/contents/note").and_then(Value::as_str),
            Some("literal } = { in text")
        );
        assert_eq!(
            parsed.pointer("/contents/nested/ok").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn live_chat_maps_text_super_chat_and_membership_events() {
        let value = json!({
            "nextPageToken": "next-token",
            "pollingIntervalMillis": "1500",
            "activeLiveChatId": "chat-id",
            "items": [
                {
                    "id": "msg-1",
                    "snippet": {
                        "type": "textMessageEvent",
                        "liveChatId": "chat-id",
                        "publishedAt": "2026-11-12T12:00:01Z",
                        "displayMessage": "hello",
                        "authorChannelId": {"value": CHANNEL_ID}
                    },
                    "authorDetails": {
                        "channelId": CHANNEL_ID,
                        "displayName": "Viewer"
                    }
                },
                {
                    "id": "msg-2",
                    "snippet": {
                        "type": "superChatEvent",
                        "liveChatId": "chat-id",
                        "publishedAt": "2026-11-12T12:00:02Z",
                        "displayMessage": "Super Chat",
                        "superChatDetails": {
                            "amountMicros": "5000000",
                            "currency": "USD",
                            "amountDisplayString": "$5.00",
                            "tier": "2",
                            "userComment": "good stream"
                        }
                    }
                },
                {
                    "id": "msg-3",
                    "snippet": {
                        "type": "newSponsorEvent",
                        "liveChatId": "chat-id",
                        "publishedAt": "2026-11-12T12:00:03Z",
                        "newSponsorDetails": {"memberLevelName": "Gold"}
                    }
                }
            ]
        });
        let page = parse_live_chat_page(&value);
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.next_page_token.as_deref(), Some("next-token"));
        assert_eq!(page.polling_interval_millis, Some(1500));
        assert_eq!(page.active_live_chat_id.as_deref(), Some("chat-id"));
        assert_eq!(page.messages[0].display_message.as_deref(), Some("hello"));
        let monetary = page.messages[1].monetary_event.as_ref().expect("super chat");
        assert_eq!(monetary.kind, "superChatEvent");
        assert_eq!(monetary.amount_micros, Some(5_000_000));
        assert_eq!(monetary.currency.as_deref(), Some("USD"));
        assert_eq!(monetary.comment.as_deref(), Some("good stream"));
        let membership = page.messages[2]
            .membership_event
            .as_ref()
            .expect("membership");
        assert_eq!(membership.kind, "newSponsorEvent");
        assert_eq!(membership.member_level_name.as_deref(), Some("Gold"));
    }

    #[test]
    fn member_chat_is_not_a_membership_event() {
        let value = json!({
            "items": [{
                "id": "member-chat-1",
                "snippet": {
                    "type": "memberChatEvent",
                    "liveChatId": "chat-id",
                    "publishedAt": "2026-11-12T12:00:04Z",
                    "displayMessage": "members-only chat"
                }
            }]
        });
        let page = parse_live_chat_page(&value);
        assert_eq!(page.messages.len(), 1);
        assert!(page.messages[0].membership_event.is_none());
    }

    #[test]
    fn super_chat_events_map_amount_currency_and_user() {
        let value = json!({
            "items": [{
                "id": "super-chat-1",
                "snippet": {
                    "liveChatId": "chat-id",
                    "createdAt": "2026-11-12T12:00:02Z",
                    "amountMicros": "1000000",
                    "currency": "JPY",
                    "amountDisplayString": "¥1,000",
                    "commentText": "support",
                    "tier": "1",
                    "userDetails": {
                        "channelId": CHANNEL_ID,
                        "displayName": "Supporter"
                    }
                }
            }]
        });
        let events = parse_super_chat_events(&value);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "superChatEvent");
        assert_eq!(events[0].amount_micros, Some(1_000_000));
        assert_eq!(events[0].currency.as_deref(), Some("JPY"));
        assert_eq!(events[0].comment.as_deref(), Some("support"));
        assert_eq!(events[0].user_display_name.as_deref(), Some("Supporter"));
    }

    #[test]
    fn super_chat_events_map_super_sticker_details_and_kind() {
        let value = json!({
            "items": [{
                "id": "super-sticker-1",
                "snippet": {
                    "liveChatId": "chat-id",
                    "createdAt": "2026-11-12T12:00:03Z",
                    "superStickerDetails": {
                        "amountMicros": "2500000",
                        "currency": "USD",
                        "amountDisplayString": "$2.50",
                        "tier": "1"
                    },
                    "userDetails": {
                        "channelId": CHANNEL_ID,
                        "displayName": "Sticker Fan"
                    }
                }
            }]
        });
        let events = parse_super_chat_events(&value);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "superStickerEvent");
        assert_eq!(events[0].amount_micros, Some(2_500_000));
        assert_eq!(events[0].currency.as_deref(), Some("USD"));
        assert_eq!(
            events[0].amount_display_string.as_deref(),
            Some("$2.50")
        );
        assert_eq!(events[0].user_display_name.as_deref(), Some("Sticker Fan"));
    }

    #[test]
    fn youtube_api_list_shape_fails_closed() {
        assert!(validate_api_list_response(&json!({})).is_ok());
        assert!(validate_api_list_response(&json!({"items": []})).is_ok());
        assert!(validate_api_list_response(&json!({"items": {}})).is_err());
        assert!(validate_api_list_response(&json!({"nextPageToken": 1})).is_err());
        assert!(validate_api_list_response(&json!([])).is_err());
    }

    #[test]
    fn youtube_quota_error_has_stable_code() {
        let body = br#"{"error":{"message":"quota exceeded","errors":[{"reason":"quotaExceeded"}]}}"#;
        let error = youtube_http_error(StatusCode::FORBIDDEN, body);
        assert!(matches!(
            error,
            AppError::External {
                ref code,
                ..
            } if code == "YOUTUBE_QUOTA_EXCEEDED"
        ));
    }

    #[test]
    fn community_page_url_accepts_channel_id_handle_and_url() {
        assert_eq!(
            community_page_url(CHANNEL_ID).expect("channel"),
            format!("https://www.youtube.com/channel/{CHANNEL_ID}/community")
        );
        assert_eq!(
            community_page_url("@Example").expect("handle"),
            "https://www.youtube.com/@Example/community"
        );
        assert_eq!(
            community_page_url("https://www.youtube.com/@Example").expect("url"),
            "https://www.youtube.com/@Example/community"
        );
        assert!(community_page_url("https://example.com/@Example").is_err());
    }
}
