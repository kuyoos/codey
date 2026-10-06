//! Codey 局域网远程控制网关。
//!
//! 桌面自身的 app-server 走 stdio，与桌面窗口一一绑定，浏览器无法接入；浏览器也不能
//! 直连 app-server，因为它暴露文件、命令与配置接口。网关于是运行在桌面 app-server 的包装
//! 进程里，把 app-server 转成带令牌的局域网 HTTP 与 SSE 供网页使用。
//!
//! 默认的共享模式（见 [`shared`]）直接复用桌面这条 stdio：桌面数据流逐行直通，网页请求
//! 注入同一连接，因此桌面正在运行的会话也能收发，且不影响桌面行为。配置里的 `isolated`
//! 模式会另起一个同源 app-server，只能读磁盘历史，仅在需要严格隔离时使用。
//!
//! 网关不是独立进程：只有拿到 `CODEX_HOME` 下独占文件锁的那一次包装器启动才会运行，
//! 因此跟随桌面 app-server 的生命周期，也不会重复监听端口。

pub(crate) mod console;
mod frp;
mod http;
mod shared;
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
use tokio::sync::{Mutex, broadcast, oneshot};

use http::{EventStream, Request, read_request, write_json, write_response};
use upstream::{Event, Upstream};

const CONFIG_FILE_NAME: &str = ".codey-remote-gateway.json";
const TOKEN_FILE_NAME: &str = ".codey-remote-gateway.token";
const URL_FILE_NAME: &str = ".codey-remote-gateway.url";
const LOCK_FILE_NAME: &str = ".codey-remote-gateway.lock";
const WS_TOKEN_FILE_NAME: &str = ".codey-remote-gateway.ws-token";
const DEFAULT_PORT: u16 = 8799;
const DEFAULT_PAGE_TURNS: u32 = 8;
/// 网页只展示概要，工具输出保留一小段预览。
const OUTPUT_PREVIEW_CHARS: usize = 400;
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
    #[serde(default)]
    mode: GatewayMode,
    #[serde(default)]
    frp: FrpConfig,
}

/// 上游连接方式；控制台只暴露开关与端口，模式留作严格隔离时的兜底。
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum GatewayMode {
    /// 复用桌面 app-server 的 stdio，运行中的会话也能收发。
    #[default]
    Shared,
    /// 另起一个同源 app-server，只读磁盘历史，不影响桌面会话。
    Isolated,
}

fn enabled_default() -> bool {
    true
}

fn port_default() -> u16 {
    DEFAULT_PORT
}

fn frp_server_port_default() -> u16 {
    7000
}

/// frp 映射配置；控制台只暴露常用字段，其余保持默认。
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FrpConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    server_addr: String,
    #[serde(default = "frp_server_port_default")]
    server_port: u16,
    #[serde(default)]
    token: String,
    #[serde(default)]
    remote_port: u16,
    /// 留空表示用自动下载的 frpc；填写后使用本机已有的可执行文件。
    #[serde(default)]
    binary: String,
}

impl Default for FrpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_addr: String::new(),
            server_port: frp_server_port_default(),
            token: String::new(),
            remote_port: 0,
            binary: String::new(),
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: enabled_default(),
            port: port_default(),
            mode: GatewayMode::default(),
            frp: FrpConfig::default(),
        }
    }
}

/// 在桌面 app-server 的包装进程里尝试启动网关；每个进程只尝试一次。
///
/// 返回 `Some` 表示网关要接管这次 app-server 的 stdio（共享模式），包装器需要在 spawn
/// 之后把管道交给 [`SharedUpstream::serve`]；返回 `None` 表示网关未启用，或已按独占模式
/// 自行启动（独占模式不需要包装器改管道）。`args` 是去掉受管配置后重新拼装的 app-server
/// 参数，网关沿用同一份本地路由配置。
pub(crate) fn prepare_upstream(
    target: &Path,
    args: &[OsString],
    overrides: &[String],
) -> Option<SharedUpstream> {
    if ATTEMPTED.set(()).is_err() {
        return None;
    }
    let home = crate::codex_config::codex_home().to_path_buf();
    let config = load_or_create_config(&home);
    if !config.enabled {
        log(
            "codey.remote_gateway.disabled",
            json!({ "config": home.join(CONFIG_FILE_NAME).display().to_string() }),
        );
        return None;
    }
    let guard = acquire_lock(&home)?;
    let token = load_or_create_token(&home)?;
    let _ = LOCK_GUARD.set(guard);
    let overrides = overrides.to_vec();
    if config.mode == GatewayMode::Isolated {
        start_isolated_upstream(&home, target, args, overrides, &token, config.port);
        return None;
    }
    Some(SharedUpstream {
        home,
        http_port: config.port,
        overrides,
    })
}

/// 独占模式：另起一个同源 app-server，只监听回环 WebSocket 并核验能力令牌。
fn start_isolated_upstream(
    home: &Path,
    target: &Path,
    args: &[OsString],
    overrides: Vec<String>,
    token: &str,
    http_port: u16,
) {
    if std::fs::write(home.join(WS_TOKEN_FILE_NAME), token).is_err() {
        log(
            "codey.remote_gateway.ws_token_failed",
            json!({ "message": "无法写入上游能力令牌" }),
        );
        return;
    }
    let upstream_port = match free_loopback_port() {
        Ok(port) => port,
        Err(_) => return,
    };
    let args = transport_args(args, upstream_port, &home.join(WS_TOKEN_FILE_NAME));
    let target = target.to_path_buf();
    let home = home.to_path_buf();
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
                upstream_port,
                http_port,
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

/// 共享模式的接管参数。
pub(crate) struct SharedUpstream {
    home: PathBuf,
    http_port: u16,
    overrides: Vec<String>,
}

impl SharedUpstream {
    /// 在独立线程里启动网关；`server_stdin`/`server_stdout` 是桌面 app-server 的管道，
    /// `source` 是桌面输入（本地路由模式下为转发器的输出），缺省时读取包装器自身的 stdin。
    pub(crate) fn serve(
        self,
        server_stdin: Option<std::process::ChildStdin>,
        server_stdout: Option<std::process::ChildStdout>,
        source: Option<Box<dyn std::io::Read + Send>>,
    ) {
        let (Some(server_stdin), Some(server_stdout)) = (server_stdin, server_stdout) else {
            log(
                "codey.remote_gateway.stdio_failed",
                json!({ "message": "桌面 app-server 管道不可用" }),
            );
            return;
        };
        let local_router =
            crate::codex_startup_patch::local_router_runtime_enabled(&self.overrides);
        let SharedUpstream {
            home,
            http_port,
            overrides,
        } = self;
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
                let source = source.unwrap_or_else(|| Box::new(std::io::stdin()));
                let upstream = shared::start(server_stdin, server_stdout, source, local_router);
                runtime.block_on(serve_shared(home, http_port, overrides, upstream));
            });
        if let Err(error) = spawned {
            log(
                "codey.remote_gateway.thread_failed",
                json!({ "message": format!("{error}") }),
            );
        }
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
    frp::ensure_started(&home, port);
    let state = Arc::new(State {
        upstream: UpstreamLink::Isolated(Arc::clone(&upstream)),
        approvals: Approvals::default(),
        browsers: AtomicUsize::new(0),
        token,
        overrides,
        shared: false,
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

/// 共享模式：上游就是桌面 app-server 的 stdio，网关只负责转成局域网 HTTP 与 SSE。
async fn serve_shared(
    home: PathBuf,
    http_port: u16,
    overrides: Vec<String>,
    upstream: Arc<shared::SharedLink>,
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
    frp::ensure_started(&home, port);
    let state = Arc::new(State {
        upstream: UpstreamLink::Shared(upstream),
        approvals: Approvals::default(),
        browsers: AtomicUsize::new(0),
        token,
        overrides,
        shared: true,
    });
    spawn_request_responder(Arc::clone(&state));
    publish_url(&home, port, &state.token);
    log(
        "codey.remote_gateway.started",
        json!({
            "mode": "shared",
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
    let token = new_token();
    std::fs::write(&path, &token).ok()?;
    Some(token)
}

/// 网页访问密钥：控制台重新生成时也走这里，保证格式一致。
pub(crate) fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
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

/// 网页请求的上游通道。
enum UpstreamLink {
    /// 独占模式：另起的同源 app-server，通过回环 WebSocket 通信。
    Isolated(Arc<Upstream>),
    /// 共享模式：注入桌面 app-server 已有的 stdio 连接。
    Shared(Arc<shared::SharedLink>),
}

impl UpstreamLink {
    async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        match self {
            Self::Isolated(upstream) => upstream.request(method, params).await,
            Self::Shared(upstream) => upstream.request(method, params).await,
        }
    }

    async fn reply(&self, id: Value, result: Result<Value, Value>) {
        match self {
            Self::Isolated(upstream) => upstream.reply(id, result).await,
            Self::Shared(upstream) => upstream.reply(id, result).await,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        match self {
            Self::Isolated(upstream) => upstream.subscribe(),
            Self::Shared(upstream) => upstream.subscribe(),
        }
    }
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
    upstream: UpstreamLink,
    approvals: Approvals,
    browsers: AtomicUsize,
    token: String,
    overrides: Vec<String>,
    /// 共享模式的上游就是桌面会话，反向请求由桌面应答，网页不能替它给默认值。
    shared: bool,
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

/// 只有网页能给出合法应答的审批请求才交给浏览器；其余请求由桌面应答或给出安全默认应答，
/// 避免会话卡死。
///
/// 旧版 `applyPatchApproval` 与 `execCommandApproval` 的决策枚举、以及 `item/permissions`
/// 需要回传具体权限，网页无法完整构造，因此保持默认拒绝。
fn is_interactive_approval(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    )
}

/// 独占模式下网页不处理的请求一律给出安全的默认应答，避免会话卡死；共享模式由桌面应答。
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
                // 共享模式的上游就是桌面会话，其余反向请求由桌面应答，网页不能抢先给默认值。
                if !state.shared {
                    let _ = state.upstream.reply(id, default_reply(&method)).await;
                }
                continue;
            }
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                // 独占模式没有桌面兜底：没有网页在看时立即拒绝，避免无人应答的会话长时间挂起。
                if !state.shared && state.browsers.load(Ordering::Relaxed) == 0 {
                    state.upstream.reply(id, default_reply(&method)).await;
                    return;
                }
                let wait = state.approvals.register(&id).await;
                match tokio::time::timeout(APPROVAL_TIMEOUT, wait).await {
                    Ok(Ok(value)) => state.upstream.reply(id, Ok(value)).await,
                    _ => {
                        state.approvals.forget(&id).await;
                        // 共享模式：桌面仍在等用户确认，网页超时不要替它决定。
                        if !state.shared {
                            state.upstream.reply(id, default_reply(&method)).await;
                        }
                    }
                }
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
    // 共享模式复用桌面连接，没有独立上游连接状态；直接给出已连接，避免网页停在“连接中”。
    if state.shared {
        sse.send(&json!({ "kind": "status", "connected": true, "detail": "" }))
            .await?;
    }
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

/// app-server 在会话被其他写入者占用时只回一句笼统的 `thread not found`；把发送前接入
/// 会话的真实原因带上，网页才能给出可读提示。
fn describe_send_failure(error: Value, resume_error: Option<&Value>) -> Value {
    let Some(reason) = resume_error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
    else {
        return error;
    };
    let mut described = error;
    if !described.is_object() {
        return described;
    }
    let message = described
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    described["message"] = json!(format!("{message}（发送前接入会话失败：{reason}）"));
    described
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
        ("GET", ["api", "threads", id, "usage"]) => api_usage(state, id).await,
        ("GET", ["api", "threads", id, "queue"]) => api_queue(state, id).await,
        ("POST", ["api", "threads", id, "messages"]) => api_message(state, request, id).await,
        ("POST", ["api", "threads", id, "queue"]) => api_queue_action(state, request, id).await,
        ("POST", ["api", "threads", id, "command"]) => api_command(state, request, id).await,
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
    let mut turns = value.get("data").cloned().unwrap_or(json!([]));
    slim_turns(&mut turns);
    Ok(json!({
        "turns": turns,
        "nextCursor": value.get("nextCursor").cloned().unwrap_or(Value::Null),
    }))
}

/// 网页只需要“做了什么”的概要：推理正文、工具参数与结果、命令输出正文、文件改动清单与图片结果
/// 在真实会话里能占响应九成以上，在网关侧统一裁剪后，网页加载更快、流量更低。
fn slim_turns(turns: &mut Value) {
    let Some(list) = turns.as_array_mut() else {
        return;
    };
    for turn in list.iter_mut() {
        let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items.iter_mut() {
            let Some(object) = item.as_object_mut() else {
                continue;
            };
            match object.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    let _ = object.remove("summary");
                    let _ = object.remove("content");
                }
                Some("commandExecution") => {
                    let _ = object.remove("commandActions");
                    // 网页只显示「工具与命令 · N 项」的计数，命令与输出正文不再下发。
                    let _ = object.remove("command");
                    let _ = object.remove("aggregatedOutput");
                }
                Some("mcpToolCall" | "dynamicToolCall") => {
                    for key in ["arguments", "result", "appContext", "mcpAppUi", "error"] {
                        let _ = object.remove(key);
                    }
                }
                Some("functionCallOutput") => {
                    truncate_field(object, "output", OUTPUT_PREVIEW_CHARS);
                }
                Some("webSearch") => {
                    let _ = object.remove("results");
                }
                Some("collabAgentToolCall") => {
                    let _ = object.remove("agentsStates");
                    truncate_field(object, "prompt", OUTPUT_PREVIEW_CHARS);
                }
                // 网页只显示「图片 N 项」的计数，图片路径与生成结果都不再下发。
                Some("imageView") => {
                    let _ = object.remove("path");
                }
                Some("imageGeneration") => {
                    for key in ["result", "revisedPrompt", "savedPath"] {
                        let _ = object.remove(key);
                    }
                }
                Some("fileChange") => {
                    // 网页只显示「修改 N 个文件」的计数，改了哪些文件、怎么改的都不再下发。
                    let count = object
                        .remove("changes")
                        .and_then(|changes| changes.as_array().map(Vec::len))
                        .unwrap_or(0);
                    object.insert("changeCount".to_owned(), json!(count));
                }
                _ => {}
            }
        }
    }
}

/// 命令与输出只保留开头一段，超出部分标成截断，网页仍能看出大致内容。
fn truncate_field(object: &mut serde_json::Map<String, Value>, key: &str, max_chars: usize) {
    let Some(Value::String(text)) = object.get(key) else {
        return;
    };
    if text.chars().count() <= max_chars {
        return;
    }
    let mut preview: String = text.chars().take(max_chars).collect();
    preview.push_str("\n…（已截断）");
    object.insert(key.to_owned(), Value::String(preview));
}

/// app-server 的回合结构不含 token 字段，用量只落在 rollout jsonl 里：会话续写会把同一个
/// sessionId 拆成多个分段文件，因此按 sessionId 找到全部分段，逐个取回合与线程的累计值。
async fn api_usage(state: &State, id: &str) -> ApiResult {
    let value = state
        .upstream
        .request("thread/read", json!({ "threadId": id }))
        .await
        .map_err(upstream_error)?;
    let thread = value.get("thread").cloned().unwrap_or(Value::Null);
    let session_id = thread
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            thread
                .get("path")
                .and_then(Value::as_str)
                .and_then(session_id_from_rollout)
        });
    let Some(session_id) = session_id else {
        return Ok(json!({ "total": Value::Null, "turns": {} }));
    };
    let usage = tokio::task::spawn_blocking(move || collect_token_usage(&session_id))
        .await
        .unwrap_or_else(|_| json!({ "total": Value::Null, "turns": {} }));
    Ok(usage)
}

/// `rollout-2026-10-05T09-27-08-<sessionId>[_<分段>].jsonl`：时间戳固定形如
/// `YYYY-MM-DDTHH-MM-SS`，其后的 `T` 再往后 9 个字符起，到 `_` 之间就是 sessionId。
fn session_id_from_rollout(path: &str) -> Option<String> {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let after_date = name.get(name.find('T')? + 1..)?;
    let id = after_date.get(9..)?.split('_').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// 按 sessionId 在 sessions 树里递归匹配分段文件；文件名以 `rollout-<时间戳>` 开头，
/// 字典序即时间序，后写的分段持有更大的累计值。只解析含 token 记录的行。
fn collect_token_usage(session_id: &str) -> Value {
    let mut files = Vec::new();
    let root = crate::codex_config::codex_home().join("sessions");
    collect_rollout_files(&root, session_id, &mut files);
    files.sort();
    let mut turns = serde_json::Map::new();
    let mut total: Option<Value> = None;
    let mut best_total = -1i64;
    for file in files {
        let Ok(handle) = std::fs::File::open(&file) else {
            continue;
        };
        let mut reader = std::io::BufReader::new(handle);
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut reader, &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            if !line.contains("\"token_usage_record\"") {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(payload) = record.get("payload") else {
                continue;
            };
            if let (Some(turn_id), Some(turn_usage)) = (
                payload.get("turn_id").and_then(Value::as_str),
                payload.get("turn_token_usage"),
            ) {
                turns.insert(turn_id.to_owned(), turn_usage.clone());
            }
            if let Some(thread_usage) = payload.get("thread_token_usage") {
                let tokens = thread_usage
                    .get("total_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if tokens >= best_total {
                    best_total = tokens;
                    total = Some(thread_usage.clone());
                }
            }
        }
    }
    json!({ "total": total.unwrap_or(Value::Null), "turns": turns })
}

fn collect_rollout_files(dir: &Path, session_id: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, session_id, found);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(session_id) && name.ends_with(".jsonl"))
        {
            found.push(path);
        }
    }
}

/// 网页在会话回合运行中再次发送时的投递方式：引导并入当前回合，或排队等回合结束后执行。
#[derive(Debug, PartialEq, Eq)]
enum SendMode {
    Start,
    Steer(String),
    Queue,
}

/// 解析发送方式；引导必须带上正在运行的回合 ID，与上游 `expectedTurnId` 的前置条件一致。
fn parse_send_mode(body: &Value) -> Result<SendMode, (u16, Value)> {
    match body.get("mode").and_then(Value::as_str).map(str::trim) {
        None | Some("") | Some("start") => Ok(SendMode::Start),
        Some("steer") => {
            let turn_id = body
                .get("turnId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|turn| !turn.is_empty())
                .ok_or_else(|| bad("缺少正在运行的回合 ID"))?;
            Ok(SendMode::Steer(turn_id.to_owned()))
        }
        Some("queue") => Ok(SendMode::Queue),
        Some(_) => Err(bad("不支持的消息发送方式")),
    }
}

async fn api_message(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| bad("消息内容为空"))?;
    let mode = parse_send_mode(&body)?;
    let input = json!([{ "type": "text", "text": text }]);
    // 引导与排队都只作用于正在运行的回合：上游此刻已持有该会话，再 resume 只会撞上写入者冲突，
    // 因此直接在当前连接上投递。新回合需要先接入会话，失败也继续，由 turn/start 决定最终结果。
    let (method, params, resume_error) = match &mode {
        SendMode::Start => {
            let mut params = json!({ "threadId": id, "input": input });
            // app-server 没有单独的会话设置接口，模型与推理强度只能随回合覆盖，并会沿用到后续回合。
            if let Some(model) = body.get("model").and_then(Value::as_str) {
                params["model"] = json!(model);
            }
            if let Some(effort) = body.get("effort").and_then(Value::as_str) {
                params["effort"] = json!(effort);
            }
            let resume_error = state
                .upstream
                .request(
                    "thread/resume",
                    json!({ "threadId": id, "excludeTurns": true }),
                )
                .await
                .err();
            ("turn/start", params, resume_error)
        }
        SendMode::Steer(turn_id) => (
            "turn/steer",
            json!({
                "threadId": id,
                "expectedTurnId": turn_id,
                "clientUserMessageId": uuid::Uuid::new_v4().to_string(),
                "input": input,
            }),
            None,
        ),
        SendMode::Queue => (
            "thread/queue/add",
            json!({
                "threadId": id,
                "clientUserMessageId": uuid::Uuid::new_v4().to_string(),
                "input": input,
            }),
            None,
        ),
    };
    let value = state
        .upstream
        .request(method, params)
        .await
        .map_err(|error| upstream_error(describe_send_failure(error, resume_error.as_ref())))?;
    Ok(match &mode {
        SendMode::Start => json!({
            "mode": "start",
            "turn": value.get("turn").cloned().unwrap_or(Value::Null),
        }),
        SendMode::Steer(turn_id) => json!({
            "mode": "steer",
            "turnId": value.get("turnId").cloned().unwrap_or(json!(turn_id)),
        }),
        SendMode::Queue => json!({
            "mode": "queue",
            "queuedSubmission": value.get("queuedSubmission").cloned().unwrap_or(Value::Null),
        }),
    })
}

/// 网页一次最多列出这么多条排队消息，不需要完整分页；预览只取正文前若干字。
const QUEUE_LIMIT: u32 = 50;
const QUEUE_PREVIEW_CHARS: usize = 200;

/// 排队消息在网页上只用一行预览表示：只取文本片段并压掉空白，图片等非文本输入不下发。
fn queue_item(submission: &Value) -> Value {
    let text = submission
        .get("input")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(QUEUE_PREVIEW_CHARS).collect();
    let preview = if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    };
    json!({
        "id": submission.get("id").cloned().unwrap_or(Value::Null),
        "text": if preview.is_empty() { "（非文本消息）".to_owned() } else { preview },
    })
}

/// 网页可对排队消息执行的动作：立即插队执行，或删除。都只按 ID 作用于单条提交。
#[derive(Debug, PartialEq, Eq)]
enum QueueAction {
    Delete(String),
    Start(String),
}

fn queued_submission_id(body: &Value) -> Result<&str, (u16, Value)> {
    body.get("queuedSubmissionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| bad("缺少排队消息 ID"))
}

fn parse_queue_action(body: &Value) -> Result<QueueAction, (u16, Value)> {
    match body.get("action").and_then(Value::as_str).map(str::trim) {
        Some("delete") => Ok(QueueAction::Delete(queued_submission_id(body)?.to_owned())),
        Some("start") => Ok(QueueAction::Start(queued_submission_id(body)?.to_owned())),
        _ => Err(bad("不支持的排队操作")),
    }
}

async fn api_queue(state: &State, id: &str) -> ApiResult {
    let value = state
        .upstream
        .request(
            "thread/queue/list",
            json!({ "threadId": id, "limit": QUEUE_LIMIT }),
        )
        .await
        .map_err(upstream_error)?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .map(|data| data.iter().map(queue_item).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({ "items": items }))
}

async fn api_queue_action(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    match parse_queue_action(&body)? {
        QueueAction::Delete(submission) => {
            let value = state
                .upstream
                .request(
                    "thread/queue/delete",
                    json!({ "threadId": id, "queuedSubmissionId": submission }),
                )
                .await
                .map_err(upstream_error)?;
            Ok(json!({
                "deleted": value.get("deleted").cloned().unwrap_or(json!(true)),
            }))
        }
        QueueAction::Start(submission) => {
            // 不重写内容，只让这条插队开始；上游返回它开启的回合。
            let value = state
                .upstream
                .request(
                    "thread/queue/start",
                    json!({ "threadId": id, "queuedSubmissionId": submission }),
                )
                .await
                .map_err(upstream_error)?;
            Ok(json!({
                "started": true,
                "turn": value.get("turn").cloned().unwrap_or(Value::Null),
            }))
        }
    }
}

/// 网页可触发的斜杠命令。网关是局域网可达的带令牌入口，只放行这几个固定动作，
/// 绝不把任意 app-server 方法透传出去。
#[derive(Debug, PartialEq, Eq)]
enum WebCommand {
    Compact,
    Review,
    Rename(String),
}

const COMMAND_NAME_MAX_CHARS: usize = 200;

/// 把请求体解析成白名单命令；未知命令、缺失或超长参数都在这里直接拒绝。
fn parse_web_command(body: &Value) -> Result<WebCommand, (u16, Value)> {
    let command = body
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .ok_or_else(|| bad("缺少命令"))?;
    match command {
        "compact" => Ok(WebCommand::Compact),
        "review" => Ok(WebCommand::Review),
        "rename" => {
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| bad("缺少新名称"))?;
            if name.chars().count() > COMMAND_NAME_MAX_CHARS {
                return Err(bad("名称过长"));
            }
            Ok(WebCommand::Rename(name.to_owned()))
        }
        _ => Err(bad("不支持的命令")),
    }
}

async fn api_command(state: &State, request: &Request, id: &str) -> ApiResult {
    let body = request.json().map_err(|error| bad(&error.to_string()))?;
    let command = parse_web_command(&body)?;
    match command {
        WebCommand::Compact => {
            // 压缩与评审都要求会话已接入上游，先 resume；失败也继续，由命令本身给出最终错误。
            let _ = state
                .upstream
                .request(
                    "thread/resume",
                    json!({ "threadId": id, "excludeTurns": true }),
                )
                .await;
            state
                .upstream
                .request("thread/compact/start", json!({ "threadId": id }))
                .await
                .map_err(upstream_error)?;
            Ok(json!({ "started": "compact" }))
        }
        WebCommand::Review => {
            let _ = state
                .upstream
                .request(
                    "thread/resume",
                    json!({ "threadId": id, "excludeTurns": true }),
                )
                .await;
            // 默认 inline 投递：评审在当前会话内进行，不另开评审会话。
            let value = state
                .upstream
                .request(
                    "review/start",
                    json!({ "threadId": id, "target": { "type": "uncommittedChanges" } }),
                )
                .await
                .map_err(upstream_error)?;
            Ok(json!({
                "started": "review",
                "turn": value.get("turn").cloned().unwrap_or(Value::Null),
            }))
        }
        WebCommand::Rename(name) => {
            state
                .upstream
                .request("thread/name/set", json!({ "threadId": id, "name": name }))
                .await
                .map_err(upstream_error)?;
            Ok(json!({ "name": name }))
        }
    }
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
    // 共享模式的上游就是桌面正在使用的同一条连接：`thread/unsubscribe` 会把桌面对该会话
    // 的订阅一并退掉，桌面从此收不到 `turn/completed` 与状态变化，界面就一直停在“运行中”。
    // 网页只放弃自己的本地视图，不得改动桌面连接上的订阅。
    if state.shared {
        return Ok(json!({ "closed": true }));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_items_are_slimmed_for_the_web() {
        let long_output = "行".repeat(OUTPUT_PREVIEW_CHARS + 50);
        let mut turns = json!([
            {
                "id": "t1",
                "items": [
                    { "type": "reasoning", "id": "r1", "summary": ["很长的推理"], "content": ["正文"] },
                    { "type": "agentMessage", "id": "a1", "text": "回答" },
                    { "type": "commandExecution", "id": "c1", "command": "cargo build", "aggregatedOutput": long_output, "commandActions": [{ "command": "cargo build" }] },
                    { "type": "mcpToolCall", "id": "m1", "server": "srv", "tool": "tool", "arguments": { "big": "参数" }, "result": { "big": "结果" } },
                    { "type": "functionCallOutput", "id": "o1", "name": "shell", "output": long_output },
                    { "type": "fileChange", "id": "f1", "changes": [{ "path": "src/a.rs", "kind": { "type": "update", "move_path": null }, "diff": "@@ -1,1 +1,2 @@" }] },
                    { "type": "webSearch", "id": "w1", "query": "问题", "results": [{ "huge": "结果" }] },
                    { "type": "imageView", "id": "v1", "path": "file:///tmp/a.png" },
                    { "type": "imageGeneration", "id": "g1", "status": "completed", "result": "data:image/png;base64,AAAA", "revisedPrompt": "改过的提示词", "savedPath": "file:///tmp/b.png" }
                ]
            },
            { "id": "t2" }
        ]);
        slim_turns(&mut turns);
        let items = turns[0]["items"].as_array().expect("items");
        assert_eq!(items.len(), 9);
        assert_eq!(items[0]["type"], json!("reasoning"));
        assert!(items[0].get("summary").is_none());
        assert!(items[0].get("content").is_none());
        assert_eq!(items[1]["text"], json!("回答"));
        // 工具与命令只留计数，命令与输出正文不再下发。
        assert!(items[2].get("command").is_none());
        assert!(items[2].get("aggregatedOutput").is_none());
        assert!(items[2].get("commandActions").is_none());
        assert!(items[3].get("arguments").is_none());
        assert!(items[3].get("result").is_none());
        assert_eq!(items[3]["server"], json!("srv"));
        // 工具输出仍保留一小段预览并标记截断。
        let output = items[4]["output"].as_str().expect("output");
        assert!(output.ends_with("…（已截断）"));
        assert!(output.chars().count() < long_output.chars().count());
        // 文件改动只留计数，改动清单与 diff 都不再下发。
        assert!(items[5].get("changes").is_none());
        assert_eq!(items[5]["changeCount"], json!(1));
        assert!(items[6].get("results").is_none());
        assert_eq!(items[6]["query"], json!("问题"));
        // 图片只留计数所需字段，路径、提示词与生成结果都不再下发。
        assert!(items[7].get("path").is_none());
        assert_eq!(items[7]["type"], json!("imageView"));
        assert!(items[8].get("result").is_none());
        assert!(items[8].get("revisedPrompt").is_none());
        assert!(items[8].get("savedPath").is_none());
        assert_eq!(items[8]["status"], json!("completed"));
    }

    #[test]
    fn session_id_is_parsed_from_segmented_rollout_names() {
        let id = "01a10721-2cf5-7271-b22f-544b877b192e";
        assert_eq!(
            session_id_from_rollout(&format!("rollout-2026-10-04T21-36-17-{id}.jsonl")),
            Some(id.to_owned())
        );
        // 会话续写的分段文件带 `_<分段>` 后缀；目录用 `/` 分隔，让断言在 Windows 与 Linux 上都成立。
        assert_eq!(
            session_id_from_rollout(&format!(
                "sessions/2026/10/05/rollout-2026-10-05T09-27-08-{id}_01a109ab-fa6d-7ad1-ad34-0afa27f448e6.jsonl"
            )),
            Some(id.to_owned())
        );
        assert_eq!(session_id_from_rollout("thread.jsonl"), None);
        assert_eq!(
            session_id_from_rollout("rollout-2026-10-05T09-27-08.jsonl"),
            None
        );
    }

    #[test]
    fn send_mode_gates_steer_and_queue() {
        // 缺省与显式 start 都走新回合，保持既有行为。
        assert_eq!(parse_send_mode(&json!({})), Ok(SendMode::Start));
        assert_eq!(
            parse_send_mode(&json!({ "mode": "start" })),
            Ok(SendMode::Start)
        );
        assert_eq!(
            parse_send_mode(&json!({ "mode": "queue" })),
            Ok(SendMode::Queue)
        );
        assert_eq!(
            parse_send_mode(&json!({ "mode": "steer", "turnId": " t1 " })),
            Ok(SendMode::Steer("t1".to_owned()))
        );
        // 引导必须带正在运行的回合 ID，未知方式一律拒绝，不向上游透传。
        assert!(parse_send_mode(&json!({ "mode": "steer" })).is_err());
        assert!(parse_send_mode(&json!({ "mode": "steer", "turnId": "  " })).is_err());
        assert!(parse_send_mode(&json!({ "mode": "broadcast" })).is_err());
    }

    #[test]
    fn queued_items_are_slimmed_and_actions_gated() {
        // 只保留标识与文本预览：图片等非文本输入不下发，长正文截断，空白压成一行。
        let item = queue_item(&json!({
            "id": "q1",
            "clientUserMessageId": "c1",
            "input": [
                { "type": "text", "text": "  第一行\n第二行  " },
                { "type": "image", "url": "https://example.com/a.png" },
            ],
        }));
        assert_eq!(item["id"], json!("q1"));
        assert_eq!(item["text"], json!("第一行 第二行"));
        assert!(item.get("clientUserMessageId").is_none());
        assert_eq!(
            queue_item(&json!({ "id": "q2", "input": [{ "type": "image", "url": "x" }] }))["text"],
            json!("（非文本消息）")
        );
        let long = "字".repeat(QUEUE_PREVIEW_CHARS + 5);
        let preview =
            queue_item(&json!({ "id": "q3", "input": [{ "type": "text", "text": long }] }));
        let text = preview["text"].as_str().expect("text");
        assert!(text.ends_with('…'));
        assert_eq!(text.chars().count(), QUEUE_PREVIEW_CHARS + 1);

        assert_eq!(
            parse_queue_action(&json!({ "action": "delete", "queuedSubmissionId": " q1 " })),
            Ok(QueueAction::Delete("q1".to_owned()))
        );
        assert_eq!(
            parse_queue_action(&json!({ "action": "start", "queuedSubmissionId": "q2" })),
            Ok(QueueAction::Start("q2".to_owned()))
        );
        // 未知动作与缺 ID 一律拒绝，不向上游透传。
        assert!(parse_queue_action(&json!({ "action": "reorder" })).is_err());
        assert!(parse_queue_action(&json!({ "action": "delete" })).is_err());
        assert!(
            parse_queue_action(&json!({ "action": "start", "queuedSubmissionId": " " })).is_err()
        );
        assert!(parse_queue_action(&json!({})).is_err());
    }

    #[test]
    fn web_commands_are_whitelisted() {
        assert_eq!(
            parse_web_command(&json!({ "command": "compact" })),
            Ok(WebCommand::Compact)
        );
        assert_eq!(
            parse_web_command(&json!({ "command": " review " })),
            Ok(WebCommand::Review)
        );
        assert_eq!(
            parse_web_command(&json!({ "command": "rename", "name": " 新标题 " })),
            Ok(WebCommand::Rename("新标题".to_owned()))
        );
        // 未知命令与缺失参数一律拒绝，不做任何上游透传。
        assert!(parse_web_command(&json!({ "command": "delete" })).is_err());
        assert!(parse_web_command(&json!({ "command": "rename" })).is_err());
        assert!(parse_web_command(&json!({ "command": "rename", "name": "   " })).is_err());
        assert!(parse_web_command(&json!({})).is_err());
        let long = "长".repeat(COMMAND_NAME_MAX_CHARS + 1);
        assert!(parse_web_command(&json!({ "command": "rename", "name": long })).is_err());
    }
}
