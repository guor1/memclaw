//! TUI 应用（设计 §8）。ratatui + crossterm。
//!
//! 布局：消息流区 + 输入区 + 状态栏。事件循环 select 键盘输入与 daemon 帧。

use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use oc_proto::{
    ChatSendParams, ConnectParams, Event, Frame, LifecyclePhase, Method, Req, ReqId, ResResult,
    PROTO_VERSION,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame as UiFrame, Terminal};

use crate::client::{ClientTransport, ConnectTo};

type Term = Terminal<CrosstermBackend<Stdout>>;

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
            tokio::select! {
                // 键盘
                maybe_key = keys.next() => {
                    if let Some(Ok(CtEvent::Key(key))) = maybe_key {
                        self.on_key(key.code, key.modifiers).await?;
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
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if !text.is_empty() && self.connected {
                    self.msgs.push(Msg { who: "你", text: text.clone() });
                    self.send_chat(text).await?;
                }
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_chat(&mut self, text: String) -> Result<()> {
        let id = format!("req-{}", self.next_req);
        self.next_req += 1;
        let req = Req {
            id: ReqId::new(id),
            method: Method::ChatSend(ChatSendParams { session: None, text }),
            idempotency_key: Some(oc_proto::IdemKey::new(uuid_like(self.next_req))),
        };
        self.client.send(&Frame::Req(req)).await?;
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Event(ev) => self.on_event(ev),
            Frame::Res(_) => { /* M2：run_id 应答暂不展示 */ }
            Frame::Req(_) => {}
        }
    }

    fn on_event(&mut self, ev: Event) {
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
            Event::Tool { .. } | Event::Task { .. } => {}
        }
    }

    fn draw(&self, f: &mut UiFrame) {
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
                    _ => Color::DarkGray,
                };
                Line::from(vec![
                    Span::styled(format!("{}: ", m.who), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::raw(m.text.clone()),
                ])
            })
            .collect();
        let msgs = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("oc"))
            .wrap(Wrap { trim: false });
        f.render_widget(msgs, chunks[0]);

        // 输入
        let input = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("输入"));
        f.render_widget(input, chunks[1]);

        // 状态栏
        let status = Paragraph::new(self.status.as_str())
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, chunks[2]);
    }
}

/// 简易唯一键（避免为 TUI 引入 uuid 依赖）。
fn uuid_like(n: u64) -> String {
    format!("tui-{}-{}", std::process::id(), n)
}
