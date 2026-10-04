//! 远程控制网关的最小 HTTP/1.1 服务端：只支持静态页面、JSON 接口与 SSE 事件流。
//!
//! 桌面端依赖的 hyper 只在客户端方向使用，这里没有可复用的服务端实现，
//! 因此按需解析请求行、少量请求头与定长请求体，并统一用 `Connection: close`
//! 结束响应；只有事件流保持长连接并按块编码持续推送。

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEAD_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub(crate) fn query_value(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    pub(crate) fn json(&self) -> Result<Value> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body).context("请求体不是有效 JSON")
    }
}

pub(crate) async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(index) = find_head_end(&buffer) {
            break index;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            bail!("请求头过大");
        }
        let read = stream.read(&mut chunk).await.context("读取请求失败")?;
        if read == 0 {
            bail!("连接已关闭");
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(&buffer[..head_end]).context("请求头不是有效 UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("请求行缺失")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("请求方法缺失")?.to_owned();
    let target = parts.next().context("请求目标缺失")?.to_owned();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let (path, query) = split_target(&target);
    let length = headers
        .get("content-length")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        bail!("请求体过大");
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.context("读取请求体失败")?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    let mut params = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(name), percent_decode(value));
    }
    (percent_decode(path), params)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        output.push(byte);
                        index += 3;
                    }
                    None => {
                        output.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

pub(crate) async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .context("写入响应失败")?;
    stream.write_all(body).await.context("写入响应失败")?;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
    Ok(())
}

pub(crate) async fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    let encoded = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write_response(stream, status, "application/json; charset=utf-8", &encoded).await
}

/// 单向事件流；浏览器用 `EventSource` 消费，因此保持长连接并按块编码推送。
pub(crate) struct EventStream {
    stream: TcpStream,
}

impl EventStream {
    pub(crate) async fn start(mut stream: TcpStream) -> Result<Self> {
        let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream; charset=utf-8\r\n\
                    Cache-Control: no-store\r\n\
                    Connection: keep-alive\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        stream
            .write_all(head.as_bytes())
            .await
            .context("写入事件流响应头失败")?;
        stream.flush().await.ok();
        Ok(Self { stream })
    }

    pub(crate) async fn send(&mut self, payload: &Value) -> Result<()> {
        let mut frame = String::from("data: ");
        frame.push_str(&payload.to_string());
        frame.push_str("\n\n");
        self.chunk(frame.as_bytes()).await
    }

    pub(crate) async fn keepalive(&mut self) -> Result<()> {
        self.chunk(b": keepalive\n\n").await
    }

    async fn chunk(&mut self, bytes: &[u8]) -> Result<()> {
        let head = format!("{:x}\r\n", bytes.len());
        self.stream.write_all(head.as_bytes()).await?;
        self.stream.write_all(bytes).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await?;
        Ok(())
    }
}
