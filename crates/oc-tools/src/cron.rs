//! cron 工具（P1-5）：让模型直接创建定时任务，而不是在 run 内阻塞等待。
//!
//! 设计 §12.2；此前 cron 能力已实现（proactive.rs + CLI），但模型无法调用。
//! 本工具暴露 `cron_add` 让模型在遇到「X 时提醒我做 Y」时能秒设任务并即时反馈。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ToolError, ToolResult};
use crate::types::{ToolCtx, ToolOutput, ToolPolicy, ToolSpec};
use crate::Tool;

/// cron 工具：创建定时任务。
///
/// 模型调此工具 → 通过 `ToolCtx::cron_gate` 回调外层（ToolExecutor）写库 + 重排调度。
/// 解耦：工具层不依赖 store / proactive，由编排层（tools_bridge）桥接。
pub struct CronTool;

#[derive(Deserialize)]
struct CronAddArgs {
    /// cron 表达式（如 "50 12 * * *" = 每天 12:50）。
    expr: String,
    /// 触发时注入的 prompt（如"提醒我喝水"）。
    prompt: String,
    /// 时区（如 "Asia/Shanghai"）。
    #[serde(default = "default_tz")]
    tz: String,
}

fn default_tz() -> String {
    "UTC".to_string()
}

#[async_trait]
impl Tool for CronTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cron_add".to_string(),
            description: "创建定时任务：指定时刻触发提醒。用于「X 时提醒我做 Y」\
                         「每天 N 点…」等场景。**不要用 shell 睡眠阻塞 run 等待**。"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "expr": {
                        "type": "string",
                        "description": "cron 表达式（5 字段：分 时 日 月 周，* 表通配）。\
                                       例：'50 12 * * *' = 每天 12:50；'0 9 * * 1-5' = 工作日 9:00"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "触发时注入的提醒内容，如 '提醒我喝水'"
                    },
                    "tz": {
                        "type": "string",
                        "description": "时区（IANA 格式，如 'Asia/Shanghai'）。省略则 UTC"
                    }
                },
                "required": ["expr", "prompt"]
            }),
        }
    }

    fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            may_need_approval: false, // 创建提醒不危险
            timeout: std::time::Duration::from_secs(5),
            backgroundable: false,
        }
    }

    async fn invoke(&self, args: serde_json::Value, cx: ToolCtx) -> ToolResult<ToolOutput> {
        let a: CronAddArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;

        let Some(gate) = &cx.cron else {
            // 无 cron 门（如非常驻场景）→ 无法创建定时任务，保守失败。
            return Ok(ToolOutput::err(
                "无法创建定时任务：当前会话不支持 cron 调度。",
            ));
        };

        // 委托外层校验表达式 + 写库 + 调度。
        let cron_id = gate
            .add(a.expr.clone(), a.prompt.clone(), a.tz.clone())
            .await
            .map_err(|e| ToolError::Failed(e))?;

        Ok(ToolOutput::text(format!(
            "定时任务已创建（id={}）：{} [{}] → 「{}」",
            cron_id, a.expr, a.tz, a.prompt
        )))
    }
}
