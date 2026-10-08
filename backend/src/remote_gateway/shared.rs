//! 共享上游：复用桌面 app-server 的 stdio 连接。
//!
//! 桌面 app-server 只接受一个写入者，独立进程无法接管正在运行的会话——`thread/resume`
//! 会报 `already has an active writer`，随后 `turn/start` 只剩一句笼统的 `thread not found`。
//! 这里在包装进程内把这条 stdio 拆成两条路：桌面与 app-server 之间逐行直通，网页请求以
//! 独立 id 注入同一连接，回应按 id 分流。桌面看到的数据流保持原样，所以分流即使失败也只
//! 影响网页，不会影响桌面会话。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use super::upstream::{Event, upstream_stopped};

const EVENT_BUFFER: usize = 1024;
/// 待转发的输入行上限：写线程阻塞时让上游读取自然降速。
const OUTGOING_CAPACITY: usize = 256;
/// 网页请求只使用带前缀的字符串 id，与桌面的数字 id 不可能冲突，回应因此能精确分流。
const REQUEST_ID_PREFIX: &str = "codey-gateway:";

type Reply = oneshot::Sender<Result<Value, Value>>;

/// 网页侧的上游：请求注入桌面已有的 app-server 会话。
pub(crate) struct SharedLink {
    outgoing: SyncSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, Reply>>>,
    events: broadcast::Sender<Event>,
    local_router: bool,
    next_id: AtomicU64,
}

impl SharedLink {
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = format!(
            "{REQUEST_ID_PREFIX}{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let mut payload = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.local_router {
            // 桌面侧的同一改写由本地路由转发器完成；网页发出的会话请求在这里补齐。
            crate::codex_startup_patch::rewrite_local_router_message(&mut payload);
        }
        let (reply, wait) = oneshot::channel();
        self.pending
            .lock()
            .expect("待响应请求表不可用")
            .insert(id.clone(), reply);
        if send_line(&self.outgoing, &payload).is_err() {
            self.pending.lock().expect("待响应请求表不可用").remove(&id);
            return Err(upstream_stopped());
        }
        wait.await.map_err(|_| upstream_stopped())?
    }

    pub(crate) async fn reply(&self, id: Value, result: Result<Value, Value>) {
        let payload = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        let _ = send_line(&self.outgoing, &payload);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

/// 接管桌面 app-server 的 stdio：`source` 是桌面输入（本地路由模式下是转发器的输出）。
pub(crate) fn start(
    server_stdin: ChildStdin,
    server_stdout: ChildStdout,
    source: Box<dyn Read + Send>,
    local_router: bool,
) -> Arc<SharedLink> {
    let (outgoing, lines) = sync_channel::<Vec<u8>>(OUTGOING_CAPACITY);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    spawn_writer(lines, server_stdin);
    spawn_relay(source, outgoing.clone());
    spawn_reader(server_stdout, Arc::clone(&pending), events.clone());
    Arc::new(SharedLink {
        outgoing,
        pending,
        events,
        local_router,
        next_id: AtomicU64::new(1),
    })
}

fn send_line(outgoing: &SyncSender<Vec<u8>>, payload: &Value) -> Result<(), ()> {
    let mut line = serde_json::to_vec(payload).map_err(|_| ())?;
    line.push(b'\n');
    outgoing.send(line).map_err(|_| ())
}

fn spawn_named(name: &str, task: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(task);
    if let Err(error) = spawned {
        super::log(
            "codey.remote_gateway.shared_thread_failed",
            json!({ "thread": name, "message": format!("{error}") }),
        );
    }
}

/// 唯一写入 app-server stdin 的线程：桌面输入与网页请求都经过这里，保证整行不被交错。
fn spawn_writer(lines: std::sync::mpsc::Receiver<Vec<u8>>, mut stdin: ChildStdin) {
    spawn_named("codey-gateway-stdin", move || {
        while let Ok(line) = lines.recv() {
            if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                break;
            }
        }
    });
}

/// 桌面输入逐行直通给 app-server；本地路由改写由桌面侧的转发器完成。
fn spawn_relay(source: Box<dyn Read + Send>, outgoing: SyncSender<Vec<u8>>) {
    spawn_named("codey-gateway-relay", move || {
        let mut reader = BufReader::new(source);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if outgoing.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });
}

/// app-server 输出按 id 分流：网页请求的回应回给网页，其余原样写给桌面。
fn spawn_reader(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<String, Reply>>>,
    events: broadcast::Sender<Event>,
) {
    spawn_named("codey-gateway-stdout", move || {
        let mut reader = BufReader::new(stdout);
        let mut output = std::io::stdout();
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                if let Some(id) = value.get("id").and_then(Value::as_str)
                    && let Some(reply) = pending.lock().expect("待响应请求表不可用").remove(id)
                {
                    let _ = reply.send(reply_result(&value));
                    continue;
                }
                publish(&value, &events);
            }
            if output
                .write_all(&line)
                .and_then(|()| output.flush())
                .is_err()
            {
                break;
            }
        }
        // 上游结束：让等待中的网页请求立即失败，并提示网页离线。
        pending.lock().expect("待响应请求表不可用").clear();
        let _ = events.send(Event::Status {
            connected: false,
            detail: "桌面 Codex 的 app-server 已结束".to_owned(),
        });
        super::log("codey.remote_gateway.shared_ended", json!({}));
    });
}

fn reply_result(value: &Value) -> Result<Value, Value> {
    match value.get("error") {
        Some(error) => Err(error.clone()),
        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// 通知全部转发给网页；反向请求由桌面负责应答，只有网页能给出合法决策的审批才一并广播。
fn publish(value: &Value, events: &broadcast::Sender<Event>) {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("id") {
        None => {
            let _ = events.send(Event::Notification {
                method: method.to_owned(),
                params,
            });
        }
        Some(id) if super::is_interactive_approval(method) => {
            let _ = events.send(Event::Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
        }
        Some(_) => {}
    }
}
