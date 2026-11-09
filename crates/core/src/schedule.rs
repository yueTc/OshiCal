use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone, Utc, Weekday};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::models::ReserveCard;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSchedule {
    pub title: String,
    pub start_at: DateTime<Utc>,
    pub end_at: Option<DateTime<Utc>>,
    pub confidence: f64,
}

pub fn parse_text_schedule(
    text: &str,
    published_at: DateTime<Utc>,
    now: DateTime<Utc>,
    fallback_title: &str,
) -> Option<ParsedSchedule> {
    parse_text_schedule_inner(text, published_at, now, fallback_title)
}

fn parse_text_schedule_inner(
    text: &str,
    published_at: DateTime<Utc>,
    now: DateTime<Utc>,
    fallback_title: &str,
) -> Option<ParsedSchedule> {
    let china = FixedOffset::east_opt(8 * 3600)?;
    let now_cn = now.with_timezone(&china);
    let published_cn = published_at.with_timezone(&china);
    let time = parse_time(text)?;
    let date = resolve_date(text, published_cn.date_naive(), now_cn.date_naive())?;
    let naive_start = date.and_hms_opt(time.hour, time.minute, 0)?;
    let local_start = china.from_local_datetime(&naive_start).single()?;
    if local_start < now_cn {
        return None;
    }
    let start_at = local_start.with_timezone(&Utc);
    let end_at = time.end.and_then(|(hour, minute)| {
        let mut naive_end = date.and_hms_opt(hour, minute, 0).unwrap_or(naive_start);
        if naive_end <= naive_start {
            naive_end += Duration::days(1);
        }
        china
            .from_local_datetime(&naive_end)
            .single()
            .map(|value| value.with_timezone(&Utc))
    });
    Some(ParsedSchedule {
        title: extract_title(text, fallback_title),
        start_at,
        end_at,
        confidence: 0.9,
    })
}

pub fn parse_reserve_schedule(
    reserve: &ReserveCard,
    published_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<ParsedSchedule> {
    // `ADDITIONAL_TYPE_RESERVE` also carries video reservations. Only live
    // reservations (`stype = 2`) belong in the live schedule.
    if !reserve.is_live() {
        return None;
    }
    let time_text = format!("{} {}", reserve.title, reserve.description);
    if !has_calendar_date(&time_text) {
        return None;
    }
    // 卡片上的“今天/明天 18:00”是相对卡片发布时刻说的，不是相对刷新时刻。
    // 这里用发布日的零点当“现在”解析：同一发布日的场次都能算出来，
    // 是否已经开过由下面的真实 now 判断，刷新再晚也不会把日期往后挪一格。
    let china = FixedOffset::east_opt(8 * 3600)?;
    let published_cn = published_at.with_timezone(&china);
    let publish_naive = published_cn.date_naive().and_hms_opt(0, 0, 0)?;
    let publish_day_start = china
        .from_local_datetime(&publish_naive)
        .single()?
        .with_timezone(&Utc);
    let mut schedule =
        parse_text_schedule(&time_text, published_at, publish_day_start, &reserve.title)?;
    // 已经开过的场次不再进日历。
    if schedule.start_at < now {
        return None;
    }
    schedule.title = reserve_title_without_bilibili_prefix(&reserve.title);
    schedule.confidence = 1.0;
    Some(schedule)
}

fn reserve_title_without_bilibili_prefix(title: &str) -> String {
    let title = title.trim();
    if let Some(rest) = title.strip_prefix("直播预约") {
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('：').or_else(|| rest.strip_prefix(':'));
        if let Some(rest) = rest {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_owned();
            }
        }
    }
    title.to_owned()
}

fn has_calendar_date(text: &str) -> bool {
    static DATE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let date = DATE_RE.get_or_init(|| {
        Regex::new(r"\d{4}-\d{1,2}-\d{1,2}|\d{1,2}\s*月\s*\d{1,2}\s*日")
            .expect("valid reserve date regex")
    });
    date.is_match(text)
        || month_day_regex().is_match(text)
        || slash_date_regex().is_match(text)
        || dot_date_regex().is_match(text)
        || ["今天", "今晚", "明天", "明晚", "后天"]
            .iter()
            .any(|token| text.contains(token))
}

fn resolve_date(text: &str, published_date: NaiveDate, today: NaiveDate) -> Option<NaiveDate> {
    static CN_DATE_HINT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static ISO_DATE_HINT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let cn_date = CN_DATE_HINT.get_or_init(|| {
        Regex::new(r"(?:(\d{4})\s*年\s*)?(\d{1,2})\s*月\s*(\d{1,2})\s*日")
            .expect("valid cn date hint")
    });
    let iso_date =
        ISO_DATE_HINT.get_or_init(|| Regex::new(r"\d{4}-\d{1,2}-\d{1,2}").expect("valid iso hint"));
    // 1. 写死的日期：ISO、`10月30日`、`11-02`、`10/31`。
    //    文本既然写了日期，就以它为准；解析不出来（`2月30日`）说明是坏数据，不要退回发布日。
    if cn_date.is_match(text)
        || iso_date.is_match(text)
        || month_day_regex().is_match(text)
        || slash_date_regex().is_match(text)
    {
        let date = parse_explicit_date(text, published_date)?;
        return (date >= today).then_some(date);
    }
    // 2. 今天/今晚/明天/明晚/后天。相对词是相对发布时刻说的：一条“明天 20:00”的
    //    动态过两天再被刷新，不该变成“后天”。已经过掉就继续看后面的线索。
    let relative = parse_relative_date(text, published_date);
    if let Some(date) = relative {
        if date > today {
            return Some(date);
        }
    }
    //    正文里的“今晚吃饭都更香了”会先命中“今晚”，可真正的场次另写着
    //    “下周一 18:00”。说到“下周X/下个星期X”时，让星期几优先。
    if relative == Some(today) && (text.contains("下周") || text.contains("下个星期")) {
        if let Some(date) = parse_weekday_date(text, published_date) {
            if date > today {
                return Some(date);
            }
        }
    }
    if let Some(date) = relative {
        return Some(date);
    }
    // 3. 周X / 下周X，同样以发布日为基准。
    if text.contains('周') || text.contains("星期") {
        if let Some(date) = parse_weekday_date(text, published_date) {
            return (date >= today).then_some(date);
        }
    }
    // 4. `11.9` 这种点分写法：和版本号（`【2D鸣潮】2.6下`）撞型，
    //    只在前面几条都没有答案时才拿来兜底。
    if let Some(date) = parse_dot_date(text, published_date) {
        if date >= today {
            return Some(date);
        }
    }
    (published_date >= today).then_some(published_date)
}
fn parse_explicit_date(text: &str, published_date: NaiveDate) -> Option<NaiveDate> {
    static ISO_DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let iso_date = ISO_DATE
        .get_or_init(|| Regex::new(r"(\d{4})-(\d{1,2})-(\d{1,2})").expect("valid iso date regex"));
    if let Some(caps) = iso_date.captures(text) {
        let year = caps.get(1)?.as_str().parse::<i32>().ok()?;
        let month = caps.get(2)?.as_str().parse::<u32>().ok()?;
        let day = caps.get(3)?.as_str().parse::<u32>().ok()?;
        return NaiveDate::from_ymd_opt(year, month, day);
    }
    static CN_DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let cn_date = CN_DATE.get_or_init(|| {
        Regex::new(r"(?:(\d{4})\s*年\s*)?(\d{1,2})\s*月\s*(\d{1,2})\s*日")
            .expect("valid cn date regex")
    });
    if let Some(caps) = cn_date.captures(text) {
        let month = caps.get(2)?.as_str().parse::<u32>().ok()?;
        let day = caps.get(3)?.as_str().parse::<u32>().ok()?;
        if let Some(year) = caps
            .get(1)
            .and_then(|value| value.as_str().parse::<i32>().ok())
        {
            return NaiveDate::from_ymd_opt(year, month, day);
        }
        return month_day_on_publication_year(published_date, month, day);
    }
    if let Some(caps) = month_day_regex().captures(text) {
        let month = caps.get(1)?.as_str().parse::<u32>().ok()?;
        let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
        return month_day_on_publication_year(published_date, month, day);
    }
    if let Some(caps) = slash_date_regex().captures(text) {
        let month = caps.get(1)?.as_str().parse::<u32>().ok()?;
        let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
        return month_day_on_publication_year(published_date, month, day);
    }
    None
}

/// 斜杠日期 `10/31`：写法足够明确，只要左右不贴着数字/字母就认。
fn slash_date_regex() -> &'static Regex {
    static SLASH_DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    SLASH_DATE.get_or_init(|| {
        Regex::new(r"(?:^|[^0-9A-Za-z_:])(\d{1,2})\s*/\s*(\d{1,2})(?:$|[^0-9A-Za-z_%])")
            .expect("valid slash date regex")
    })
}

/// 点分日期 `10.30`：和版本号/小数（`2.6下`、`v2.6.1`、`2.6.0补丁`）写法完全重叠，
/// 所以只在 `resolve_date` 最后兜底，并且要求左右不是数字/字母/小数点。
fn dot_date_regex() -> &'static Regex {
    static DOT_DATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    DOT_DATE.get_or_init(|| {
        Regex::new(r"(?:^|[^0-9A-Za-z_.:])(\d{1,2})\s*\.\s*(\d{1,2})(?:$|[^0-9A-Za-z_%])")
            .expect("valid dot date regex")
    })
}

fn parse_dot_date(text: &str, published_date: NaiveDate) -> Option<NaiveDate> {
    let caps = dot_date_regex().captures(text)?;
    let month = caps.get(1)?.as_str().parse::<u32>().ok()?;
    let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
    // 点分写法误判的代价很高（版本号 `2.6` 会变成 2 月 6 日并被推到明年），
    // 跨年推断只给一个月的窗口：只有“12月底约1月初”这种才值得顺延。
    month_day_on_publication_year_within(published_date, month, day, 31)
}

fn month_day_on_publication_year(
    published_date: NaiveDate,
    month: u32,
    day: u32,
) -> Option<NaiveDate> {
    month_day_on_publication_year_within(published_date, month, day, 180)
}

fn month_day_on_publication_year_within(
    published_date: NaiveDate,
    month: u32,
    day: u32,
    rollover_days: i64,
) -> Option<NaiveDate> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let same_year = NaiveDate::from_ymd_opt(published_date.year(), month, day)?;
    if same_year >= published_date {
        return Some(same_year);
    }
    let next_year = NaiveDate::from_ymd_opt(published_date.year() + 1, month, day)?;
    // Only infer a year rollover when the date is reasonably close to publication.
    (next_year <= published_date + Duration::days(rollover_days)).then_some(next_year)
}

fn month_day_regex() -> &'static Regex {
    static MONTH_DAY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    MONTH_DAY.get_or_init(|| {
        Regex::new(r"(?:^|[^0-9:])(\d{1,2})\s*-\s*(\d{1,2})(?:$|[^0-9:])")
            .expect("valid month-day date regex")
    })
}

fn parse_relative_date(text: &str, today: NaiveDate) -> Option<NaiveDate> {
    if text.contains("后天") {
        Some(today + Duration::days(2))
    } else if text.contains("明天") || text.contains("明晚") {
        Some(today + Duration::days(1))
    } else if text.contains("今天") || text.contains("今晚") {
        Some(today)
    } else {
        None
    }
}

fn parse_weekday_date(text: &str, today: NaiveDate) -> Option<NaiveDate> {
    static WEEKDAY_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = WEEKDAY_RE.get_or_init(|| {
        Regex::new(r"(?:周|星期)([一二三四五六日天])").expect("valid weekday regex")
    });
    let caps = regex.captures(text)?;
    let weekday = match caps.get(1)?.as_str() {
        "一" => Weekday::Mon,
        "二" => Weekday::Tue,
        "三" => Weekday::Wed,
        "四" => Weekday::Thu,
        "五" => Weekday::Fri,
        "六" => Weekday::Sat,
        "日" | "天" => Weekday::Sun,
        _ => return None,
    };
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let week_offset = if text.contains("上周") {
        -7
    } else if text.contains("下周") {
        7
    } else {
        0
    };
    Some(
        monday
            + Duration::days(weekday.num_days_from_monday() as i64)
            + Duration::days(week_offset),
    )
}

#[derive(Debug, Clone)]
struct ParsedTime {
    hour: u32,
    minute: u32,
    end: Option<(u32, u32)>,
}

fn time_regex() -> &'static Regex {
    static TIME_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    TIME_RE.get_or_init(|| {
        Regex::new(
            r#"(?:(上午|中午|下午|晚上|今晚|明晚)\s*)?(\d{1,2})(?:\s*[:：]\s*(\d{2})|\s*[点時时]\s*(?:(\d{1,2})\s*分?)?)"#,
        )
        .expect("valid time regex")
    })
}

fn parse_time(text: &str) -> Option<ParsedTime> {
    let regex = time_regex();
    let mut iter = regex.captures_iter(text);
    let first = iter.next()?;
    let modifier = first.get(1).map(|value| value.as_str()).unwrap_or_default();
    let hour = first.get(2)?.as_str().parse::<u32>().ok()?;
    let minute = first
        .get(3)
        .or_else(|| first.get(4))
        .and_then(|value| value.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    let hour = normalize_hour(modifier, hour)?;
    if minute > 59 {
        return None;
    }
    let first_end = first.get(0)?.end();
    let end = iter.next().and_then(|second| {
        if second.get(0)?.start() <= first_end {
            return None;
        }
        let between = &text[first_end..second.get(0)?.start()];
        if !["-", "~", "～", "至", "到"]
            .iter()
            .any(|token| between.contains(token))
        {
            return None;
        }
        let second_modifier = second
            .get(1)
            .map(|value| value.as_str())
            .unwrap_or(modifier);
        let second_hour = second.get(2)?.as_str().parse::<u32>().ok()?;
        let second_minute = second
            .get(3)
            .or_else(|| second.get(4))
            .and_then(|value| value.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        if second_minute > 59 {
            return None;
        }
        let second_hour = normalize_hour(second_modifier, second_hour)?;
        Some((second_hour, second_minute))
    });
    Some(ParsedTime { hour, minute, end })
}

fn normalize_hour(modifier: &str, hour: u32) -> Option<u32> {
    if hour > 23 {
        return None;
    }
    let evening = matches!(modifier, "下午" | "晚上" | "今晚" | "明晚");
    let noon = modifier == "中午";
    let hour = if (evening && hour < 12) || (noon && hour < 11) {
        hour + 12
    } else {
        hour
    };
    (hour <= 23).then_some(hour)
}

fn extract_title(text: &str, fallback: &str) -> String {
    static DATE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let date_re = DATE_RE.get_or_init(|| {
        Regex::new(r"(?:(\d{4})\s*年\s*)?(\d{1,2})\s*月\s*(\d{1,2})\s*日|(\d{1,2})/(\d{1,2})|(?:今天|今晚|明天|明晚|后天)|(?:周|星期)[一二三四五六日天]")
            .expect("valid title date regex")
    });
    let time_line = text
        .lines()
        .enumerate()
        .find(|(_, line)| time_regex().is_match(line));
    if let Some((index, line)) = time_line {
        let lines: Vec<&str> = text.lines().collect();
        let inline = clean_title_candidate(line, true, date_re);
        if !inline.is_empty() {
            return finalize_title(inline, fallback);
        }
        let previous = lines
            .iter()
            .take(index)
            .rev()
            .copied()
            .find(|candidate| !candidate.trim().is_empty());
        if let Some(previous) = previous {
            let title = clean_title_candidate(previous, false, date_re);
            if !title.is_empty() {
                return finalize_title(title, fallback);
            }
        }
        return finalize_title(clean_title_candidate(line, true, date_re), fallback);
    }
    let source = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(fallback);
    finalize_title(clean_title_candidate(source, true, date_re), fallback)
}

fn clean_title_candidate(source: &str, strip_date: bool, date_re: &Regex) -> String {
    let mut title = if strip_date {
        date_re.replace_all(source, " ").to_string()
    } else {
        source.to_owned()
    };
    title = time_regex().replace_all(&title, " ").to_string();
    for noise in [
        "直播预约",
        "直播",
        "开播",
        "预约",
        "～",
        "~",
        "-",
        "|",
        "｜",
        "·",
    ] {
        title = title.replace(noise, " ");
    }
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|ch: char| ch.is_whitespace() || "，。！!：:（）()[]【】".contains(ch))
        .to_owned()
}

fn finalize_title(title: String, fallback: &str) -> String {
    if title.trim().is_empty() {
        fallback.trim().to_owned()
    } else {
        title.chars().take(200).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn china_now() -> DateTime<Utc> {
        let offset = FixedOffset::east_opt(8 * 3600).expect("offset");
        offset
            .with_ymd_and_hms(2026, 10, 26, 12, 0, 0)
            .single()
            .expect("date")
            .with_timezone(&Utc)
    }

    fn published() -> DateTime<Utc> {
        let offset = FixedOffset::east_opt(8 * 3600).expect("offset");
        offset
            .with_ymd_and_hms(2026, 10, 26, 10, 0, 0)
            .single()
            .expect("date")
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_explicit_date_and_time() {
        let value = parse_text_schedule(
            "周五杂谈\n10月30日 20:00 直播",
            published(),
            china_now(),
            "直播",
        )
        .expect("schedule");
        assert_eq!(
            value.start_at,
            "2026-10-30T12:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("date")
        );
        assert_eq!(value.title, "周五杂谈");
    }

    #[test]
    fn parses_tonight_and_range() {
        let value = parse_text_schedule("今晚杂谈 20:00-22:00", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-10-26 12:00:00 UTC");
        assert_eq!(
            value.end_at.expect("end").to_string(),
            "2026-10-26 14:00:00 UTC"
        );
    }

    #[test]
    fn parses_tomorrow_chinese_hour() {
        let value = parse_text_schedule("明天晚上 8 点开播", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-10-27 12:00:00 UTC");
    }

    #[test]
    fn parses_current_week_sunday() {
        let value = parse_text_schedule("周日 19:00 直播", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-11-01 11:00:00 UTC");
    }

    #[test]
    fn rejects_past_or_missing_time() {
        assert!(
            parse_text_schedule("上周一 20:00 直播", published(), china_now(), "直播").is_none()
        );
        assert!(parse_text_schedule("周五直播", published(), china_now(), "直播").is_none());
        assert!(parse_text_schedule("今天想吃火锅", published(), china_now(), "直播").is_none());
    }

    #[test]
    fn rejects_invalid_time() {
        assert!(parse_text_schedule("今天 24:90 直播", published(), china_now(), "直播").is_none());
    }

    #[test]
    fn reserve_title_strips_bilibili_prefix() {
        let reserve = ReserveCard {
            title: "直播预约：飓风商店夏促".to_owned(),
            description: "10月30日 20:00 直播".to_owned(),
            room_id: Some("4003487".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        let value = parse_reserve_schedule(&reserve, published(), china_now()).expect("schedule");
        assert_eq!(value.title, "飓风商店夏促");
        assert_eq!(value.confidence, 1.0);

        let mut half_width = reserve.clone();
        half_width.title = "直播预约: 雨爱特别回".to_owned();
        let value =
            parse_reserve_schedule(&half_width, published(), china_now()).expect("schedule");
        assert_eq!(value.title, "雨爱特别回");

        let mut plain = reserve;
        plain.title = "飓风商店夏促".to_owned();
        let value = parse_reserve_schedule(&plain, published(), china_now()).expect("schedule");
        assert_eq!(value.title, "飓风商店夏促");
    }

    #[test]
    fn rejects_video_reservation_even_with_a_valid_time() {
        let reserve = ReserveCard {
            title: "视频预约：雨爱".to_owned(),
            description: "预计今天 17:20发布".to_owned(),
            room_id: Some("5848306".to_owned()),
            jump_url: Some("https://www.bilibili.com/video/BV1dBxA6PEcw/".to_owned()),
            stype: Some(1),
        };
        assert!(parse_reserve_schedule(&reserve, published(), china_now()).is_none());
    }

    #[test]
    fn reserve_uses_date_from_title_and_rejects_weekday_only() {
        let dated = ReserveCard {
            title: "直播预约：10月30日 周五杂谈".to_owned(),
            description: "20:00 直播".to_owned(),
            room_id: Some("4003487".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        let value =
            parse_reserve_schedule(&dated, published(), china_now()).expect("dated reserve");
        assert_eq!(value.start_at.to_string(), "2026-10-30 12:00:00 UTC");

        let weekday_only = ReserveCard {
            title: "直播预约：周五杂谈".to_owned(),
            description: "20:00 直播".to_owned(),
            room_id: Some("4003487".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        assert!(parse_reserve_schedule(&weekday_only, published(), china_now()).is_none());
    }

    #[test]
    fn parses_reserve_with_hyphen_separated_month_day() {
        let reserve = ReserveCard {
            title: "直播预约：【突击】和塔菲一起玩闹鬼爪子".to_owned(),
            description: "11-02 20:00 直播".to_owned(),
            room_id: Some("5848335".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        let value = parse_reserve_schedule(&reserve, published(), china_now()).expect("schedule");
        assert_eq!(value.title, "【突击】和塔菲一起玩闹鬼爪子");
        assert_eq!(value.start_at.to_string(), "2026-11-02 12:00:00 UTC");
    }

    #[test]
    fn parses_hyphen_separated_month_day() {
        let value = parse_text_schedule("11-02 20:00 直播", published(), china_now(), "直播")
            .expect("month-day date");
        assert_eq!(value.start_at.to_string(), "2026-11-02 12:00:00 UTC");
    }

    #[test]
    fn parses_next_year_month_day_across_new_year() {
        let now = DateTime::parse_from_rfc3339("2026-12-31T12:00:00+08:00")
            .expect("now")
            .with_timezone(&Utc);
        let published = DateTime::parse_from_rfc3339("2026-12-31T10:00:00+08:00")
            .expect("published")
            .with_timezone(&Utc);
        let value = parse_text_schedule("1-10 20:00 直播", published, now, "直播")
            .expect("next-year schedule");
        assert_eq!(value.start_at.to_string(), "2027-01-10 12:00:00 UTC");
    }

    #[test]
    fn does_not_roll_past_month_day_far_from_new_year() {
        let now = DateTime::parse_from_rfc3339("2027-01-20T12:00:00+08:00")
            .expect("now")
            .with_timezone(&Utc);
        let published = DateTime::parse_from_rfc3339("2027-01-20T10:00:00+08:00")
            .expect("published")
            .with_timezone(&Utc);
        assert!(parse_text_schedule("1-10 20:00 直播", published, now, "直播").is_none());
    }

    #[test]
    fn parses_dot_separated_date() {
        let value = parse_text_schedule("10.30 20:00 直播", published(), china_now(), "直播")
            .expect("dot date");
        assert_eq!(value.start_at.to_string(), "2026-10-30 12:00:00 UTC");
    }

    #[test]
    fn parses_slash_date() {
        let value = parse_text_schedule("10/31 21:00 万圣节直播", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-10-31 13:00:00 UTC");
    }

    #[test]
    fn parses_future_time_equal_publish_day() {
        let value = parse_text_schedule("22:30 深夜杂谈直播", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-10-26 14:30:00 UTC");
    }

    #[test]
    fn parses_next_week_weekday() {
        let value = parse_text_schedule("下周二 20:00 直播", published(), china_now(), "直播")
            .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-11-03 12:00:00 UTC");
    }

    #[test]
    fn rejects_non_schedule_and_impossible_dates() {
        assert!(parse_text_schedule("今天吃了火锅", published(), china_now(), "直播").is_none());
        assert!(
            parse_text_schedule("2月30日 20:00 直播", published(), china_now(), "直播").is_none()
        );
        assert!(parse_text_schedule("今天 09:00 直播", published(), china_now(), "直播").is_none());
    }

    #[test]
    fn game_version_number_is_not_a_dot_date() {
        // 回归：【2D鸣潮】2.6下！ 曾被当成 2 月 6 日直播，跨年后落到 2027-02-06，
        // 现场次真实时间（2026-11-09 18:00）在日历上整场消失。
        let publish = DateTime::parse_from_rfc3339("2026-11-07T20:25:12+08:00")
            .expect("publish")
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2026-11-09T14:29:00+08:00")
            .expect("now")
            .with_timezone(&Utc);
        let value = parse_text_schedule(
            "奥古斯塔是一个很有魅力的人～\n感觉今晚吃饭都更香了～\n下周一18:00继续2.6下哦！",
            publish,
            now,
            "直播",
        )
        .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-11-09 10:00:00 UTC");
    }

    #[test]
    fn version_number_dot_date_does_not_roll_into_next_year() {
        // “2.6” 与 11 月相隔 90 天，不该被推断成明年 2 月 6 日的预约；
        // 认不出来时回落到发布日，而不是跨年。
        let value = parse_text_schedule("2.6 20:00 直播", published(), china_now(), "直播")
            .expect("fallback to publication day");
        assert_eq!(value.start_at.to_string(), "2026-10-26 12:00:00 UTC");
    }

    #[test]
    fn relative_day_wins_over_version_number() {
        // 标题里的版本号不该覆盖描述里的“今天”。
        let value = parse_text_schedule(
            "【2D鸣潮】2.6下！ 今天 18:00 直播",
            published(),
            china_now(),
            "直播",
        )
        .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-10-26 10:00:00 UTC");
    }

    #[test]
    fn reserve_relative_day_is_anchored_to_publication() {
        // “今天 18:00” 是相对发布时刻说的；发布时 18:00 已过，就不该再产出一场。
        let publish = DateTime::parse_from_rfc3339("2026-11-07T20:25:12+08:00")
            .expect("publish")
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2026-11-09T14:29:00+08:00")
            .expect("now")
            .with_timezone(&Utc);
        let reserve = ReserveCard {
            title: "直播预约：【2D鸣潮】2.6下！".to_owned(),
            description: "今天 18:00 直播".to_owned(),
            room_id: Some("30858592".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        assert!(parse_reserve_schedule(&reserve, publish, now).is_none());

        // 同一条卡片在发布当天解析，仍然给出当天的场次。
        let same_day = DateTime::parse_from_rfc3339("2026-11-07T17:00:00+08:00")
            .expect("now")
            .with_timezone(&Utc);
        let value = parse_reserve_schedule(&reserve, publish, same_day).expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-11-07 10:00:00 UTC");
        assert_eq!(value.title, "【2D鸣潮】2.6下！");
    }

    #[test]
    fn evening_chatter_does_not_swallow_next_week_slot() {
        // 同一条正文在抓取当天（now = 发布时刻）解析，也要落在下周一。
        let publish = DateTime::parse_from_rfc3339("2026-11-07T20:25:12+08:00")
            .expect("publish")
            .with_timezone(&Utc);
        let value = parse_text_schedule(
            "奥古斯塔是一个很有魅力的人～\n感觉今晚吃饭都更香了～\n下周一18:00继续2.6下哦！",
            publish,
            publish,
            "直播",
        )
        .expect("schedule");
        assert_eq!(value.start_at.to_string(), "2026-11-09 10:00:00 UTC");
    }
}
