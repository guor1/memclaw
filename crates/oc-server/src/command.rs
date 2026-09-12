//! 斜杠指令（`/...`）的唯一解析与执行层。
//!
//! 客户端（TUI / Web UI）只负责「这条输入是不是以 `/` 开头」，是就把它作为
//! [`CommandParams`] 发来，由这里解析、执行、格式化。新增指令只需改本模块一处，
//! 两个端自动获得——避免每个终端各写一份解析器、文案与行为逐步漂移（此前 TUI 的
//! `parse_input` + `HELP_LINES` 就与 Web UI「完全没实现」脱节）。
//!
//! 指令命名对齐 openclaw（见 `docs/tools/slash-commands.md`），但只收**本后端有
//! 能力支撑**的子集：会话/运行动作 + 只读查询。换模型、directive（think/fast/…）、
//! loop/learn/config/mcp/plugins/acp/btw 等无对应基础设施，不做。

use std::sync::Arc;

use oc_proto::{CommandParams, CommandResult, ProtoError, SessionId};

use crate::state::ServerState;

/// 解析并执行一条斜杠指令。
pub async fn handle_command(
    p: &CommandParams,
    state: &Arc<ServerState>,
) -> Result<CommandResult, ProtoError> {
    let text = p.text.trim();
    let session = p.session.clone().unwrap_or_else(SessionId::main);

    // 拆出「指令名 + 空白 + 参数」。名字大小写不敏感（`/Help` 也能用）。
    let (name, arg) = match text.split_once(char::is_whitespace) {
        Some((n, a)) => (n.to_ascii_lowercase(), a.trim().to_string()),
        None => (text.to_ascii_lowercase(), String::new()),
    };

    match name.as_str() {
        "/help" => Ok(CommandResult { text: help_text(), switch_session: None, clear_view: None }),
        "/new" => {
            let id = new_session_id();
            Ok(CommandResult {
                text: format!("已切到新会话「{id}」（旧会话保留，可用 /session <id> 切回）"),
                switch_session: Some(SessionId::new(id)),
                clear_view: Some(session),
            })
        }
        "/session" => {
            if arg.is_empty() {
                return Err(bad_request("用法：/session <id>（不带参数视为打错，不静默吞掉）"));
            }
            Ok(CommandResult {
                text: format!("已切换到会话「{arg}」（新 id 将在首次发送时创建）"),
                switch_session: Some(SessionId::new(arg)),
                clear_view: None,
            })
        }
        // openclaw 用 `/reset`；`/clear` 更符合直觉，两个都收（帮助只列 `/clear`）。
        "/clear" | "/reset" => {
            crate::dispatch::reset_session(state, &session).await?;
            Ok(CommandResult {
                text: "已清空当前会话上下文（历史保留在库中，不再进入提示词）".to_string(),
                switch_session: None,
                clear_view: Some(session),
            })
        }
        "/compact" => {
            crate::dispatch::compact_session(state, &session).await;
            Ok(CommandResult {
                text: "已请求压缩上下文（摘要将在后台生成）".to_string(),
                switch_session: None,
                clear_view: None,
            })
        }
        "/stop" => {
            // 中止该会话当前活跃 run：从诊断注册表拿 run_id，不再靠客户端自己记。
            let active = state
                .diag()
                .snapshot_session(&session)
                .and_then(|d| d.active.map(|r| r.run_id));
            match active {
                Some(run_id) => {
                    state.registry().abort_all(run_id.clone(), true).await;
                    Ok(CommandResult {
                        text: format!("已请求停止 run「{}」", run_id.as_str()),
                        switch_session: None,
                        clear_view: None,
                    })
                }
                None => Ok(CommandResult {
                    text: "当前没有进行中的回合可停止".to_string(),
                    switch_session: None,
                    clear_view: None,
                }),
            }
        }
        "/sessions" => {
            let text = crate::dispatch::sessions_text(state, &session).await?;
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/status" => {
            let text = crate::dispatch::status_text(state, &session);
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/model" => {
            let text = crate::dispatch::model_text(state);
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/tasks" => {
            let text = crate::dispatch::tasks_text(state);
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/cron" => {
            if arg.trim() != "list" {
                return Err(bad_request("用法：/cron list（本后端只读展示定时任务）"));
            }
            let text = crate::dispatch::cron_text(state).await?;
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/intent" => {
            if arg.trim() != "list" {
                return Err(bad_request("用法：/intent list（本后端只读展示话题待办）"));
            }
            let text = crate::dispatch::intent_text(state).await?;
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/memory" => {
            // 只认 `search <query>`；`list` 等无对应能力，明确拒绝而非误读。
            let Some(query) = arg.strip_prefix("search") else {
                return Err(bad_request("用法：/memory search <关键词>"));
            };
            let query = query.trim();
            if query.is_empty() {
                return Err(bad_request("用法：/memory search <关键词>"));
            }
            let text = crate::dispatch::memory_text(state, query).await?;
            Ok(CommandResult { text, switch_session: None, clear_view: None })
        }
        "/whoami" | "/id" => Ok(CommandResult {
            text: format!("当前会话：{}", session.as_str()),
            switch_session: None,
            clear_view: None,
        }),
        _ => Err(bad_request(&format!(
            "未知指令「{text}」。可用指令见 /help（不会当成聊天发出去）"
        ))),
    }
}

fn bad_request(msg: &str) -> ProtoError {
    ProtoError { kind: oc_proto::ErrorKind::BadRequest, message: msg.to_string() }
}

/// 指令一览（单一事实源）：用法 + 说明，供 [`help_text`] 排版。
const HELP_ROWS: &[(&str, &str)] = &[
    ("/help", "显示本说明"),
    ("/new", "开一个新会话（旧会话保留）"),
    ("/session <id>", "切换到指定会话（不存在则首次发送时创建）"),
    ("/sessions", "列出所有会话"),
    ("/clear", "清空当前会话上下文（历史保留在库中，不再进提示词）"),
    ("/compact", "压缩当前上下文（摘要旧历史，保留语义）"),
    ("/stop", "中止当前进行中的回合"),
    ("/status", "运行状态：活跃 run / 排队 / 上下文用量 / 模型"),
    ("/model", "当前生效的模型与 provider / 端点"),
    ("/tasks", "列出后台任务"),
    ("/cron list", "列出定时任务"),
    ("/intent list", "列出话题待办"),
    ("/memory search <q>", "按关键词检索记忆"),
    ("/whoami", "显示当前会话 id（别名 /id）"),
];

/// 帮助文案（两端一致）。
///
/// 用法列按最长项对齐，且**至少留两个空格**再接说明——手工补空格容易漂移，而
/// 「用法 + 2 空格 + 说明」这个形状同时是 Web UI 识别两列输出、渲染成对齐网格的
/// 依据（见 `CommandCard.vue`）；一格空格的行会退化成纯文本块。
fn help_text() -> String {
    let width = HELP_ROWS.iter().map(|(usage, _)| usage.chars().count()).max().unwrap_or(0);
    HELP_ROWS
        .iter()
        .map(|(usage, desc)| format!("{usage:<width$}  {desc}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/new` 的新会话 id：本地时间戳（秒），让 `/sessions` 列出时能一眼看出先后。
fn new_session_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("s{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条 help 行都以一个可识别指令开头——文案与解析不能脱节。
    #[test]
    fn help_lines_are_all_known_commands() {
        for line in help_text().lines() {
            let usage = line.split_whitespace().next().expect("非空帮助行");
            assert!(
                matches!(usage, "/help" | "/new" | "/session" | "/sessions" | "/clear" | "/compact"
                    | "/stop" | "/status" | "/model" | "/tasks" | "/cron" | "/intent"
                    | "/memory" | "/whoami"),
                "帮助里宣传了 {usage}，但 handle_command 不识别"
            );
        }
    }

    /// `/new` 生成的 id 要能被 `/session` 切回去（不含空格等会被拆分吃的字符）。
    #[test]
    fn new_session_id_is_session_routable() {
        let id = new_session_id();
        assert!(!id.contains(char::is_whitespace), "id 不应含空白: {id}");
        assert!(id.starts_with('s'));
    }
}
