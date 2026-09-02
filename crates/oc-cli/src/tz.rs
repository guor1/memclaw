//! 本机时区探测（P1-5）。
//!
//! 为什么在 CLI 而不是 server：`time` crate 取本地 UTC 偏移在多线程进程里不可靠
//! （Unix 上直接返回错误），而 daemon 是重度多线程的。CLI 进程启动早期是单线程，
//! 这里探到 IANA 名后传给 server，server 只按名字查表、不碰系统时区。
//!
//! 两个使用方：`provider_setup`（填 `SessionConfig::default_tz`，供 cron 工具缺省）
//! 与 `cli_client`（`oc cron add` 未指定 `--tz` 时的缺省）。

/// 探测本机 IANA 时区名（如 `Asia/Shanghai`）；探测失败退回 `UTC`。
///
/// 失败时**告警而非静默**：静默按 UTC 正是 P1-5 那个「定时提醒永不触发」缺陷的
/// 表现形态——用户看到任务建好了，却差了整个时区偏移。
pub fn local_tz() -> String {
    match time_tz::system::get_timezone() {
        Ok(tz) => time_tz::TimeZone::name(tz).to_string(),
        Err(e) => {
            eprintln!("[warn] 无法探测本机时区（{e}），按 UTC 处理；建议显式指定时区");
            "UTC".to_string()
        }
    }
}
