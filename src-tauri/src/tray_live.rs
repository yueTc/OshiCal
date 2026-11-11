//! Native tray live list, independent of the main WebView and of network live polling.
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use oshical_core::{load_settings, DataMode, LiveStatus};
use tauri::image::Image;
use tauri::menu::{IconMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager};

use crate::{map_error, open_external_url, server_cache_key, server_cache_store, DesktopState};

pub(crate) const TRAY_ID: &str = "oshical-tray";
const LIVE_PREFIX: &str = "tray-live:";
const DIRECT_LIMIT: usize = 8;
const AVATAR_LIMIT: usize = 128;
const AVATAR_BYTES: usize = 1024 * 1024;
const AVATAR_BASE64_BYTES: usize = AVATAR_BYTES.div_ceil(3) * 4;
const AVATAR_RETRY: Duration = Duration::from_secs(300);
type AvatarCache = HashMap<String, (Instant, Option<Image<'static>>)>;
type AvatarDownload = BoxFuture<'static, (String, Option<Image<'static>>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveEntry {
    id: i64,
    label: String,
    avatar: String,
    url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    // Server IDs are only unique within one server's cache, not across servers.
    source: String,
    entries: Vec<LiveEntry>,
    message: Option<&'static str>,
}

impl Snapshot {
    fn message(message: &'static str) -> Self {
        Self {
            source: String::new(),
            entries: Vec::new(),
            message: Some(message),
        }
    }

    fn from_statuses(source: String, statuses: Vec<LiveStatus>) -> Self {
        let entries = statuses
            .into_iter()
            .filter(|status| status.is_live)
            .map(|status| LiveEntry {
                id: status.streamer_id,
                label: menu_label(&status.streamer_name),
                avatar: status.avatar_url,
                url: live_url(status.live_url.as_deref()),
            })
            .collect();
        Self {
            source,
            entries,
            message: None,
        }
    }

    fn item_id(&self, entry: &LiveEntry) -> String {
        format!("{LIVE_PREFIX}{}:{}", self.source, entry.id)
    }
}

fn menu_label(name: &str) -> String {
    // Native Windows menu labels interpret ampersands as mnemonics and tabs as shortcuts.
    let clean: String = name
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return "未命名主播".into();
    }
    let mut text: String = trimmed.chars().take(48).collect();
    if trimmed.chars().count() > 48 {
        text.push('…');
    }
    text.replace('&', "&&")
}

fn live_url(value: Option<&str>) -> Option<String> {
    let parsed = url::Url::parse(value?).ok()?;
    (parsed.scheme() == "https"
        && parsed.host_str() == Some("live.bilibili.com")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed
            .path()
            .trim_matches('/')
            .parse::<u64>()
            .is_ok_and(|id| id > 0))
    .then(|| parsed.to_string())
}

// Core and server sync normally store a small inline WebP instead of a remote URL.
fn inline_avatar_payload(value: &str) -> Option<&str> {
    let (mime, payload) = value.strip_prefix("data:")?.split_once(',')?;
    (matches!(
        mime,
        "image/webp;base64" | "image/png;base64" | "image/jpeg;base64"
    ) && !payload.is_empty()
        && payload.len() <= AVATAR_BASE64_BYTES)
        .then_some(payload)
}

fn allowed_avatar(value: &str) -> bool {
    if value.starts_with("data:") {
        return inline_avatar_payload(value).is_some();
    }
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed
            .host_str()
            .is_some_and(|host| host.ends_with(".hdslb.com"))
}

async fn current_snapshot(app: &AppHandle) -> Result<Snapshot, String> {
    let state = app.state::<DesktopState>();
    let main_store = state.service.store();
    let settings = load_settings(main_store).await.map_err(map_error)?;
    let (source, store) = if settings.data_mode == DataMode::Server {
        let source = server_cache_key(&settings.server_url)?;
        let store = server_cache_store(
            state.sync_stores.clone(),
            state.server_cache_root.clone(),
            main_store.clone(),
            settings.server_url,
        )
        .await?;
        (source, store)
    } else {
        ("local".to_owned(), main_store.clone())
    };
    let statuses = store.list_live_statuses().await.map_err(map_error)?;
    Ok(Snapshot::from_statuses(source, statuses))
}

pub(crate) fn initial_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    build_menu(
        app,
        &Snapshot::message("正在读取直播状态…"),
        &HashMap::new(),
    )
}

fn build_menu(
    app: &AppHandle,
    snapshot: &Snapshot,
    avatars: &AvatarCache,
) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "open", "打开推历 OshiCal", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &refresh, &PredefinedMenuItem::separator(app)?],
    )?;
    if let Some(message) = snapshot.message {
        menu.append(&MenuItem::new(app, message, false, None::<&str>)?)?;
    } else if snapshot.entries.is_empty() {
        menu.append(&MenuItem::new(
            app,
            "暂无主播正在直播",
            false,
            None::<&str>,
        )?)?;
    } else {
        let title = format!("正在直播（{}）", snapshot.entries.len());
        let submenu = if snapshot.entries.len() > DIRECT_LIMIT {
            Some(Submenu::new(app, &title, true)?)
        } else {
            menu.append(&MenuItem::new(app, &title, false, None::<&str>)?)?;
            None
        };
        for entry in &snapshot.entries {
            // A loading/invalid avatar stays blank; the app logo is not the streamer's face.
            let icon = avatars
                .get(&entry.avatar)
                .and_then(|(_, image)| image.clone());
            let item = IconMenuItem::with_id(
                app,
                snapshot.item_id(entry),
                &entry.label,
                entry.url.is_some(),
                icon,
                None::<&str>,
            )?;
            if let Some(submenu) = &submenu {
                submenu.append(&item)?;
            } else {
                menu.append(&item)?;
            }
        }
        if let Some(submenu) = submenu {
            menu.append(&submenu)?;
        }
    }
    menu.append_items(&[
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
    ])?;
    Ok(menu)
}

pub(crate) fn is_live_item(id: &str) -> bool {
    id.starts_with(LIVE_PREFIX)
}

pub(crate) fn open_live_room(app: &AppHandle, id: &str) {
    let app = app.clone();
    let id = id.to_owned();
    tauri::async_runtime::spawn(async move {
        // Re-read on click: a still-open menu must not open an old server's identically numbered room.
        let result = async {
            let snapshot = current_snapshot(&app).await?;
            let url = snapshot
                .entries
                .iter()
                .find(|entry| snapshot.item_id(entry) == id)
                .and_then(|entry| entry.url.clone())
                .ok_or_else(|| "直播状态已变化，请重新打开托盘菜单".to_owned())?;
            open_external_url(app.clone(), url)
        }
        .await;
        if let Err(error) = result {
            let _ = app.emit("monitor-error", error);
        }
    });
}

fn decode_avatar(bytes: Vec<u8>) -> Option<Image<'static>> {
    if bytes.len() > AVATAR_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .ok()?
        .resize_exact(32, 32, image::imageops::FilterType::Triangle)
        .into_rgba8();
    Some(Image::new_owned(rgba.into_raw(), 32, 32))
}

async fn download_avatar(client: Option<reqwest::Client>, url: &str) -> Option<Image<'static>> {
    if !allowed_avatar(url) {
        return None;
    }
    let bytes = if let Some(payload) = inline_avatar_payload(url) {
        // No HTTP client or request is needed for the avatar already included by sync.
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()?
    } else {
        let mut response = client?
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if response
            .content_length()
            .is_some_and(|length| length > AVATAR_BYTES as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len().saturating_add(chunk.len()) > AVATAR_BYTES {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        bytes
    };
    // Even bounded image decoding belongs off the async scheduler and away from the UI thread.
    tokio::task::spawn_blocking(move || decode_avatar(bytes))
        .await
        .ok()?
}

pub(crate) fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .user_agent("OshiCal/tray-avatar")
            .build()
            .ok();
        let mut avatars: AvatarCache = HashMap::new();
        let mut loading = HashSet::new();
        let mut downloads: FuturesUnordered<AvatarDownload> = FuturesUnordered::new();
        let mut snapshot = Snapshot::message("正在读取直播状态…");
        let mut dirty = true;
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let next = match current_snapshot(&app).await {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            if snapshot.message != Some("暂时无法读取直播状态") {
                                tracing::warn!(%error, "读取托盘直播列表失败");
                            }
                            Snapshot::message("暂时无法读取直播状态")
                        }
                    };
                    if next != snapshot { snapshot = next; dirty = true; }
                }
                Some((url, icon)) = downloads.next(), if !downloads.is_empty() => {
                    loading.remove(&url);
                    avatars.insert(url, (Instant::now(), icon));
                    dirty = true;
                }
            }
            let wanted: HashSet<_> = snapshot
                .entries
                .iter()
                .map(|entry| entry.avatar.clone())
                .filter(|url| allowed_avatar(url))
                .take(AVATAR_LIMIT)
                .collect();
            avatars.retain(|url, _| wanted.contains(url));
            for url in wanted {
                if downloads.len() >= 4 {
                    break;
                }
                if loading.contains(&url)
                    || avatars
                        .get(&url)
                        .is_some_and(|(at, icon)| icon.is_some() || at.elapsed() < AVATAR_RETRY)
                {
                    continue;
                }
                loading.insert(url.clone());
                let client = client.clone();
                downloads.push(Box::pin(async move {
                    let icon = download_avatar(client, &url).await;
                    (url, icon)
                }));
            }
            if dirty {
                let result = build_menu(&app, &snapshot, &avatars).and_then(|menu| {
                    if let Some(tray) = app.tray_by_id(TRAY_ID) {
                        tray.set_menu(Some(menu))
                    } else {
                        Ok(())
                    }
                });
                match result {
                    Ok(()) => dirty = false,
                    Err(error) => tracing::warn!(%error, "更新托盘直播菜单失败"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(id: i64, name: &str, is_live: bool, url: Option<&str>) -> LiveStatus {
        serde_json::from_value(json!({
            "streamerId": id, "mid": id.to_string(), "streamerName": name,
            "avatarUrl": "https://i0.hdslb.com/bfs/face/avatar.jpg", "isLive": is_live,
            "title": "直播", "liveUrl": url, "online": 0,
            "checkedAt": "2026-11-11T12:00:00Z", "liveNotifyEnabled": false
        }))
        .unwrap()
    }

    #[test]
    fn filters_offline_but_does_not_filter_notification_disabled_streamers() {
        let snapshot = Snapshot::from_statuses(
            "local".into(),
            vec![
                status(1, "开播", true, Some("https://live.bilibili.com/123")),
                status(2, "未开播", false, None),
            ],
        );
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].label, "开播");
        assert_eq!(
            snapshot.entries[0].url.as_deref(),
            Some("https://live.bilibili.com/123")
        );
    }

    #[test]
    fn empty_and_missing_room_are_safe() {
        assert!(Snapshot::from_statuses("local".into(), vec![])
            .entries
            .is_empty());
        let snapshot =
            Snapshot::from_statuses("local".into(), vec![status(1, "无房间", true, None)]);
        assert_eq!(snapshot.entries.len(), 1);
        assert!(snapshot.entries[0].url.is_none());
    }

    #[test]
    fn menu_labels_escape_mnemonics_and_control_characters() {
        assert_eq!(menu_label("  A&B\t\r\n主播 "), "A&&B主播");
        assert_eq!(menu_label("\n\t"), "未命名主播");
        assert_eq!(
            menu_label(&"推".repeat(49)),
            format!("{}…", "推".repeat(48))
        );
    }

    #[test]
    fn item_ids_distinguish_duplicate_names_and_different_server_sources() {
        let statuses = vec![status(1, "同名", true, None), status(2, "同名", true, None)];
        let first = Snapshot::from_statuses("server-a".into(), statuses.clone());
        let second = Snapshot::from_statuses("server-b".into(), statuses);
        assert_ne!(
            first.item_id(&first.entries[0]),
            first.item_id(&first.entries[1])
        );
        assert_ne!(
            first.item_id(&first.entries[0]),
            second.item_id(&second.entries[0])
        );
        assert!(is_live_item(&first.item_id(&first.entries[0])));
        assert!(!is_live_item("quit"));
    }

    #[test]
    fn only_https_numeric_bilibili_rooms_are_clickable() {
        assert!(live_url(Some("https://live.bilibili.com/123?broadcast_type=0")).is_some());
        for value in [
            "http://live.bilibili.com/123",
            "https://evil.test/123",
            "https://live.bilibili.com.evil.test/123",
            "https://user@live.bilibili.com/123",
            "https://live.bilibili.com:8443/123",
            "https://live.bilibili.com/0",
            "https://live.bilibili.com/blanc/123",
            "file:///C:/Windows/notepad.exe",
            "not-a-url",
        ] {
            assert!(live_url(Some(value)).is_none(), "{value}");
        }
    }

    #[test]
    fn avatar_urls_reject_non_cdn_credentials_and_alternate_ports() {
        assert!(allowed_avatar("https://i0.hdslb.com/bfs/face/a.jpg"));
        for value in [
            "http://i0.hdslb.com/a",
            "https://127.0.0.1/a",
            "https://hdslb.com.evil.test/a",
            "https://evilhdslb.com/a",
            "https://user@i0.hdslb.com/a",
            "https://i0.hdslb.com:8443/a",
        ] {
            assert!(!allowed_avatar(value), "{value}");
        }
    }

    #[tokio::test]
    async fn cached_inline_webp_avatar_is_decoded_without_an_http_client() {
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(8, 8, image::Rgba([12, 123, 231, 255]))
            .write_to(&mut encoded, image::ImageFormat::WebP)
            .unwrap();
        let source = format!(
            "data:image/webp;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(encoded.into_inner())
        );
        let icon = download_avatar(None, &source).await.expect(
            "server stores data:image/webp avatars; these must not fall back to the app icon",
        );
        assert_eq!((icon.width(), icon.height()), (32, 32));
        assert_eq!(&icon.rgba()[..4], &[12, 123, 231, 255]);
    }

    #[tokio::test]
    async fn inline_png_and_jpeg_are_supported_and_bad_data_stays_without_an_icon() {
        for (mime, format) in [
            ("image/png", image::ImageFormat::Png),
            ("image/jpeg", image::ImageFormat::Jpeg),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image::RgbImage::from_pixel(4, 8, image::Rgb([80, 120, 160]))
                .write_to(&mut encoded, format)
                .unwrap();
            let source = format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(encoded.into_inner())
            );
            assert!(allowed_avatar(&source));
            assert!(download_avatar(None, &source).await.is_some(), "{mime}");
        }
        for source in [
            "data:image/png;base64,!!!!",
            "data:image/webp;base64,YWJj",
            "data:image/png;base64,",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:text/html;base64,PGh0bWw+",
            "data:image/png,not-base64",
        ] {
            assert!(download_avatar(None, source).await.is_none(), "{source}");
        }
        let oversized = format!(
            "data:image/webp;base64,{}",
            "A".repeat(AVATAR_BASE64_BYTES + 1)
        );
        assert!(!allowed_avatar(&oversized));
        assert!(download_avatar(None, &oversized).await.is_none());
    }

    // Manual local-fixture regression: exports only decoded avatar pixels, never account credentials.
    #[tokio::test]
    #[ignore = "requires OSHICAL_TRAY_AVATAR_FIXTURE containing a read-only avatar export"]
    async fn actual_cached_inline_avatars_decode_to_distinct_icons() {
        let fixture = std::env::var("OSHICAL_TRAY_AVATAR_FIXTURE").expect("set fixture JSON path");
        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
        assert!(!rows.is_empty());
        let output = std::env::var("OSHICAL_TRAY_AVATAR_OUTPUT")
            .ok()
            .map(std::path::PathBuf::from);
        if let Some(output) = &output {
            std::fs::create_dir_all(output).unwrap();
        }
        let mut pixels = HashSet::new();
        for row in &rows {
            let source = row["avatar"].as_str().unwrap();
            assert!(source.starts_with("data:image/"));
            let icon = download_avatar(None, source)
                .await
                .expect("cached inline avatar must decode");
            assert_eq!((icon.width(), icon.height()), (32, 32));
            pixels.insert(icon.rgba().to_vec());
            if let Some(output) = &output {
                let id = row["id"].as_i64().unwrap();
                image::save_buffer(
                    output.join(format!("avatar-{id}.png")),
                    icon.rgba(),
                    icon.width(),
                    icon.height(),
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
        }
        assert!(
            pixels.len() > 1,
            "must load distinct streamer faces, not one app logo"
        );
        println!(
            "decoded {} cached inline avatars into {} distinct icons without an HTTP client",
            rows.len(),
            pixels.len()
        );
    }

    #[test]
    fn avatars_decode_resize_and_reject_corrupt_or_oversized_input() {
        assert!(decode_avatar(b"not an image".to_vec()).is_none());
        assert!(decode_avatar(vec![0; AVATAR_BYTES + 1]).is_none());
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(4, 8, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let icon = decode_avatar(encoded.into_inner()).unwrap();
        assert_eq!(
            (icon.width(), icon.height(), icon.rgba().len()),
            (32, 32, 32 * 32 * 4)
        );
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::new(2049, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert!(decode_avatar(encoded.into_inner()).is_none());
    }
}
