use std::time::Instant;

use reqwest::{Client, ClientBuilder, Proxy};
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

pub fn client_builder(proxy_port: Option<u16>) -> AppResult<ClientBuilder> {
    validate_proxy_port(proxy_port)?;
    let builder = Client::builder();
    match proxy_port {
        Some(port) => Ok(builder.no_proxy().proxy(Proxy::all(proxy_url(port))?)),
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
