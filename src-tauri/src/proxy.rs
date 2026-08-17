use axum::body::Body;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
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
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    /// 实际监听的端口（配置端口被占用时会回退为系统分配端口，须记住真实值）。
    port: Mutex<Option<u16>>,
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

/// 在不发送任何数据包的情况下探测面向局域网的 IPv4 地址。
/// 优先借助默认路由（UDP connect 只选路由不发包）；依次尝试多个公共 DNS，
/// 全部不可达时（纯内网、无外网路由的 Win10 环境）回退为主机名解析。
pub fn lan_ip() -> Option<String> {
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
<p>访问该 DeepSeek Harness 端点需要有效的访问令牌。<br>请在 URL 后追加 <code>?token=&lt;令牌&gt;</code>，或从桌面应用中打开。</p>
</div></body></html>"#;
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
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

    let body_bytes = body.collect().await?.to_bytes();
    let mut upstream_req = state.client.request(parts.method.clone(), &url);
    for (name, value) in parts.headers.iter() {
        if is_hop(name.as_str()) {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }
    upstream_req = upstream_req.header("host", state.target.as_str());
    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes);
    }

    let upstream_resp = upstream_req.send().await?;
    let status = upstream_resp.status();
    let mut builder = Response::builder().status(status.as_u16());
    if let Some(h) = builder.headers_mut() {
        copy_headers(upstream_resp.headers(), h);
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
    let running = runtime.shutdown.lock().await.is_some();
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
