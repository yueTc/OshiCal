use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Timelike, Utc};
use oshical_core::{AppError, AppResult};
use reqwest::redirect::Policy;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;
use tokio::sync::Mutex;

const SERVER_RELEASE_TAG: &str = "server-latest";
const SERVER_MANIFEST_CHANNEL: &str = "server";
const DIRECT_MANIFEST_URL: &str =
    "https://github.com/yueTc/OshiCal/releases/download/server-latest/latest-server.json";
const CHECK_INTERVAL_MINUTES: i64 = 30;
const FAILED_RETRY_MINUTES: i64 = 30;
const AGENT_ONLINE_SECONDS: u64 = 90;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerUpdateSettings {
    pub auto_check_enabled: bool,
    pub auto_update_enabled: bool,
    pub update_window_start: String,
    pub update_window_end: String,
    pub github_mirror_url: String,
}

impl Default for ServerUpdateSettings {
    fn default() -> Self {
        Self {
            auto_check_enabled: true,
            auto_update_enabled: false,
            update_window_start: "03:00".to_owned(),
            update_window_end: "05:00".to_owned(),
            github_mirror_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ReleaseInfo {
    channel: String,
    version: String,
    image_digest: Option<String>,
    signature: Option<String>,
    checked_at: Option<DateTime<Utc>>,
    release_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct UpdateStatus {
    status: String,
    phase: String,
    message: String,
    target_version: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    error: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            status: "idle".to_owned(),
            phase: "idle".to_owned(),
            message: "尚未检查服务器更新".to_owned(),
            target_version: None,
            updated_at: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerUpdateView {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub status: String,
    pub phase: String,
    pub message: String,
    pub auto_check_enabled: bool,
    pub auto_update_enabled: bool,
    pub update_window_start: String,
    pub update_window_end: String,
    pub github_mirror_url: String,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_updated_at: Option<DateTime<Utc>>,
    pub agent_online: bool,
    pub target_version: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ServerUpdateManager {
    dir: PathBuf,
    client: reqwest::Client,
    operation_lock: Arc<Mutex<()>>,
}

impl ServerUpdateManager {
    pub fn new(dir: PathBuf) -> AppResult<Self> {
        std::fs::create_dir_all(&dir)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("OshiCal-Server-Updater")
            .https_only(true)
            // GitHub release downloads redirect to GitHub-controlled object
            // storage. Never allow a redirect to another origin or to HTTP.
            .redirect(Policy::custom(|attempt| {
                let Some(host) = attempt.url().host_str() else {
                    return attempt.stop();
                };
                let allowed_host = host == "github.com"
                    || host == "www.github.com"
                    || host.ends_with(".githubusercontent.com");
                if attempt.previous().len() < 5 && attempt.url().scheme() == "https" && allowed_host
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            dir,
            client,
            operation_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn update_dir(&self) -> &Path {
        &self.dir
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("server-update-settings.json")
    }

    fn latest_path(&self) -> PathBuf {
        self.dir.join("server-update-latest.json")
    }

    fn status_path(&self) -> PathBuf {
        self.dir.join("server-update-status.json")
    }

    fn request_path(&self) -> PathBuf {
        self.dir.join("update-request.env")
    }

    fn heartbeat_path(&self) -> PathBuf {
        self.dir.join("updater-heartbeat.json")
    }

    pub async fn settings(&self) -> AppResult<ServerUpdateSettings> {
        let path = self.settings_path();
        if !fs::try_exists(&path).await? {
            return Ok(ServerUpdateSettings::default());
        }
        let raw = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub async fn save_settings(
        &self,
        settings: ServerUpdateSettings,
    ) -> AppResult<ServerUpdateView> {
        let _guard = self.operation_lock.lock().await;
        validate_hhmm(&settings.update_window_start)?;
        validate_hhmm(&settings.update_window_end)?;
        validate_mirror_url(&settings.github_mirror_url)?;
        let mut settings = settings;
        if settings.auto_update_enabled {
            settings.auto_check_enabled = true;
        }
        write_json(&self.settings_path(), &settings).await?;
        self.view().await
    }

    pub async fn view(&self) -> AppResult<ServerUpdateView> {
        let settings = self.settings().await?;
        let latest = self.load_latest().await?;
        let mut status = self.load_status().await?;
        let latest_version = Some(latest.version.clone()).filter(|value| {
            latest.channel == SERVER_MANIFEST_CHANNEL
                && !value.is_empty()
                && latest
                    .image_digest
                    .as_deref()
                    .is_some_and(|digest| validate_image_digest(digest).is_ok())
                && latest
                    .signature
                    .as_deref()
                    .is_some_and(is_valid_update_signature)
        });
        let update_available = latest_version
            .as_deref()
            .is_some_and(|value| is_newer_version(value, env!("CARGO_PKG_VERSION")));
        let request_pending = fs::try_exists(self.request_path()).await?;
        if request_pending && matches!(status.status.as_str(), "idle" | "succeeded" | "failed") {
            status.status = "requested".to_owned();
            status.phase = "queued".to_owned();
            status.message = "等待宿主机更新代理处理".to_owned();
        }
        let agent_online = self.agent_online().await;
        let message = effective_message(&status, &settings, update_available, agent_online);
        Ok(ServerUpdateView {
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            latest_version,
            update_available,
            status: status.status,
            phase: status.phase,
            message,
            auto_check_enabled: settings.auto_check_enabled,
            auto_update_enabled: settings.auto_update_enabled,
            update_window_start: settings.update_window_start,
            update_window_end: settings.update_window_end,
            github_mirror_url: settings.github_mirror_url,
            last_checked_at: latest.checked_at,
            last_updated_at: status.updated_at,
            agent_online,
            target_version: status.target_version,
            error: status.error,
        })
    }

    pub async fn check_now(&self) -> AppResult<ServerUpdateView> {
        let _guard = self.operation_lock.lock().await;
        let settings = self.settings().await?;
        self.check_release(&settings.github_mirror_url).await?;
        self.view().await
    }

    pub async fn install_now(&self) -> AppResult<ServerUpdateView> {
        let _guard = self.operation_lock.lock().await;
        let settings = self.settings().await?;
        let latest = self.check_release(&settings.github_mirror_url).await?;
        if !is_newer_version(&latest.version, env!("CARGO_PKG_VERSION")) {
            return Err(AppError::validation(
                "服务器已经是最新版本，拒绝重复安装或降级",
            ));
        }
        self.write_request(
            &latest.version,
            latest.image_digest.as_deref(),
            latest.signature.as_deref(),
            true,
            &settings,
        )
        .await?;
        self.view().await
    }

    pub async fn tick(&self) -> AppResult<()> {
        let _guard = self.operation_lock.lock().await;
        let settings = self.settings().await?;
        let latest = self.load_latest().await?;
        let stale = latest
            .checked_at
            .map(|checked_at| (Utc::now() - checked_at).num_minutes() >= CHECK_INTERVAL_MINUTES)
            .unwrap_or(true);
        if settings.auto_check_enabled && stale {
            self.check_release(&settings.github_mirror_url).await?;
        }
        if !settings.auto_update_enabled {
            return Ok(());
        }
        let latest = self.load_latest().await?;
        if latest.channel != SERVER_MANIFEST_CHANNEL
            || !is_newer_version(&latest.version, env!("CARGO_PKG_VERSION"))
            || latest
                .image_digest
                .as_deref()
                .is_none_or(|digest| validate_image_digest(digest).is_err())
            || latest
                .signature
                .as_deref()
                .is_none_or(|signature| !is_valid_update_signature(signature))
        {
            return Ok(());
        }
        let status = self.load_status().await?;
        if self.request_pending().await || is_active_status(&status.status) {
            return Ok(());
        }
        if status.status == "failed"
            && status.target_version.as_deref() == Some(latest.version.as_str())
            && status
                .updated_at
                .map(|updated_at| (Utc::now() - updated_at).num_minutes() < FAILED_RETRY_MINUTES)
                .unwrap_or(false)
        {
            return Ok(());
        }
        if !window_contains(
            &settings.update_window_start,
            &settings.update_window_end,
            now_hhmm(),
        ) {
            return Ok(());
        }
        self.write_request(
            &latest.version,
            latest.image_digest.as_deref(),
            latest.signature.as_deref(),
            false,
            &settings,
        )
        .await
    }

    async fn check_release(&self, github_mirror_url: &str) -> AppResult<ReleaseInfo> {
        let body = self.fetch_manifest(github_mirror_url).await?;
        let channel = body
            .get("channel")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::validation("服务器更新清单缺少 channel"))?;
        if channel != SERVER_MANIFEST_CHANNEL {
            return Err(AppError::validation(format!(
                "更新清单 channel 无效: {channel}，服务器只接受 {SERVER_MANIFEST_CHANNEL}"
            )));
        }
        let version = body
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::validation("更新清单缺少 version"))?
            .trim_start_matches('v');
        Version::parse(version)
            .map_err(|error| AppError::validation(format!("服务器版本号无效: {error}")))?;
        let image_digest = match body.get("imageDigest") {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                let value = value.trim();
                validate_image_digest(value)?;
                value.to_owned()
            }
            None | Some(Value::Null) | Some(Value::String(_)) => {
                return Err(AppError::validation(
                    "服务器更新清单缺少 imageDigest，拒绝使用可变版本标签更新",
                ));
            }
            Some(_) => {
                return Err(AppError::validation(
                    "服务器更新清单 imageDigest 必须是字符串",
                ));
            }
        };
        let signature = body
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::validation("服务器更新清单缺少 signature，拒绝未签名更新"))?
            .trim();
        // The HMAC key intentionally lives only on the host updater. This
        // process validates the shape and forwards the signature; the root
        // agent is the trust boundary that verifies it before pulling an image.
        validate_update_signature(signature)?;
        let release = ReleaseInfo {
            channel: SERVER_MANIFEST_CHANNEL.to_owned(),
            version: version.to_owned(),
            image_digest: Some(image_digest),
            signature: Some(signature.to_owned()),
            checked_at: Some(Utc::now()),
            release_url: Some(format!(
                "https://github.com/yueTc/OshiCal/releases/tag/{SERVER_RELEASE_TAG}"
            )),
        };
        write_json(&self.latest_path(), &release).await?;
        Ok(release)
    }

    async fn fetch_manifest(&self, github_mirror_url: &str) -> AppResult<Value> {
        // Revalidate persisted settings too: an older version may have stored a
        // plaintext HTTP mirror before this policy was enforced.
        validate_mirror_url(github_mirror_url)?;
        if let Some(mirror) = mirror_manifest_url(github_mirror_url) {
            match self.fetch_mirror_json(&mirror).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    tracing::warn!(%error, %mirror, "GitHub 镜像检查失败，回退直连");
                }
            }
        }
        self.fetch_json(&self.client, DIRECT_MANIFEST_URL).await
    }

    async fn fetch_mirror_json(&self, url: &str) -> AppResult<Value> {
        validate_mirror_url(url)?;
        let parsed = url::Url::parse(url)
            .map_err(|error| AppError::validation(format!("GitHub 镜像地址无效: {error}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| AppError::validation("GitHub 镜像地址缺少主机名"))?
            .to_owned();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let resolve_host = host.clone();
        let addresses = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                (resolve_host.as_str(), port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect::<Vec<_>>())
            }),
        )
        .await
        .map_err(|_| AppError::External {
            code: "UPDATE_MIRROR_DNS_TIMEOUT".to_owned(),
            message: "解析 GitHub 镜像地址超时".to_owned(),
        })?
        .map_err(|error| AppError::External {
            code: "UPDATE_MIRROR_DNS".to_owned(),
            message: format!("解析 GitHub 镜像地址失败: {error}"),
        })?
        .map_err(|error| AppError::External {
            code: "UPDATE_MIRROR_DNS".to_owned(),
            message: format!("解析 GitHub 镜像地址失败: {error}"),
        })?;
        if addresses.is_empty() {
            return Err(AppError::validation("GitHub 镜像地址没有可用 DNS 记录"));
        }
        if addresses
            .iter()
            .any(|address| is_non_public_ip(address.ip()))
        {
            return Err(AppError::validation(
                "GitHub 镜像地址解析到了回环、私网、链路本地或保留地址",
            ));
        }
        // Pin reqwest to the already validated address so a second DNS lookup
        // cannot rebind the mirror hostname to an internal service.
        let pinned = addresses
            .iter()
            .find(|address| address.is_ipv4())
            .or_else(|| addresses.first())
            .copied()
            .ok_or_else(|| AppError::validation("GitHub 镜像地址没有可用 IP 地址"))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("OshiCal-Server-Updater")
            .https_only(true)
            .redirect(Policy::none())
            .no_proxy()
            .resolve(&host, pinned)
            .build()?;
        self.fetch_json(&client, url).await
    }

    async fn fetch_json(&self, client: &reqwest::Client, url: &str) -> AppResult<Value> {
        let mut response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::External {
                code: format!("SERVER_RELEASE_{status}"),
                message: format!("读取更新清单失败: HTTP {status}"),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
        {
            return Err(AppError::validation("服务器更新清单超过大小限制"));
        }
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(MAX_MANIFEST_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
                return Err(AppError::validation("服务器更新清单超过大小限制"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn write_request(
        &self,
        target_version: &str,
        image_digest: Option<&str>,
        signature: Option<&str>,
        force: bool,
        settings: &ServerUpdateSettings,
    ) -> AppResult<()> {
        validate_hhmm(&settings.update_window_start)?;
        validate_hhmm(&settings.update_window_end)?;
        Version::parse(target_version)
            .map_err(|error| AppError::validation(format!("目标版本号无效: {error}")))?;
        if !is_newer_version(target_version, env!("CARGO_PKG_VERSION")) {
            return Err(AppError::validation("目标版本必须严格高于当前服务器版本"));
        }
        let image_digest =
            image_digest.ok_or_else(|| AppError::validation("更新请求缺少不可变镜像摘要"))?;
        validate_image_digest(image_digest)?;
        let signature = signature.ok_or_else(|| AppError::validation("更新请求缺少发布者签名"))?;
        validate_update_signature(signature)?;
        let target_digest_line = format!("target_digest={image_digest}\n");
        let signature_line = format!("signature={signature}\n");
        let request_id = uuid::Uuid::new_v4().to_string();
        let content = format!(
            "request_id={request_id}
target_version={target_version}
{target_digest_line}{signature_line}force={}
window_start={}
window_end={}
requested_at={}
",
            if force { "true" } else { "false" },
            settings.update_window_start,
            settings.update_window_end,
            Utc::now().to_rfc3339(),
        );
        write_text(&self.request_path(), &content).await?;
        let status = UpdateStatus {
            status: "requested".to_owned(),
            phase: "queued".to_owned(),
            message: if force {
                "已提交立即更新请求".to_owned()
            } else {
                "已提交自动更新请求".to_owned()
            },
            target_version: Some(target_version.to_owned()),
            updated_at: Some(Utc::now()),
            error: None,
        };
        write_json(&self.status_path(), &status).await
    }

    async fn load_latest(&self) -> AppResult<ReleaseInfo> {
        let path = self.latest_path();
        if !fs::try_exists(&path).await? {
            return Ok(ReleaseInfo::default());
        }
        let raw = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    async fn load_status(&self) -> AppResult<UpdateStatus> {
        let path = self.status_path();
        if !fs::try_exists(&path).await? {
            return Ok(UpdateStatus::default());
        }
        let raw = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    async fn request_pending(&self) -> bool {
        fs::try_exists(self.request_path()).await.unwrap_or(false)
    }

    async fn agent_online(&self) -> bool {
        let path = self.heartbeat_path();
        let Ok(metadata) = fs::metadata(path).await else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        modified
            .elapsed()
            .map(|elapsed| elapsed.as_secs() <= AGENT_ONLINE_SECONDS)
            .unwrap_or(false)
    }
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    let data = serde_json::to_vec_pretty(value)?;
    write_bytes(path, &data).await
}

async fn write_text(path: &Path, value: &str) -> AppResult<()> {
    write_bytes(path, value.as_bytes()).await
}

async fn write_bytes(path: &Path, value: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&temporary, value).await?;
    // The temporary file is in the same directory, so rename is an atomic
    // replacement on the Linux server host and readers never observe a gap.
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

fn mirror_manifest_url(github_mirror_url: &str) -> Option<String> {
    let base = github_mirror_url.trim();
    if base.is_empty() {
        return None;
    }
    let base = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    Some(format!("{base}{DIRECT_MANIFEST_URL}"))
}

fn validate_mirror_url(value: &str) -> AppResult<()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let url = url::Url::parse(value)
        .map_err(|error| AppError::validation(format!("GitHub 镜像地址无效: {error}")))?;
    if url.scheme() != "https" {
        return Err(AppError::validation(
            "GitHub 镜像地址必须使用 HTTPS，禁止明文 HTTP",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::validation("GitHub 镜像地址不能包含用户名或密码"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AppError::validation(
            "GitHub 镜像地址不能包含查询参数或片段",
        ));
    }
    if url.port_or_known_default() != Some(443) {
        return Err(AppError::validation(
            "GitHub 镜像地址必须使用 HTTPS 默认端口 443",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| AppError::validation("GitHub 镜像地址缺少主机名"))?;
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized_host == "localhost"
        || normalized_host.ends_with(".localhost")
        || normalized_host.ends_with(".local")
        || normalized_host.ends_with(".internal")
    {
        return Err(AppError::validation(
            "GitHub 镜像地址不能指向本机或内部域名",
        ));
    }
    if let Ok(ip) = normalized_host.parse::<std::net::IpAddr>() {
        if is_non_public_ip(ip) {
            return Err(AppError::validation(
                "GitHub 镜像地址不能指向回环、私网、链路本地或保留地址",
            ));
        }
    }
    Ok(())
}

fn is_non_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.octets()[0] >= 240
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || segments[0] == 0x2002
                || (segments[0] == 0x2001 && matches!(segments[1], 0x0000 | 0x0db8))
                || segments[..5].iter().all(|segment| *segment == 0)
        }
    }
}

fn validate_hhmm(value: &str) -> AppResult<()> {
    parse_hhmm(value)
        .map(|_| ())
        .ok_or_else(|| AppError::validation(format!("更新时间格式无效: {value}")))
}

fn parse_hhmm(value: &str) -> Option<u32> {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let hour = parse_decimal_pair(bytes[0], bytes[1])?;
    let minute = parse_decimal_pair(bytes[3], bytes[4])?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

fn parse_decimal_pair(tens: u8, ones: u8) -> Option<u32> {
    let tens = tens.checked_sub(b'0').filter(|value| *value <= 9)?;
    let ones = ones.checked_sub(b'0').filter(|value| *value <= 9)?;
    Some(u32::from(tens) * 10 + u32::from(ones))
}

fn validate_image_digest(value: &str) -> AppResult<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(AppError::validation(
            "服务器镜像摘要必须使用 sha256:<64hex> 格式",
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(AppError::validation(
            "服务器镜像摘要必须使用 sha256:<64hex> 格式",
        ));
    }
    Ok(())
}

fn is_valid_update_signature(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_update_signature(value: &str) -> AppResult<()> {
    if is_valid_update_signature(value) {
        Ok(())
    } else {
        Err(AppError::validation(
            "服务器更新清单 signature 必须是 64 位小写十六进制 HMAC-SHA256",
        ))
    }
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

fn now_hhmm() -> u32 {
    let offset = FixedOffset::east_opt(8 * 60 * 60).expect("valid Asia/Shanghai offset");
    let now = Utc::now().with_timezone(&offset);
    now.hour() * 60 + now.minute()
}

fn window_contains(start: &str, end: &str, current: u32) -> bool {
    let Some(start) = parse_hhmm(start) else {
        return false;
    };
    let Some(end) = parse_hhmm(end) else {
        return false;
    };
    if start == end {
        return true;
    }
    if start < end {
        current >= start && current < end
    } else {
        current >= start || current < end
    }
}

fn is_active_status(status: &str) -> bool {
    matches!(status, "requested" | "waiting_window" | "running")
}

fn effective_message(
    status: &UpdateStatus,
    settings: &ServerUpdateSettings,
    update_available: bool,
    agent_online: bool,
) -> String {
    if !agent_online && is_active_status(&status.status) {
        return "等待宿主机更新代理上线".to_owned();
    }
    if settings.auto_update_enabled
        && update_available
        && !is_active_status(&status.status)
        && !window_contains(
            &settings.update_window_start,
            &settings.update_window_end,
            now_hhmm(),
        )
    {
        return format!(
            "新版本将在 {} - {} 自动安装",
            settings.update_window_start, settings.update_window_end
        );
    }
    status.message.clone()
}

#[cfg(test)]
mod tests {
    use super::{
        is_newer_version, mirror_manifest_url, parse_hhmm, validate_image_digest,
        validate_mirror_url, validate_update_signature, window_contains, ServerUpdateManager,
        ServerUpdateSettings,
    };

    #[test]
    fn mirror_url_is_normalized_and_validated() {
        assert_eq!(mirror_manifest_url(""), None);
        assert_eq!(
            mirror_manifest_url("https://gh-proxy.com"),
            Some(
                "https://gh-proxy.com/https://github.com/yueTc/OshiCal/releases/download/server-latest/latest-server.json"
                    .to_owned()
            )
        );
        assert!(validate_mirror_url("https://gh-proxy.com/").is_ok());
        assert!(validate_mirror_url("http://gh-proxy.com/").is_err());
        assert!(validate_mirror_url("https://127.0.0.1/").is_err());
        assert!(validate_mirror_url("https://localhost/").is_err());
        assert!(validate_mirror_url("https://gh-proxy.com/?next=example").is_err());
        assert!(validate_mirror_url("ftp://example.com/").is_err());
    }

    #[test]
    fn update_time_validation() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("03:30"), Some(210));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert!(parse_hhmm("24:00").is_none());
        assert!(parse_hhmm("3:5").is_none());
        assert!(parse_hhmm("03:5").is_none());
        assert!(parse_hhmm("3:05").is_none());
        assert!(parse_hhmm("003:05").is_none());
        assert!(parse_hhmm("abc").is_none());
    }

    #[tokio::test]
    async fn update_settings_and_request_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("oshical-update-manager-{}", uuid::Uuid::new_v4()));
        let manager = ServerUpdateManager::new(dir.clone()).expect("manager");
        let settings = ServerUpdateSettings {
            auto_check_enabled: true,
            auto_update_enabled: true,
            update_window_start: "23:30".to_owned(),
            update_window_end: "02:30".to_owned(),
            github_mirror_url: "https://gh-proxy.com/".to_owned(),
        };
        let view = manager
            .save_settings(settings.clone())
            .await
            .expect("settings");
        assert!(view.auto_check_enabled);
        assert!(view.auto_update_enabled);
        assert_eq!(view.update_window_start, "23:30");

        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let signature = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        manager
            .write_request("9.9.9", Some(digest), Some(signature), false, &settings)
            .await
            .expect("request");
        let request = tokio::fs::read_to_string(manager.request_path())
            .await
            .expect("request content");
        assert!(request.contains("target_version=9.9.9"));
        assert!(request.contains(&format!("target_digest={digest}")));
        assert!(request.contains(&format!("signature={signature}")));
        assert!(request.contains("force=false"));
        assert!(request.contains("window_start=23:30"));
        let status = tokio::fs::read_to_string(manager.status_path())
            .await
            .expect("status content");
        assert!(status.contains("\"status\": \"requested\""));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn image_digest_validation_is_strict() {
        assert!(validate_image_digest(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_ok());
        assert!(validate_image_digest("sha256:abc").is_err());
        assert!(validate_image_digest(
            "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_err());
        assert!(validate_image_digest(
            "sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_err());
    }

    #[test]
    fn update_signature_validation_is_strict() {
        assert!(validate_update_signature(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_ok());
        assert!(validate_update_signature("abc").is_err());
        assert!(validate_update_signature(
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_err());
    }

    #[test]
    fn update_only_accepts_strictly_newer_versions() {
        assert!(is_newer_version("0.2.5", "0.2.4"));
        assert!(is_newer_version("0.3.0-beta.1", "0.2.4"));
        assert!(!is_newer_version("0.2.4", "0.2.4"));
        assert!(!is_newer_version("0.2.3", "0.2.4"));
        assert!(!is_newer_version("invalid", "0.2.4"));
    }

    #[test]
    fn update_window_supports_normal_and_cross_midnight() {
        assert!(window_contains("03:00", "05:00", 180));
        assert!(window_contains("03:00", "05:00", 299));
        assert!(!window_contains("03:00", "05:00", 300));
        assert!(window_contains("23:00", "02:00", 23 * 60));
        assert!(window_contains("23:00", "02:00", 60));
        assert!(!window_contains("23:00", "02:00", 12 * 60));
        assert!(window_contains("03:00", "03:00", 12 * 60));
    }
}
