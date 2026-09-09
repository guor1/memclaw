//! 后台任务台账（设计 §7.5）。
//!
//! M4：内存台账 + 事件推送。跟踪 process 工具移交的后台进程，支持 list/cancel。
//! store 持久化在 M5 随对话落库一起接入。

use std::sync::Arc;

use dashmap::DashMap;
use oc_proto::{Event, SessionId, TaskId, TaskState, TaskUpdate, TaskView};
use oc_tools::process::BackgroundHandoff;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

struct TaskEntry {
    kind: String,
    detail: String,
    state: TaskState,
    cancel: CancellationToken,
}

/// 后台任务台账。
#[derive(Clone)]
pub struct TaskLedger {
    tasks: Arc<DashMap<TaskId, TaskEntry>>,
    events: broadcast::Sender<Event>,
}

impl TaskLedger {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            events,
        }
    }

    /// 登记一个后台进程移交，跟踪其输出与完成，返回 task_id。
    pub fn register(&self, handoff: BackgroundHandoff) -> TaskId {
        let task_id = TaskId::new(uuid::Uuid::now_v7().to_string());
        let cancel = CancellationToken::new();
        self.tasks.insert(
            task_id.clone(),
            TaskEntry {
                kind: "exec_bg".to_string(),
                detail: handoff.command.clone(),
                state: TaskState::Running,
                cancel: cancel.clone(),
            },
        );
        self.emit(&task_id, TaskState::Running, Some(handoff.command.clone()));

        let tasks = Arc::clone(&self.tasks);
        let events = self.events.clone();
        let tid = task_id.clone();
        let BackgroundHandoff { mut output, done, .. } = handoff;
        tokio::spawn(async move {
            let mut last_line = String::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        set_state(&tasks, &events, &tid, TaskState::Cancelled, None);
                        return;
                    }
                    line = output.recv() => {
                        match line {
                            Some(l) => { last_line = l; }
                            None => break,
                        }
                    }
                }
            }
            // 输出流结束，等退出码。
            let code = done.await.unwrap_or(-1);
            let state = if code == 0 { TaskState::Done } else { TaskState::Failed };
            set_state(&tasks, &events, &tid, state, Some(format!("退出码 {code}；末行: {last_line}")));
        });

        task_id
    }

    /// 列出所有任务视图。
    pub fn list(&self) -> Vec<TaskView> {
        self.tasks
            .iter()
            .map(|e| TaskView {
                id: e.key().clone(),
                kind: e.kind.clone(),
                state: e.state,
                detail: Some(e.detail.clone()),
            })
            .collect()
    }

    /// 未结束（排队中 / 执行中）的任务数，供 `status` 展示。
    ///
    /// 不能用 `tasks.len()` 代替：台账完成后不删条目（`task.list` 要能看到结果与
    /// 退出码），拿总数当「在跑几个」会把这个 daemon 生命周期里跑过的全算进来。
    pub fn unfinished_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|e| matches!(e.state, TaskState::Queued | TaskState::Running))
            .count()
    }

    /// 取消一个任务。
    pub fn cancel(&self, id: &TaskId) -> bool {
        if let Some(e) = self.tasks.get(id) {
            e.cancel.cancel();
            info!(task_id = %id, "取消后台任务");
            true
        } else {
            false
        }
    }

    fn emit(&self, id: &TaskId, state: TaskState, detail: Option<String>) {
        // 后台任务当前不跟踪来源会话，事件归属 main（与 proactive 一致）。
        let _ = self.events.send(Event::Task {
            session: SessionId::main(),
            task_id: id.clone(),
            update: TaskUpdate { state, detail },
        });
    }
}

fn set_state(
    tasks: &DashMap<TaskId, TaskEntry>,
    events: &broadcast::Sender<Event>,
    id: &TaskId,
    state: TaskState,
    detail: Option<String>,
) {
    if let Some(mut e) = tasks.get_mut(id) {
        e.state = state;
    }
    let _ = events.send(Event::Task {
        session: SessionId::main(),
        task_id: id.clone(),
        update: TaskUpdate { state, detail },
    });
}
