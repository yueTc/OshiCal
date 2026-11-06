use std::time::Instant;

use reqwest::{Client, ClientBuilder, NoProxy, Proxy};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub const PROXY_HOST: &str = "127.0.0.1";
const PROXY_TEST_URL: &str = "https://www.google.com/generate_204";
const PROXY_TEST_TARGET: &str = "Google (www.google.com)";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestInput {
    pub proxy_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestReport {
    pub connected: bool,
    pub latency_ms: u64,
    pub target: String,
    pub proxy_url: String,
    pub status_code: Option<u16>,
    pub message: String,
}

pub fn validate_proxy_port(proxy_port: Option<u16>) -> AppResult<()> {
    if proxy_port == Some(0) {
        return Err(AppError::validation("代理端口必须在 1 到 65535 之间"));
    }
    Ok(())
}

pub fn proxy_url(proxy_port: u16) -> String {
    format!("http://{PROXY_HOST}:{proxy_port}")
}

const WEBSOCKET_PROXY_TARGET_HOST: &str = "broadcastlv.chat.bilibili.com";
const WEBSOCKET_PROXY_TARGET_PORT: u16 = 443;

/// Returns the proxy that should also be used for long-lived WebSocket traffic.
///
/// Reqwest honors system proxy environment variables automatically, while
/// tokio-tungstenite does not. Keeping the selection here makes the live
/// collector and the regular Bilibili HTTP client use the same route.
pub fn websocket_proxy_url(proxy_port: Option<u16>) -> Option<String> {
    if no_proxy_bypasses_host(WEBSOCKET_PROXY_TARGET_HOST, WEBSOCKET_PROXY_TARGET_PORT) {
        return None;
    }
    if let Some(port) = proxy_port {
        return Some(proxy_url(port));
    }
    ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| normalized_websocket_proxy_url(&value))
        })
}

fn normalized_websocket_proxy_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = parse_proxy_url(value)?;
    let path_is_root = matches!(parsed.path(), "" | "/");
    if parsed.scheme() != "http"
        || parsed.host_str().is_none()
        || !path_is_root
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        tracing::warn!(
            proxy = %redact_proxy_url(value),
            "忽略弹幕 WebSocket 不支持的代理配置"
        );
        return None;
    }
    Some(parsed.to_string().trim_end_matches('/').to_owned())
}

pub(crate) fn redact_proxy_url(value: &str) -> String {
    let value = value.trim();
    let Some(mut parsed) = parse_proxy_url(value) else {
        return "<invalid proxy URL>".to_owned();
    };
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("redacted");
    }
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.set_path("/");
    parsed.to_string()
}

fn parse_proxy_url(value: &str) -> Option<url::Url> {
    if value.contains("://") {
        url::Url::parse(value).ok()
    } else {
        url::Url::parse(&format!("http://{value}"))
            .ok()
            .or_else(|| url::Url::parse(value).ok())
    }
}

fn no_proxy_bypasses_host(host: &str, port: u16) -> bool {
    ["NO_PROXY", "no_proxy"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .is_some_and(|value| no_proxy_matches(&value, host, port))
}

fn no_proxy_matches(no_proxy: &str, host: &str, port: u16) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    no_proxy.split(',').any(|raw_entry| {
        let entry = raw_entry.trim();
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        let (entry_host, entry_port) = split_no_proxy_host_port(entry);
        if entry_port.is_some_and(|value| value != port) {
            return false;
        }
        let pattern = entry_host
            .trim()
            .trim_end_matches('.')
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        host == pattern || host.ends_with(&format!(".{pattern}"))
    })
}

fn split_no_proxy_host_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|value| value.parse().ok());
            return (host, port);
        }
    }
    if entry.matches(':').count() == 1 {
        if let Some((host, port)) = entry.rsplit_once(':') {
            if let Ok(port) = port.parse() {
                return (host, Some(port));
            }
        }
    }
    (entry, None)
}

pub fn client_builder(proxy_port: Option<u16>) -> AppResult<ClientBuilder> {
    validate_proxy_port(proxy_port)?;
    let builder = Client::builder();
    match proxy_port {
        Some(port) => {
            let proxy = Proxy::all(proxy_url(port))?.no_proxy(NoProxy::from_env());
            Ok(builder.no_proxy().proxy(proxy))
        }
        None => Ok(builder),
    }
}

pub async fn test_google(proxy_port: u16) -> AppResult<ProxyTestReport> {
    validate_proxy_port(Some(proxy_port))?;
    let proxy_url = proxy_url(proxy_port);
    let client = client_builder(Some(proxy_port))?
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let started = Instant::now();
    match client.get(PROXY_TEST_URL).send().await {
        Ok(response) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = response.status();
            let connected = status.is_success() || status.is_redirection();
            Ok(ProxyTestReport {
                connected,
                latency_ms,
                target: PROXY_TEST_TARGET.to_owned(),
                proxy_url,
                status_code: Some(status.as_u16()),
                message: if connected {
                    format!("已通过代理连接 Google，HTTP {}", status.as_u16())
                } else {
                    format!("代理已建立连接，但 Google 返回 HTTP {}", status.as_u16())
                },
            })
        }
        Err(error) => Ok(ProxyTestReport {
            connected: false,
            latency_ms: started.elapsed().as_millis() as u64,
            target: PROXY_TEST_TARGET.to_owned(),
            proxy_url,
            status_code: None,
            message: format!("代理连接 Google 失败：{error}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_uses_loopback_and_port() {
        assert_eq!(proxy_url(7890), "http://127.0.0.1:7890");
    }

    #[test]
    fn rejects_zero_port() {
        assert!(validate_proxy_port(Some(0)).is_err());
        assert!(validate_proxy_port(None).is_ok());
        assert!(validate_proxy_port(Some(7890)).is_ok());
    }

    #[test]
    fn websocket_proxy_only_accepts_http_connect_proxies() {
        assert_eq!(
            normalized_websocket_proxy_url("http://127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalized_websocket_proxy_url("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalized_websocket_proxy_url("localhost:7890").as_deref(),
            Some("http://localhost:7890")
        );
        assert!(normalized_websocket_proxy_url("socks5://127.0.0.1:7890").is_none());
        assert!(normalized_websocket_proxy_url("http://127.0.0.1:7890/proxy.pac").is_none());
        assert!(normalized_websocket_proxy_url("http://127.0.0.1:7890?token=secret").is_none());
    }

    #[test]
    fn proxy_redaction_removes_credentials_path_query_and_fragment() {
        let redacted = redact_proxy_url("http://user:secret@127.0.0.1:7890/proxy?token=hidden#x");
        assert!(redacted.contains("redacted"));
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("hidden"));
        assert!(!redacted.contains("token"));
    }

    #[test]
    fn no_proxy_matches_hosts_suffixes_and_ports() {
        assert!(no_proxy_matches(
            "localhost,*.bilibili.com",
            "broadcastlv.chat.bilibili.com",
            443
        ));
        assert!(no_proxy_matches(
            ".example.com,api.test:8443",
            "api.test",
            8443
        ));
        assert!(!no_proxy_matches("api.test:8443", "api.test", 443));
        assert!(!no_proxy_matches(
            "bilibili.com.evil.test",
            "broadcastlv.chat.bilibili.com",
            443
        ));
    }

    #[tokio::test]
    #[ignore = "requires a local proxy and internet access"]
    async fn real_proxy_google_smoke() {
        let port = std::env::var("OSHICAL_PROXY_PORT")
            .expect("set OSHICAL_PROXY_PORT")
            .parse::<u16>()
            .expect("valid port");
        let report = test_google(port).await.expect("proxy test");
        assert!(report.connected, "{}", report.message);
    }
}
