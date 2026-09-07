//! oc-tools 集成测试：exec 审批门 + file 读写 + 结果净化。

use std::time::Duration;

use oc_core::tool::ApprovalMode;
use oc_tools::exec::ExecTool;
use oc_tools::file::FileTool;
use oc_tools::types::{ApprovalGate, ApprovalReply, ToolCtx};
use oc_tools::Tool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn exec_safe_command_runs() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30));
    let cx = ToolCtx::detached(CancellationToken::new());
    // echo 在 cmd.exe 与 POSIX sh 下写法一致，无需按平台分支。
    let echo = "echo hello";
    let out = tool
        .invoke(serde_json::json!({ "command": echo }), cx)
        .await
        .expect("exec ok");
    assert!(out.success);
    assert!(out.content.contains("hello"));
}

#[tokio::test]
async fn exec_dangerous_needs_approval_and_denied() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30));

    // 建审批门，自动拒绝。
    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            let _ = r.reply.send(ApprovalReply::Deny);
        }
    });

    let res = tool
        .invoke(serde_json::json!({ "command": "rm -rf /tmp/whatever" }), cx)
        .await;
    assert!(res.is_err(), "危险命令被拒应返回 Err");
}

/// 无人回执时，审批等待必须在 `approval_timeout` 后按拒绝收敛。
///
/// 这是本次修复的核心：cron / HTTP 网关没有 TUI 响应审批，若无上限，
/// 该轮会一直占着会话车道直到心跳的卡死诊断兜底（默认 360s）。
///
/// `start_paused` 用虚拟时钟：不真睡 120s，但仍走完整的 timeout 分支。
#[tokio::test(start_paused = true)]
async fn exec_approval_timeout_denies() {
    let tool = ExecTool::new(
        ApprovalMode::Prompt,
        Duration::from_secs(10),
        Duration::from_secs(120),
    );

    // 建审批门但**永不回执**——模拟无人值守。
    let (req_tx, _req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };

    let started = tokio::time::Instant::now();
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo rm -rf /tmp/whatever" }), cx)
        .await;

    assert!(res.is_err(), "无人回执应超时按拒绝处理，而非挂住");
    // 虚拟时钟：断言确实等满了超时，而不是被别的路径提前否掉。
    assert!(
        started.elapsed() >= Duration::from_secs(120),
        "应等满 approval_timeout，实际 {:?}",
        started.elapsed()
    );
}

/// `Duration::ZERO` = 不设上限（保留给交互式场景显式选择）。
/// 这里验证它不会退化成「立即拒绝」——那会让 TUI 下的审批完全不可用。
#[tokio::test(start_paused = true)]
async fn exec_approval_zero_timeout_waits_for_reply() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::ZERO);

    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };

    // 拖很久才回执：ZERO 应一直等，最终拿到用户的 Deny（而非超时的 Deny）。
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let _ = r.reply.send(ApprovalReply::Deny);
        }
    });

    let started = tokio::time::Instant::now();
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo rm -rf /tmp/whatever" }), cx)
        .await;

    assert!(res.is_err(), "用户拒绝应返回 Err");
    assert!(
        started.elapsed() >= Duration::from_secs(600),
        "ZERO 应不设上限、一直等到回执，实际 {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn exec_dangerous_approved_runs() {
    let tool = ExecTool::new(ApprovalMode::Prompt, Duration::from_secs(10), Duration::from_secs(30));
    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let cx = ToolCtx {
        cancel: CancellationToken::new(),
        emit: mpsc::unbounded_channel().0,
        approval: Some(ApprovalGate { request: req_tx }),
        input: None,
        cron: None,
        cwd: std::env::current_dir().unwrap(),
    };
    // 审批放行；命令本身用无害的 sudo 替身——这里用 echo 触发 needs-approval 的替代。
    // 用 "sudo" 前缀触发审批，但实际执行会因无 sudo 而失败/或在 win 上不识别；
    // 为可移植，改用一个被判定为 NeedsApproval 且能在两平台跑的命令不易得，
    // 故只验证"放行后进入执行路径"（不强求成功）。
    tokio::spawn(async move {
        if let Some(r) = req_rx.recv().await {
            let _ = r.reply.send(ApprovalReply::Allow);
        }
    });
    let res = tool
        .invoke(serde_json::json!({ "command": "sudo echo hi" }), cx)
        .await;
    // 放行后不应是 Denied；可能因环境无 sudo 而 Failed/非零退出，但不是 Err(Denied)。
    match res {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            assert!(!msg.contains("拒绝"), "放行后不应被拒: {msg}");
        }
    }
}

#[tokio::test]
async fn file_write_then_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let tool = FileTool::new(vec![root.clone()]);

    let target = root.join("note.txt");
    let cx = ToolCtx::detached(CancellationToken::new());
    let w = tool
        .invoke(
            serde_json::json!({ "op": "write", "path": target.to_str().unwrap(), "content": "记住这句" }),
            cx,
        )
        .await
        .expect("write ok");
    assert!(w.success);

    let cx = ToolCtx::detached(CancellationToken::new());
    let r = tool
        .invoke(serde_json::json!({ "op": "read", "path": target.to_str().unwrap() }), cx)
        .await
        .expect("read ok");
    assert!(r.content.contains("记住这句"));
}

#[tokio::test]
async fn file_outside_root_denied() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let tool = FileTool::new(vec![root]);

    // 尝试读一个明显在根外的路径。
    let outside = if cfg!(windows) { "C:\\Windows\\system.ini" } else { "/etc/hosts" };
    let cx = ToolCtx::detached(CancellationToken::new());
    let res = tool
        .invoke(serde_json::json!({ "op": "read", "path": outside }), cx)
        .await;
    assert!(res.is_err(), "根外路径应被拒");
}

/// 回归：allowed_roots 传入**未 canonicalize** 的裸路径（真实 wiring 就是裸
/// current_dir），而 candidate 经 canonicalize 带 Windows `\\?\` 前缀，
/// 两者必须归一后比较，否则合法路径被误拒（截图里的 bug）。
#[tokio::test]
async fn file_raw_root_allows_relative_path_via_cwd() {
    let dir = tempfile::tempdir().unwrap();
    // 故意用裸 path（不 canonicalize），模拟 provider_setup 传入的 current_dir。
    let raw_root = dir.path().to_path_buf();
    let tool = FileTool::new(vec![raw_root.clone()]);

    // 在根内建文件。
    std::fs::write(raw_root.join("hello.txt"), "内容").unwrap();

    // cwd = 裸根；用相对路径读。
    let mut cx = ToolCtx::detached(CancellationToken::new());
    cx.cwd = raw_root.clone();
    let r = tool
        .invoke(serde_json::json!({ "op": "read", "path": "hello.txt" }), cx)
        .await
        .expect("裸 root + 相对路径应放行");
    assert!(r.content.contains("内容"));
}
