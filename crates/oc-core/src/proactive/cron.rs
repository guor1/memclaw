//! 极简 5 字段 cron 解析 + 下次触发计算（设计 §4.7）。
//!
//! 字段：`分 时 日 月 周`。每字段支持 `*`、`*/n`、`a-b`、`a,b,c` 及其组合（如 `1,3,5-7`）。
//! 周：0=周日..6=周六。
//!
//! **表达式按给定时区解释**（P1-5 修复）：`next_fire(expr, after, tz)` 的 `tz` 是 IANA
//! 名（如 `Asia/Shanghai`）。此前一律按 UTC 解释、`tz` 字段存了没人读，导致用户说
//! 「21:46 提醒我」→ 存成 21:46 UTC → 本地次日 05:46 才到期（东八区差 8 小时）。
//!
//! 实现用"逐分钟前进 + 匹配"，上限 366 天（超出返回 None）。不引入外部 cron crate。
//!
//! **DST 边界**（夏令时，个人助手场景下可接受的简化）：
//! - 春季跳表：本地不存在的时刻（如 02:30 被跳过）当天不触发，顺延到下一个匹配日。
//! - 秋季回拨：重复出现的本地时刻可能触发两次。一次性任务（`once`）由 store 侧
//!   claim 保证只触发一次，重复任务在这一天多触发一次不影响可用性。

use time::{Duration, OffsetDateTime};
use time_tz::{Offset, TimeZone, Tz};

/// cron 表达式解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronParseError {
    /// 字段数不是 5。
    WrongFieldCount,
    /// 某字段值非法（越界或格式错误）。
    BadField(&'static str),
    /// 时区名不是已知的 IANA 名（如拼错 `Asia/Shanghia`）。
    BadTimezone(String),
}

impl std::fmt::Display for CronParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CronParseError::WrongFieldCount => write!(f, "cron 表达式必须是 5 个字段：分 时 日 月 周"),
            CronParseError::BadField(name) => write!(f, "cron 字段非法：{name}"),
            CronParseError::BadTimezone(tz) => {
                write!(f, "未知时区：{tz}（需 IANA 名，如 Asia/Shanghai）")
            }
        }
    }
}

impl std::error::Error for CronParseError {}

/// 解析 IANA 时区名。空串视为 UTC（兼容旧数据：M6 建的行 tz 可能是 "UTC" 或空）。
fn resolve_tz(tz: &str) -> Result<&'static Tz, CronParseError> {
    let name = tz.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("utc") {
        // time_tz 认得 "UTC"，但空串不认，统一在此归一。
        return time_tz::timezones::get_by_name("UTC")
            .ok_or_else(|| CronParseError::BadTimezone("UTC".to_string()));
    }
    time_tz::timezones::get_by_name(name)
        .ok_or_else(|| CronParseError::BadTimezone(name.to_string()))
}

/// 解析后的 cron 计划：每字段一个允许值集合（已按范围展开）。
struct Schedule {
    minutes: Vec<u8>, // 0..=59
    hours: Vec<u8>,   // 0..=23
    doms: Vec<u8>,    // 1..=31
    months: Vec<u8>,  // 1..=12
    dows: Vec<u8>,    // 0..=6 (周日=0)
}

/// 计算 `after`（unix 秒）**之后**下一次触发的 unix 秒。无匹配（366 天内）返回 None。
///
/// `tz`：IANA 时区名（如 `Asia/Shanghai`）。表达式里的「时/分」按**该时区的墙上时间**
/// 解释；返回值仍是 unix 秒（绝对时刻）。空串或 `UTC` = 按 UTC 解释。
pub fn next_fire(expr: &str, after_secs: i64, tz: &str) -> Result<Option<i64>, CronParseError> {
    let sched = parse(expr)?;
    let zone = resolve_tz(tz)?;
    let start = OffsetDateTime::from_unix_timestamp(after_secs)
        .map_err(|_| CronParseError::BadField("after"))?;

    // 从下一整分钟开始（丢弃秒），逐分钟前进。前进在 UTC 轴上做（每步恒为 60s，
    // 不受 DST 影响）；匹配时把该时刻投影到目标时区看墙上时间。这样跳表被自然跳过、
    // 回拨时段的重复墙上时间会各匹配一次。
    let mut t = start
        .replace_second(0)
        .and_then(|t| t.replace_nanosecond(0))
        .unwrap_or(start)
        + Duration::minutes(1);

    let limit = start + Duration::days(366);
    while t <= limit {
        // 投影到目标时区：取该 unix 时刻在 tz 下的偏移，换算出墙上时间。
        let offset = zone.get_offset_utc(&t).to_utc();
        if sched.matches(&t.to_offset(offset)) {
            return Ok(Some(t.unix_timestamp()));
        }
        t += Duration::minutes(1);
    }
    Ok(None)
}

impl Schedule {
    /// `t` 必须已投影到目标时区（其 `hour()`/`day()` 等即墙上时间）。
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

/// 把 unix 秒渲染成 `tz` 下的墙上时间（`YYYY-MM-DD HH:MM`）。未知时区返回 `None`。
///
/// 存在的理由：回执给模型/用户看的必须是**本地时间**。真机上模型只拿到 unix 秒，
/// 没法向用户复述「几点触发」，于是也无从发现自己把时区算错了 8 小时。
pub fn fmt_in_tz(ts: i64, tz: &str) -> Option<String> {
    let zone = resolve_tz(tz).ok()?;
    let t = OffsetDateTime::from_unix_timestamp(ts).ok()?;
    let local = t.to_offset(zone.get_offset_utc(&t).to_utc());
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year(),
        local.month() as u8,
        local.day(),
        local.hour(),
        local.minute()
    ))
}

/// 一次性延时任务的表达式标记（P1-5）。
///
/// cron 最小粒度是分钟，无法表达「10 秒后」；且「N 分钟后」用重复表达式会每天
/// 再触发一次（真机上就踩了：用户要 90 秒后提醒，得到一条「每日 21:46」）。
/// 故一次性任务不走表达式匹配——`next_at` 直接存绝对秒，`expr` 存此标记供展示与识别。
pub const ONCE_EXPR: &str = "@once";

/// 是否是一次性延时任务（`next_at` 为绝对时刻，不参与表达式推算）。
pub fn is_once(expr: &str) -> bool {
    expr.trim() == ONCE_EXPR
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

    use time::macros::datetime;

    // 2024-01-01 00:00:00 UTC = 周一。
    const BASE: i64 = 1_704_067_200;
    /// 旧行为（UTC 语义）的用例统一用这个 tz，保持断言不变。
    const UTC: &str = "UTC";
    const SH: &str = "Asia/Shanghai"; // 东八，无 DST
    const NY: &str = "America/New_York"; // 有 DST，用于跳表/回拨用例

    #[test]
    fn every_five_minutes() {
        // */5：下一分钟起找 5 的倍数分钟。从 00:00 之后 → 00:05。
        let next = next_fire("*/5 * * * *", BASE, UTC).unwrap().unwrap();
        assert_eq!(next, BASE + 5 * 60);
    }

    #[test]
    fn daily_fixed_time() {
        // 每天 09:30。从 00:00 → 当天 09:30。
        let next = next_fire("30 9 * * *", BASE, UTC).unwrap().unwrap();
        assert_eq!(next, BASE + 9 * 3600 + 30 * 60);
    }

    #[test]
    fn next_day_when_passed() {
        // 已过当天触发点 → 次日。基准 10:00，任务 09:00 → 次日 09:00。
        let at_10 = BASE + 10 * 3600;
        let next = next_fire("0 9 * * *", at_10, UTC).unwrap().unwrap();
        assert_eq!(next, BASE + 24 * 3600 + 9 * 3600);
    }

    #[test]
    fn weekday_match() {
        // 每周五 08:00。2024-01-01 是周一；本周五 = 01-05。
        let next = next_fire("0 8 * * 5", BASE, UTC).unwrap().unwrap();
        let expected = BASE + 4 * 86400 + 8 * 3600; // +4 天到周五
        assert_eq!(next, expected);
    }

    #[test]
    fn list_and_range() {
        // 分钟 0,30；小时 9-11。第一个匹配 09:00。
        let next = next_fire("0,30 9-11 * * *", BASE, UTC).unwrap().unwrap();
        assert_eq!(next, BASE + 9 * 3600);
    }

    #[test]
    fn invalid_expressions() {
        assert_eq!(next_fire("* * * *", BASE, UTC), Err(CronParseError::WrongFieldCount));
        assert!(matches!(next_fire("60 * * * *", BASE, UTC), Err(CronParseError::BadField(_))));
        assert!(matches!(next_fire("*/0 * * * *", BASE, UTC), Err(CronParseError::BadField(_))));
        assert!(matches!(next_fire("abc * * * *", BASE, UTC), Err(CronParseError::BadField(_))));
    }

    #[test]
    fn impossible_date_returns_none() {
        // 2 月 30 日永不存在 → 366 天内无匹配。
        assert_eq!(next_fire("0 0 30 2 *", BASE, UTC).unwrap(), None);
    }

    /// **P1-5 修复的真机 bug**：用户 21:44（本地，东八）说「90 秒后提醒」，
    /// 模型建 `46 21 * * *` + `tz=Asia/Shanghai`，期望 2 分钟后触发。
    ///
    /// 旧实现按 UTC 解释 → 存 21:46 UTC = 本地次日 05:46，差 8 小时，当晚永不触发。
    /// 这是本次修复的核心回归。
    #[test]
    fn shanghai_expression_fires_at_local_wall_clock() {
        // 真机日志时刻：13:44:33 UTC = 本地（东八）21:44:33。
        let now = datetime!(2026-09-01 13:44:33 UTC).unix_timestamp();
        let next = next_fire("46 21 * * *", now, SH).unwrap().unwrap();
        // 期望：本地 21:46 = 同日 13:46 UTC（约 87 秒后），而非次日 05:46。
        assert_eq!(next, datetime!(2026-09-01 13:46:00 UTC).unix_timestamp());
        assert!(next - now < 120, "应 2 分钟内触发，而非 8 小时后");
    }

    /// 同一表达式在不同 tz 下必须给出不同的绝对时刻（差正好是偏移量）。
    #[test]
    fn same_expression_differs_by_offset_across_tz() {
        let utc_next = next_fire("0 12 * * *", BASE, UTC).unwrap().unwrap();
        let sh_next = next_fire("0 12 * * *", BASE, SH).unwrap().unwrap();
        // 本地 12:00 东八 = 04:00 UTC，比 UTC 的 12:00 早 8 小时。
        assert_eq!(utc_next - sh_next, 8 * 3600);
    }

    /// 空 tz 兼容旧数据（M6 建的行可能是空串），按 UTC 处理而非报错。
    #[test]
    fn empty_tz_falls_back_to_utc() {
        let empty = next_fire("30 9 * * *", BASE, "").unwrap().unwrap();
        let utc = next_fire("30 9 * * *", BASE, UTC).unwrap().unwrap();
        assert_eq!(empty, utc);
    }

    #[test]
    fn unknown_tz_is_rejected() {
        let err = next_fire("0 9 * * *", BASE, "Asia/Shanghia").unwrap_err();
        assert!(matches!(err, CronParseError::BadTimezone(_)), "拼错的时区名应报错而非静默按 UTC");
    }

    /// DST 跳表：美东 2024-03-10 当地 02:00 直接跳到 03:00，故 **02:30 这个墙上时间
    /// 当天不存在**。该日不应触发，顺延到次日同一墙上时间——不报错、不卡死。
    #[test]
    fn dst_spring_forward_skips_nonexistent_local_time() {
        // 起点取 3/10 当地 00:00（EST = UTC-5）→ 05:00 UTC。当天 02:30 不存在。
        let start = datetime!(2024-03-10 05:00:00 UTC).unix_timestamp();
        let next = next_fire("30 2 * * *", start, NY)
            .unwrap()
            .expect("应顺延到次日，而非 None");
        // 次日 3/11 的 02:30 EDT（UTC-4）= 06:30 UTC。
        assert_eq!(next, datetime!(2024-03-11 06:30:00 UTC).unix_timestamp());
    }

    /// 对照组：同一表达式在**没有**跳表的前一天（3/9）应当天就触发，
    /// 证明上面那条的顺延确实来自 DST，而不是实现总是跳一天。
    #[test]
    fn normal_day_fires_same_day() {
        let start = datetime!(2024-03-09 05:00:00 UTC).unix_timestamp(); // 3/9 当地 00:00
        let next = next_fire("30 2 * * *", start, NY).unwrap().unwrap();
        assert_eq!(next, datetime!(2024-03-09 07:30:00 UTC).unix_timestamp()); // 02:30 EST
    }

    #[test]
    fn once_marker_recognized() {
        assert!(is_once(ONCE_EXPR));
        assert!(is_once("  @once  "));
        assert!(!is_once("0 9 * * *"));
    }
}
