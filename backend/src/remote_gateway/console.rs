//! 控制台读写远程控制配置。
//!
//! 网关只在桌面 app-server 启动时读取配置与密钥，所以保存和重新生成密钥都要重启 Codex
//! 才生效；这里如实回传生效状态，由控制台提示，而不是假装已经生效。

use std::path::Path;

use serde_json::{Value, json};

use super::{
    CONFIG_FILE_NAME, FrpConfig, GatewayConfig, GatewayMode, TOKEN_FILE_NAME, URL_FILE_NAME, frp,
    load_or_create_config, load_or_create_token, new_token,
};

/// 端口下限，避免占用系统保留端口。
const MIN_PORT: u16 = 1024;

pub(crate) async fn invoke(command: &str, args: &Value) -> Result<Value, String> {
    match command {
        "remote_gateway_status" => Ok(status()),
        "save_remote_gateway_config" => save(args),
        "regenerate_remote_gateway_token" => regenerate_token(),
        _ => Err(format!("未知的远程控制命令：{command}")),
    }
}

fn home() -> std::path::PathBuf {
    crate::codex_config::codex_home().to_path_buf()
}

/// 回传控制台需要的状态：是否启用、端口、密钥、最近一次发布的访问地址与生效情况。
fn status() -> Value {
    let home = home();
    let config = load_or_create_config(&home);
    let token = load_or_create_token(&home).unwrap_or_default();
    let url = std::fs::read_to_string(home.join(URL_FILE_NAME))
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|url| !url.is_empty());
    // 地址文件由正在运行的网关写出：端口与密钥都对得上才算已经生效。
    let active = config.enabled
        && url
            .as_deref()
            .is_some_and(|url| url.contains(&format!(":{}", config.port)) && url.contains(&token));
    // frp 运行状态由网关线程写状态文件，这里只做合并展示。
    let frp_state = frp::read_status(&home);
    json!({
        "status": "ok",
        "enabled": config.enabled,
        "port": config.port,
        "mode": mode_name(config.mode),
        "token": token,
        "url": url,
        "active": active,
        "frp": {
            "enabled": config.frp.enabled,
            "serverAddr": config.frp.server_addr,
            "serverPort": config.frp.server_port,
            "token": config.frp.token,
            "remotePort": config.frp.remote_port,
            "binary": config.frp.binary,
            "state": frp_state.get("state").cloned().unwrap_or(Value::Null),
            "message": frp_state.get("message").cloned().unwrap_or(Value::Null),
            "endpoint": frp_state.get("endpoint").cloned().unwrap_or(Value::Null),
            "logPath": frp_state.get("logPath").cloned().unwrap_or(Value::Null),
        },
        "configPath": home.join(CONFIG_FILE_NAME).display().to_string(),
    })
}

fn save(args: &Value) -> Result<Value, String> {
    let enabled = args
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| "缺少参数：enabled".to_string())?;
    let port = required_port(args, "port", "监听端口")?;
    let home = home();
    let mut config = load_or_create_config(&home);
    config.enabled = enabled;
    config.port = port;
    // 缺省字段沿用现有配置，便于旧版前端或部分更新时保持兼容。
    let frp_enabled = args
        .get("frpEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(config.frp.enabled);
    let server_addr =
        string_arg(args, "frpServerAddr").unwrap_or(config.frp.server_addr.trim().to_owned());
    let server_port =
        optional_port(args, "frpServerPort", "frp 服务器端口")?.unwrap_or(config.frp.server_port);
    let token = string_arg(args, "frpToken").unwrap_or(config.frp.token.trim().to_owned());
    let remote_port =
        optional_port(args, "frpRemotePort", "远程端口")?.unwrap_or(config.frp.remote_port);
    let binary = string_arg(args, "frpBinary").unwrap_or(config.frp.binary.trim().to_owned());
    if frp_enabled {
        if server_addr.is_empty() {
            return Err("启用端口映射后需要填写 frp 服务器地址".to_owned());
        }
        if remote_port < MIN_PORT {
            return Err(format!("远程端口必须是 {MIN_PORT}-65535 之间的整数"));
        }
    }
    config.frp = FrpConfig {
        enabled: frp_enabled,
        server_addr,
        server_port,
        token,
        remote_port,
        binary,
    };
    write_config(&home, &config)?;
    Ok(status())
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_owned())
}

fn required_port(args: &Value, key: &str, label: &str) -> Result<u16, String> {
    optional_port(args, key, label)?.ok_or_else(|| format!("缺少参数：{key}"))
}

/// 解析端口参数；缺失返回 `None`，存在但越界直接报错，避免静默回退掩盖输入错误。
fn optional_port(args: &Value, key: &str, label: &str) -> Result<Option<u16>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let port = value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port >= MIN_PORT)
        .ok_or_else(|| format!("{label}必须是 {MIN_PORT}-65535 之间的整数"))?;
    Ok(Some(port))
}

fn regenerate_token() -> Result<Value, String> {
    let home = home();
    let token = new_token();
    std::fs::write(home.join(TOKEN_FILE_NAME), &token)
        .map_err(|error| format!("写入访问密钥失败：{error}"))?;
    Ok(status())
}

fn write_config(home: &Path, config: &GatewayConfig) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("序列化远程控制配置失败：{error}"))?;
    std::fs::write(home.join(CONFIG_FILE_NAME), raw)
        .map_err(|error| format!("写入远程控制配置失败：{error}"))
}

fn mode_name(mode: GatewayMode) -> &'static str {
    match mode {
        GatewayMode::Shared => "shared",
        GatewayMode::Isolated => "isolated",
    }
}
