//! 极简 5 字段 cron 解析 + 下次触发计算（设计 §4.7）。
//!
//! 字段：`分 时 日 月 周`。每字段支持 `*`、`*/n`、`a-b`、`a,b,c` 及其组合（如 `1,3,5-7`）。
//! 语义为 **UTC**（tz 简化；秒级 unix 时间）。周：0=周日..6=周六。
//!
//! 实现用"逐分钟前进 + 匹配"，上限 366 天（超出返回 None）。不引入外部 cron crate。

use time::{Duration, OffsetDateTime};

/// cron 表达式解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronParseError {
    /// 字段数不是 5。
    WrongFieldCount,
    /// 某字段值非法（越界或格式错误）。
    BadField(&'static str),
}

impl std::fmt::Display for CronParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CronParseError::WrongFieldCount => write!(f, "cron 表达式必须是 5 个字段：分 时 日 月 周"),
            CronParseError::BadField(name) => write!(f, "cron 字段非法：{name}"),
        }
    }
}

impl std::error::Error for CronParseError {}

/// 解析后的 cron 计划：每字段一个允许值集合（已按范围展开）。
struct Schedule {
    minutes: Vec<u8>, // 0..=59
    hours: Vec<u8>,   // 0..=23
    doms: Vec<u8>,    // 1..=31
    months: Vec<u8>,  // 1..=12
    dows: Vec<u8>,    // 0..=6 (周日=0)
}

/// 计算 `after`（unix 秒）**之后**下一次触发的 unix 秒。无匹配（366 天内）返回 None。
pub fn next_fire(expr: &str, after_secs: i64) -> Result<Option<i64>, CronParseError> {
    let sched = parse(expr)?;
    let start = OffsetDateTime::from_unix_timestamp(after_secs)
        .map_err(|_| CronParseError::BadField("after"))?;

    // 从下一整分钟开始（丢弃秒），逐分钟前进。
    let mut t = start
        .replace_second(0)
        .and_then(|t| t.replace_nanosecond(0))
        .unwrap_or(start)
        + Duration::minutes(1);

    let limit = start + Duration::days(366);
    while t <= limit {
        if sched.matches(&t) {
            return Ok(Some(t.unix_timestamp()));
        }
        t += Duration::minutes(1);
    }
    Ok(None)
}

impl Schedule {
    fn matches(&self, t: &OffsetDateTime) -> bool {
        let dow = t.weekday().number_days_from_sunday(); // 周日=0
        self.minutes.contains(&(t.minute()))
            && self.hours.contains(&(t.hour()))
            && self.months.contains(&(t.month() as u8))
            // cron 惯例：day-of-month 与 day-of-week 若都非 `*` 取"或"；
            // 这里简化为都须匹配（AND），足够个人助手场景（多数只用其一）。
            && self.doms.contains(&(t.day()))
            && self.dows.contains(&dow)
    }
}

fn parse(expr: &str) -> Result<Schedule, CronParseError> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CronParseError::WrongFieldCount);
    }
    Ok(Schedule {
        minutes: parse_field(fields[0], 0, 59, "minute")?,
        hours: parse_field(fields[1], 0, 23, "hour")?,
        doms: parse_field(fields[2], 1, 31, "day-of-month")?,
        months: parse_field(fields[3], 1, 12, "month")?,
        dows: parse_field(fields[4], 0, 6, "day-of-week")?,
    })
}

/// 解析单个字段为允许值集合。支持 `*`、`*/n`、`a-b`、`a,b,c` 组合。
fn parse_field(field: &str, min: u8, max: u8, name: &'static str) -> Result<Vec<u8>, CronParseError> {
    let mut out: Vec<u8> = Vec::new();
    for part in field.split(',') {
        parse_part(part, min, max, name, &mut out)?;
    }
    if out.is_empty() {
        return Err(CronParseError::BadField(name));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn parse_part(part: &str, min: u8, max: u8, name: &'static str, out: &mut Vec<u8>) -> Result<(), CronParseError> {
    // 拆 step：`base/step`。
    let (base, step) = match part.split_once('/') {
        Some((b, s)) => {
            let step: u8 = s.parse().map_err(|_| CronParseError::BadField(name))?;
            if step == 0 {
                return Err(CronParseError::BadField(name));
            }
            (b, step)
        }
        None => (part, 1),
    };

    // base 展开为 [lo, hi] 区间。
    let (lo, hi) = if base == "*" {
        (min, max)
    } else if let Some((a, b)) = base.split_once('-') {
        let a: u8 = a.parse().map_err(|_| CronParseError::BadField(name))?;
        let b: u8 = b.parse().map_err(|_| CronParseError::BadField(name))?;
        (a, b)
    } else {
        let v: u8 = base.parse().map_err(|_| CronParseError::BadField(name))?;
        (v, v)
    };

    if lo < min || hi > max || lo > hi {
        return Err(CronParseError::BadField(name));
    }
    let mut v = lo;
    while v <= hi {
        out.push(v);
        v += step;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-01-01 00:00:00 UTC = 周一。
    const BASE: i64 = 1_704_067_200;

    #[test]
    fn every_five_minutes() {
        // */5：下一分钟起找 5 的倍数分钟。从 00:00 之后 → 00:05。
        let next = next_fire("*/5 * * * *", BASE).unwrap().unwrap();
        assert_eq!(next, BASE + 5 * 60);
    }

    #[test]
    fn daily_fixed_time() {
        // 每天 09:30。从 00:00 → 当天 09:30。
        let next = next_fire("30 9 * * *", BASE).unwrap().unwrap();
        assert_eq!(next, BASE + 9 * 3600 + 30 * 60);
    }

    #[test]
    fn next_day_when_passed() {
        // 已过当天触发点 → 次日。基准 10:00，任务 09:00 → 次日 09:00。
        let at_10 = BASE + 10 * 3600;
        let next = next_fire("0 9 * * *", at_10).unwrap().unwrap();
        assert_eq!(next, BASE + 24 * 3600 + 9 * 3600);
    }

    #[test]
    fn weekday_match() {
        // 每周五 08:00。2024-01-01 是周一；本周五 = 01-05。
        let next = next_fire("0 8 * * 5", BASE).unwrap().unwrap();
        let expected = BASE + 4 * 86400 + 8 * 3600; // +4 天到周五
        assert_eq!(next, expected);
    }

    #[test]
    fn list_and_range() {
        // 分钟 0,30；小时 9-11。第一个匹配 09:00。
        let next = next_fire("0,30 9-11 * * *", BASE).unwrap().unwrap();
        assert_eq!(next, BASE + 9 * 3600);
    }

    #[test]
    fn invalid_expressions() {
        assert_eq!(next_fire("* * * *", BASE), Err(CronParseError::WrongFieldCount));
        assert!(matches!(next_fire("60 * * * *", BASE), Err(CronParseError::BadField(_))));
        assert!(matches!(next_fire("*/0 * * * *", BASE), Err(CronParseError::BadField(_))));
        assert!(matches!(next_fire("abc * * * *", BASE), Err(CronParseError::BadField(_))));
    }

    #[test]
    fn impossible_date_returns_none() {
        // 2 月 30 日永不存在 → 366 天内无匹配。
        assert_eq!(next_fire("0 0 30 2 *", BASE).unwrap(), None);
    }
}
