import assert from "node:assert/strict";
import test from "node:test";

import { readSource } from "./helpers/read-source.mjs";


// Static contracts keep Windows-only wiring visible on non-Windows CI jobs. Runtime
// behavior remains covered by Rust tests and the dedicated Windows build job.
test("Windows source contract: Codey uses the GUI subsystem", async () => {
  const [main, library, manifest] = await Promise.all([
    readSource("backend/src/main.rs"),
    readSource("backend/src/lib.rs"),
    readSource("backend/Cargo.toml"),
  ]);

  assert.match(
    main,
    /^#!\[cfg_attr\(target_os = "windows", windows_subsystem = "windows"\)\]/,
  );
  // 插件日志子进程可按需创建控制台，桌面入口仍应直接使用 GUI 子系统。
  assert.doesNotMatch(main, /AllocConsole|AttachConsole|ShowWindow|GetConsoleWindow/);
  assert.doesNotMatch(library, /AllocConsole|AttachConsole|ShowWindow|GetConsoleWindow/);
  assert.match(manifest, /Win32_UI_WindowsAndMessaging/);
});

test("Windows source contract: fatal startup failures remain visible", async () => {
  const library = await readSource("backend/src/lib.rs");
  const failureStart = library.indexOf("let shutdown_reason =");
  const shutdownCleanup = library.indexOf(
    "let cleanup = stop_runtime_with_retry(&state).await;",
    failureStart,
  );

  assert.notEqual(failureStart, -1);
  assert.ok(shutdownCleanup > failureStart);

  const failureBranch = library.slice(failureStart, shutdownCleanup);
  assert.match(failureBranch, /commands::launch_codey_runtime\(&state\)\.await/);
  assert.match(failureBranch, /stop_runtime_with_retry\(&state\)\.await/);
  assert.match(failureBranch, /initial_startup_failure_error\(/);
  assert.match(
    failureBranch,
    /show_initial_startup_failure\(&failure\)\.await;/,
  );
  assert.match(failureBranch, /return Err\(anyhow::Error::msg\(failure\)\)/);

  const cleanupHelper = library.slice(
    library.indexOf("async fn stop_runtime_with_retry"),
    library.indexOf("fn initial_startup_failure_error"),
  );
  assert.match(cleanupHelper, /stop_codey_runtime\(state\)\.await/);
  assert.match(cleanupHelper, /tokio::time::sleep/);
  assert.equal(cleanupHelper.match(/stop_codey_runtime\(state\)/g)?.length, 2);

  assert.match(
    library,
    /rfd::MessageDialog::new\(\)[\s\S]*?MessageLevel::Error[\s\S]*?MessageButtons::Ok[\s\S]*?\.show\(\)/,
  );
  assert.match(library, /tokio::task::spawn_blocking/);
  assert.match(library, /\.set_title\("Codey 启动失败"\)/);
  assert.match(library, /Codey 将退出。处理上述问题后，请重新启动 Codey。/);
});

test("Windows source contract: background helpers request no-window execution", async () => {
  const [launcherPlatform, processCleanup, runtimeAppPaths] = await Promise.all([
    readSource("backend/src/launcher/platform.rs"),
    readSource("backend/src/process_cleanup.rs"),
    readSource("vendor/CodeyRuntime/crates/codey-runtime-core/src/app_paths.rs"),
  ]);

  assert.match(
    launcherPlatform,
    /Command::new\(executable\)[\s\S]*creation_flags\(codey_runtime_core::windows_create_no_window\(\)\)[\s\S]*\.spawn\(\)/,
  );
  assert.doesNotMatch(processCleanup, /Command::new\("taskkill"\)/);
  assert.match(
    processCleanup,
    /codey_runtime_core::windows_terminate_process_if_matches/,
  );
  assert.match(
    runtimeAppPaths,
    /fn registered_codex_packages\(\)[\s\S]*?registered_windows_package_values\(/,
  );
  const nativePackages = runtimeAppPaths.match(
    /#\[cfg\(windows\)\]\s*fn registered_windows_package_values<T>\([\s\S]*?\n\}/,
  )?.[0];
  assert.ok(nativePackages, "Windows 包发现应使用原生枚举");
  assert.match(nativePackages, /PackageManager::new\(\)/);
  assert.match(nativePackages, /FindPackagesByUserSecurityId\(&HSTRING::new\(\)\)/);
  assert.match(nativePackages, /packages\.First\(\)\.context\([^\n]+\)\?/);
  assert.match(nativePackages, /\.MoveNext\(\)\s*\.context\([^\n]+\)\?/);
  assert.doesNotMatch(nativePackages, /Command::new|CreateProcess|ShellExecute/);
  assert.doesNotMatch(
    runtimeAppPaths,
    /Command::new\(\s*"(?:powershell|pwsh|cmd)(?:\.exe)?"/i,
  );
});

test("Windows source contract: packaged Codex exit uses an OS process wait", async () => {
  const [launcherProcess, coreLauncher] = await Promise.all([
    readSource("backend/src/launcher/process.rs"),
    readSource("vendor/CodeyRuntime/crates/codey-runtime-core/src/launcher.rs"),
  ]);
  const watcher = launcherProcess.slice(
    launcherProcess.indexOf("#[cfg(windows)]\npub(super) fn spawn_codex_exit_watcher"),
    launcherProcess.indexOf("struct SpawnedCodex"),
  );

  assert.match(
    watcher,
    /codey_runtime_core::launcher::wait_for_windows_process_id\(process_id\)/,
  );
  assert.match(
    coreLauncher,
    /pub async fn wait_for_windows_process_id\(process_id: u32\)/,
  );
  assert.match(coreLauncher, /WaitForSingleObject\(HANDLE\(handle\.as_raw_handle\(\)\), 0\)/);
  assert.match(coreLauncher, /OwnedHandle/);
  assert.doesNotMatch(coreLauncher, /WaitForSingleObject\([^\n]*INFINITE\)/);
});

test("Windows source contract: helper entry points run before the desktop app", async () => {
  const main = await readSource("backend/src/main.rs");

  // 桌面入口先按参数分派到各自的后台助手，未命中任何助手时才启动桌面应用。
  assert.match(
    main,
    /codey_lib::run_plugin_log_terminal_if_requested\(\)[\s\S]*codey_lib::run_desktop_application\(\)/,
  );
  assert.match(
    main,
    /codey_lib::run_overlay_recovery_if_requested\(\)\?[\s\S]*codey_lib::run_desktop_application\(\)/,
  );
  assert.doesNotMatch(main, /run_update_helper_if_requested|update_helper/);
});

test("Windows source contract: missing Codex paths recover before startup", async () => {
  const [commands, runtime] = await Promise.all([
    readSource("backend/src/commands.rs"),
    readSource("backend/src/commands/runtime.rs"),
  ]);
  const launch = runtime.slice(
    runtime.indexOf("async fn launch_codey_inner_locked"),
    runtime.indexOf("pub async fn launch_codey_runtime"),
  );

  const recoverBeforeLaunch = launch.match(
    /ensure_windows_codex_app_path\(state\)\s*\.await\s*\.map_err\(anyhow::Error::msg\)\?/,
  );
  assert.ok(recoverBeforeLaunch, "启动前必须恢复 Codex 路径并传播失败");
  assert.ok(
    recoverBeforeLaunch.index < launch.indexOf("CodeyRuntime::start"),
  );
  assert.match(
    commands,
    /FileDialog::new\(\)[\s\S]*选择 Codex 桌面应用安装目录[\s\S]*pick_folder\(\)/,
  );
  const recoverPath = commands.match(
    /async fn ensure_windows_codex_app_path\([\s\S]*?\n\}/,
  )?.[0];
  assert.ok(recoverPath, "应存在 Windows Codex 路径恢复函数");
  assert.match(recoverPath, /config\.codex_app_path = app_dir\.to_string_lossy\(\)\.to_string\(\)/);
  assert.match(
    recoverPath,
    /let config = save_config_to_store\(state, config\)\s*\.await\s*\.map_err\([^\n]+\)\?;\s*\*state\.config\.write\(\)\.await = config;/,
  );
});
