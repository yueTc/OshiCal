//! LAPLACE（laplace.live /ovu）生理期数据客户端。
//!
//! 数据由 LAPLACE 及其投稿者维护，属于第三方内容；本模块只做只读抓取，
//! 不调用任何写入接口（`POST /laplace/fertility-contribute`）。

use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

/// 全量列表（含已过期角色），实测 1638 条，覆盖在册 998 条之外的历史角色。
const DEFAULT_DIRECT_URL: &str = "https://workers.vrp.moe/laplace/fertilities?showOutdated=1";
/// 直连被拦时的兜底：Reader 文本代理。
const DEFAULT_READER_URL: &str =
    "https://r.jina.ai/https://workers.vrp.moe/laplace/fertilities?showOutdated=1";
/// 覆盖直连地址。可指向自建中转或镜像：部署环境里 Cloudflare 按 TLS 指纹做托管挑战
/// 时，用它把抓取切到能过的出口（例如同机上的浏览器指纹中转）。
const DIRECT_URL_ENV: &str = "OSHICAL_FERTILITY_URL";
/// 覆盖 Reader 兜底地址。
const READER_URL_ENV: &str = "OSHICAL_FERTILITY_READER_URL";
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";
const MAX_ATTEMPTS: usize = 3;

/// 读取环境变量里配置的地址；没配或为空时用默认值。
fn configured_url(env_key: &str, fallback: &str) -> String {
    std::env::var(env_key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// 打开开关后两侧采集循环的抓取周期：6 小时。
pub const FERTILITY_POLL_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// 抓取结果在 SQLite 中的缓存有效期：6 小时。
pub const FERTILITY_CACHE_TTL_HOURS: i64 = 6;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityResponse {
    pub data: Vec<FertilityRecord>,
    #[serde(default)]
    pub total: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityRecord {
    pub uid: i64,
    pub user: FertilityUser,
    pub status: String,
    #[serde(default)]
    pub day_in_cycle: Option<i64>,
    #[serde(default)]
    pub ovulation_day: Option<i64>,
    #[serde(default)]
    pub next_period: Option<String>,
    #[serde(default)]
    pub effective_cycle_length: Option<i64>,
    #[serde(default)]
    pub effective_period_length: Option<i64>,
    #[serde(default)]
    pub data_points: i64,
    #[serde(default)]
    pub cycles_elapsed_since_observation: Option<i64>,
    #[serde(default)]
    pub prediction_accuracy: Option<FertilityAccuracy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityUser {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub room: Option<i64>,
    #[serde(default)]
    pub live_status: Option<i64>,
    #[serde(default)]
    pub live_fans_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityAccuracy {
    #[serde(default)]
    pub mean_abs_error_days: Option<f64>,
    #[serde(default)]
    pub mean_error_days: Option<f64>,
    #[serde(default)]
    pub samples: i64,
}

#[derive(Clone)]
pub struct FertilityClient {
    http: reqwest::Client,
    direct_url: String,
    reader_url: String,
}

impl FertilityClient {
    pub fn new() -> AppResult<Self> {
        Self::new_with_proxy(None)
    }

    pub fn new_with_proxy(proxy_port: Option<u16>) -> AppResult<Self> {
        let http = crate::network::client_builder(proxy_port)?
            .user_agent(BROWSER_USER_AGENT)
            .connect_timeout(StdDuration::from_secs(10))
            .timeout(StdDuration::from_secs(45))
            .build()?;
        Ok(Self {
            http,
            direct_url: configured_url(DIRECT_URL_ENV, DEFAULT_DIRECT_URL),
            reader_url: configured_url(READER_URL_ENV, DEFAULT_READER_URL),
        })
    }

    /// 抓取全量列表。直连优先，逐次退避重试；三次都失败后回退到 Reader 文本代理。
    pub async fn fetch(&self) -> AppResult<(FertilityResponse, String, DateTime<Utc>)> {
        let mut last_error: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let backoff = StdDuration::from_millis(800 * (1u64 << attempt));
                tokio::time::sleep(backoff).await;
            }
            match self.fetch_direct().await {
                Ok(payload) => return Ok((payload, "laplace".to_owned(), Utc::now())),
                Err(error) => {
                    tracing::warn!(attempt = attempt + 1, %error, "laplace 直连抓取失败");
                    last_error = Some(error.to_string());
                }
            }
        }

        match self.fetch_reader().await {
            Ok(payload) => Ok((payload, "jina-reader".to_owned(), Utc::now())),
            Err(reader_error) => {
                tracing::warn!(%reader_error, "laplace Reader 兜底抓取失败");
                Err(AppError::External {
                    code: "FERTILITY_UNAVAILABLE".to_owned(),
                    message: match last_error {
                        Some(direct) => format!("LAPLACE 抓取失败：{direct}；代理兜底也失败"),
                        None => format!("LAPLACE 抓取失败：{reader_error}"),
                    },
                })
            }
        }
    }

    async fn fetch_direct(&self) -> AppResult<FertilityResponse> {
        let response = self
            .http
            .get(self.direct_url.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::REFERER, "https://laplace.live/ovu")
            .send()
            .await
            .map_err(|error| AppError::External {
                code: "FERTILITY_HTTP".to_owned(),
                message: format!("LAPLACE 请求失败：{error}"),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::External {
                code: "FERTILITY_HTTP".to_owned(),
                message: format!("LAPLACE 返回 HTTP {}", status.as_u16()),
            });
        }
        let body = response.text().await.map_err(|error| AppError::External {
            code: "FERTILITY_HTTP".to_owned(),
            message: format!("LAPLACE 响应读取失败：{error}"),
        })?;
        parse_fertility_payload(&body)
    }

    async fn fetch_reader(&self) -> AppResult<FertilityResponse> {
        let response = self
            .http
            .get(self.reader_url.as_str())
            .header(reqwest::header::ACCEPT, "text/plain")
            .send()
            .await
            .map_err(|error| AppError::External {
                code: "FERTILITY_HTTP".to_owned(),
                message: format!("LAPLACE 代理请求失败：{error}"),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::External {
                code: "FERTILITY_HTTP".to_owned(),
                message: format!("LAPLACE 代理返回 HTTP {}", status.as_u16()),
            });
        }
        let body = response.text().await.map_err(|error| AppError::External {
            code: "FERTILITY_HTTP".to_owned(),
            message: format!("LAPLACE 代理响应读取失败：{error}"),
        })?;
        parse_fertility_payload(&body)
    }
}

/// 解析 LAPLACE 列表。既接受纯 JSON，也接受 Reader 代理包了一层的文本。
pub fn parse_fertility_payload(body: &str) -> AppResult<FertilityResponse> {
    let start = body
        .find("{\"data\"")
        .or_else(|| body.find('{'))
        .ok_or_else(|| AppError::validation("LAPLACE 数据中没有 JSON 对象"))?;
    let mut stream =
        serde_json::Deserializer::from_str(&body[start..]).into_iter::<FertilityResponse>();
    let parsed = stream
        .next()
        .ok_or_else(|| AppError::validation("LAPLACE JSON 为空"))??;
    if parsed.data.is_empty() {
        return Err(AppError::validation("LAPLACE 数据列表为空"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::parse_fertility_payload;

    #[test]
    fn parses_direct_json() {
        let body = r#"{"data":[{"uid":1282271864,"user":{"username":"自由魂儿儿儿","avatar":"https://example.com/a.jpg","room":123,"liveStatus":1,"liveFansCount":45678},"status":"ovulating","dayInCycle":14,"ovulationDay":14,"nextPeriod":"2026-11-12","effectiveCycleLength":28,"effectivePeriodLength":4,"dataPoints":15,"cyclesElapsedSinceObservation":0,"predictionAccuracy":{"meanAbsErrorDays":1.3,"meanErrorDays":-1.3,"samples":4}}],"total":1008}"#;
        let parsed = parse_fertility_payload(body).expect("payload");
        assert_eq!(parsed.total, 1008);
        assert_eq!(parsed.data[0].uid, 1282271864);
        assert_eq!(parsed.data[0].status, "ovulating");
        assert_eq!(parsed.data[0].next_period.as_deref(), Some("2026-11-12"));
        assert_eq!(parsed.data[0].day_in_cycle, Some(14));
        assert_eq!(parsed.data[0].data_points, 15);
        assert_eq!(parsed.data[0].user.live_fans_count, Some(45678));
        assert_eq!(
            parsed.data[0]
                .prediction_accuracy
                .as_ref()
                .map(|accuracy| accuracy.samples),
            Some(4)
        );
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let body = r#"{"data":[{"uid":1,"user":{"username":"only-name"},"status":"normal"}]}"#;
        let parsed = parse_fertility_payload(body).expect("payload");
        assert_eq!(parsed.total, 0);
        assert!(parsed.data[0].next_period.is_none());
        assert_eq!(parsed.data[0].data_points, 0);
        assert_eq!(parsed.data[0].user.avatar, None);
    }

    #[test]
    fn parses_reader_reader_text() {
        let body = "Title:\n\nURL Source: https://workers.vrp.moe/laplace/fertilities\n\nMarkdown Content:\n{\"data\":[{\"uid\":1,\"user\":{\"username\":\"test\",\"avatar\":\"\"},\"status\":\"normal\",\"dayInCycle\":3,\"ovulationDay\":14,\"nextPeriod\":null,\"effectiveCycleLength\":28,\"effectivePeriodLength\":5,\"dataPoints\":1,\"cyclesElapsedSinceObservation\":0,\"predictionAccuracy\":{\"meanAbsErrorDays\":null,\"meanErrorDays\":null,\"samples\":0}}],\"total\":1}\n";
        let parsed = parse_fertility_payload(body).expect("payload");
        assert_eq!(parsed.data[0].user.username.as_deref(), Some("test"));
        assert_eq!(parsed.data[0].status, "normal");
    }

    #[test]
    fn rejects_empty_payload() {
        assert!(parse_fertility_payload("{\"data\":[]}").is_err());
        assert!(parse_fertility_payload("no json here").is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn real_fertility_fetch() {
        let client = super::FertilityClient::new().expect("client");
        let (payload, source, fetched_at) = client.fetch().await.expect("fetch");
        assert!(!payload.data.is_empty());
        assert!(payload.total >= payload.data.len());
        assert!(!source.is_empty());
        assert!(fetched_at <= chrono::Utc::now());
    }
}
