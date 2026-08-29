//! TUI 应用（设计 §8）。ratatui + crossterm。
//!
//! 布局：消息流区 + 输入区 + 状态栏。事件循环 select 键盘输入与 daemon 帧。

use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use oc_proto::{
    ChatSendParams, ConnectParams, Event, Frame, LifecyclePhase, Method, Req, ReqId, ResResult,
    RunId, SessionId, PROTO_VERSION,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame as UiFrame, Terminal};

use crate::client::{ClientTransport, ConnectTo};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// PageUp/PageDown 每次滚动的保守行数（draw 时按可视高度再夹紧）。
const SCROLL_PAGE: u16 = 10;

/// 一行消息（用于渲染）。
struct Msg {
    who: &'static str,
    text: String,
}

pub struct App {
    client: ClientTransport,
    msgs: Vec<Msg>,
    input: String,
    status: String,
    connected: bool,
    next_req: u64,
    should_quit: bool,
    /// 待处理审批 id（非 None 时输入区进入 y/n 审批模式）。
    pending_approval: Option<oc_proto::ApprovalId>,
    /// 最近一次 chat.send 分配的 run_id（用于 /stop）。
    active_run: Option<RunId>,
    /// 当前活跃会话（chat.send 归属；事件按此过滤显示）。
    current_session: SessionId,
    /// 上下文用量提示（已用/窗口），随 Usage 事件更新，显示在状态栏。
    usage_hint: Option<String>,
    /// 消息区滚动：距底部的行数偏移。0 = 跟随底部（自动滚到最新消息）；
    /// >0 = 向上回滚查看历史。draw 时按可视高度夹紧。
    scroll_back: u16,
}

impl App {
    pub async fn connect(to: &ConnectTo) -> Result<Self> {
        let mut client = ClientTransport::connect(to).await?;
        // 建连握手。
        let req = Req {
            id: ReqId::new("connect-0"),
            method: Method::Connect(ConnectParams {
                proto_version: PROTO_VERSION,
                token: None,
            }),
            idempotency_key: None,
        };
        client.send(&Frame::Req(req)).await?;

        let mut app = Self {
            client,
            msgs: Vec::new(),
            input: String::new(),
            status: "连接中…".to_string(),
            connected: false,
            next_req: 1,
            should_quit: false,
            pending_approval: None,
            active_run: None,
            current_session: SessionId::main(),
            usage_hint: None,
            scroll_back: 0,
        };
        // 等 hello。
        if let Some(Frame::Res(res)) = app.client.recv().await? {
            match res.result {
                ResResult::Ok(_) => {
                    app.connected = true;
                    app.status = "已连接".to_string();
                    app.push_sys("已连接到 oc daemon。输入消息回车发送，Ctrl-C 退出。");
                }
                ResResult::Err(e) => {
                    app.status = format!("连接被拒: {}", e.message);
                }
            }
        }
        Ok(app)
    }

    fn push_sys(&mut self, s: &str) {
        self.msgs.push(Msg {
            who: "系统",
            text: s.to_string(),
        });
    }

    /// 主事件循环。
    pub async fn run(&mut self, term: &mut Term) -> Result<()> {
        let mut keys = EventStream::new();
        term.draw(|f| self.draw(f))?;

        while !self.should_quit {
            // 注：draw 需 &mut self（夹紧 scroll_back），下面两处 term.draw 同。
            tokio::select! {
                // 键盘
                maybe_key = keys.next() => {
                    if let Some(Ok(CtEvent::Key(key))) = maybe_key {
                        // Windows 控制台会同时上报 Press/Release（甚至 Repeat），
                        // 只处理 Press，否则一次按键被处理多次（字符重复/多空格）。
                        if key.kind == KeyEventKind::Press {
                            self.on_key(key.code, key.modifiers).await?;
                        }
                    }
                }
                // daemon 帧
                frame = self.client.recv() => {
                    match frame? {
                        Some(f) => self.on_frame(f),
                        None => {
                            self.status = "daemon 已断开".to_string();
                            self.connected = false;
                        }
                    }
                }
            }
            term.draw(|f| self.draw(f))?;
        }
        Ok(())
    }

    async fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<()> {
        // 审批模式：y/n 优先处理（Ctrl-C 仍可退出）。
        if self.pending_approval.is_some()
            && !(matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL))
        {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.reply_approval(true).await?;
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.reply_approval(false).await?;
                    return Ok(());
                }
                _ => return Ok(()), // 审批期间忽略其它输入
            }
        }
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text == "/stop" {
                    // 完整 /stop：hard abort（先 drain 排队轮再中止活跃 run）。
                    if let Some(run_id) = self.active_run.clone() {
                        self.abort_run(run_id, true).await?;
                        self.msgs.push(Msg { who: "系统", text: "已请求停止".into() });
                    }
                } else if text == "/sessions" {
                    self.list_sessions().await?;
                } else if text == "/compact" {
                    self.compact().await?;
                    self.push_sys("已请求压缩上下文…");
                } else if let Some(arg) = text.strip_prefix("/session ") {
                    // 切换/新建会话：后续 chat.send 归属该会话，事件按此过滤。
                    let id = arg.trim();
                    if !id.is_empty() {
                        self.current_session = SessionId::new(id.to_string());
                        self.active_run = None;
                        self.msgs.push(Msg {
                            who: "系统",
                            text: format!("已切换到会话「{id}」（新 id 将在首次发送时创建）"),
                        });
                    }
                } else if !text.is_empty() && self.connected {
                    self.msgs.push(Msg { who: "你", text: text.clone() });
                    self.send_chat(text).await?;
                }
                // 发送后回到底部跟随，确保看到自己的消息与后续回复。
                self.scroll_back = 0;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            // 消息区滚动。PageUp/Down 翻一屏（用保守步长，draw 时再按可视高度夹紧）；
            // Ctrl+Home 跳最顶，Ctrl+End 回底部跟随。
            KeyCode::PageUp => {
                self.scroll_back = self.scroll_back.saturating_add(SCROLL_PAGE);
            }
            KeyCode::PageDown => {
                self.scroll_back = self.scroll_back.saturating_sub(SCROLL_PAGE);
            }
            KeyCode::Home if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_back = u16::MAX; // draw 夹到顶部最大偏移
            }
            KeyCode::End if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_back = 0;
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    async fn reply_approval(&mut self, allow: bool) -> Result<()> {
        let Some(id) = self.pending_approval.take() else {
            return Ok(());
        };
        let req_id = format!("appr-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(req_id),
            method: Method::ApprovalReply(oc_proto::ApprovalReplyParams {
                approval_id: id,
                allow,
            }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        self.msgs.push(Msg {
            who: "系统",
            text: if allow { "已批准".into() } else { "已拒绝".into() },
        });
        self.status = "已连接".to_string();
        Ok(())
    }

    async fn send_chat(&mut self, text: String) -> Result<()> {
        let id = format!("req-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::ChatSend(ChatSendParams {
                session: Some(self.current_session.clone()),
                text,
            }),
            idempotency_key: Some(oc_proto::IdemKey::new(uuid_like(self.next_req))),
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    /// 请求压缩当前会话上下文。
    async fn compact(&mut self) -> Result<()> {
        let id = format!("req-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::Compact(oc_proto::CompactParams {
                session: Some(self.current_session.clone()),
            }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    /// 请求会话列表并显示。
    async fn list_sessions(&mut self) -> Result<()> {
        let id = format!("req-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::SessionsList,
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    async fn abort_run(&mut self, run_id: RunId, hard: bool) -> Result<()> {
        let req_id = format!("abort-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(req_id),
            method: Method::ChatAbort(oc_proto::ChatAbortParams { run_id, hard }),
            idempotency_key: None,
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Event(ev) => self.on_event(ev),
            Frame::Res(res) => match res.result {
                // 记录 chat.send 分配的 run_id，供 /stop 使用。
                ResResult::Ok(oc_proto::MethodOk::ChatSend { run_id }) => {
                    self.active_run = Some(run_id);
                }
                ResResult::Ok(oc_proto::MethodOk::Sessions(list)) => {
                    if list.is_empty() {
                        self.push_sys("（无会话）");
                    } else {
                        for s in list {
                            let cur = if s.id == self.current_session { " ←当前" } else { "" };
                            self.msgs.push(Msg {
                                who: "会话",
                                text: format!("{} [{}]{}", s.id.as_str(), s.kind, cur),
                            });
                        }
                    }
                }
                _ => {}
            },
            Frame::Req(_) => {}
        }
    }

    fn on_event(&mut self, ev: Event) {
        // 只显示归属当前会话的事件（Proactive 归属 main，见下单独放行）。
        if let Some(sid) = event_session(&ev) {
            let is_proactive = matches!(ev, Event::Proactive { .. });
            if sid != &self.current_session && !is_proactive {
                return;
            }
        }
        match ev {
            Event::Lifecycle { phase, .. } => match phase {
                LifecyclePhase::Start => self.status = "助手思考中…".to_string(),
                LifecyclePhase::End => self.status = "已连接".to_string(),
                LifecyclePhase::Error { message, .. } => {
                    self.status = format!("错误: {message}");
                }
            },
            Event::Assistant { delta, .. } => {
                // 流式增量：追加到最后一条 assistant 消息，或新建。
                if let Some(last) = self.msgs.last_mut() {
                    if last.who == "助手" {
                        last.text.push_str(&delta);
                        return;
                    }
                }
                self.msgs.push(Msg { who: "助手", text: delta });
            }
            Event::Proactive { text, .. } => {
                if !text.is_empty() {
                    self.msgs.push(Msg { who: "主动提醒", text });
                }
            }
            Event::Tool { phase, .. } => match phase {
                oc_proto::ToolPhase::Start { name, args_preview } => {
                    self.msgs.push(Msg {
                        who: "工具",
                        text: format!("{name}: {args_preview}"),
                    });
                }
                oc_proto::ToolPhase::Update { chunk } => {
                    if let Some(last) = self.msgs.last_mut() {
                        if last.who == "工具" {
                            last.text.push_str(&chunk);
                            return;
                        }
                    }
                    self.msgs.push(Msg { who: "工具", text: chunk });
                }
                oc_proto::ToolPhase::End { .. } => {}
            },
            Event::Approval { approval_id, summary, command, .. } => {
                self.msgs.push(Msg {
                    who: "审批",
                    text: format!("{summary}\n  命令: {command}\n  批准执行？(y/n)"),
                });
                self.pending_approval = Some(approval_id);
                self.status = "等待审批：按 y 批准 / n 拒绝".to_string();
            }
            Event::Task { .. } => {}
            Event::Usage { input_tokens, context_window, .. } => {
                // 实时更新上下文用量提示（显示在状态栏）。
                self.usage_hint = Some(format_usage(input_tokens, context_window));
            }
        }
    }

    /// 消息区标题：回滚查看历史时提示当前偏移，让用户知道未在底部。
    fn msg_title(&self) -> String {
        if self.scroll_back > 0 {
            format!("oc [↑历史 -{} 行  PgDn/Ctrl-End 回底部]", self.scroll_back)
        } else {
            "oc".to_string()
        }
    }

    fn draw(&mut self, f: &mut UiFrame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(f.area());

        // 消息流
        let lines: Vec<Line> = self
            .msgs
            .iter()
            .map(|m| {
                let color = match m.who {
                    "你" => Color::Cyan,
                    "助手" => Color::Green,
                    "主动提醒" => Color::Yellow,
                    "审批" => Color::Red,
                    "工具" => Color::Magenta,
                    _ => Color::DarkGray,
                };
                Line::from(vec![
                    Span::styled(format!("{}: ", m.who), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::raw(m.text.clone()),
                ])
            })
            .collect();

        // 可视区（去掉上下边框各 1 行）与内容宽度（去左右边框各 1 列）。
        let area = chunks[0];
        let visible = area.height.saturating_sub(2);
        let inner_w = area.width.saturating_sub(2);

        // 先用无边框副本按内容宽度算 wrap 后真实行数（含中文双宽），据此夹紧
        // scroll_back，再构建带标题的正式段落——标题读的是夹紧后的值。
        let body = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let total = body.line_count(inner_w) as u16;
        let max_off = total.saturating_sub(visible);
        // 夹紧回滚偏移：0..=max_off。scroll_back=0 贴底跟随最新。
        self.scroll_back = self.scroll_back.min(max_off);
        let y = max_off - self.scroll_back;

        let msgs = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(self.msg_title()))
            .wrap(Wrap { trim: false })
            .scroll((y, 0));
        f.render_widget(msgs, area);

        // 输入
        let input = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("输入"));
        f.render_widget(input, chunks[1]);

        // 状态栏：状态文本 +（可选）上下文用量。
        let status_line = match &self.usage_hint {
            Some(u) => format!("{}  |  ctx {}", self.status, u),
            None => self.status.clone(),
        };
        let status = Paragraph::new(status_line).style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, chunks[2]);
    }
}

/// 格式化上下文用量：「12.3K/64K (19%)」。
fn format_usage(used: u32, window: u32) -> String {
    let pct = if window > 0 { used as f64 / window as f64 * 100.0 } else { 0.0 };
    format!("{}/{} ({:.0}%)", short_k(used), short_k(window), pct)
}

/// 紧凑显示 token 数：1234→1.2K，65536→64K。
fn short_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// 取事件归属的会话 id（所有变体都带 session）。
fn event_session(ev: &Event) -> Option<&SessionId> {
    Some(match ev {
        Event::Lifecycle { session, .. }
        | Event::Assistant { session, .. }
        | Event::Tool { session, .. }
        | Event::Proactive { session, .. }
        | Event::Task { session, .. }
        | Event::Usage { session, .. }
        | Event::Approval { session, .. } => session,
    })
}

/// 简易唯一键（避免为 TUI 引入 uuid 依赖）。
fn uuid_like(n: u64) -> String {
    format!("tui-{}-{}", std::process::id(), n)
}
