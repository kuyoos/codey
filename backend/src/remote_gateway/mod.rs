//! Codey 局域网远程控制网关。
//!
//! 桌面自身的 app-server 走 stdio，与桌面窗口一一绑定，浏览器无法接入；浏览器也不能
//! 直连 app-server，因为它暴露文件、命令与配置接口。这里在桌面 app-server 的包装进程里
//! 另起一个同源 app-server（同一 `CODEX_HOME` 与同一份本地路由配置），只监听回环
//! WebSocket 并核验能力令牌，再由网关转成带令牌的局域网 HTTP 与 SSE 供网页使用。
//!
//! 网关不是独立进程：只有拿到 `CODEX_HOME` 下独占文件锁的那一次包装器启动才会运行，
//! 因此跟随桌面 app-server 的生命周期，也不会重复监听端口。

mod http;
mod upstream;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};

use http::{EventStream, Request, read_request, write_json, write_response};
use upstream::{Event, Upstream};

const CONFIG_FILE_NAME: &str = ".codey-remote-gateway.json";
const TOKEN_FILE_NAME: &str = ".codey-remote-gateway.token";
const URL_FILE_NAME: &str = ".codey-remote-gateway.url";
const LOCK_FILE_NAME: &str = ".codey-remote-gateway.lock";
const WS_TOKEN_FILE_NAME: &str = ".codey-remote-gateway.ws-token";
const DEFAULT_PORT: u16 = 8799;
const DEFAULT_PAGE_TURNS: u32 = 8;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const PAGE: &str = include_str!("page.html");

/// 包装器协议变量不能传给同源 app-server，否则会被误认为受控 CLI 启动。
const WRAPPER_ENV: &[&str] = &[
    "CODEX_CLI_PATH",
    "CODEY_CODEX_CLI_WRAPPER_TARGET",
    "CODEY_CODEX_CLI_WRAPPER_SOURCE",
    "CODEY_CODEX_CLI_WRAPPER_OVERRIDES",
    "CODEY_CODEX_CLI_STDIN_RELAY",
    "CODEY_CODEX_CLI_WRAPPER_SUBAGENT",
    "CODEY_CODEX_CLI_WRAPPER_PORT",
    "CODEY_CODEX_CLI_WRAPPER_TOKEN",
    "CODEY_CODEX_CLI_WRAPPER_MARKER",
    "CODEY_CODEX_CLI_WRAPPER_HANDSHAKE_OPTIONAL",
    "CODEY_STARTUP_PATCH_MARKER",
    "NODE_OPTIONS",
];

static ATTEMPTED: OnceLock<()> = OnceLock::new();
static LOCK_GUARD: OnceLock<std::fs::File> = OnceLock::new();

#[derive(serde::Serialize, serde::Deserialize)]
struct GatewayConfig {
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default = "port_default")]
    port: u16,
}

fn enabled_default() -> bool {
    true
}

fn port_default() -> u16 {
    DEFAULT_PORT
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: enabled_default(),
            port: port_default(),
        }
    }
}

/// 在桌面 app-server 的包装进程里尝试启动网关；每个进程只尝试一次。
///
/// `args` 是去掉受管配置后重新拼装的 app-server 参数，网关沿用同一份本地路由配置。
pub(crate) fn start_if_enabled(target: &Path, args: &[OsString], overrides: &[String]) {
    if ATTEMPTED.set(()).is_err() {
        return;
    }
    let home = crate::codex_config::codex_home().to_path_buf();
    let config = load_or_create_config(&home);
    if !config.enabled {
        log(
            "codey.remote_gateway.disabled",
            json!({ "config": home.join(CONFIG_FILE_NAME).display().to_string() }),
        );
        return;
    }
    let Some(guard) = acquire_lock(&home) else {
        return;
    };
    let Some(token) = load_or_create_token(&home) else {
        return;
    };
    let _ = LOCK_GUARD.set(guard);
    if std::fs::write(home.join(WS_TOKEN_FILE_NAME), &token).is_err() {
        log(
            "codey.remote_gateway.ws_token_failed",
            json!({ "message": "无法写入上游能力令牌" }),
        );
        return;
    }
    let port = match free_loopback_port() {
        Ok(port) => port,
        Err(_) => return,
    };
    let args = transport_args(args, port, &home.join(WS_TOKEN_FILE_NAME));
    let overrides = overrides.to_vec();
    let target = target.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("codey-remote-gateway".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log(
                        "codey.remote_gateway.runtime_failed",
                        json!({ "message": format!("{error}") }),
                    );
                    return;
                }
            };
            runtime.block_on(serve_forever(
                target,
                args,
                overrides,
                port,
                config.port,
                home,
            ));
        });
    if let Err(error) = spawned {
        log(
            "codey.remote_gateway.thread_failed",
            json!({ "message": format!("{error}") }),
        );
    }
}

async fn serve_forever(
    target: PathBuf,
    args: Vec<OsString>,
    overrides: Vec<String>,
    upstream_port: u16,
    http_port: u16,
    home: PathBuf,
) {
    let token = match std::fs::read_to_string(home.join(TOKEN_FILE_NAME)) {
        Ok(token) => token.trim().to_owned(),
        Err(error) => {
            log(
                "codey.remote_gateway.token_failed",
                json!({ "message": format!("{error}") }),
            );
            return;
        }
    };
    let app = Arc::new(AppServer {
        target,
        args,
        child: Mutex::new(None),
    });
    app.ensure_running().await;
    let keep_alive = {
        let app = Arc::clone(&app);
        Arc::new(move || {
            let app = Arc::clone(&app);
            tokio::spawn(async move { app.ensure_running().await });
        })
    };
    let upstream = Upstream::start(
        format!("ws://127.0.0.1:{upstream_port}"),
        token.clone(),
        keep_alive,
    );
    let listener = match bind_listener(http_port).await {
        Ok(listener) => listener,
        Err(error) => {
            log(
                "codey.remote_gateway.listen_failed",
                json!({ "message": format!("{error:#}") }),
            );
            return;
        }
    };
    let port = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(http_port);
    let state = Arc::new(State {
        upstream: Arc::clone(&upstream),
        approvals: Approvals::default(),
        browsers: AtomicUsize::new(0),
        token,
        overrides,
    });
    spawn_request_responder(Arc::clone(&state));
    tokio::spawn(async move {
        loop {
            app.ensure_running().await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    publish_url(&home, port, &state.token);
    log(
        "codey.remote_gateway.started",
        json!({
            "port": port,
            "lan": lan_ipv4().map(|address| address.to_string()),
            "link": home.join(URL_FILE_NAME).display().to_string(),
        }),
    );
    accept_loop(listener, state).await;
}

async fn bind_listener(port: u16) -> Result<TcpListener> {
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("0.0.0.0", 0))
            .await
            .context("绑定远程网关端口失败"),
    }
}

fn publish_url(home: &Path, port: u16, token: &str) {
    let host = lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
    let link = format!("http://{host}:{port}/?token={token}");
    let _ = std::fs::write(home.join(URL_FILE_NAME), format!("{link}\n"));
}

fn lan_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_loopback() => Some(address),
        _ => None,
    }
}

fn free_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("分配上游端口失败")?;
    Ok(listener.local_addr()?.port())
}

fn load_or_create_config(home: &Path) -> GatewayConfig {
    let path = home.join(CONFIG_FILE_NAME);
    if let Ok(raw) = std::fs::read_to_string(&path)
        && let Ok(config) = serde_json::from_str::<GatewayConfig>(&raw)
    {
        return config;
    }
    let config = GatewayConfig::default();
    if let Ok(raw) = serde_json::to_vec_pretty(&config) {
        let _ = std::fs::write(&path, raw);
    }
    config
}

fn load_or_create_token(home: &Path) -> Option<String> {
    let path = home.join(TOKEN_FILE_NAME);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let token = raw.trim();
        if !token.is_empty() {
            return Some(token.to_owned());
        }
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    std::fs::write(&path, &token).ok()?;
    Some(token)
}

fn acquire_lock(home: &Path) -> Option<std::fs::File> {
    let path = home.join(LOCK_FILE_NAME);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

/// 把 `--listen stdio://` 换成回环 WebSocket，并附带能力令牌校验。
fn transport_args(args: &[OsString], port: u16, token_file: &Path) -> Vec<OsString> {
    let mut output = Vec::with_capacity(args.len() + 6);
    let mut inserted = false;
    let mut skip_next = false;
    for (index, argument) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        let text = argument.to_string_lossy();
        if matches!(
            text.as_ref(),
            "--listen"
                | "--ws-auth"
                | "--ws-token-file"
                | "--ws-token-sha256"
                | "--ws-shared-secret-file"
                | "--ws-issuer"
                | "--ws-audience"
                | "--ws-max-clock-skew-seconds"
        ) {
            skip_next = index + 1 < args.len();
            continue;
        }
        if text.starts_with("--listen=") || text.starts_with("--ws-") {
            continue;
        }
        output.push(argument.clone());
        if !inserted && text == "app-server" {
            inserted = true;
            output.push("--listen".into());
            output.push(format!("ws://127.0.0.1:{port}").into());
            output.push("--ws-auth".into());
            output.push("capability-token".into());
            output.push("--ws-token-file".into());
            output.push(token_file.as_os_str().to_owned());
        }
    }
    output
}

fn log(event: &str, detail: Value) {
    let _ = codey_runtime_core::diagnostic_log::append_diagnostic_log(event, detail);
}

struct AppServer {
    target: PathBuf,
    args: Vec<OsString>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl AppServer {
    /// 上游 app-server 未运行或已退出时重新拉起。
    async fn ensure_running(&self) {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            return;
        }
        *guard = None;
        let mut command = tokio::process::Command::new(&self.target);
        command.args(&self.args);
        command.env("CODEX_HOME", crate::codex_config::codex_home());
        for name in WRAPPER_ENV {
            command.env_remove(name);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(codey_runtime_core::windows_create_no_window());
        match command.spawn() {
            Ok(child) => {
                log(
                    "codey.remote_gateway.upstream_started",
                    json!({ "pid": child.id() }),
                );
                *guard = Some(child);
            }
            Err(error) => log(
                "codey.remote_gateway.upstream_failed",
                json!({ "message": format!("{error}") }),
            ),
        }
    }
}

struct State {
    upstream: Arc<Upstream>,
    approvals: Approvals,
    browsers: AtomicUsize,
    token: String,
    overrides: Vec<String>,
}

#[derive(Default)]
struct Approvals {
    waiters: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl Approvals {
    async fn register(&self, id: &Value) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();
        self.waiters.lock().await.insert(id.to_string(), sender);
        receiver
    }

    async fn resolve(&self, key: &str, value: Value) -> bool {
        match self.waiters.lock().await.remove(key) {
            Some(sender) => sender.send(value).is_ok(),
            None => false,
        }
    }

    async fn forget(&self, id: &Value) {
        self.waiters.lock().await.remove(&id.to_string());
    }
}

/// 只有网页能给出合法应答的审批请求才交给浏览器；其余一律安全默认应答，避免会话卡死。
///
/// 旧版 `applyPatchApproval` 与 `execCommandApproval` 的决策枚举、以及 `item/permissions`
/// 需要回传具体权限，网页无法完整构造，因此保持默认拒绝。
fn is_interactive_approval(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    )
}

/// 网页不处理的请求一律给出安全的默认应答，避免会话卡死。
fn default_reply(method: &str) -> Result<Value, Value> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Ok(json!({ "decision": "decline" }))
        }
        "applyPatchApproval" | "execCommandApproval" => Ok(json!({
            "decision": { "denied": { "rejection": "Codey 远程网页端未确认" } }
        })),
        "item/permissions/requestApproval" => Ok(json!({ "permissions": {}, "scope": "turn" })),
        "item/tool/requestUserInput" => Ok(json!({ "answers": {} })),
        "mcpServer/elicitation/request" => Ok(json!({ "action": "decline" })),
        "item/tool/call" => Ok(json!({
            "success": false,
            "contentItems": [
                { "type": "inputText", "text": "Codey 远程网页端不支持该工具调用" }
            ]
        })),
        "currentTime/read" => Ok(json!({ "currentTimeAt": unix_seconds() })),
        "attestation/generate" => Ok(json!({ "token": "" })),
        "account/chatgptAuthTokens/refresh" => Err(json!({
            "code": -32601,
            "message": "Codey 远程网页端无法刷新 ChatGPT 令牌"
        })),
        _ => Err(json!({
            "code": -32601,
            "message": format!("Codey 远程网页端不支持 {method}")
        })),
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn spawn_request_responder(state: Arc<State>) {
    let mut events = state.upstream.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Event::Request { id, method, .. } = event else {
                continue;
            };
            if !is_interactive_approval(&method) {
                let _ = state.upstream.reply(id, default_reply(&method)).await;
                continue;
            }
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                // 没有网页在看时立即拒绝，避免无人应答的会话长时间挂起。
                let reply = if state.browsers.load(Ordering::Relaxed) == 0 {
                    None
                } else {
                    let wait = state.approvals.register(&id).await;
                    match tokio::time::timeout(APPROVAL_TIMEOUT, wait).await {
                        Ok(Ok(value)) => Some(Ok(value)),
                        _ => {
                            state.approvals.forget(&id).await;
                            None
                        }
                    }
                };
                let reply = reply.unwrap_or_else(|| default_reply(&method));
                state.upstream.reply(id, reply).await;
            });
        }
    });
}

async fn accept_loop(listener: TcpListener, state: Arc<State>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _ = handle_connection(stream, state).await;
        });
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<State>) -> Result<()> {
    let mut stream = stream;
    let read = tokio::time::timeout(Duration::from_secs(15), read_request(&mut stream)).await;
    let request = match read {
        Ok(Ok(request)) => request,
        Ok(Err(_)) | Err(_) => return Ok(()),
    };
    route(stream, &state, request).await
}

async fn route(mut stream: TcpStream, state: &Arc<State>, request: Request) -> Result<()> {
    if request.method == "GET" && (request.path == "/" || request.path == "/index.html") {
        return write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        )
        .await;
    }
    if !request.path.starts_with("/api/") {
        return write_json(
            &mut stream,
            404,
            &json!({ "error": { "message": "未找到" } }),
        )
        .await;
    }
    if !authorized(&request, &state.token) {
        return write_json(
            &mut stream,
            401,
            &json!({ "error": { "message": "令牌无效，请使用 Codey 生成的远程控制链接" } }),
        )
        .await;
    }
    if request.method == "GET" && request.path == "/api/events" {
        return stream_events(stream, Arc::clone(state)).await;
    }
    let (status, body) = api(state, &request).await;
    write_json(&mut stream, status, &body).await
}

async fn stream_events(stream: TcpStream, state: Arc<State>) -> Result<()> {
    let mut events = state.upstream.subscribe();
    let mut sse = EventStream::start(stream).await?;
    state.browsers.fetch_add(1, Ordering::Relaxed);
    let _browser = BrowserGuard(Arc::clone(&state));
    let payload = |event: &Event| match event {
        Event::Notification { method, params } => {
            json!({ "kind": "notification", "method": method, "params": params })
        }
        // `idKey` 是服务端 id 的规范文本形式，网页原样回传即可与审批等待表对齐。
        Event::Request { id, method, params } => {
            json!({
                "kind": "request",
                "id": id,
                "idKey": id.to_string(),
                "method": method,
                "params": params,
            })
        }
        Event::Status { connected, detail } => {
            json!({ "kind": "status", "connected": connected, "detail": detail })
        }
    };
    loop {
        tokio::select! {
            received = events.recv() => match received {
                Ok(event) => sse.send(&payload(&event)).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    sse.send(&json!({ "kind": "lagged", "skipped": skipped })).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(Duration::from_secs(20)) => sse.keepalive().await?,
        }
    }
    Ok(())
}

/// 事件流连接结束时归还浏览器计数。
struct BrowserGuard(Arc<State>);

impl Drop for BrowserGuard {
    fn drop(&mut self) {
        self.0.browsers.fetch_sub(1, Ordering::Relaxed);
    }
}

fn authorized(request: &Request, token: &str) -> bool {
    if request
        .query_value("token")
        .is_some_and(|value| constant_time_eq(value, token))
    {
        return true;
    }
    if let Some(header) = request.header("authorization")
        && let Some(value) = header.strip_prefix("Bearer ")
        && constant_time_eq(value.trim(), token)
    {
        return true;
    }
    if let Some(cookie) = request.header("cookie") {
        for part in cookie.split(';') {
            if let Some(value) = part.trim().strip_prefix("codey_gateway_token=")
                && constant_time_eq(value, token)
            {
                return true;
            }
        }
    }
    false
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

type ApiResult = Result<Value, (u16, Value)>;

fn bad(message: &str) -> (u16, Value) {
    (400, json!({ "error": { "message": message } }))
}

fn upstream_error(error: Value) -> (u16, Value) {
    (502, json!({ "error": error }))
}

async fn api(state: &Arc<State>, request: &Request) -> (u16, Value) {
    let segments = request
        .path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let result = match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["api", "health"]) => Ok(json!({
            "status": "ok",
            "localRouter": state
                .overrides
                .iter()
                .any(|entry| entry.contains("codey_router")),
        })),
        ("GET", ["api", "models"]) => api_models(state).await,
        ("GET", ["api", "threads"]) => api_threads(state, request).await,
        ("GET", ["api", "threads", id]) => api_thread(state, id).await,
        ("GET", ["api", "threads", id, "turns"]) => api_turns(state, request, id).await,
        ("POST", ["api", "threads", id, "messages"]) => api_message(state, request, id).await,
        ("POST", ["api", "threads", id, "interrupt"]) => api_interrupt(state, request, id).await,
        ("POST", ["api", "threads", id, "close"]) => api_close(state, id).await,
        ("POST", ["api", "approvals", id]) => api_approval(state, request, id).await,
        _ => Err((404, json!({ "error": { "message": "接口不存在" } }))),
    };
    match result {
        Ok(value) => (200, value),
        Err(error) => error,
    }
}

async fn api_models(state: &State) -> ApiResult {
    let value = state
        .upstream
        .request("model/list", json!({ "limit": 100 }))
        .await
        .map_err(upstream_error)?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let models = data
        .iter()
        .filter(|model| model.get("hidden").and_then(Value::as_bool) != Some(true))
        .map(|model| {
            json!({
                "id": model.get("id"),
                "model": model.get("model"),
                "displayName": model.get("displayName"),
                "description": model.get("description"),
                "isDefault": model.get("isDefault"),
                "defaultReasoningEffort": model.get("defaultReasoningEffort"),
                "supportedReasoningEfforts": model.get("supportedReasoningEfforts"),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "models": models }))
}

async fn api_threads(state: &State, request: &Request) -> ApiResult {
    let mut params = json!({ "limit": 60, "archived": false });
    if let Some(search) = request
        .query_value("search")
        .map(str::trim)
        .filter(|search| !search.is_empty())
    {
        params["searchTerm"] = json!(search);
    }
    if let Some(cursor) = request
        .query_value("cursor")
        .filter(|cursor| !cursor.is_empty())
    {
        params["cursor"] = json!(cursor);
    }
    let value = state
        .upstream
        .request("thread/list", params)
        .await
        .map_err(upstream_error)?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let threads = data
        .iter()
        .filter(|thread| {
            thread
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| seen.insert(id.to_owned()))
        })
        .map(compact_thread)
        .collect::<Vec<_>>();
    Ok(json!({
        "threads": threads,
        "nextCursor": value.get("nextCursor").cloned().unwrap_or(Value::Null),
    }))
}

fn compact_thread(thread: &Value) -> Value {
    json!({
        "id": thread.get("id"),
        "name": thread.get("name"),
        "preview": thread.get("preview"),
        "cwd": thread.get("cwd"),
        "createdAt": thread.get("createdAt"),
        "updatedAt": thread.get("updatedAt"),
        "model": thread.get("model"),
        "status": thread.get("status"),
        "source": thread.get("source"),
    })
}

async fn api_thread(state: &State, id: &str) -> ApiResult {
    // 只读取磁盘历史与会话元数据，不 resume：围观桌面正在运行的会话时不会惊动桌面进程。
    let value = state
        .upstream
        .request("thread/read", json!({ "threadId": id }))
        .await
        .map_err(upstream_error)?;
    let thread = value.get("thread").cloned().unwrap_or(Value::Null);
    let model = thread.get("model").cloned().unwrap_or(Value::Null);
    let effort = thread
        .get("reasoningEffort")
        .cloned()
        .unwrap_or(Value::Null);
    let cwd = thread.get("cwd").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "thread": thread,
        "model": model,
        "reasoningEffort": effort,
        "cwd": cwd,
    }))
}

async fn api_turns(state: &State, request: &Request, id: &str) -> ApiResult {
    let limit = request
        .query_value("limit")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0 && *value <= 40)
        .unwrap_or(DEFAULT_PAGE_TURNS);
    let mut params = json!({
        "threadId": id,
        "limit": limit,
        "itemsView": "full",
        "sortDirection": "desc",
    });
    if let Some(cursor) = request
        .query_value("cursor")
        .filter(|cursor| !cursor.is_empty())
    {
        params["cursor"] = json!(cursor);
    }
    let value = state
        .upstream
        .request("thread/turns/list", params)
        .await
        .map_err(upstream_error)?;
    Ok(json!({
        "turns": value.get("data").cloned().unwrap_or(json!([])),
        "nextCursor": value.get("nextCursor").cloned().unwrap_or(Value::Null),
    }))
}

async fn api_message(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| bad("消息内容为空"))?;
    let mut params = json!({
        "threadId": id,
        "input": [{ "type": "text", "text": text }],
    });
    // app-server 没有单独的会话设置接口，模型与推理强度只能随回合覆盖，并会沿用到后续回合。
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        params["model"] = json!(model);
    }
    if let Some(effort) = body.get("effort").and_then(Value::as_str) {
        params["effort"] = json!(effort);
    }
    // 发送前把会话接入本进程；失败也继续尝试，由 turn/start 决定最终结果。
    let _ = state
        .upstream
        .request(
            "thread/resume",
            json!({ "threadId": id, "excludeTurns": true }),
        )
        .await;
    let value = state
        .upstream
        .request("turn/start", params)
        .await
        .map_err(upstream_error)?;
    Ok(json!({ "turn": value.get("turn").cloned().unwrap_or(Value::Null) }))
}

async fn api_interrupt(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    let turn_id = body
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty())
        .ok_or_else(|| bad("缺少 turnId"))?;
    state
        .upstream
        .request(
            "turn/interrupt",
            json!({ "threadId": id, "turnId": turn_id }),
        )
        .await
        .map_err(upstream_error)?;
    Ok(json!({ "stopped": true }))
}

async fn api_close(state: &State, id: &str) -> ApiResult {
    let _ = state
        .upstream
        .request("thread/unsubscribe", json!({ "threadId": id }))
        .await;
    Ok(json!({ "closed": true }))
}

async fn api_approval(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    let result = body
        .get("result")
        .cloned()
        .ok_or_else(|| bad("缺少审批结果"))?;
    if state.approvals.resolve(id, result).await {
        Ok(json!({ "answered": true }))
    } else {
        Err((
            404,
            json!({ "error": { "message": "审批请求不存在或已过期" } }),
        ))
    }
}
