use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

const DIRECT_URL: &str = "https://workers.vrp.moe/laplace/fertilities";
const READER_URL: &str = "https://r.jina.ai/https://workers.vrp.moe/laplace/fertilities";
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";

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
    pub day_in_cycle: Option<i64>,
    pub ovulation_day: Option<i64>,
    pub next_period: Option<String>,
    pub effective_cycle_length: Option<i64>,
    pub effective_period_length: Option<i64>,
    #[serde(default)]
    pub data_points: i64,
    pub cycles_elapsed_since_observation: Option<i64>,
    pub prediction_accuracy: Option<FertilityAccuracy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityUser {
    pub username: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityAccuracy {
    pub mean_abs_error_days: Option<f64>,
    pub mean_error_days: Option<f64>,
    #[serde(default)]
    pub samples: i64,
}

#[derive(Clone)]
pub struct FertilityClient {
    http: reqwest::Client,
}

impl FertilityClient {
    pub fn new() -> AppResult<Self> {
        Self::new_with_proxy(None)
    }

    pub fn new_with_proxy(proxy_port: Option<u16>) -> AppResult<Self> {
        let http = crate::network::client_builder(proxy_port)?
            .user_agent(BROWSER_USER_AGENT)
            .timeout(StdDuration::from_secs(45))
            .build()?;
        Ok(Self { http })
    }

    pub async fn fetch(&self) -> AppResult<(FertilityResponse, String, DateTime<Utc>)> {
        let direct = self
            .http
            .get(DIRECT_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::REFERER, "https://laplace.live/ovu")
            .send()
            .await;

        match direct {
            Ok(response) if response.status().is_success() => {
                let body = response.text().await?;
                let payload = parse_fertility_payload(&body)?;
                return Ok((payload, "laplace".to_owned(), Utc::now()));
            }
            Ok(response) => {
                tracing::warn!(
                    status = response.status().as_u16(),
                    "direct laplace fertility request was rejected"
                );
            }
            Err(error) => {
                tracing::warn!(%error, "direct laplace fertility request failed");
            }
        }

        let response = self
            .http
            .get(READER_URL)
            .header(reqwest::header::ACCEPT, "text/plain")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::External {
                code: "FERTILITY_HTTP".to_owned(),
                message: format!("魔法期数据代理返回 HTTP {}", status.as_u16()),
            });
        }
        let body = response.text().await?;
        let payload = parse_fertility_payload(&body)?;
        Ok((payload, "jina-reader".to_owned(), Utc::now()))
    }
}

pub fn parse_fertility_payload(body: &str) -> AppResult<FertilityResponse> {
    let start = body
        .find("{\"data\"")
        .or_else(|| body.find('{'))
        .ok_or_else(|| AppError::validation("魔法期数据中没有 JSON 对象"))?;
    let mut stream =
        serde_json::Deserializer::from_str(&body[start..]).into_iter::<FertilityResponse>();
    let parsed = stream
        .next()
        .ok_or_else(|| AppError::validation("魔法期 JSON 为空"))??;
    if parsed.data.is_empty() {
        return Err(AppError::validation("魔法期数据列表为空"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::parse_fertility_payload;

    #[test]
    fn parses_direct_json() {
        let body = r#"{"data":[{"uid":1282271864,"user":{"username":"自由魂儿儿儿","avatar":"https://example.com/a.jpg"},"status":"ovulating","dayInCycle":14,"ovulationDay":14,"nextPeriod":"2026-11-12","effectiveCycleLength":28,"effectivePeriodLength":4,"dataPoints":15,"cyclesElapsedSinceObservation":0,"predictionAccuracy":{"meanAbsErrorDays":1.3,"meanErrorDays":-1.3,"samples":4}}],"total":1008}"#;
        let parsed = parse_fertility_payload(body).expect("payload");
        assert_eq!(parsed.total, 1008);
        assert_eq!(parsed.data[0].uid, 1282271864);
        assert_eq!(parsed.data[0].status, "ovulating");
        assert_eq!(parsed.data[0].next_period.as_deref(), Some("2026-11-12"));
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

    #[test]
    fn parses_jina_reader_text() {
        let body = "Title:\n\nURL Source: https://workers.vrp.moe/laplace/fertilities\n\nMarkdown Content:\n{\"data\":[{\"uid\":1,\"user\":{\"username\":\"test\",\"avatar\":\"\"},\"status\":\"normal\",\"dayInCycle\":3,\"ovulationDay\":14,\"nextPeriod\":null,\"effectiveCycleLength\":28,\"effectivePeriodLength\":5,\"dataPoints\":1,\"cyclesElapsedSinceObservation\":0,\"predictionAccuracy\":{\"meanAbsErrorDays\":null,\"meanErrorDays\":null,\"samples\":0}}],\"total\":1}\n";
        let parsed = parse_fertility_payload(body).expect("payload");
        assert_eq!(parsed.data[0].user.username.as_deref(), Some("test"));
        assert_eq!(parsed.data[0].status, "normal");
    }
}
