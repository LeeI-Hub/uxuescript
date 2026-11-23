//! 远程控制层。
//!
//! 在本地（也可配置为局域网/隧道）暴露一个小型 HTTP + JSON 接口，让外部程序或
//! AI 助手在不触碰图形界面的前提下驱动客户端：查看状态、启动/停止课程自动化、
//! 跳转任务页、调用答题内核、执行任意注入脚本。
//!
//! 设计约束：
//! * 不引入额外依赖，HTTP/1.1 由本模块自行解析（连接短、请求体小）。
//! * 默认只监听 `127.0.0.1`，所有请求必须携带 Bearer 令牌。
//! * 自动确认只在 `chaoxing` Webview 上生效，避免误伤主界面的确认弹窗。

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::commands::task::TaskSession;
use crate::config::CONFIG;
use crate::core::quiz::Folio;

/// 确认策略：0 = 交给界面，1 = 自动确认，2 = 自动取消。
const CONFIRM_ASK: u8 = 0;
const CONFIRM_YES: u8 = 1;
const CONFIRM_NO: u8 = 2;

/// 远程启动/停止后，自动确认策略的生效窗口；结束后回到界面确认。
const CONFIRM_WINDOW: u64 = 15;

#[derive(Default)]
pub struct ControlState {
    confirm_mode: AtomicU8,
    last_status: Mutex<Option<Value>>,
}

impl ControlState {
    pub fn set_confirm(&self, mode: u8) {
        self.confirm_mode.store(mode, Ordering::SeqCst);
    }

    pub fn confirm_mode(&self) -> u8 {
        self.confirm_mode.load(Ordering::SeqCst)
    }

    /// 远程自动确认策略；`None` 表示交还界面自行确认。
    pub fn auto_confirm(&self) -> Option<bool> {
        match self.confirm_mode.load(Ordering::SeqCst) {
            CONFIRM_YES => Some(true),
            CONFIRM_NO => Some(false),
            _ => None,
        }
    }

    pub fn set_last_status(&self, value: Value) {
        *self.last_status.lock() = Some(value);
    }

    pub fn last_status(&self) -> Option<Value> {
        self.last_status.lock().clone()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RemoteConfig {
    enabled: bool,
    addr: String,
    token: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        let seed = format!(
            "{:?}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
            std::process::id()
        );
        Self {
            enabled: true,
            addr: "127.0.0.1:8765".to_owned(),
            token: format!("{:x}", md5::compute(seed.as_bytes())),
        }
    }
}

fn config_path() -> PathBuf {
    CONFIG
        .paths
        .dirs
        .get("data")
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("remote.toml")
}

/// 读取远程控制配置；首次运行生成随机令牌并落盘。
fn load_config() -> RemoteConfig {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        match toml::from_str::<RemoteConfig>(&text) {
            Ok(config) => return config,
            Err(error) => log::error!("解析 {} 失败: {error}", path.display()),
        }
    }
    let config = RemoteConfig::default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string(&config) {
        Ok(text) => {
            if let Err(error) = std::fs::write(&path, text) {
                log::error!("写入 {} 失败: {error}", path.display());
            }
        }
        Err(error) => log::error!("生成默认远程配置失败: {error}"),
    }
    config
}

/// 在 Tauri setup 阶段启动控制接口；配置关闭时只记录日志。
pub fn start(app: AppHandle) {
    let config = load_config();
    if !config.enabled {
        log::info!("远程控制已停用（{}）", config_path().display());
        return;
    }
    let addr: SocketAddr = match config.addr.parse() {
        Ok(addr) => addr,
        Err(error) => {
            log::error!("远程控制地址无效 [{}]: {error}", config.addr);
            return;
        }
    };
    let token = config.token.clone();
    let path = config_path();

    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("远程控制接口监听 {addr} 失败: {error}");
                return;
            }
        };
        log::info!("远程控制接口已启动: http://{addr}（令牌见 {}）", path.display());
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let app = app.clone();
                    let token = token.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(error) = handle(stream, app, token).await {
                            log::debug!("远程控制连接结束: {error}");
                        }
                    });
                }
                Err(error) => log::warn!("远程控制 accept 失败: {error}"),
            }
        }
    });
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_owned(), value.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect()
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = header_end(&buf) {
            break position;
        }
        if buf.len() > 1 << 20 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "请求头过大"));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "连接提前关闭"));
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_owned();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), parse_query(query)),
        None => (target, HashMap::new()),
    };

    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
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

async fn respond(stream: &mut TcpStream, code: u16, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

fn authorized(headers: &HashMap<String, String>, token: &str) -> bool {
    let header = headers
        .get("authorization")
        .map(|value| value.trim())
        .or_else(|| headers.get("x-token").map(|value| value.trim()));
    match header {
        Some(value) => {
            let value = value.strip_prefix("Bearer ").unwrap_or(value);
            value.trim() == token
        }
        None => false,
    }
}

fn json_body(body: &[u8]) -> Result<Value, (u16, Value)> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|error| {
        (
            400,
            json!({"error": format!("请求体不是合法 JSON: {error}")}),
        )
    })
}

fn course_active(app: &AppHandle) -> bool {
    app.state::<TaskSession>().is_active()
}

fn current_url(app: &AppHandle) -> Option<String> {
    app.get_webview("chaoxing")
        .and_then(|webview| webview.url().ok())
        .map(|url| url.to_string())
}

fn status_value(app: &AppHandle) -> Value {
    let control = app.state::<ControlState>();
    json!({
        "ok": true,
        "courseActive": course_active(app),
        "url": current_url(app),
        "confirmMode": match control.confirm_mode() {
            CONFIRM_YES => "yes",
            CONFIRM_NO => "no",
            _ => "ask",
        },
        "lastStatus": control.last_status(),
    })
}

/// 让自动确认策略只在短时间窗口内生效，随后交还界面。
fn schedule_confirm_reset(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CONFIRM_WINDOW)).await;
        app.state::<ControlState>().set_confirm(CONFIRM_ASK);
    });
}

fn with_course_webview(
    app: &AppHandle,
    action: impl FnOnce(&tauri::Webview) -> anyhow::Result<()>,
) -> Result<Value, (u16, Value)> {
    let webview = app.get_webview("chaoxing").ok_or((
        409,
        json!({"error": "未找到 chaoxing Webview，请等待主界面初始化"}),
    ))?;
    action(&webview).map_err(|error| (500, json!({"error": format!("{error:#}")})))?;
    Ok(Value::Null)
}

/// 启动课程自动化：把当前/指定课程页重新加载，页面注入脚本后自动确认启动。
fn start_course(app: &AppHandle, url: Option<String>) -> Result<Value, (u16, Value)> {
    app.state::<ControlState>().set_confirm(CONFIRM_YES);
    schedule_confirm_reset(app);
    let action = url.clone();
    with_course_webview(app, |webview| match action.as_ref() {
        Some(target) => {
            let parsed: tauri::Url = target
                .parse()
                .map_err(|error| anyhow::anyhow!("URL 无效: {error}"))?;
            webview.navigate(parsed)?;
            Ok(())
        }
        None => {
            webview.eval("location.reload()")?;
            Ok(())
        }
    })?;
    Ok(json!({
        "ok": true,
        "action": if url.is_some() { "navigate" } else { "reload" },
        "autoConfirm": true,
        "note": "页面重新加载后脚本会自动确认启动；约 15 秒后自动确认失效",
    }))
}

/// 停止课程自动化：清空任务会话并重载页面，重载后脚本会被自动取消。
fn stop_course(app: &AppHandle) -> Result<Value, (u16, Value)> {
    app.state::<ControlState>().set_confirm(CONFIRM_NO);
    schedule_confirm_reset(app);
    app.state::<TaskSession>().clear();
    with_course_webview(app, |webview| {
        webview.eval("location.reload()")?;
        Ok(())
    })?;
    Ok(json!({"ok": true, "stopped": true}))
}

fn tail_lines(path: &std::path::Path, count: usize) -> std::io::Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = content.lines().map(|line| line.to_owned()).collect();
    if lines.len() > count {
        lines.drain(..lines.len() - count);
    }
    Ok(lines)
}

fn logs_value(count: usize) -> Result<Value, (u16, Value)> {
    let dir = CONFIG.paths.dirs.get("logs").cloned().unwrap_or_else(|| {
        PathBuf::from(".")
    });
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "log") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
                newest = Some((modified, path));
            }
        }
    }
    let (_, path) = newest.ok_or((404, json!({"error": "没有找到日志文件"})))?;
    let lines = tail_lines(&path, count)
        .map_err(|error| (500, json!({"error": format!("读取日志失败: {error}")})))?;
    Ok(json!({"ok": true, "file": path.to_string_lossy(), "lines": lines}))
}

async fn route(
    app: &AppHandle,
    request: Request,
) -> Result<(u16, Value), (u16, Value)> {
    let path = request.path.as_str();

    if path == "/" || path == "/help" {
        return Ok((
            200,
            json!({
                "app": "uXueScript remote control",
                "version": env!("CARGO_PKG_VERSION"),
                "endpoints": {
                    "GET /health": "存活检查",
                    "GET /status": "课程状态、当前 URL、确认策略、最近状态事件",
                    "POST /start": "启动课程自动化，可选 {\"url\": \"...\"}",
                    "POST /stop": "停止课程自动化并重载页面",
                    "POST /reload": "重载 chaoxing 当前页面",
                    "POST /navigate": "跳转 {\"url\": \"...\"}",
                    "POST /eval": "在 chaoxing 页面执行 {\"js\": \"...\"}",
                    "POST /solve": "解题 {\"payload\": Folio, \"allowPartial\": false}",
                    "GET /logs?lines=N": "查看最新日志尾部"
                },
                "auth": "Authorization: Bearer <remote.toml 中的 token>"
            }),
        ));
    }

    if path == "/health" {
        return Ok((200, json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})));
    }

    if path == "/status" && request.method == "GET" {
        return Ok((200, status_value(app)));
    }

    if path == "/logs" && request.method == "GET" {
        let count = request
            .query
            .get("lines")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100)
            .min(2000);
        return logs_value(count).map(|value| (200, value));
    }

    if path == "/start" && request.method == "POST" {
        let body = json_body(&request.body)?;
        let url = body
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_owned);
        return start_course(app, url).map(|value| (200, value));
    }

    if path == "/stop" && request.method == "POST" {
        return stop_course(app).map(|value| (200, value));
    }

    if path == "/reload" && request.method == "POST" {
        with_course_webview(app, |webview| {
            webview.eval("location.reload()")?;
            Ok(())
        })?;
        return Ok((200, json!({"ok": true})));
    }

    if path == "/navigate" && request.method == "POST" {
        let body = json_body(&request.body)?;
        let target = body
            .get("url")
            .and_then(Value::as_str)
            .ok_or((400, json!({"error": "缺少 url"})))?
            .to_owned();
        with_course_webview(app, |webview| {
            let parsed: tauri::Url = target
                .parse()
                .map_err(|error| anyhow::anyhow!("URL 无效: {error}"))?;
            webview.navigate(parsed)?;
            Ok(())
        })?;
        return Ok((200, json!({"ok": true, "url": target})));
    }

    if path == "/eval" && request.method == "POST" {
        let body = json_body(&request.body)?;
        let script = body
            .get("js")
            .and_then(Value::as_str)
            .ok_or((400, json!({"error": "缺少 js"})))?
            .to_owned();
        with_course_webview(app, |webview| {
            webview.eval(&script)?;
            Ok(())
        })?;
        return Ok((200, json!({"ok": true, "evaluated": true})));
    }

    if path == "/solve" && request.method == "POST" {
        let body = json_body(&request.body)?;
        let payload: Folio = serde_json::from_value(
            body.get("payload")
                .cloned()
                .ok_or((400, json!({"error": "缺少 payload"})))?,
        )
        .map_err(|error| (400, json!({"error": format!("payload 解析失败: {error}")})))?;
        let allow_partial = body
            .get("allowPartial")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let answers = crate::core::quiz::solve(payload, allow_partial)
            .await
            .map_err(|error| (500, json!({"error": format!("{error:#}")})))?;
        return Ok((200, json!({"ok": true, "answers": answers})));
    }

    Err((404, json!({"error": format!("未知接口: {} {}", request.method, path) })))
}

async fn handle(mut stream: TcpStream, app: AppHandle, token: String) -> std::io::Result<()> {
    let request = read_request(&mut stream).await?;
    if !authorized(&request.headers, &token) {
        return respond(&mut stream, 401, &json!({"error": "令牌无效或缺失"})).await;
    }
    // OPTIONS 预检直接放行，方便浏览器/网页侧调试。
    if request.method == "OPTIONS" {
        return respond(&mut stream, 200, &json!({"ok": true})).await;
    }
    match route(&app, request).await {
        Ok((code, value)) => respond(&mut stream, code, &value).await,
        Err((code, value)) => respond(&mut stream, code, &value).await,
    }
}
