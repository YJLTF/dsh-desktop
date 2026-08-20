use axum::body::Body;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::settings;

const TOKEN_COOKIE: &str = "dsh_proxy_token";
const TOKEN_QUERY: &str = "token";

/// 逐跳（hop-by-hop）请求头，不得跨代理转发。
const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone)]
struct ProxyState {
    /// 上游 dsh 服务的基础 URL，例如 `http://127.0.0.1:3080`。
    target: Arc<String>,
    token: Arc<String>,
    client: reqwest::Client,
}

/// 跟踪运行中的代理服务，便于在关闭开关时拆除。
#[derive(Default)]
pub struct ProxyRuntime {
    /// 串行化启动/停止流程：开机自动恢复与用户手动开关并发时，
    /// 分段锁（shutdown/join）可能造成双绑定或状态错乱。
    op_lock: Mutex<()>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    /// 实际监听的端口（配置端口被占用时会回退为系统分配端口，须记住真实值）。
    port: Mutex<Option<u16>>,
}

/// 代理当前是否正在运行（依据停机信号是否存在判断）。
pub async fn is_running(runtime: &ProxyRuntime) -> bool {
    runtime.shutdown.lock().await.is_some()
}

#[derive(serde::Serialize, Clone)]
pub struct ProxyInfo {
    pub running: bool,
    pub port: u16,
    /// 其他机器可访问的局域网地址（最佳推测值）。
    pub lan_ip: Option<String>,
    pub url: Option<String>,
}

/// 判断是否为可用于局域网通信的 IPv4 地址（排除环回、链路本地与未指定地址）。
fn is_lan_ipv4(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(_) => false,
    }
}

/// 探测面向局域网的 IPv4 地址（带 10 秒缓存，含失败结果缓存）。
/// 该函数被 get_proxy_info / 复制链接 / 托盘路径高频调用，
/// 而兜底的主机名解析在某些 DNS/NetBIOS 配置下较慢，不应每次都跑。
pub fn lan_ip() -> Option<String> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<Option<(std::time::Instant, Option<String>)>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let Ok(mut guard) = cache.lock() else {
        return lan_ip_probe();
    };
    if let Some((at, v)) = guard.as_ref() {
        if at.elapsed() < Duration::from_secs(10) {
            return v.clone();
        }
    }
    let fresh = lan_ip_probe();
    *guard = Some((std::time::Instant::now(), fresh.clone()));
    fresh
}

/// 实际探测：优先借助默认路由（UDP connect 只选路由不发包），依次尝试多个公共 DNS，
/// 全部不可达时（纯内网、无外网路由的 Win10 环境）回退为主机名解析。
fn lan_ip_probe() -> Option<String> {
    for target in ["8.8.8.8:80", "114.114.114.114:80", "1.1.1.1:53"] {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        if let Ok(addr) = socket.local_addr() {
            let ip = addr.ip();
            if is_lan_ipv4(&ip) {
                return Some(ip.to_string());
            }
        }
    }
    lan_ip_from_hostname()
}

/// 离线内网的兜底：解析主机名得到本机网卡上的局域网 IPv4。
/// Windows 上 getaddrinfo 通常能将 NetBIOS 计算机名解析为本机 IP。
fn lan_ip_from_hostname() -> Option<String> {
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()?;
    let addrs = (hostname.as_str(), 0).to_socket_addrs().ok()?;
    addrs
        .map(|a| a.ip())
        .find(|ip| is_lan_ipv4(ip))
        .map(|ip| ip.to_string())
}

fn copy_headers(src: &reqwest::header::HeaderMap, dest: &mut axum::http::HeaderMap) {
    for (name, value) in src.iter() {
        let name_str = name.as_str();
        if HOP_HEADERS.contains(&name_str) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name_str),
            HeaderValue::try_from(value.as_bytes()),
        ) {
            dest.insert(n, v);
        }
    }
}

fn is_hop(name: &str) -> bool {
    HOP_HEADERS.contains(&name)
}

/// 通过 cookie 或查询参数校验请求携带的令牌。
/// 返回 `(是否已授权, 是否经查询参数授权)`。
fn authorize(req: &Request, token: &str) -> (bool, bool) {
    if token.is_empty() {
        return (false, false);
    }

    // 校验 cookie。
    if let Some(cookie_hdr) = req.headers().get("cookie") {
        if let Ok(s) = cookie_hdr.to_str() {
            for cookie in s.split(';') {
                let cookie = cookie.trim();
                if let Some(rest) = cookie.strip_prefix(&format!("{TOKEN_COOKIE}=")) {
                    if rest == token {
                        return (true, false);
                    }
                }
            }
        }
    }

    // 校验查询参数。
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(rest) = pair.strip_prefix(&format!("{TOKEN_QUERY}=")) {
                if rest == token {
                    return (true, true);
                }
            }
        }
    }

    (false, false)
}

fn make_cookie_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{TOKEN_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// 令牌缺失或无效时返回的最小 HTML 页面。
fn unauthorized_html() -> Response {
    let body = r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><title>未授权</title></head>
<body style="font-family:system-ui,'Segoe UI',sans-serif;display:flex;height:100vh;margin:0;align-items:center;justify-content:center;background:#0f172a;color:#e2e8f0">
<div style="text-align:center">
<h1>401 — 未授权</h1>
<p>访问该 DSH 端点需要有效的访问令牌。<br>请在 URL 后追加 <code>?token=&lt;令牌&gt;</code>，或从桌面应用中打开。</p>
</div></body></html>"#;
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// 注入到经代理 HTML 的垫片脚本。
///
/// `crypto.randomUUID` 是安全上下文（HTTPS / localhost）专属 API：局域网代理以
/// `http://<内网IP>:<端口>` 明文访问，页面处于非安全上下文，浏览器不暴露该方法；
/// 而 dsh 前端为每条 RPC 生成 ID 都要调用它（如打开工作区文件夹），随即抛出
/// "crypto.randomUUID is not a function"。`crypto.getRandomValues` 在非安全
/// 上下文同样可用，据此实现 RFC 4122 v4 UUID 垫片；已存在原生实现时不动。
const POLYFILL_SCRIPT: &str = concat!(
    r#"<script>(function(){"#,
    r#"if(typeof crypto==="object"&&crypto&&!crypto.randomUUID){"#,
    r#"crypto.randomUUID=function(){"#,
    r#"var b=new Uint8Array(16);crypto.getRandomValues(b);"#,
    r#"b[6]=(b[6]&0x0f)|0x40;b[8]=(b[8]&0x3f)|0x80;"#,
    r#"var h="";for(var i=0;i<16;i++){h+=b[i].toString(16).padStart(2,"0")}"#,
    r#"return h.slice(0,8)+"-"+h.slice(8,12)+"-"+h.slice(12,16)+"-"+h.slice(16,20)+"-"+h.slice(20)"#,
    r#"}}})();</script>"#
);

/// HTML 注入的防御性长度上限：超过则放弃改写原样返回（正常页面仅数 KB），
/// 避免上游异常超大响应在代理内整包占用内存。
const HTML_INJECT_LIMIT: usize = 4 * 1024 * 1024;

/// 判断响应是否为可安全改写的 HTML 文档：
/// content-type 为 `text/html` 且响应体未压缩（identity）——压缩体无法直接注入文本。
fn is_plain_html(headers: &reqwest::header::HeaderMap) -> bool {
    let html = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v.to_ascii_lowercase().starts_with("text/html"));
    let identity = headers
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map_or(true, |v| v.eq_ignore_ascii_case("identity"));
    html && identity
}

/// 将垫片脚本注入 HTML 文档 `<head>` 开始标签之后（退而求其次 `<html>`），
/// 保证它先于页面内所有脚本执行；找不到合法插入点或超限时返回 `None`（不改写）。
fn inject_polyfill(bytes: &[u8]) -> Option<String> {
    if bytes.len() > HTML_INJECT_LIMIT {
        return None;
    }
    let html = std::str::from_utf8(bytes).ok()?;
    let lower = html.to_ascii_lowercase();
    let at = ["<head", "<html"].iter().find_map(|tag| {
        lower
            .find(tag)
            .and_then(|i| lower[i..].find('>').map(|g| i + g + 1))
    })?;
    // 切点紧随 ASCII '>' 之后，必然是 UTF-8 字符边界。
    Some(format!("{}{}{}", &html[..at], POLYFILL_SCRIPT, &html[at..]))
}

/// 兜底处理器：先做令牌鉴权，再进行 HTTP 转发或 WebSocket 隧道。
async fn proxy_handler(
    State(state): State<ProxyState>,
    OriginalUri(uri): OriginalUri,
    ws_upgrade: Option<WebSocketUpgrade>,
    req: Request,
) -> Response {
    let (authorized, via_query) = authorize(&req, &state.token);
    if !authorized {
        return unauthorized_html();
    }

    let path = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    // 是否为 WebSocket 升级请求？
    if let Some(ws) = ws_upgrade {
        let state = state.clone();
        let path = path.clone();
        return ws.on_upgrade(move |socket| forward_ws(socket, state, path));
    }

    // 普通 HTTP 转发。
    let mut resp = match forward_http(state.clone(), path, req).await {
        Ok(r) => r,
        Err(e) => {
            log::error!("代理转发出错：{e}");
            (StatusCode::BAD_GATEWAY, format!("上游错误：{e}")).into_response()
        }
    };

    // 若经查询参数授权，则下发 cookie，使后续请求（含 WS）自动携带。
    if via_query {
        resp.headers_mut().insert(
            axum::http::header::SET_COOKIE,
            make_cookie_header(&state.token),
        );
    }
    resp
}

/// 将普通 HTTP 请求转发至上游并以流的方式回传响应。
async fn forward_http(
    state: ProxyState,
    path: String,
    req: Request,
) -> Result<Response, Box<dyn std::error::Error>> {
    let (parts, body) = req.into_parts();
    let url = format!("{}{}", state.target, path);

    let mut upstream_req = state.client.request(parts.method.clone(), &url);
    // dsh 上游按 Origin 做同源校验：局域网代理源（http://<内网IP>:<端口>）与上游
    // 自身源不同，原样转发会被 403 拒绝（POST /api/* 全挂）。改写为上游源即可
    // 与直连访问等价；referer 同理剥离，上游不依赖它。
    let had_origin = parts.headers.contains_key("origin");
    for (name, value) in parts.headers.iter() {
        if is_hop(name.as_str()) {
            continue;
        }
        // 剥掉 accept-encoding：上游若返回压缩体，HTML 垫片（见 POLYFILL_SCRIPT）
        // 将无法注入；代理与 dsh 之间是本机回环，放弃压缩没有实际代价。
        if name.as_str() == "accept-encoding" {
            continue;
        }
        if name.as_str() == "origin" || name.as_str() == "referer" {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }
    upstream_req = upstream_req.header("host", state.target.as_str());
    if had_origin {
        upstream_req = upstream_req.header("origin", state.target.as_str());
    }
    // 流式透传请求体：整体缓冲会让大附件上传时代理内存峰值与请求体等大。
    // GET/HEAD 按 HTTP 语义不带请求体，跳过（分块空体会让部分服务端拒绝）。
    if !matches!(parts.method.as_str(), "GET" | "HEAD") {
        // BodyStream 产出的是 http_body 帧（数据/尾随帧），须解包为纯字节流。
        let stream = http_body_util::BodyStream::new(body)
            .map(|res| res.map(|frame| frame.into_data().unwrap_or_default()));
        upstream_req = upstream_req.body(reqwest::Body::wrap_stream(stream));
    }

    let upstream_resp = upstream_req.send().await?;
    let status = upstream_resp.status();
    let is_html = is_plain_html(upstream_resp.headers());
    let mut builder = Response::builder().status(status.as_u16());
    if let Some(h) = builder.headers_mut() {
        copy_headers(upstream_resp.headers(), h);
    }

    // HTML 文档需注入 secure-context 垫片，整包读出改写；其余响应一律流式透传。
    if is_html {
        let bytes = upstream_resp.bytes().await?;
        let body = match inject_polyfill(&bytes) {
            Some(html) => Body::from(html),
            None => Body::from(bytes),
        };
        return Ok(builder.body(body)?);
    }

    let stream = upstream_resp.bytes_stream();
    let body = Body::from_stream(stream);
    Ok(builder.body(body)?)
}

/// 在浏览器与上游之间双向隧道化 WebSocket。
async fn forward_ws(socket: WebSocket, state: ProxyState, path: String) {
    let ws_url = format!("ws://{}{}", state.target.trim_start_matches("http://"), path);

    let upstream = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok((s, _)) => s,
        Err(e) => {
            log::error!("代理：连接上游 WebSocket 失败 {ws_url}：{e}");
            return;
        }
    };

    let (mut client_tx, mut client_rx) = socket.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let client_to_up = tokio::spawn(async move {
        while let Some(msg) = client_rx.next().await {
            match msg {
                Ok(m) => {
                    if let Some(t) = to_tungstenite(m) {
                        if up_tx.send(t).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let up_to_client = tokio::spawn(async move {
        while let Some(msg) = up_rx.next().await {
            match msg {
                Ok(m) => {
                    if let Some(a) = to_axum(m) {
                        if client_tx.send(a).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(client_to_up, up_to_client);
}

fn to_tungstenite(m: ws::Message) -> Option<tungstenite::Message> {
    match m {
        ws::Message::Text(t) => Some(tungstenite::Message::Text(t.into())),
        ws::Message::Binary(b) => Some(tungstenite::Message::Binary(b.into())),
        ws::Message::Ping(b) => Some(tungstenite::Message::Ping(b.into())),
        ws::Message::Pong(b) => Some(tungstenite::Message::Pong(b.into())),
        ws::Message::Close(c) => Some(tungstenite::Message::Close(c.map(|f| {
            tungstenite::protocol::CloseFrame {
                code: f.code.into(),
                reason: f.reason.to_string().into(),
            }
        }))),
    }
}

fn to_axum(m: tungstenite::Message) -> Option<ws::Message> {
    match m {
        tungstenite::Message::Text(t) => Some(ws::Message::Text(t.to_string())),
        tungstenite::Message::Binary(b) => Some(ws::Message::Binary(b.to_vec())),
        tungstenite::Message::Ping(b) => Some(ws::Message::Ping(b.to_vec())),
        tungstenite::Message::Pong(b) => Some(ws::Message::Pong(b.to_vec())),
        tungstenite::Message::Close(c) => Some(ws::Message::Close(c.map(|f| ws::CloseFrame {
            code: u16::from(f.code),
            reason: f.reason.to_string().into(),
        }))),
        tungstenite::Message::Frame(_) => None,
    }
}

fn build_router(state: ProxyState) -> Router {
    Router::new()
        .fallback(any(proxy_handler))
        .with_state(state)
}

fn current_info(running: bool, port: u16) -> ProxyInfo {
    let lan_ip = if running { lan_ip() } else { None };
    let url = lan_ip
        .as_ref()
        .map(|ip| format!("http://{}:{port}/?token=", ip));
    ProxyInfo {
        running,
        port,
        lan_ip,
        url,
    }
}

fn emit_proxy_info(handle: &AppHandle, running: bool, port: u16) {
    let _ = handle.emit("proxy-info", current_info(running, port));
}

/// 绑定代理监听端口；配置端口绑定失败时自动回退到系统分配的临时端口。
/// Windows 10 上 Hyper-V / WSL2 / VPN 客户端会随机保留若干连续端口段（重启后还会变化），
/// 配置端口落入保留段时 bind 会以 WSAEACCES(10013) 失败：端口看似空闲却无法监听，
/// 表现为“局域网代理无法启动”。先重试一次排除偶发竞争，仍失败则回退临时端口保证可用。
async fn bind_listener(port: u16) -> std::io::Result<(TcpListener, u16)> {
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 1..=2 {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => return Ok((listener, port)),
            Err(e) => {
                log::warn!("绑定代理端口 {port} 失败（第 {attempt} 次）：{e}");
                last_err = Some(e);
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let listener = TcpListener::bind(("0.0.0.0", 0)).await.map_err(|_| {
        last_err.unwrap_or_else(|| std::io::Error::other("绑定临时端口失败"))
    })?;
    let bound = listener.local_addr()?.port();
    Ok((listener, bound))
}

/// 在 `0.0.0.0:port` 上启动局域网代理服务。
#[tauri::command]
pub async fn start_proxy(
    handle: AppHandle,
    runtime: tauri::State<'_, ProxyRuntime>,
) -> Result<ProxyInfo, String> {
    // 串行化启动流程（见 ProxyRuntime.op_lock 注释）。
    let _op = runtime.op_lock.lock().await;
    // 先停止任何已存在的实例。
    stop_inner(&runtime).await;

    let settings = settings::current(&handle);
    let port = settings.lan_proxy_port;

    let state = ProxyState {
        target: Arc::new(format!("http://{}:{}", settings.dsh_host, settings.dsh_port)),
        token: Arc::new(settings.lan_proxy_token.clone()),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("代理客户端构建失败：{e}"))?,
    };

    let (listener, bound_port) = bind_listener(port)
        .await
        .map_err(|e| format!("绑定代理端口 {port} 失败：{e}"))?;
    if bound_port != port {
        log::warn!(
            "代理端口 {port} 不可用（Windows 10 上常见原因：Hyper-V/WSL/VPN 保留了该端口段，可用 netsh interface ipv4 show excludedportrange protocol=tcp 查询），已回退为系统分配端口 {bound_port}"
        );
    }

    *runtime.port.lock().await = Some(bound_port);

    let (tx, rx) = oneshot::channel::<()>();
    {
        let mut guard = runtime.shutdown.lock().await;
        *guard = Some(tx);
    }

    let app = build_router(state);
    let join = tokio::spawn(async move {
        let serve = axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            });
        if let Err(e) = serve.await {
            log::error!("代理服务结束：{e}");
        }
    });
    {
        let mut guard = runtime.join.lock().await;
        *guard = Some(join);
    }

    emit_proxy_info(&handle, true, bound_port);

    // 首次启用时提示防火墙：Windows 首次监听会弹出防火墙授权，
    // 用户拒绝后局域网访问会静默失败，这里显式提醒一次。
    if !settings.firewall_hint_shown {
        let state = handle.state::<settings::SettingsState>();
        let mut guard = state.0.lock().expect("设置锁已中毒");
        guard.firewall_hint_shown = true;
        let next = guard.clone();
        drop(guard);
        settings::persist_and_sync(&next);
        let _ = crate::notify(
            "局域网代理已启用",
            &format!(
                "若其他设备无法访问，请在 Windows 防火墙中放行端口 {bound_port}（或允许 DSH Desktop 通过防火墙）。"
            ),
        );
    }

    Ok(current_info(true, bound_port))
}

async fn stop_inner(runtime: &ProxyRuntime) {
    if let Some(tx) = runtime.shutdown.lock().await.take() {
        let _ = tx.send(());
    }
    if let Some(handle) = runtime.join.lock().await.take() {
        let _ = handle.await;
    }
    *runtime.port.lock().await = None;
}

/// 停止局域网代理服务。
#[tauri::command]
pub async fn stop_proxy(
    handle: AppHandle,
    runtime: tauri::State<'_, ProxyRuntime>,
) -> Result<(), String> {
    let _op = runtime.op_lock.lock().await;
    stop_inner(&runtime).await;
    let s = settings::current(&handle);
    emit_proxy_info(&handle, false, s.lan_proxy_port);
    Ok(())
}

/// 当前代理状态及可分享的 URL。
#[tauri::command]
pub async fn get_proxy_info(
    handle: AppHandle,
    runtime: tauri::State<'_, ProxyRuntime>,
) -> Result<ProxyInfo, String> {
    let running = is_running(&runtime).await;
    let bound = *runtime.port.lock().await;
    let s = settings::current(&handle);
    // 运行中优先回报实际监听端口（可能因回退与配置端口不同）。
    let port = if running { bound.unwrap_or(s.lan_proxy_port) } else { s.lan_proxy_port };
    Ok(current_info(running, port))
}

/// 返回访问令牌（供界面展示，便于用户复制分享链接）。
#[tauri::command]
pub async fn get_proxy_token(handle: AppHandle) -> Result<String, String> {
    Ok(settings::current(&handle).lan_proxy_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_after_head_tag() {
        let out = inject_polyfill(b"<!doctype html><html><head><title>t</title></head><body></body></html>")
            .expect("应注入");
        let head_end = out.find("<title>").unwrap();
        assert!(out[..head_end].contains(POLYFILL_SCRIPT), "垫片应在 title 之前");
        assert!(out.ends_with("</html>"), "文档其余部分保持原样");
        assert!(out.starts_with("<!doctype html>"), "doctype 保持在前");
    }

    #[test]
    fn head_tag_match_is_case_insensitive() {
        let out = inject_polyfill(b"<HTML><HEAD><meta charset='utf-8'></HEAD></HTML>").expect("应注入");
        assert!(out.contains(&format!("<HEAD>{POLYFILL_SCRIPT}<meta")), "大写标签后应紧跟垫片");
    }

    #[test]
    fn falls_back_to_html_tag_without_head() {
        let out = inject_polyfill(b"<html><body><p>x</p></body></html>").expect("应注入");
        assert!(
            out.starts_with(&format!("<html>{POLYFILL_SCRIPT}<body>")),
            "无 head 时应紧随 <html> 注入"
        );
    }

    #[test]
    fn skips_non_html_or_oversized() {
        assert_eq!(inject_polyfill(b"plain text, no tags"), None);
        assert_eq!(inject_polyfill(b"\xff\xfe<html>"), None, "非 UTF-8 不改写");
        let oversized = vec![b'<'; HTML_INJECT_LIMIT + 1];
        assert_eq!(inject_polyfill(&oversized), None, "超上限不改写");
    }

    #[test]
    fn polyfill_script_fragments_present() {
        // 垫片脚本本身在浏览器执行；这里校验其文本结构关键片段不缺失。
        for fragment in ["getRandomValues", "crypto.randomUUID=function", "0x40", "0x80", "<script>"] {
            assert!(POLYFILL_SCRIPT.contains(fragment), "垫片缺少片段：{fragment}");
        }
    }

    /// 垫片脚本的 ()/{}/[] 必须配平——脚本若有语法错误，解析阶段即死、
    /// 静默不执行，页面会原样复现 "crypto.randomUUID is not a function"
    /// （0.1.3 首版垫片就因少一个 `}` 栽在这里，浏览器只报
    /// "Uncaught SyntaxError: Unexpected token ')'"）。
    /// 脚本内字符串字面量不含括号字符，朴素计数即足够。
    #[test]
    fn polyfill_script_brackets_balanced() {
        let js = POLYFILL_SCRIPT
            .strip_prefix("<script>")
            .and_then(|s| s.strip_suffix("</script>"))
            .expect("垫片应包裹在 script 标签内");
        let mut paren = 0i32;
        let mut brace = 0i32;
        let mut bracket = 0i32;
        for c in js.chars() {
            match c {
                '(' => paren += 1,
                ')' => paren -= 1,
                '{' => brace += 1,
                '}' => brace -= 1,
                '[' => bracket += 1,
                ']' => bracket -= 1,
                _ => {}
            }
            assert!(paren >= 0 && brace >= 0 && bracket >= 0, "垫片脚本括号提前闭合");
        }
        assert_eq!((paren, brace, bracket), (0, 0, 0), "垫片脚本括号必须配平");
        assert!(js.ends_with("})();"), "垫片应以立即执行调用收尾");
    }

    #[test]
    fn detects_plain_html_responses() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::CONTENT_TYPE, "text/html; charset=utf-8".parse().unwrap());
        assert!(is_plain_html(&headers));
        headers.insert(reqwest::header::CONTENT_ENCODING, "identity".parse().unwrap());
        assert!(is_plain_html(&headers), "identity 视为未压缩");

        headers.insert(reqwest::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        assert!(!is_plain_html(&headers), "压缩体不可注入");
        headers.remove(reqwest::header::CONTENT_ENCODING);
        headers.insert(reqwest::header::CONTENT_TYPE, "text/javascript".parse().unwrap());
        assert!(!is_plain_html(&headers), "仅 text/html 可注入");
    }
}
