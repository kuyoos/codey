//! 渲染进程早期挂载：浏览器端点上的 Target 自动挂载。
//!
//! Electron 的页面 target 在首个导航提交前可被 `waitForDebuggerOnStart`
//! 暂停：此刻 Fetch 拦截与文档脚本能赶在第一个资产请求之前落位，源码级
//! 请求门禁（app-server 模块改写）在模块求值时即可就绪，不必等整页水合
//! 后走运行时探测。武装失败或错过窗口都安全回退为普通注入路径。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use codey_runtime_core::bridge::{ConnectedCdpSocket, connect_cdp_websocket};
use codey_runtime_core::cdp::{
    CdpTarget, browser_websocket_url, is_primary_codex_page_target, list_targets,
    pick_injectable_codex_page_target,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// 武装尝试的重试窗口：页面出现得再晚也有机会被暂停；窗口内每次尝试只是
/// loopback 上的几次请求。与 /json 枚举并行跑，不阻塞 Ready 路径。
const ARM_BUDGET: Duration = Duration::from_secs(8);
/// 浏览器端点握手（连接 + setAutoAttach 应答）的单步预算。
const ARM_STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// 放行/清理命令的发送预算：尽力而为，不阻塞注入主流程。
const RELEASE_TIMEOUT: Duration = Duration::from_secs(3);
/// /json 目标枚举的探测间隔。
const TARGET_POLL_INTERVAL: Duration = Duration::from_millis(100);

static NEXT_EARLY_ATTACH_ID: AtomicU64 = AtomicU64::new(900_000);

fn next_id() -> u64 {
    NEXT_EARLY_ATTACH_ID.fetch_add(1, Ordering::Relaxed)
}

/// 预取拿到的页面目标：是否带暂停态决定注入路径要不要先放行。
pub enum PrefetchedPage {
    /// 首个导航前已暂停：拦截/文档脚本装好之后必须 resume 才会真正开始加载。
    Paused {
        target: CdpTarget,
        hold: EarlyAttachHold,
    },
    /// 页面已可注入但错过了暂停窗口（武装晚于导航或端点不支持）。
    Ready { target: CdpTarget },
}

/// 被暂停主页面的挂载句柄：持有浏览器端点写端与扁平化会话 id。
/// 任何路径上 Drop 都会尽力放行——页面停在暂停态远比丢一次拦截严重；
/// 浏览器会话已断开时改走页面级 WebSocket 兜底放行。
pub struct EarlyAttachHold {
    writer: Option<SplitSink<ConnectedCdpSocket, Message>>,
    session_id: String,
    target_ws_url: String,
}

impl EarlyAttachHold {
    /// 放行被暂停的页面：恢复等待中的调试器挂起并断开本次自动挂载会话。
    /// 这是尽力而为的发送：目标进程若已退出，发送失败也无副作用。
    pub async fn resume(mut self) {
        self.release().await;
    }

    async fn release(&mut self) {
        // 清空 session_id 作为已放行哨兵：随后的 Drop 不会再补发一次。
        let session_id = std::mem::take(&mut self.session_id);
        if let Some(mut writer) = self.writer.take() {
            let resume = json!({
                "sessionId": session_id,
                "id": next_id(),
                "method": "Runtime.runIfWaitingForDebugger",
            });
            let detach = json!({
                "id": next_id(),
                "method": "Target.detachFromTarget",
                "params": {"sessionId": session_id},
            });
            let _ = tokio::time::timeout(RELEASE_TIMEOUT, async {
                let _ = writer.send(Message::Text(resume.to_string().into())).await;
                let _ = writer.send(Message::Text(detach.to_string().into())).await;
                let _ = writer.close().await;
            })
            .await;
            return;
        }
        resume_paused_target_via_page_ws(&self.target_ws_url).await;
    }
}

impl Drop for EarlyAttachHold {
    fn drop(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        let writer = self.writer.take();
        let session_id = std::mem::take(&mut self.session_id);
        let target_ws_url = std::mem::take(&mut self.target_ws_url);
        // 进程收尾时没有 reactor：此时目标 renderer 通常也随进程退出，强行
        // spawn 反而 panic，直接放弃。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                EarlyAttachHold {
                    writer,
                    session_id,
                    target_ws_url,
                }
                .release()
                .await;
            });
        }
    }
}

/// 浏览器会话已不可用时，用目标自己的页面级 WebSocket 补发恢复命令：
/// `Runtime.runIfWaitingForDebugger` 是 target 级状态，任意已挂载会话
/// 都能放行。
async fn resume_paused_target_via_page_ws(target_ws_url: &str) {
    let _ = tokio::time::timeout(RELEASE_TIMEOUT, async {
        let Ok(mut socket) = connect_cdp_websocket(target_ws_url).await else {
            return;
        };
        let resume = json!({
            "id": next_id(),
            "method": "Runtime.runIfWaitingForDebugger",
        });
        let _ = socket.send(Message::Text(resume.to_string().into())).await;
        let _ = socket.close(None).await;
    })
    .await;
}

struct BrowserArm {
    writer: SplitSink<ConnectedCdpSocket, Message>,
    reader: SplitStream<ConnectedCdpSocket>,
    armed: bool,
}

impl BrowserArm {
    async fn send(&mut self, payload: Value) -> Result<()> {
        writer_send(&mut self.writer, payload).await
    }
}

async fn writer_send(
    writer: &mut SplitSink<ConnectedCdpSocket, Message>,
    payload: Value,
) -> Result<()> {
    writer
        .send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|error| anyhow::anyhow!("browser endpoint send failed: {error}"))
}

fn log_early_attach(event: &str, detail: Value) {
    let _ = codey_runtime_core::diagnostic_log::append_diagnostic_log(event, detail);
}

/// 武装尝试的结果：服务未就绪可重试；协议拒绝自动挂载就该永久放弃——
/// 反复发送注定失败的命令只会污染诊断。Armed 附带握手窗口内被跳过
/// 的挂载事件：页面创建可能落在应答之前，事件不补处理目标会一直暂停。
enum ArmAttempt {
    Armed(BrowserArm, Vec<Value>),
    Retry,
    GiveUp,
}

/// 连上浏览器端点并开启扁平化自动挂载（一次尝试）。参数按 Codex 构建的
/// 实测协议面收窄：waitForDebuggerOnStart 是必填字段；filter 会被接受但
/// 静默排除全部 target，不能用——新目标都在事件里按 waitingForDebugger
/// 逐个放行，语义相同。
async fn arm_browser_endpoint(debug_port: u16) -> ArmAttempt {
    let attempt = async {
        let ws_url = browser_websocket_url(debug_port).await.ok()?;
        connect_cdp_websocket(&ws_url).await.ok()
    };
    let Some(socket) = attempt.await else {
        return ArmAttempt::Retry;
    };
    let (writer, reader) = socket.split();
    let mut arm = BrowserArm {
        writer,
        reader,
        armed: false,
    };
    let attach_id = next_id();
    let request = json!({
        "id": attach_id,
        "method": "Target.setAutoAttach",
        "params": {
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true,
        },
    });
    if arm.send(request).await.is_err() {
        log_early_attach(
            "early_attach.arm_failed",
            json!({"stage": "autoAttachSend"}),
        );
        return ArmAttempt::Retry;
    }
    match wait_for_response(&mut arm.reader, attach_id, ARM_STEP_TIMEOUT).await {
        Some((true, pending)) => {
            arm.armed = true;
            log_early_attach("early_attach.armed", json!({}));
            ArmAttempt::Armed(arm, pending)
        }
        Some((false, _)) => {
            log_early_attach(
                "early_attach.arm_failed",
                json!({"stage": "autoAttachRejected"}),
            );
            ArmAttempt::GiveUp
        }
        None => {
            log_early_attach(
                "early_attach.arm_failed",
                json!({"stage": "autoAttachNoResponse"}),
            );
            ArmAttempt::Retry
        }
    }
}

/// 等一次握手应答：attachedToTarget 事件可能先于应答到达（页面在武装
/// 过程中已创建）——事件不能丢，随应答一并交回主循环处理。返回 None
/// 表示连接/超时失败；(false, ..) 表示协议拒绝了命令。
async fn wait_for_response(
    reader: &mut SplitStream<ConnectedCdpSocket>,
    expected_id: u64,
    timeout: Duration,
) -> Option<(bool, Vec<Value>)> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut pending = Vec::new();
    loop {
        let message = tokio::time::timeout_at(deadline, reader.next())
            .await
            .ok()??;
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if payload.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Some((payload.get("error").is_none(), pending));
        }
        if payload.get("method").and_then(Value::as_str) == Some("Target.attachedToTarget") {
            pending.push(payload);
        }
    }
}

/// 主页面判定与运行期 picker 同源：attachedToTarget 的 targetInfo 里不带
/// webSocketDebuggerUrl，按同一套规则构造等价的 CdpTarget 再判定。
fn is_primary_target_info(target_info: &Value) -> bool {
    let target = CdpTarget {
        id: target_info
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        target_type: target_info
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: target_info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: target_info
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        web_socket_debugger_url: Some("ws://localhost/devtools/page/placeholder".to_string()),
    };
    is_primary_codex_page_target(&target)
}

/// 启动预取：浏览器端点自动挂载与 /json 枚举并行。页面在挂载生效后创建会被
/// 暂停（拦截装全由注入侧放行）；页面先行出现就回退为常规目标。
pub fn spawn_early_attach_prefetch(
    debug_port: u16,
    budget: Duration,
) -> tokio::task::JoinHandle<Option<PrefetchedPage>> {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + budget;
        run_early_attach_prefetch(debug_port, deadline).await
    })
}

async fn run_early_attach_prefetch(
    debug_port: u16,
    deadline: tokio::time::Instant,
) -> Option<PrefetchedPage> {
    // 武装与枚举并行：devtools 服务就绪时间不可控，两者谁先有结果谁赢。
    // 武装重试只保留一段预算——页面创建早于武装完成就没必要再试。
    let arm_deadline = (tokio::time::Instant::now() + ARM_BUDGET).min(deadline);
    let mut arm: Option<BrowserArm> = None;
    let mut arm_disabled = false;
    let mut next_poll = tokio::time::Instant::now();
    let mut next_arm_attempt = tokio::time::Instant::now();
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            biased;
            message = async {
                match arm.as_mut() {
                    Some(arm) => arm.reader.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if payload.get("method").and_then(Value::as_str)
                            == Some("Target.attachedToTarget")
                            && let Some(result) =
                                handle_attached(&mut arm, debug_port, &payload).await
                        {
                            return Some(result);
                        }
                    }
                    Some(Ok(_)) => {}
                    // 浏览器会话断开：武装状态整体作废，直接回退为 /json 枚举。
                    Some(Err(_)) | None => {
                        return poll_targets_until_deadline(debug_port, deadline).await;
                    }
                }
            }
            _ = tokio::time::sleep_until(next_poll) => {
                if let Some(target) = poll_targets_once(debug_port).await {
                    return Some(PrefetchedPage::Ready { target });
                }
                next_poll = tokio::time::Instant::now() + TARGET_POLL_INTERVAL;
            }
            _ = tokio::time::sleep_until(next_arm_attempt), if arm.is_none() && !arm_disabled => {
                if tokio::time::Instant::now() >= arm_deadline {
                    arm_disabled = true;
                    continue;
                }
                match arm_browser_endpoint(debug_port).await {
                    ArmAttempt::Armed(session, pending) => {
                        arm = Some(session);
                        for payload in pending {
                            if let Some(result) =
                                handle_attached(&mut arm, debug_port, &payload).await
                            {
                                return Some(result);
                            }
                        }
                    }
                    ArmAttempt::Retry => {
                        next_arm_attempt =
                            tokio::time::Instant::now() + TARGET_POLL_INTERVAL;
                    }
                    ArmAttempt::GiveUp => arm_disabled = true,
                }
            }
        }
    }
    None
}

/// 处理一次挂载事件。主页面被真正暂停时持有它并解除自动挂载——后续窗口
/// （avatar-overlay、detached-window 等）不再进入暂停；其余被暂停的
/// target 立即放行，不拖累 Codex 自己的启动。
async fn handle_attached(
    arm: &mut Option<BrowserArm>,
    debug_port: u16,
    payload: &Value,
) -> Option<PrefetchedPage> {
    let params = payload.get("params")?;
    let session_id = params.get("sessionId").and_then(Value::as_str)?.to_string();
    let target_info = params.get("targetInfo")?;
    let target_id = target_info
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = target_info
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let waiting = params
        .get("waitingForDebugger")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 暂停发生在首个导航提交之前：此刻 targetInfo.url 必然为空，按 URL 判主
    // 页面永远失败。启动预取窗口里第一个被暂停的 page 即主窗口（其余窗口都是
    // 挂载后由用户动作触发），URL 未提交的暂停 page 直接视为主目标。
    let unnavigated = url.is_empty() || url == "about:blank";
    let primary = is_primary_target_info(target_info)
        || (waiting
            && unnavigated
            && target_info.get("type").and_then(Value::as_str) == Some("page"));
    log_early_attach(
        "early_attach.attached",
        json!({
            "targetId": target_id,
            "type": target_info.get("type").and_then(Value::as_str),
            "title": target_info.get("title").and_then(Value::as_str),
            "url": url.split('?').next().unwrap_or_default(),
            "waiting": waiting,
            "primary": primary,
            "targetInfo": target_info,
        }),
    );
    if primary && waiting {
        let arm = arm.take()?;
        // 先把自动挂载关掉再交还写端：晚到的窗口不再被暂停，resume 之后
        // 这条浏览器会话只为当前持有服务。本构建的 setAutoAttach 要求
        // waitForDebuggerOnStart 必填，关闭时也要带上。
        let mut writer = arm.writer;
        let _ = writer_send(
            &mut writer,
            json!({
                "id": next_id(),
                "method": "Target.setAutoAttach",
                "params": {"autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true},
            }),
        )
        .await;
        let _ = codey_runtime_core::diagnostic_log::append_diagnostic_log(
            "early_attach.paused",
            json!({
                "targetId": target_id,
                "url": url,
            }),
        );
        let target_ws_url = format!("ws://localhost:{debug_port}/devtools/page/{target_id}");
        return Some(PrefetchedPage::Paused {
            target: CdpTarget {
                id: target_id,
                target_type: "page".to_string(),
                title: target_info
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                url,
                web_socket_debugger_url: Some(target_ws_url.clone()),
            },
            hold: EarlyAttachHold {
                writer: Some(writer),
                session_id,
                target_ws_url,
            },
        });
    }
    // 非主目标：暂停的先放行再断开会话；未暂停的（武装前已存在）直接断开，
    // 避免无关会话拖长这条 ws 的事件流。
    if let Some(arm) = arm.as_mut() {
        if waiting {
            let _ = arm
                .send(json!({
                    "sessionId": session_id,
                    "id": next_id(),
                    "method": "Runtime.runIfWaitingForDebugger",
                }))
                .await;
        }
        let _ = arm
            .send(json!({
                "id": next_id(),
                "method": "Target.detachFromTarget",
                "params": {"sessionId": session_id},
            }))
            .await;
    }
    None
}

async fn poll_targets_once(debug_port: u16) -> Option<CdpTarget> {
    match list_targets(debug_port).await {
        Ok(targets) => pick_injectable_codex_page_target(&targets).ok(),
        Err(_) => None,
    }
}

/// 浏览器会话不可用时的纯枚举回退：与 spawn_injectable_target_prefetch
/// 的旧语义一致。
async fn poll_targets_until_deadline(
    debug_port: u16,
    deadline: tokio::time::Instant,
) -> Option<PrefetchedPage> {
    while tokio::time::Instant::now() < deadline {
        if let Some(target) = poll_targets_once(debug_port).await {
            return Some(PrefetchedPage::Ready { target });
        }
        tokio::time::sleep(TARGET_POLL_INTERVAL).await;
    }
    None
}
