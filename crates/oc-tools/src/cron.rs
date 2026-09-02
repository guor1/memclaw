//! cron 工具（P1-5）：让模型自己管理定时提醒，而不是在 run 内阻塞等待。
//!
//! 设计 §12.2。cron 能力早就实现了（proactive.rs + CLI），但模型看不见也用不上，
//! 于是真机上出现两类退化：
//! 1. 想在一次 run 内**同步等到点**——反复查时间 + `Start-Sleep` 硬凑，撞 loop detection。
//! 2. 建完任务就失联——没有查询/删除手段，用户说「没收到」时只能猜，
//!    猜成「本环境 cron 送不到」并改用更不可靠的替代方案。
//!
//! 故本工具四个 op 一起给：`add`（重复）/ `delay`（一次性）/ `list` / `rm`。
//! 其中 `delay` 不可省——cron 最小粒度是分钟，「10 秒后提醒我」无法用表达式表达。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ToolError, ToolResult};
use crate::types::{CronOp, ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// cron 工具：定时提醒的增删查。
///
/// 落地委托编排层（`ToolCtx::cron` 门 → server proactive），本层只做参数校验。
pub struct CronTool;

#[derive(Deserialize)]
struct Args {
    op: String,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    secs: Option<i64>,
    #[serde(default)]
    id: Option<String>,
}

#[async_trait]
impl Tool for CronTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cron".to_string(),
            description: "管理定时/延时提醒（到点由系统主动推送给用户，不占用当前对话）。\
                          op=delay 用于「N 秒/分钟后提醒我」；op=add 用于「每天/每周某时提醒我」；\
                          op=list 查看已设任务及下次触发时间；op=rm 删除。\
                          需要等待未来某个时刻时**必须**用本工具，绝不要用 shell 睡眠去阻塞等待。"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["add", "delay", "list", "rm"],
                        "description": "add=重复提醒；delay=一次性延时提醒；list=列出；rm=删除"
                    },
                    "expr": {
                        "type": "string",
                        "description": "仅 op=add：5 字段 cron 表达式（分 时 日 月 周）。\
                                        例：'50 12 * * *'=每天 12:50；'0 9 * * 1-5'=工作日 9:00。\
                                        时/分按 tz 的本地时间解释"
                    },
                    "secs": {
                        "type": "integer",
                        "description": "仅 op=delay：多少秒后触发（一次性，不重复）。\
                                        「10 秒后」传 10，「5 分钟后」传 300"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "op=add/delay 必填：到点要提醒用户的内容，如 '提醒喝水'"
                    },
                    "tz": {
                        "type": "string",
                        "description": "仅 op=add：IANA 时区名（如 'Asia/Shanghai'）。\
                                        省略则用本机时区——通常省略即可"
                    },
                    "id": {
                        "type": "string",
                        "description": "仅 op=rm：要删除的任务 id（先用 op=list 查）"
                    }
                },
                "required": ["op"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            // 设个提醒不危险，且不需要用户逐次确认。
            may_need_approval: false,
            // 纯本地库操作 + 一次 channel 往返，正常是毫秒级。
            timeout: std::time::Duration::from_secs(10),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let a: Args =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;

        let Some(gate) = &cx.cron else {
            // 无 cron 门（非常驻场景/测试）→ 明确说不支持，别让模型以为设成功了。
            return Ok(ToolOutput::err(
                "无法管理定时任务：当前会话未接入 cron 调度。",
            ));
        };

        // 参数校验放在工具层：错参数不该占用一次 channel 往返，且报错能直接指明缺哪个字段。
        let op = match a.op.trim() {
            "add" => {
                let expr = require(a.expr, "expr", "op=add")?;
                let prompt = require(a.prompt, "prompt", "op=add")?;
                CronOp::Add { expr, prompt, tz: a.tz }
            }
            "delay" => {
                let secs = a
                    .secs
                    .ok_or_else(|| ToolError::BadArgs("op=delay 需要 secs（延时秒数）".into()))?;
                let prompt = require(a.prompt, "prompt", "op=delay")?;
                CronOp::Delay { secs, prompt }
            }
            "list" => CronOp::List,
            "rm" => CronOp::Rm { id: require(a.id, "id", "op=rm")? },
            other => {
                return Err(ToolError::BadArgs(format!(
                    "未知 op: {other}（可用：add/delay/list/rm）"
                )))
            }
        };

        match gate.call(op).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            // 表达式非法/时区拼错等属可纠正的用法错误：回 err 让模型看到原因并重试，
            // 而不是抛 ToolError 让整轮显示为工具故障。
            Err(msg) => Ok(ToolOutput::err(msg)),
        }
    }
}

/// 取必填字段，缺了就报明确的错（含所属 op，便于模型自我纠正）。
fn require(v: Option<String>, field: &str, ctx: &str) -> ToolResult<String> {
    match v {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(ToolError::BadArgs(format!("{ctx} 需要 {field}"))),
    }
}
