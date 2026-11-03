use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone, Utc};

use crate::error::{AppError, AppResult};

pub fn parse_hhmm(value: &str) -> Option<(u32, u32)> {
    let (hour, minute) = value.trim().split_once(':')?;
    let hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse::<u32>().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

#[allow(clippy::too_many_arguments)]
pub fn validate_rule_fields(
    title: &str,
    weekdays_mask: u8,
    start_time: &str,
    end_time: Option<&str>,
    timezone: &str,
    room_url: Option<&str>,
    effective_from: &str,
    effective_until: Option<&str>,
) -> AppResult<()> {
    if title.trim().is_empty() || title.chars().count() > 200 {
        return Err(AppError::validation("固定周表主题必须为 1 到 200 字"));
    }
    if weekdays_mask == 0 || weekdays_mask > 0b0111_1111 {
        return Err(AppError::validation("固定周表至少选择一个星期"));
    }
    if parse_hhmm(start_time).is_none() {
        return Err(AppError::validation("固定周表开始时间格式应为 HH:MM"));
    }
    if end_time.is_some_and(|value| parse_hhmm(value).is_none()) {
        return Err(AppError::validation("固定周表结束时间格式应为 HH:MM"));
    }
    if timezone != "Asia/Shanghai" {
        return Err(AppError::validation(
            "固定周表当前只支持 Asia/Shanghai 时区",
        ));
    }
    if let Some(url) = room_url {
        let parsed = url::Url::parse(url)?;
        if parsed.scheme() != "https" || parsed.host_str() != Some("live.bilibili.com") {
            return Err(AppError::validation(
                "固定周表直播间只允许 https://live.bilibili.com/",
            ));
        }
    }
    let from = NaiveDate::parse_from_str(effective_from, "%Y-%m-%d")
        .map_err(|_| AppError::validation("固定周表生效日期格式错误"))?;
    if let Some(until) = effective_until {
        let until = NaiveDate::parse_from_str(until, "%Y-%m-%d")
            .map_err(|_| AppError::validation("固定周表结束日期格式错误"))?;
        if until < from {
            return Err(AppError::validation("固定周表结束日期不能早于生效日期"));
        }
    }
    Ok(())
}

pub fn occurs_on(weekdays_mask: u8, date: NaiveDate) -> bool {
    let bit = 1_u8 << date.weekday().num_days_from_monday();
    weekdays_mask & bit != 0
}

pub fn occurrence_bounds(
    start_time: &str,
    end_time: Option<&str>,
    occurrence_date: &str,
) -> Option<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    let china = FixedOffset::east_opt(8 * 3600)?;
    let date = NaiveDate::parse_from_str(occurrence_date, "%Y-%m-%d").ok()?;
    let (start_hour, start_minute) = parse_hhmm(start_time)?;
    let start = china
        .from_local_datetime(&date.and_hms_opt(start_hour, start_minute, 0)?)
        .single()?
        .with_timezone(&Utc);
    let end = end_time.and_then(|value| {
        let (hour, minute) = parse_hhmm(value)?;
        let mut end_date = date;
        let mut local = china
            .from_local_datetime(&end_date.and_hms_opt(hour, minute, 0)?)
            .single()?
            .with_timezone(&Utc);
        if local <= start {
            end_date += Duration::days(1);
            local = china
                .from_local_datetime(&end_date.and_hms_opt(hour, minute, 0)?)
                .single()?
                .with_timezone(&Utc);
        }
        Some(local)
    });
    Some((start, end))
}

pub fn china_date(value: DateTime<Utc>) -> NaiveDate {
    value
        .with_timezone(&FixedOffset::east_opt(8 * 3600).expect("china offset"))
        .date_naive()
}
