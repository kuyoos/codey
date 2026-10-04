//! 网关到本机 app-server 的 WebSocket 连接。
//!
//! 网关自己启动的同源 app-server 只监听回环地址并使用能力令牌；网页永远不直接连接它。
//! 这里维持唯一的 JSON-RPC 连接：客户端请求按 id 关联，服务端通知与服务端反向请求
//! 广播给订阅者（网页事件流与审批处理各订阅一份）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const EVENT_BUFFER: usize = 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type SocketSink = SplitSink<Socket, Message>;
type SocketStream = SplitStream<Socket>;

/// 上游 app-server 推给网关的事件。
#[derive(Clone, Debug)]
pub(crate) enum Event {
    /// 服务端通知（无 id）。
    Notification { method: String, params: Value },
    /// 服务端反向请求（有 id 与方法），必须应答，否则会话会一直等待。
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// 连接状态变化。
    Status { connected: bool, detail: String },
}

enum Command {
    Request {
        id: u64,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, Value>>,
    },
    Reply {
        id: Value,
        result: Result<Value, Value>,
    },
}

pub(crate) struct Upstream {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
    next_id: AtomicU64,
}

impl Upstream {
    /// `keep_alive` 在每次重连前调用，用于按需拉起已退出的 app-server。
    pub(crate) fn start(
        ws_url: String,
        token: String,
        keep_alive: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        let (commands, receiver) = mpsc::channel(256);
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let upstream = Arc::new(Self {
            commands,
            events: events.clone(),
            next_id: AtomicU64::new(1),
        });
        tokio::spawn(run(ws_url, token, receiver, events, keep_alive));
        upstream
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, wait) = oneshot::channel();
        self.commands
            .send(Command::Request {
                id,
                method: method.to_owned(),
                params,
                reply,
            })
            .await
            .map_err(|_| upstream_stopped())?;
        wait.await.map_err(|_| upstream_stopped())?
    }

    pub(crate) async fn reply(&self, id: Value, result: Result<Value, Value>) {
        let _ = self.commands.send(Command::Reply { id, result }).await;
    }
}

fn upstream_stopped() -> Value {
    json!({ "code": -32000, "message": "远程网关上游 app-server 未连接" })
}

async fn run(
    ws_url: String,
    token: String,
    mut commands: mpsc::Receiver<Command>,
    events: broadcast::Sender<Event>,
    keep_alive: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        keep_alive();
        match connect(&ws_url, &token).await {
            Ok(socket) => {
                let _ = events.send(Event::Status {
                    connected: true,
                    detail: String::new(),
                });
                let detail = match pump(socket, &mut commands, &events).await {
                    Ok(()) => "上游连接已结束".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                let _ = events.send(Event::Status {
                    connected: false,
                    detail,
                });
            }
            Err(error) => {
                let _ = events.send(Event::Status {
                    connected: false,
                    detail: format!("{error:#}"),
                });
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn connect(ws_url: &str, token: &str) -> Result<Socket> {
    let mut request = ws_url
        .into_client_request()
        .context("上游 app-server 地址无效")?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().context("上游令牌无效")?,
    );
    let connecting = tokio_tungstenite::connect_async(request);
    let (socket, _) = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Ok(Ok(connected)) => connected,
        Ok(Err(error)) => return Err(error).context("连接上游 app-server 失败"),
        Err(_) => return Err(anyhow!("连接上游 app-server 超时")),
    };
    Ok(socket)
}

async fn pump(
    socket: Socket,
    commands: &mut mpsc::Receiver<Command>,
    events: &broadcast::Sender<Event>,
) -> Result<()> {
    let (mut sink, mut stream) = socket.split();
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, Value>>> = HashMap::new();

    send_json(
        &mut sink,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "codey-remote-gateway",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {},
            },
        }),
    )
    .await?;

    let mut ready = false;
    let deadline = tokio::time::Instant::now() + INITIALIZE_TIMEOUT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline), if !ready => {
                bail!("上游 app-server 初始化超时");
            }
            command = commands.recv(), if ready => {
                match command {
                    None => return Ok(()),
                    Some(Command::Request { id, method, params, reply }) => {
                        pending.insert(id, reply);
                        if let Err(error) = send_request(&mut sink, id, &method, &params).await {
                            pending.remove(&id);
                            return Err(error);
                        }
                    }
                    Some(Command::Reply { id, result }) => send_reply(&mut sink, id, result).await?,
                }
            }
            message = stream.next() => {
                let Some(message) = message else {
                    bail!("上游连接已关闭");
                };
                let message = message.context("读取上游消息失败")?;
                match message {
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_ref()) else {
                            continue;
                        };
                        let id = value.get("id").cloned();
                        let method = value
                            .get("method")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        match (id, method) {
                            (Some(id), Some(method)) => {
                                let _ = events.send(Event::Request {
                                    id,
                                    method,
                                    params: value.get("params").cloned().unwrap_or(Value::Null),
                                });
                            }
                            (Some(id), None) => {
                                let result = match value.get("error") {
                                    Some(error) => Err(error.clone()),
                                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                                };
                                if let Some(number) = id.as_u64()
                                    && let Some(sender) = pending.remove(&number)
                                {
                                    let _ = sender.send(result);
                                    continue;
                                }
                                if !ready && id.as_u64() == Some(0) {
                                    match result {
                                        Ok(_) => ready = true,
                                        Err(error) => bail!("上游 app-server 初始化失败：{error}"),
                                    }
                                }
                            }
                            (None, Some(method)) => {
                                let _ = events.send(Event::Notification {
                                    method,
                                    params: value.get("params").cloned().unwrap_or(Value::Null),
                                });
                            }
                            (None, None) => {}
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(payload) => {
                        sink.send(Message::Pong(payload)).await.context("写入上游 Pong 失败")?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => bail!("上游连接已关闭"),
                }
            }
        }
    }
}

async fn send_request(sink: &mut SocketSink, id: u64, method: &str, params: &Value) -> Result<()> {
    send_json(
        sink,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    )
    .await
}

async fn send_reply(sink: &mut SocketSink, id: Value, result: Result<Value, Value>) -> Result<()> {
    let payload = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    };
    send_json(sink, &payload).await
}

async fn send_json(sink: &mut SocketSink, payload: &Value) -> Result<()> {
    let text = serde_json::to_string(payload).context("编码上游请求失败")?;
    sink.send(Message::Text(text.into()))
        .await
        .context("写入上游 app-server 失败")
}
