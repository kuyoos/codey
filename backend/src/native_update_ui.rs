use std::sync::Arc;

#[derive(Clone)]
pub struct NativeUpdateUi {
    platform: Result<Arc<platform::PlatformUi>, Arc<str>>,
}

impl NativeUpdateUi {
    pub fn start() -> Self {
        match platform::PlatformUi::start() {
            Ok(platform) => Self {
                platform: Ok(Arc::new(platform)),
            },
            Err(error) => {
                eprintln!("{error}");
                Self {
                    platform: Err(Arc::from(error)),
                }
            }
        }
    }

    pub async fn show_startup_failure(&self, error: &str) -> Result<(), String> {
        show_dialog(
            "Codey 启动失败".to_string(),
            format!("{error}\n\nCodey 将退出。处理上述问题后，请重新启动 Codey。"),
            DialogKind::Failure,
            "退出".to_string(),
            None,
        )
        .await
        .map(|_| ())
    }

    pub fn shutdown(&self) {
        if let Ok(platform) = &self.platform {
            platform.shutdown();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogKind {
    Failure,
    RestoreContext,
}

/// 上下文恢复提示出现的时机：启动或重启 Codex 失败，或保存模型时无法应用自定义预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContextRecoveryPurpose {
    /// 启动或重启 Codex 时无法生成带自定义预算的模型目录。
    Launch,
    /// 保存模型时无法生成带自定义预算的模型目录。
    ModelSync,
}

impl ContextRecoveryPurpose {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::ModelSync => "model_sync",
        }
    }
}

pub(crate) async fn confirm_context_recovery(
    purpose: ContextRecoveryPurpose,
) -> Result<bool, String> {
    confirm_context_recovery_with_reason(
        purpose,
        crate::model_catalog::CUSTOM_CONTEXT_CATALOG_UNAVAILABLE,
    )
    .await
}

fn context_recovery_description(purpose: ContextRecoveryPurpose, reason: &str) -> String {
    let action = match purpose {
        ContextRecoveryPurpose::Launch => "重新启动",
        ContextRecoveryPurpose::ModelSync => "继续保存本次改动",
    };
    format!(
        "{reason}\n\n可以恢复所有模型的默认上下文预算并{action}，其他设置不受影响。原配置会自动备份，自定义模型目录保持不变。若要保留预算，请先检查模型目录与线路中的模型标识，或重新同步模型后重试。"
    )
}

#[cfg(test)]
#[test]
fn context_recovery_prompt_preserves_the_catalog_error_and_reset_scope() {
    let reason = "自定义模型目录缺少已启用的预算模型：custom/gpt-6-astra（custom.json）";
    for purpose in [
        ContextRecoveryPurpose::Launch,
        ContextRecoveryPurpose::ModelSync,
    ] {
        let description = context_recovery_description(purpose, reason);
        assert!(description.starts_with(reason));
        assert!(description.contains("所有模型"));
        assert!(description.contains("自动备份"));
        assert!(description.contains("自定义模型目录保持不变"));
        let action = match purpose {
            ContextRecoveryPurpose::Launch => "重新启动",
            ContextRecoveryPurpose::ModelSync => "继续保存本次改动",
        };
        assert!(description.contains(action));
    }
}

pub(crate) async fn confirm_context_recovery_with_reason(
    purpose: ContextRecoveryPurpose,
    reason: &str,
) -> Result<bool, String> {
    let (primary_label, secondary_label) = match purpose {
        ContextRecoveryPurpose::Launch => ("恢复默认预算并重试", "退出"),
        ContextRecoveryPurpose::ModelSync => ("恢复默认预算并继续", "取消"),
    };
    show_dialog(
        "Codey 上下文设置暂时无法使用".to_string(),
        context_recovery_description(purpose, reason),
        DialogKind::RestoreContext,
        primary_label.to_string(),
        Some(secondary_label.to_string()),
    )
    .await
    .map(|result| result == DialogResult::Primary)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogResult {
    Primary,
    Secondary,
}

#[cfg(any(windows, target_os = "macos"))]
async fn show_dialog(
    title: String,
    description: String,
    kind: DialogKind,
    primary_label: String,
    secondary_label: Option<String>,
) -> Result<DialogResult, String> {
    tokio::task::spawn_blocking(move || {
        let primary_label_for_result = primary_label.clone();
        let buttons = match secondary_label {
            Some(secondary_label) => {
                rfd::MessageButtons::OkCancelCustom(primary_label, secondary_label)
            }
            None => rfd::MessageButtons::OkCustom(primary_label),
        };
        let result = rfd::MessageDialog::new()
            .set_title(title)
            .set_description(description)
            .set_level(match kind {
                DialogKind::Failure => rfd::MessageLevel::Error,
                DialogKind::RestoreContext => rfd::MessageLevel::Warning,
            })
            .set_buttons(buttons)
            .show();
        match (kind, result) {
            (DialogKind::Failure, _) => DialogResult::Primary,
            (_, rfd::MessageDialogResult::Custom(label)) if label == primary_label_for_result => {
                DialogResult::Primary
            }
            (_, _) => DialogResult::Secondary,
        }
    })
    .await
    .map_err(|error| format!("原生对话框任务异常退出：{error}"))
}

#[cfg(not(any(windows, target_os = "macos")))]
async fn show_dialog(
    _title: String,
    _description: String,
    kind: DialogKind,
    _primary_label: String,
    _secondary_label: Option<String>,
) -> Result<DialogResult, String> {
    Ok(match kind {
        DialogKind::RestoreContext => DialogResult::Secondary,
        DialogKind::Failure => DialogResult::Primary,
    })
}

#[cfg(target_os = "macos")]
pub fn run_macos_application<F>(run: F) -> anyhow::Result<()>
where
    F: FnOnce(NativeUpdateUi) -> anyhow::Result<()> + Send + 'static,
{
    platform::run_application(run)
}

#[cfg(windows)]
mod platform {
    pub struct PlatformUi;

    impl PlatformUi {
        pub fn start() -> Result<Self, String> {
            Ok(Self)
        }

        pub fn shutdown(&self) {}
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::cell::RefCell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use dispatch2::MainThreadBound;
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    use super::NativeUpdateUi;

    struct MacUiState {
        app: Retained<NSApplication>,
    }

    pub struct PlatformUi {
        state: Arc<MainThreadBound<RefCell<MacUiState>>>,
    }

    impl PlatformUi {
        pub fn start() -> Result<Self, String> {
            let mtm = MainThreadMarker::new()
                .ok_or_else(|| "macOS 应用界面必须在主线程初始化".to_string())?;
            let app = NSApplication::sharedApplication(mtm);
            Ok(Self {
                state: Arc::new(MainThreadBound::new(RefCell::new(MacUiState { app }), mtm)),
            })
        }

        pub fn shutdown(&self) {
            self.state.get_on_main(|state| {
                state.borrow().app.stop(None);
            });
        }
    }

    pub fn run_application<F>(run: F) -> anyhow::Result<()>
    where
        F: FnOnce(NativeUpdateUi) -> anyhow::Result<()> + Send + 'static,
    {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| anyhow::anyhow!("macOS 应用循环必须在主线程运行"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let ui = NativeUpdateUi::start();
        let worker_ui = ui.clone();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("codey-runtime".to_string())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| run(worker_ui.clone())))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Codey 运行线程异常退出")));
                worker_ui.shutdown();
                let _ = result_tx.send(result);
            })?;

        app.run();
        let result = result_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Codey 运行线程未返回结果"))?;
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("Codey 运行线程回收失败"))?;
        drop(ui);
        result
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub struct PlatformUi;

    impl PlatformUi {
        pub fn start() -> Result<Self, String> {
            Ok(Self)
        }

        pub fn shutdown(&self) {}
    }
}
