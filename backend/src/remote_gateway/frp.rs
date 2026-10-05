//! frp 端口映射：在本机拉起一个 frpc，把远程控制网关的局域网端口映射到公网 frps。
//!
//! 网关本身只监听局域网，跨网络访问需要一台 frps。这里不引入新依赖：可执行文件优先用控制台
//! 指定的路径，其次用 `CODEX_HOME` 下的缓存，都没有时按固定版本从官方 release 下载并用系统
//! 自带的解压能力解开。运行状态写进状态文件，由控制台读取，网关进程不参与界面展示。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{FrpConfig, load_or_create_config};

/// 自动下载的 frp 版本；只在没有指定可执行文件且本地没有缓存时使用。
const FRPC_VERSION: &str = "0.71.0";
const FRPC_DIR_NAME: &str = ".codey-remote-gateway-frpc";
const FRPC_FILE_NAME: &str = "frpc.exe";
const FRPC_CONFIG_FILE_NAME: &str = ".codey-remote-gateway.frpc.toml";
const FRPC_LOG_FILE_NAME: &str = ".codey-remote-gateway.frp.log";
pub(super) const STATUS_FILE_NAME: &str = ".codey-remote-gateway.frp.json";
/// 启动后等一小段时间再判定 frpc 是否活着，尽量把配置错误暴露到控制台而不是静默重试。
const STARTUP_GRACE: Duration = Duration::from_millis(1500);
const TAIL_LINES: usize = 4;

/// frpc 的默认存放位置，也是自动下载的目标。
pub(super) fn frpc_default_path(home: &Path) -> PathBuf {
    home.join(FRPC_DIR_NAME).join(FRPC_FILE_NAME)
}

/// 网关绑定端口后调用一次：按配置决定是否拉起 frpc，并把状态写进状态文件。
pub(super) fn ensure_started(home: &Path, local_port: u16) {
    let config = load_or_create_config(home);
    if !config.frp.enabled {
        write_status(home, "disabled", "未启用端口映射", None, None);
        return;
    }
    if config.frp.server_addr.trim().is_empty() || config.frp.remote_port < 1024 {
        write_status(
            home,
            "invalid",
            "需要在控制台填写服务器地址与远程端口后重启 Codex",
            None,
            None,
        );
        return;
    }
    let home = home.to_path_buf();
    let frp = config.frp;
    let spawned = std::thread::Builder::new()
        .name("codey-frp".to_owned())
        .spawn(move || supervise(home, local_port, frp));
    if let Err(error) = spawned {
        super::log(
            "codey.remote_gateway.frp_thread_failed",
            json!({ "message": format!("{error}") }),
        );
    }
}

fn supervise(home: PathBuf, local_port: u16, frp: FrpConfig) {
    let log_path = home.join(FRPC_LOG_FILE_NAME);
    let endpoint = format!("{}:{}", frp.server_addr.trim(), frp.remote_port);
    write_status(
        &home,
        "starting",
        "正在准备 frpc…",
        Some(&endpoint),
        Some(&log_path),
    );
    let binary = match resolve_frpc(&home, &frp) {
        Ok(path) => path,
        Err(error) => {
            write_status(
                &home,
                "failed",
                &format!("{error:#}"),
                Some(&endpoint),
                Some(&log_path),
            );
            return;
        }
    };
    let config_path = home.join(FRPC_CONFIG_FILE_NAME);
    if let Err(error) = std::fs::write(&config_path, frpc_config(&frp, local_port)) {
        write_status(
            &home,
            "failed",
            &format!("写入 frpc 配置失败：{error}"),
            Some(&endpoint),
            Some(&log_path),
        );
        return;
    }
    let mut child = match spawn_frpc(&binary, &config_path, &log_path) {
        Ok(child) => child,
        Err(error) => {
            write_status(
                &home,
                "failed",
                &format!("启动 frpc 失败：{error:#}"),
                Some(&endpoint),
                Some(&log_path),
            );
            return;
        }
    };
    std::thread::sleep(STARTUP_GRACE);
    match child.try_wait() {
        Ok(Some(status)) => {
            write_status(
                &home,
                "failed",
                &exit_message(status, &log_path),
                Some(&endpoint),
                Some(&log_path),
            );
            return;
        }
        Ok(None) => write_status(
            &home,
            "running",
            "映射已启动",
            Some(&endpoint),
            Some(&log_path),
        ),
        Err(error) => write_status(
            &home,
            "unknown",
            &format!("无法确认 frpc 状态：{error}"),
            Some(&endpoint),
            Some(&log_path),
        ),
    }
    match child.wait() {
        Ok(status) => write_status(
            &home,
            "exited",
            &exit_message(status, &log_path),
            Some(&endpoint),
            Some(&log_path),
        ),
        Err(error) => write_status(
            &home,
            "unknown",
            &format!("等待 frpc 退出失败：{error}"),
            Some(&endpoint),
            Some(&log_path),
        ),
    }
}

fn resolve_frpc(home: &Path, frp: &FrpConfig) -> Result<PathBuf> {
    let custom = frp.binary.trim();
    if !custom.is_empty() {
        let path = PathBuf::from(custom);
        if path.is_file() {
            return Ok(path);
        }
        bail!("指定的 frpc 不存在：{}", path.display());
    }
    let cached = frpc_default_path(home);
    if cached.is_file() {
        return Ok(cached);
    }
    download_frpc(home).with_context(|| {
        format!(
            "自动下载 frpc 失败，也可以在控制台填写本地 frpc 路径（默认位置 {}）",
            cached.display()
        )
    })
}

/// 从官方 release 下载当前架构的 frpc，解压后放到默认位置。
fn download_frpc(home: &Path) -> Result<PathBuf> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!("暂不支持在 {other} 上自动下载 frpc，请手动指定 frpc 路径"),
    };
    let dir = home.join(FRPC_DIR_NAME);
    std::fs::create_dir_all(&dir).context("创建 frpc 目录失败")?;
    let url = format!(
        "https://github.com/fatedier/frp/releases/download/v{FRPC_VERSION}/frp_{FRPC_VERSION}_windows_{arch}.zip"
    );
    let archive = dir.join("frp.zip");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建下载运行时失败")?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .build()
            .context("创建下载客户端失败")?;
        let response = client
            .get(&url)
            .send()
            .await
            .context("请求 frpc 下载地址失败")?;
        if !response.status().is_success() {
            bail!("下载 frpc 失败：HTTP {}", response.status());
        }
        let bytes = response.bytes().await.context("读取 frpc 安装包失败")?;
        if bytes.len() < 1024 * 1024 {
            bail!("frpc 安装包异常，仅 {} 字节", bytes.len());
        }
        std::fs::write(&archive, &bytes).context("写入 frpc 安装包失败")
    })?;
    extract_archive(&archive, &dir)?;
    let _ = std::fs::remove_file(&archive);
    let found = find_frpc(&dir).context("解压后没有找到 frpc.exe")?;
    let target = frpc_default_path(home);
    if found != target {
        std::fs::rename(&found, &target)
            .or_else(|_| std::fs::copy(&found, &target).map(|_| ()))
            .context("放置 frpc 失败")?;
    }
    Ok(target)
}

/// 用系统自带能力解压，避免为一次安装引入 zip 依赖。
fn extract_archive(archive: &Path, dir: &Path) -> Result<()> {
    let script = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        ps_quote(&archive.display().to_string()),
        ps_quote(&dir.display().to_string())
    );
    let mut powershell = std::process::Command::new("powershell");
    powershell.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ]);
    hide_window(&mut powershell);
    if run_quiet(powershell).is_ok() {
        return Ok(());
    }
    let mut tar = std::process::Command::new("tar");
    tar.arg("-xf").arg(archive).arg("-C").arg(dir);
    hide_window(&mut tar);
    run_quiet(tar).context("解压 frpc 安装包失败，请手动解压并把 frpc.exe 放到配置目录")
}

fn run_quiet(mut command: std::process::Command) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("执行解压命令失败")?;
    anyhow::ensure!(status.success(), "解压命令退出码 {:?}", status.code());
    Ok(())
}

fn ps_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn find_frpc(dir: &Path) -> Option<PathBuf> {
    let mut queue = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = queue.pop() {
        if depth > 3 {
            continue;
        }
        let entries = std::fs::read_dir(&current).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push((path, depth + 1));
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(FRPC_FILE_NAME))
            {
                return Some(path);
            }
        }
    }
    None
}

fn spawn_frpc(binary: &Path, config_path: &Path, log_path: &Path) -> Result<std::process::Child> {
    let log = std::fs::File::create(log_path).context("创建 frpc 日志失败")?;
    let mut command = std::process::Command::new(binary);
    command.arg("-c").arg(config_path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    hide_window(&mut command);
    let child = command.spawn().context("启动 frpc 进程失败")?;
    // Codey 退出后不能留下仍在占用映射端口的 frpc：放进随本进程关闭而终止的隔离组。
    #[cfg(windows)]
    attach_kill_on_close_job(&child)?;
    Ok(child)
}

/// 把 frpc 加入 `KILL_ON_JOB_CLOSE` 隔离组；组句柄随本进程存续，进程退出时系统关闭句柄并
/// 终止 frpc，避免残留进程占着远程端口导致下次启动注册失败。
#[cfg(windows)]
fn attach_kill_on_close_job(child: &std::process::Child) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::{
        Foundation::CloseHandle,
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
    };
    unsafe {
        let job =
            CreateJobObjectW(None, None).map_err(|_| anyhow::anyhow!("无法创建 frpc 隔离组"))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let assigned = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of_val(&limits) as u32,
        )
        .and_then(|_| AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())));
        if assigned.is_err() {
            let _ = CloseHandle(job);
            bail!("无法把 frpc 加入隔离组，已取消启动");
        }
        // 句柄故意不关闭：随本进程退出由系统回收，frpc 随之终止。
        Ok(())
    }
}

#[cfg(windows)]
fn hide_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(codey_runtime_core::windows_create_no_window());
}

#[cfg(not(windows))]
fn hide_window(_command: &mut std::process::Command) {}

/// frp 0.52 之后使用 TOML 配置；映射目标是网关自己的局域网端口。
fn frpc_config(frp: &FrpConfig, local_port: u16) -> String {
    let mut config = format!(
        "serverAddr = \"{}\"\nserverPort = {}\n",
        escape_toml(frp.server_addr.trim()),
        frp.server_port
    );
    let token = frp.token.trim();
    if !token.is_empty() {
        config.push_str("auth.method = \"token\"\n");
        config.push_str(&format!("auth.token = \"{}\"\n", escape_toml(token)));
    }
    config.push_str(&format!(
        "\n[[proxies]]\nname = \"codey-remote\"\ntype = \"tcp\"\nlocalIP = \"127.0.0.1\"\nlocalPort = {local_port}\nremotePort = {}\n",
        frp.remote_port
    ));
    config
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn exit_message(status: std::process::ExitStatus, log_path: &Path) -> String {
    let code = status
        .code()
        .map_or_else(|| "未知".to_owned(), |code| code.to_string());
    let tail = tail_of(log_path);
    if tail.is_empty() {
        format!("frpc 已退出（退出码 {code}）")
    } else {
        format!("frpc 已退出（退出码 {code}）：{tail}")
    }
}

fn tail_of(log_path: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let lines = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join(" / ").chars().take(400).collect()
}

fn write_status(
    home: &Path,
    state: &str,
    message: &str,
    endpoint: Option<&str>,
    log_path: Option<&Path>,
) {
    let payload = json!({
        "state": state,
        "message": message,
        "endpoint": endpoint,
        "logPath": log_path.map(|path| path.display().to_string()),
        "updatedAt": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default(),
    });
    let _ = std::fs::write(home.join(STATUS_FILE_NAME), payload.to_string());
    super::log("codey.remote_gateway.frp_status", payload);
}

/// 状态文件里的字段会直接给控制台用，这里只暴露读取入口，避免各处重复解析。
pub(super) fn read_status(home: &Path) -> Value {
    std::fs::read_to_string(home.join(STATUS_FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null)
}
