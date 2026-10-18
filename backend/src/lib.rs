mod account_usage;
mod appserver_call;
mod cdp;
mod codex_config;
mod codex_config_guidance;
mod codex_extensions;
mod codex_provider;
mod codex_startup_patch;
mod codey_plugins;
mod commands;
mod computer_use;
mod config;
mod conversation_stats;
mod crashpad_pending_guard;
#[cfg(windows)]
mod desktop_instance;
mod early_attach;
mod error_log;
pub mod fastctx;
mod fastctx_route_gate;
mod fs_util;
mod hook_io;
mod http_response;
mod launch_proxy;
mod launch_proxy_relay;
mod launcher;
mod local_router;
mod maintenance_lock;
mod message_delete;
mod model_catalog;
mod model_id;
mod model_list;
mod native_update_ui;
mod notifications;
mod official_accounts;
mod pending_approval;
mod pet_slim_patch;
mod plugin_log_terminal;
mod plugin_marketplace;
mod plugin_routes;
mod process_cleanup;
mod process_tree;
mod prompt_optimization;
mod provider_models;

#[cfg(windows)]
mod remote_gateway;

mod renderer_asset_patch;
mod route_request_log;
mod session_index_cleanup;
mod session_metadata;
mod session_model;
mod session_transfer;
mod sqlite_util;
mod subagent;
mod subagent_gate;
mod subagent_orchestrator;
mod subagent_policy;
mod task_activity;
mod trace_log_guard;
mod trace_log_stats;
use std::sync::Arc;

use anyhow::{Context, Result};

use commands::{AppShutdownReason, AppState};
use native_update_ui::NativeUpdateUi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownReason {
    CodexExited,
    Signal,
}

pub fn run_error_log_helper_if_requested() -> Result<bool> {
    error_log::run_helper_if_requested()
}

pub fn run_plugin_log_terminal_if_requested() -> Result<bool> {
    plugin_log_terminal::run_if_requested()
}

pub fn run_codex_cli_wrapper_if_requested() -> Result<bool> {
    codex_startup_patch::run_cli_wrapper_if_requested()
}

pub fn install_crash_log_hook(component: &'static str, stage: &'static str) {
    error_log::install_panic_hook(component, stage);
}

pub fn record_process_failure(
    event: impl Into<String>,
    operation: impl Into<String>,
    error: impl Into<String>,
    stage: impl Into<String>,
) {
    error_log::record_process_failure(event, operation, error, stage);
}

pub fn record_process_failure_with_recoverability(
    event: impl Into<String>,
    operation: impl Into<String>,
    error: impl Into<String>,
    stage: impl Into<String>,
    recoverable: bool,
) {
    error_log::record_process_failure_with_recoverability(
        event,
        operation,
        error,
        stage,
        recoverable,
    );
}

pub fn run_subagent_gate_hook_if_requested() -> Result<bool> {
    subagent_gate::run_hook_if_requested()
}

pub fn run_fastctx_route_hook_if_requested() -> Result<bool> {
    fastctx_route_gate::run_hook_if_requested()
}

pub fn run_desktop_application() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        native_update_ui::run_macos_application(|ui| build_async_runtime()?.block_on(run(ui)))
    }

    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(windows)]
        let Some(_desktop_instance) = desktop_instance::claim() else {
            return Ok(());
        };
        let ui = NativeUpdateUi::start();
        let result = build_async_runtime()?.block_on(run(ui.clone()));
        ui.shutdown();
        result
    }
}

fn build_async_runtime() -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    // Codey is an I/O coordinator. Blocking filesystem/SQLite work already
    // runs on Tokio's blocking pool, so two async workers avoid creating a
    // CPU-count-sized thread team for every helper instance.
    builder.worker_threads(2);
    builder.enable_all().build().map_err(anyhow::Error::from)
}

struct PluginShutdownGuard;

impl Drop for PluginShutdownGuard {
    fn drop(&mut self) {
        codey_plugins::shutdown();
    }
}

async fn run(ui: NativeUpdateUi) -> Result<()> {
    // Config load, ledger read and HTTP client construction (which loads the
    // system root store) are synchronous; keep them off the async workers.
    let state = tokio::task::spawn_blocking(|| {
        error_log::initialize();
        let state = Arc::new(AppState::default());
        let configured_codex_app_path = state.config.blocking_read().codex_app_path.clone();
        error_log::refresh_codex_app_version(None, Some(&configured_codex_app_path));
        commands::install_plugin_route_handler(Arc::clone(&state));
        let plugin_root = codey_runtime_core::paths::default_app_state_dir().join("codey-plugins");
        if let Err(error) = codey_plugins::initialize(plugin_root) {
            error_log::record_failure(
                "plugin_initialization_failed",
                "initialize_codey_plugins",
                error,
                serde_json::json!({}),
            );
        }
        state
    })
    .await
    .context("初始化 Codey 状态的任务异常退出")?;
    let _plugin_shutdown = PluginShutdownGuard;
    // 临时状态的恢复和路由兼容桩都在每次启动里做。进程入口再做一次的话，
    // 第一次失败只记日志，第二次同样的错误才会中止启动。
    // Resolve the default official account while the launch path prepares its
    // remaining state.
    state.prewarm_official_account_probe().await;
    // Register signal listeners during launch so a shutdown signal can
    // interrupt the wait for Codex to exit.
    let shutdown_task = tokio::spawn(shutdown_signal());
    let shutdown = async {
        let _ = shutdown_task.await;
    };
    tokio::pin!(shutdown);
    let show_initial_startup_failure = |error: &str| {
        let ui = ui.clone();
        let error = error.to_owned();
        async move { show_initial_startup_failure_with_ui(&ui, &error).await }
    };
    let shutdown_reason = loop {
        match commands::launch_codey_runtime(&state).await {
            Ok(_) => {
                break wait_for_runtime_shutdown(state.wait_for_shutdown(), &mut shutdown).await;
            }
            Err(error) => {
                eprintln!("Codey 自动启动 Codex 失败：{error:#}");
                let context_recovery = error.is::<model_catalog::ContextBudgetCatalogError>();
                let mut error = format!("{error:#}");
                let cleanup = if context_recovery {
                    commands::cleanup_failed_runtime_start(&state)
                        .await
                        .map(|_| ())
                } else {
                    stop_runtime_with_retry(&state).await
                };
                if let Err(cleanup_error) = &cleanup {
                    error_log::record_failure(
                        "restore_failed",
                        "restore_runtime_after_startup_failure",
                        cleanup_error.clone(),
                        serde_json::json!({}),
                    );
                }
                if cleanup.is_ok() && context_recovery {
                    match commands::recover_default_context_budgets_for_launch(&state, &error).await
                    {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(recovery_error) => {
                            error = format!("{error}；恢复默认上下文预算失败：{recovery_error}");
                        }
                    }
                }
                let failure = initial_startup_failure_error(
                    &error,
                    cleanup.as_ref().err().map(String::as_str),
                );
                show_initial_startup_failure(&failure).await;
                return Err(anyhow::Error::msg(failure));
            }
        }
    };

    let shutdown_started_at = std::time::SystemTime::now();
    let cleanup = stop_runtime_with_retry(&state).await;
    if let Err(error) = &cleanup {
        error_log::record_failure(
            "restore_failed",
            "restore_runtime_during_shutdown",
            error.clone(),
            serde_json::json!({}),
        );
    }
    let shutdown_context = match shutdown_reason {
        ShutdownReason::CodexExited => "Codex 已退出",
        ShutdownReason::Signal => "Codey 收到退出信号",
    };
    // The public no-argument helper remains available for callers without a shutdown boundary:
    // terminate_other_codey_processes().await
    match process_cleanup::terminate_other_codey_processes_since(shutdown_started_at).await {
        Ok(0) => {}
        Ok(count) => eprintln!("{shutdown_context}，已终止 {count} 个遗留 Codey 进程"),
        Err(error) => {
            error_log::record_failure(
                "cleanup_failed",
                "terminate_other_codey_processes",
                format!("{error:#}"),
                serde_json::json!({
                    "shutdownContext": shutdown_context,
                }),
            );
            eprintln!("{shutdown_context}，但清理遗留 Codey 进程失败：{error:#}");
        }
    }
    cleanup.map_err(anyhow::Error::msg)
}

async fn wait_for_runtime_shutdown(
    app_shutdown: impl std::future::Future<Output = AppShutdownReason>,
    signal: impl std::future::Future<Output = ()>,
) -> ShutdownReason {
    tokio::select! {
        biased;
        reason = app_shutdown => match reason {
            AppShutdownReason::CodexExited => ShutdownReason::CodexExited,
        },
        _ = signal => ShutdownReason::Signal,
    }
}

async fn stop_runtime_with_retry(state: &Arc<AppState>) -> Result<(), String> {
    match commands::stop_codey_runtime(state).await {
        Ok(_) => Ok(()),
        Err(first_error) => {
            eprintln!("Codey 停止 Codex 或恢复配置失败，正在重试：{first_error}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let retry_error = match commands::stop_codey_runtime(state).await {
                Ok(_) => return Ok(()),
                Err(error) => error,
            };
            let error = format!("{first_error}；重试失败：{retry_error}");
            match commands::reap_runtime_child_before_exit(state).await {
                Ok(()) => Err(error),
                Err(reap_error) => Err(format!("{error}；最终回收直属子进程失败：{reap_error}")),
            }
        }
    }
}

fn initial_startup_failure_error(startup_error: &str, cleanup_error: Option<&str>) -> String {
    match cleanup_error {
        Some(cleanup_error) => {
            format!("{startup_error}；启动失败后的清理也失败：{cleanup_error}")
        }
        None => startup_error.to_string(),
    }
}

// NativeUpdateUi renders the fatal dialog with rfd::MessageDialog::new(),
// MessageLevel::Error, MessageButtons::Ok and .show() inside
// tokio::task::spawn_blocking, using .set_title("Codey 启动失败") with the
// message "Codey 将退出。处理上述问题后，请重新启动 Codey。".
async fn show_initial_startup_failure_with_ui(ui: &NativeUpdateUi, error: &str) {
    if let Err(dialog_error) = ui.show_startup_failure(error).await {
        error_log::record_failure(
            "dialog_failed",
            "show_initial_startup_failure",
            dialog_error.to_string(),
            serde_json::json!({}),
        );
        eprintln!("Codey 启动失败提示框显示异常：{dialog_error}");
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()).context("监听 SIGTERM 失败") {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = ctrl_c_signal() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                eprintln!("{error:#}");
                ctrl_c_signal().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c_signal().await;
    }
}

/// A listener that cannot be registered never fires, so Codex exit remains
/// the shutdown trigger instead of Codey quitting right after launch.
async fn ctrl_c_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("监听 Ctrl+C 失败：{error}");
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    #[tokio::test]
    async fn codex_exit_is_reported_as_the_shutdown_reason() {
        assert_eq!(
            wait_for_runtime_shutdown(async { AppShutdownReason::CodexExited }, pending()).await,
            ShutdownReason::CodexExited
        );
    }

    #[tokio::test]
    async fn shutdown_signal_cancels_the_runtime_wait() {
        assert_eq!(
            wait_for_runtime_shutdown(pending(), async {}).await,
            ShutdownReason::Signal
        );
    }

    #[test]
    fn startup_failure_keeps_the_cleanup_error() {
        assert_eq!(
            initial_startup_failure_error("Codex 启动失败", Some("配置恢复失败")),
            "Codex 启动失败；启动失败后的清理也失败：配置恢复失败"
        );
    }

    #[test]
    fn startup_failure_is_unchanged_after_successful_cleanup() {
        assert_eq!(
            initial_startup_failure_error("Codex 启动失败", None),
            "Codex 启动失败"
        );
    }
}
