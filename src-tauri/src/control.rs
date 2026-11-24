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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
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
    /// 持久化模式：为真时不会再被启动/停止后的定时器重置回界面确认。
    confirm_persistent: AtomicBool,
    last_status: Mutex<Option<Value>>,
    /// 页面侧回传数据（`/report`，讨论自动化用）。
    report: Mutex<Option<Value>>,
    /// 本次讨论任务的一次性回传口令，避免任意网页调用 `/report`。
    report_nonce: Mutex<String>,
    /// 控制接口实际监听端口，供页面侧回传地址使用。
    port: AtomicU16,
}

impl ControlState {
    pub fn set_confirm(&self, mode: u8) {
        self.confirm_mode.store(mode, Ordering::SeqCst);
    }

    pub fn set_report(&self, value: Value) {
        *self.report.lock() = Some(value);
    }

    pub fn take_report(&self) -> Option<Value> {
        self.report.lock().take()
    }

    pub fn set_report_nonce(&self, nonce: String) -> String {
        *self.report_nonce.lock() = nonce.clone();
        nonce
    }

    pub fn report_nonce(&self) -> String {
        self.report_nonce.lock().clone()
    }

    pub fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::SeqCst);
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    /// 设置持久化确认策略；`ask` 表示交还界面。
    pub fn set_confirm_persistent(&self, mode: u8) {
        self.confirm_mode.store(mode, Ordering::SeqCst);
        self.confirm_persistent
            .store(mode != CONFIRM_ASK, Ordering::SeqCst);
    }

    pub fn confirm_persistent(&self) -> bool {
        self.confirm_persistent.load(Ordering::SeqCst)
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
    /// 确认策略：`yes` / `no` / `ask`（缺省交给界面）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirm: Option<String>,
}

/// 把配置里的确认策略字符串解析成内部模式值。
fn parse_confirm(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("yes") | Some("true") | Some("1") => CONFIRM_YES,
        Some("no") | Some("false") | Some("0") => CONFIRM_NO,
        _ => CONFIRM_ASK,
    }
}

fn confirm_name(mode: u8) -> &'static str {
    match mode {
        CONFIRM_YES => "yes",
        CONFIRM_NO => "no",
        _ => "ask",
    }
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
            confirm: Some("yes".to_owned()),
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
    let mode = parse_confirm(config.confirm.as_deref());
    if mode != CONFIRM_ASK {
        app.state::<ControlState>().set_confirm_persistent(mode);
        log::info!("确认策略: {}", confirm_name(mode));
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
    app.state::<ControlState>().set_port(addr.port());

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
        "confirmMode": confirm_name(control.confirm_mode()),
        "confirmPersistent": control.confirm_persistent(),
        "lastStatus": control.last_status(),
    })
}

/// 让自动确认策略只在短时间窗口内生效，随后交还界面。
/// 写入确认策略到 remote.toml，供下次启动沿用。
fn persist_confirm(mode: &str) -> Result<(), String> {
    let path = config_path();
    let mut config = load_config();
    config.confirm = if mode == "ask" {
        None
    } else {
        Some(mode.to_owned())
    };
    let text = toml::to_string(&config).map_err(|error| error.to_string())?;
    std::fs::write(&path, text).map_err(|error| format!("写入 {} 失败: {error}", path.display()))
}

fn schedule_confirm_reset(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CONFIRM_WINDOW)).await;
        let state = app.state::<ControlState>();
        if state.confirm_persistent() {
            return;
        }
        state.set_confirm(CONFIRM_ASK);
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
    let control = app.state::<ControlState>();
    let persistent = control.confirm_persistent();
    if !persistent {
        control.set_confirm(CONFIRM_YES);
    }
    drop(control);
    if !persistent {
        schedule_confirm_reset(app);
    }
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
    let control = app.state::<ControlState>();
    let persistent = control.confirm_persistent();
    if !persistent {
        control.set_confirm(CONFIRM_NO);
    }
    drop(control);
    if !persistent {
        schedule_confirm_reset(app);
    }
    app.state::<TaskSession>().clear();
    with_course_webview(app, |webview| {
        webview.eval("location.reload()")?;
        Ok(())
    })?;
    Ok(json!({"ok": true, "stopped": true}))
}

/// 讨论任务点：从章节页取话题地址，再在话题页回帖完成。
const EXTRACT_TOPIC_JS: &str = r##"(function () {
  var out = { kind: "topic", chapterId: "__CHAPTER__", url: "", question: "", error: "" };
  try {
    var d1 = document.querySelector("#iframe").contentWindow.document;
    var w1 = d1.querySelector("iframe").contentWindow;
    var w2 = w1.document.getElementById("frame_content").contentWindow;
    var div = w2.document.getElementById("topicMainDiv");
    out.url = div ? div.getAttribute("data") : "";
    out.question = (w2.document.body ? w2.document.body.innerText : "").replace(/\s+/g, " ").trim().slice(0, 200);
  } catch (error) { out.error = String(error); }
  try {
    navigator.sendBeacon("http://127.0.0.1:__PORT__/report?nonce=__NONCE__", new Blob([JSON.stringify(out)], { type: "text/plain" }));
  } catch (error) {}
  return 1;
})();"##;

const REPLY_TOPIC_JS: &str = r##"(function () {
  var out = { kind: "reply", chapterId: "__CHAPTER__", filled: false, clicked: false, ok: false, error: "" };
  try {
    var area = document.querySelector("textarea[placeholder='回复话题']");
    if (area) {
      area.value = __TEXT__;
      area.dispatchEvent(new Event("input", { bubbles: true }));
      area.dispatchEvent(new Event("keyup", { bubbles: true }));
      out.filled = true;
    }
    var button = document.querySelector(".addReply");
    if (button) { button.click(); out.clicked = true; }
  } catch (error) { out.error = String(error); }
  setTimeout(function () {
    try {
      var text = document.body ? document.body.innerText : "";
      out.ok = text.indexOf("刚刚") >= 0;
    } catch (error) {}
    try {
      navigator.sendBeacon("http://127.0.0.1:__PORT__/report?nonce=__NONCE__", new Blob([JSON.stringify(out)], { type: "text/plain" }));
    } catch (error) {}
  }, 3000);
  return 1;
})();"##;

fn make_nonce(seed: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:x}", md5::compute(format!("{seed}-{nanos}").as_bytes()))
}

fn render_js(template: &str, chapter: &str, nonce: &str, port: u16, text: Option<&str>) -> String {
    let mut script = template
        .replace("__CHAPTER__", chapter)
        .replace("__NONCE__", nonce)
        .replace("__PORT__", &port.to_string());
    if let Some(value) = text {
        let literal = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
        script = script.replace("__TEXT__", &literal);
    }
    script
}

async fn wait_report(
    app: &AppHandle,
    kind: &str,
    chapter: &str,
    tries: usize,
) -> Result<Value, (u16, Value)> {
    for _ in 0..tries {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Some(value) = app.state::<ControlState>().take_report() {
            let same_kind = value.get("kind").and_then(Value::as_str) == Some(kind);
            let same_chapter = value.get("chapterId").and_then(Value::as_str) == Some(chapter);
            if same_kind && same_chapter {
                return Ok(value);
            }
        }
    }
    Err((
        500,
        json!({
            "error": format!("等待页面回传超时（{kind} / {chapter}）；页面可能未加载完成或不是章节页"),
        }),
    ))
}

/// 打开指定章节 → 读取讨论话题地址 → 在话题页回帖。
async fn discuss_topic(app: &AppHandle, body: Value) -> Result<Value, (u16, Value)> {
    let text_of = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let chapter = text_of("chapterId").ok_or((400, json!({"error": "缺少 chapterId"})))?;
    let course = text_of("courseId").ok_or((400, json!({"error": "缺少 courseId"})))?;
    let clazz = text_of("clazzId").ok_or((400, json!({"error": "缺少 clazzId"})))?;
    let cpi = text_of("cpi").ok_or((400, json!({"error": "缺少 cpi"})))?;
    let enc = text_of("enc").ok_or((400, json!({"error": "缺少 enc"})))?;
    let text = text_of("text").ok_or((400, json!({"error": "缺少 text（回帖内容）"})))?;
    let dry_run = body.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let wait = body
        .get("waitSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(7)
        .clamp(3, 20);

    let state = app.state::<ControlState>();
    let port = state.port();
    let nonce = state.set_report_nonce(make_nonce(&chapter));
    drop(state);

    let chapter_url = format!(
        "https://mooc1.chaoxing.com/mycourse/studentstudy?chapterId={chapter}&courseId={course}&clazzid={clazz}&cpi={cpi}&enc={enc}&mooc2=1&hidetype=0&fanyaVersion=0"
    );
    {
        let webview = app.get_webview("chaoxing").ok_or((
            409,
            json!({"error": "未找到 chaoxing Webview，请等待主界面初始化"}),
        ))?;
        let parsed: tauri::Url = chapter_url
            .parse()
            .map_err(|error| (500, json!({"error": format!("URL 无效: {error}")})))?;
        webview
            .navigate(parsed)
            .map_err(|error| (500, json!({"error": format!("跳转失败: {error}")})))?;
    }

    tokio::time::sleep(Duration::from_secs(wait)).await;

    let script = render_js(EXTRACT_TOPIC_JS, &chapter, &nonce, port, None);
    {
        let webview = app.get_webview("chaoxing").ok_or((
            409,
            json!({"error": "未找到 chaoxing Webview"}),
        ))?;
        webview
            .eval(&script)
            .map_err(|error| (500, json!({"error": format!("执行脚本失败: {error}")})))?;
    }

    let topic = wait_report(app, "topic", &chapter, 40).await?;
    let topic_url = topic
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let question = topic
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if topic_url.is_empty() {
        return Err((
            500,
            json!({"error": "未取到讨论话题地址；该章节可能不是讨论任务点", "question": question}),
        ));
    }
    if dry_run {
        return Ok(json!({"ok": true, "dryRun": true, "topicUrl": topic_url, "question": question}));
    }

    {
        let webview = app.get_webview("chaoxing").ok_or((
            409,
            json!({"error": "未找到 chaoxing Webview"}),
        ))?;
        let parsed: tauri::Url = topic_url
            .parse()
            .map_err(|error| (500, json!({"error": format!("URL 无效: {error}")})))?;
        webview
            .navigate(parsed)
            .map_err(|error| (500, json!({"error": format!("跳转失败: {error}")})))?;
    }
    tokio::time::sleep(Duration::from_secs(wait)).await;

    let script = render_js(REPLY_TOPIC_JS, &chapter, &nonce, port, Some(&text));
    {
        let webview = app.get_webview("chaoxing").ok_or((
            409,
            json!({"error": "未找到 chaoxing Webview"}),
        ))?;
        webview
            .eval(&script)
            .map_err(|error| (500, json!({"error": format!("执行脚本失败: {error}")})))?;
    }
    let reply = wait_report(app, "reply", &chapter, 40).await?;
    log::info!(
        "讨论回帖完成：章节 {chapter}，结果 {}",
        if reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            "成功"
        } else {
            "未确认"
        }
    );
    Ok(json!({
        "ok": true,
        "chapterId": chapter,
        "question": question,
        "topicUrl": topic_url,
        "filled": reply.get("filled").cloned().unwrap_or(Value::Bool(false)),
        "clicked": reply.get("clicked").cloned().unwrap_or(Value::Bool(false)),
        "confirmed": reply.get("ok").cloned().unwrap_or(Value::Bool(false)),
    }))
}

fn tail_lines(path: &std::path::Path, count: usize) -> std::io::Result<Vec<String>> {    let content = std::fs::read_to_string(path)?;
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
                    "POST /discuss": "自动完成讨论任务点 {\"courseId\",\"clazzId\",\"cpi\",\"enc\",\"chapterId\",\"text\",\"dryRun\"}",
                    "POST /solve": "解题 {\"payload\": Folio, \"allowPartial\": false}",
                    "GET /autoconfirm": "查看确认策略",
                    "POST /autoconfirm": "设置确认策略 {\"mode\": \"yes|no|ask\"}，持久生效",
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

    if path == "/report" && request.method == "POST" {
        let value: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        app.state::<ControlState>().set_report(value);
        return Ok((200, json!({"ok": true})));
    }

    if path == "/discuss" && request.method == "POST" {
        let body = json_body(&request.body)?;
        return discuss_topic(app, body).await.map(|value| (200, value));
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

    if path == "/autoconfirm" && request.method == "GET" {
        let control = app.state::<ControlState>();
        return Ok((
            200,
            json!({
                "ok": true,
                "mode": confirm_name(control.confirm_mode()),
                "persistent": control.confirm_persistent()
            }),
        ));
    }

    if path == "/autoconfirm" && request.method == "POST" {
        let body = json_body(&request.body)?;
        let mode = body
            .get("mode")
            .and_then(Value::as_str)
            .ok_or((400, json!({"error": "缺少 mode（yes/no/ask）"})))?
            .to_owned();
        let value = match mode.as_str() {
            "yes" | "no" | "ask" => mode,
            other => {
                return Err((400, json!({"error": format!("未知的策略: {other}")})));
            }
        };
        app.state::<ControlState>()
            .set_confirm_persistent(parse_confirm(Some(value.as_str())));
        if let Err(error) = persist_confirm(&value) {
            return Err((500, json!({"error": error})));
        }
        log::info!("确认策略已设置为 {value}");
        return Ok((200, json!({"ok": true, "mode": value, "persistent": value != "ask"})));
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
    // `/report` 由页面侧 sendBeacon 回传（无法附加自定义头），改用一次性口令校验。
    let report_ok = request.path == "/report"
        && request.method == "POST"
        && request
            .query
            .get("nonce")
            .is_some_and(|nonce| nonce == &app.state::<ControlState>().report_nonce());
    if !report_ok && !authorized(&request.headers, &token) {
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
