//! 单连接处理（设计 §7.6 背压 / §2.5 传输）。
//!
//! 每连接三条逻辑：
//! - **读**：收 req → dispatch → 把 res 送入出站队列
//! - **事件转发**：订阅 EventBus → 把 event 送入出站队列
//! - **写**：单一写任务从出站队列取帧写出（串行化，避免交错）
//!
//! 出站队列有界；慢 client 的事件转发在 `Lagged` 时提示重连（不阻塞 run）。

use std::sync::Arc;

use oc_proto::{Frame, Res};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::debug;

use crate::codec::{FrameReader, FrameWriter};
use crate::dispatch;
use crate::error::ServerResult;
use crate::state::ServerState;
use crate::transport::Conn;

/// 出站队列容量。
const OUTBOUND_CAP: usize = 256;

pub async fn handle(conn: Conn, state: Arc<ServerState>) -> ServerResult<()> {
    let (read_half, write_half) = tokio::io::split(conn);
    let mut reader = FrameReader::new(read_half);

    // 出站帧队列：res 与 event 都经此串行写出。
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(OUTBOUND_CAP);

    // 写任务。
    let writer_handle = tokio::spawn(async move {
        let mut writer = FrameWriter::new(write_half);
        while let Some(frame) = out_rx.recv().await {
            if writer.write_frame(&frame).await.is_err() {
                break; // 对端断开，结束写任务。
            }
        }
    });

    // 事件转发任务。
    let mut event_rx = state.subscribe();
    let event_out = out_tx.clone();
    let event_handle = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(ev) => {
                    if event_out.send(Frame::Event(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!(dropped = n, "事件订阅滞后，client 应重新 status 拉快照");
                    // 继续；不阻塞。
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 读循环。
    let read_result = read_loop(&mut reader, &state, &out_tx).await;

    // 收尾：关掉出站，等写任务排空。
    drop(out_tx);
    event_handle.abort();
    let _ = writer_handle.await;
    read_result
}

async fn read_loop<R>(
    reader: &mut FrameReader<R>,
    state: &Arc<ServerState>,
    out_tx: &mpsc::Sender<Frame>,
) -> ServerResult<()>
where
    R: AsyncReadExt + Unpin,
{
    while let Some(frame) = reader.read_frame().await? {
        match frame {
            Frame::Req(req) => {
                let result = dispatch::handle_req(&req, state, out_tx).await;
                let res = Res {
                    id: req.id,
                    result,
                };
                // 出站满则 client 太慢，直接断开该连接。
                if out_tx.send(Frame::Res(res)).await.is_err() {
                    break;
                }
            }
            // server 不处理来自 client 的 res/event。
            Frame::Res(_) | Frame::Event(_) => {
                debug!("忽略来自 client 的非请求帧");
            }
        }
    }
    Ok(())
}
