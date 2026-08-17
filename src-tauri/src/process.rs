use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::settings;

/// Windows 下 CREATE_NO_WINDOW 标志：GUI 应用拉起控制台程序（node.exe、npm.cmd 等）时
/// 不为其新建终端窗口，否则安装版每次启动 dsh 都会弹出 node.exe 控制台。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// dsh 入口的解析方式。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum Resolution {
    /// `node <script>` —— 使用系统 node 运行 Node.js 脚本。
    Node {
        node: String,
        script: String,
        source: String,
    },
    /// 独立可执行文件（自定义路径 / 全局可执行文件）。
    Executable { exe: String, source: String },
    /// 任何位置都找不到 dsh。
    NotFound { message: String },
}

impl Default for Resolution {
    fn default() -> Self {
        Resolution::NotFound {
            message: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub running: bool,
    pub host: String,
    pub port: u16,
    pub url: String,
}

impl Default for Status {
    fn default() -> Self {
        let s = settings::Settings::default();
        let host = s.dsh_host;
        let port = s.dsh_port;
        Self {
            running: false,
            url: format!("http://{}:{}", host, port),
            host,
            port,
        }
    }
}

/// 持有正在运行的 dsh 子进程（若存在）。
#[derive(Default)]
pub struct DshState {
    child: Arc<Mutex<Option<Child>>>,
    resolution: Mutex<Resolution>,
}

impl DshState {
    pub async fn resolution(&self) -> Resolution {
        self.resolution.lock().await.clone()
    }

    /// 立即结束 dsh 子进程（若存在）。供应用退出钩子同步调用，
    /// 避免应用退出后遗留孤儿 node 进程占用端口。
    pub fn shutdown_blocking(&self) {
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}

/// 在 `base` 目录下查找 `node_modules/@deepseek-ai/dsh/lib/bin.js`。
fn local_script(base: &Path) -> Option<PathBuf> {
    let candidate = base
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// 在已经是 node_modules 的目录中直接查找 `@deepseek-ai/dsh/lib/bin.js`。
/// 用于全局安装：`npm root -g` 返回的路径本身就是 node_modules 目录。
fn script_in_node_modules(node_modules_dir: &Path) -> Option<PathBuf> {
    let candidate = node_modules_dir
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// 通过 `npm root -g` 解析全局 node_modules 根目录（动态查询，跟随 npm prefix 变化，
/// 不写死路径）。这是默认路径检查的首选方式。
fn global_node_modules() -> Option<PathBuf> {
    let mut cmd = std::process::Command::new(npm_exe()?);
    cmd.args(["root", "-g"]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // npm 偶尔会在 stdout 混入通知行；路径始终在最后一个非空行上。
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()?;
    Some(PathBuf::from(line))
}

fn node_exe() -> Option<PathBuf> {
    which::which("node").ok().or_else(|| which::which("node.exe").ok())
}

/// 在 node 可执行文件同目录下查找的 npm 文件名。Windows 上 node 目录同时附带
/// 无扩展名的 npm（sh 脚本，CreateProcess 无法直接运行），必须优先检查 npm.cmd。
#[cfg(windows)]
const NPM_FILES: &[&str] = &["npm.cmd"];
#[cfg(not(windows))]
const NPM_FILES: &[&str] = &["npm"];

/// 解析 npm 可执行文件路径。GUI 子系统应用继承自 Shell 的 PATH 可能缺失用户级 npm
/// 目录（Win10 上“已安装全局包却找不到 dsh”的常见原因）：优先用 node 同目录的 npm，
/// 再查 PATH，最后回退到 npm 用户级前缀（Windows 为 %APPDATA%\npm，由环境变量推导）。
fn npm_exe() -> Option<PathBuf> {
    if let Some(node) = node_exe() {
        if let Some(dir) = node.parent() {
            for name in NPM_FILES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    for name in NPM_FILES {
        if let Ok(found) = which::which(name) {
            return Some(found);
        }
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let candidate = PathBuf::from(appdata).join("npm").join("npm.cmd");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// npm 不可用（找不到 / 执行失败）时的兜底：由环境变量推导全局 node_modules 目录。
/// Windows 上 npm 默认全局前缀为 %APPDATA%\npm，其 node_modules 子目录即全局包目录。
/// 这是基于 npm 约定 + 环境变量的推导，而非写死机器特定路径。
fn global_node_modules_fallback() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")?;
        let dir = PathBuf::from(appdata).join("npm").join("node_modules");
        if dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

/// 校验自定义 dsh 路径，返回需要向用户提示的警告信息（None 表示无问题）。
/// resolve_dsh 对无效路径会静默回退到自动查找，此函数用于让界面显式告知用户。
fn validate_custom_path(custom: &Option<String>) -> Option<String> {
    let p = custom.as_ref()?;
    if p.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(p);
    if !path.is_file() {
        return Some(format!("路径无效（不存在或不是文件），当前已回退为自动查找：{p}"));
    }
    let lower = path.to_string_lossy().to_lowercase();
    if (lower.ends_with(".js") || lower.ends_with(".mjs")) && node_exe().is_none() {
        return Some("这是一个 Node.js 脚本，但未在系统中找到 node，启动将失败".into());
    }
    None
}

/// 校验自定义 dsh 路径（供控制面板实时展示警告）。
#[tauri::command]
pub async fn validate_dsh_path(path: Option<String>) -> Result<Option<String>, String> {
    Ok(validate_custom_path(&path))
}

/// 查找 dsh 入口及 node 可执行文件（使用脚本方式时）。
pub async fn resolve_dsh(custom: &Option<String>) -> Resolution {
    // 1. 设置中的自定义路径。
    if let Some(p) = custom {
        let path = PathBuf::from(p);
        if path.is_file() {
            let lower = path.to_string_lossy().to_lowercase();
            if lower.ends_with(".js") || lower.ends_with(".mjs") {
                if let Some(node) = node_exe() {
                    return Resolution::Node {
                        node: node.to_string_lossy().into_owned(),
                        script: path.to_string_lossy().into_owned(),
                        source: "自定义路径".into(),
                    };
                }
            } else {
                return Resolution::Executable {
                    exe: path.to_string_lossy().into_owned(),
                    source: "自定义路径".into(),
                };
            }
        }
        log::warn!("自定义 dsh 路径无效，回退到自动查找：{p}");
    }

    // 2. 相对于当前工作目录与可执行文件目录的本地 node_modules。
    if let Some(node) = node_exe() {
        let cwd = std::env::current_dir().ok();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf));
        for base in cwd.iter().chain(exe_dir.iter()) {
            if let Some(script) = local_script(base) {
                return Resolution::Node {
                    node: node.to_string_lossy().into_owned(),
                    script: script.to_string_lossy().into_owned(),
                    source: format!("本地安装（{}）", base.display()),
                };
            }
        }

        // 3. 全局 node_modules：首选 `npm root -g` 动态解析（自动跟随 npm prefix，
        //    含用户自定义 prefix）；npm 不可用时回退到由环境变量推导的默认全局目录。
        let global_root = global_node_modules().or_else(global_node_modules_fallback);
        if let Some(root) = global_root {
            if let Some(script) = script_in_node_modules(&root) {
                return Resolution::Node {
                    node: node.to_string_lossy().into_owned(),
                    script: script.to_string_lossy().into_owned(),
                    source: "全局安装".into(),
                };
            }
        }
    }

    Resolution::NotFound {
        message: "未找到 @deepseek-ai/dsh，请使用以下命令安装：npm install -g @deepseek-ai/dsh".into(),
    }
}

fn status_for(host: &str, port: u16, running: bool) -> Status {
    Status {
        running,
        host: host.to_string(),
        port,
        url: format!("http://{}:{}", host, port),
    }
}

/// 轮询探测 dsh web 服务，直到其响应或超过截止时间。
/// 期间若子进程退出（例如端口被占用导致启动失败），立即返回 false。
async fn wait_for_server(
    child: Arc<Mutex<Option<Child>>>,
    host: &str,
    port: u16,
    timeout: Duration,
) -> bool {
    let url = format!("http://{}:{}", host, port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // 子进程是否仍存活？
        {
            let mut guard = child.lock().await;
            match guard.as_mut() {
                Some(c) => {
                    if c.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                        *guard = None;
                        return false;
                    }
                }
                None => return false,
            }
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        if client.get(&url).send().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 探测 dsh web 服务是否在截止时间内变为可达（不涉及子进程状态）。
/// 供打开 Harness 窗口等场景使用：若在服务启动空窗期导航，
/// WebView 会停留在连接错误页且不会自动重试。
pub async fn probe_server(host: &str, port: u16, timeout: Duration) -> bool {
    let url = format!("http://{}:{}", host, port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        if client.get(&url).send().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 清理占用指定端口的遗留 node（dsh）进程。
/// 上次会话若被强杀（如任务管理器结束进程），dsh 子进程会残留并占用端口，
/// 导致本次启动的新子进程因端口冲突静默退出。
#[cfg(windows)]
async fn cleanup_stale_port_owner(port: u16) {
    use std::process::Command as StdCommand;

    // 通过 netstat 找到监听该端口的 PID。
    let mut netstat = StdCommand::new("netstat");
    netstat.args(["-ano", "-p", "tcp"]);
    #[cfg(windows)]
    netstat.creation_flags(CREATE_NO_WINDOW);
    let Ok(out) = netstat.output() else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let suffix = format!(":{port}");
    let mut killed = false;
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // TCP  本地地址:端口  远程地址  LISTENING  PID
        if parts.len() >= 5
            && parts[0] == "TCP"
            && parts[3].eq_ignore_ascii_case("LISTENING")
            && parts[1].ends_with(&suffix)
        {
            let pid = parts[4];
            if pid.chars().all(|c| c.is_ascii_digit()) && kill_if_node(pid) {
                log::info!("已清理遗留的 dsh 进程（PID {pid}）");
                killed = true;
            }
        }
    }
    if killed {
        // 等待操作系统释放端口。
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 若 PID 对应的是 node 进程则强制结束它，返回是否已结束。
#[cfg(windows)]
fn kill_if_node(pid: &str) -> bool {
    use std::process::Command as StdCommand;

    let mut tasklist = StdCommand::new("tasklist");
    tasklist.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    #[cfg(windows)]
    tasklist.creation_flags(CREATE_NO_WINDOW);
    let Ok(out) = tasklist.output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(first_line) = text.lines().next() else {
        return false;
    };
    // 只结束 node 进程（dsh 的载体），避免误杀其他服务。
    if !first_line.to_lowercase().starts_with("\"node") {
        return false;
    }
    let mut taskkill = StdCommand::new("taskkill");
    taskkill.args(["/F", "/PID", pid]);
    #[cfg(windows)]
    taskkill.creation_flags(CREATE_NO_WINDOW);
    taskkill
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 根据解析得到的入口构建 tokio Command。
fn build_command(resolution: &Resolution) -> Option<Command> {
    match resolution {
        Resolution::Node { node, script, .. } => {
            let mut cmd = Command::new(node);
            cmd.arg(script).arg("web");
            Some(cmd)
        }
        Resolution::Executable { exe, .. } => {
            let mut cmd = Command::new(exe);
            cmd.arg("web");
            Some(cmd)
        }
        Resolution::NotFound { .. } => None,
    }
}

pub fn emit_status(handle: &AppHandle, running: bool) {
    let s = settings::current(handle);
    let status = status_for(&s.dsh_host, s.dsh_port, running);
    let _ = handle.emit("dsh-status", status);
}

/// 启动 dsh web 服务，成功时返回基础 URL。
#[tauri::command]
pub async fn start_dsh(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<String, String> {
    let settings = settings::current(&handle);

    // 是否已在运行？
    {
        let mut guard = state.child.lock().await;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    return Ok(format!("http://{}:{}", settings.dsh_host, settings.dsh_port))
                }
                _ => *guard = None,
            }
        }
    }

    let resolution = resolve_dsh(&settings.dsh_custom_path).await;
    *state.resolution.lock().await = resolution.clone();

    let mut cmd = build_command(&resolution).ok_or_else(|| match &resolution {
        Resolution::NotFound { message } => message.clone(),
        _ => "未预期的解析结果".into(),
    })?;

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    // 安装版为 GUI 子系统，需显式加 CREATE_NO_WINDOW，否则会弹出 node.exe 终端窗口。
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    // 清理上次会话遗留的 dsh 孤儿进程，避免端口冲突导致新子进程静默退出。
    #[cfg(windows)]
    cleanup_stale_port_owner(settings.dsh_port).await;

    let child = cmd.spawn().map_err(|e| format!("拉起 dsh 失败：{e}"))?;

    let child_handle = state.child.clone();
    {
        let mut guard = state.child.lock().await;
        *guard = Some(child);
    }

    emit_status(&handle, true);

    // 等待服务可达。
    let host = settings.dsh_host.clone();
    let port = settings.dsh_port;
    let reachable = wait_for_server(child_handle, &host, port, Duration::from_secs(60)).await;
    if !reachable {
        // 服务不可达：回收子进程并同步界面状态。
        let mut guard = state.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        drop(guard);
        emit_status(&handle, false);
        return Err("dsh 启动失败：进程已退出或服务未在规定时间内变为可达".into());
    }
    Ok(format!("http://{}:{}", host, port))
}

/// 若有正在运行的 dsh 进程则停止它。
#[tauri::command]
pub async fn stop_dsh(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<(), String> {
    let mut guard = state.child.lock().await;
    if let Some(mut child) = guard.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    drop(guard);
    emit_status(&handle, false);
    Ok(())
}

/// 重启 dsh。
#[tauri::command]
pub async fn restart_dsh(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<String, String> {
    stop_dsh(handle.clone(), state.clone()).await?;
    start_dsh(handle, state).await
}

/// dsh 进程的当前状态。
#[tauri::command]
pub async fn get_dsh_status(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<Status, String> {
    let s = settings::current(&handle);
    let running = {
        let mut guard = state.child.lock().await;
        match guard.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                _ => {
                    *guard = None;
                    false
                }
            },
            None => false,
        }
    };
    Ok(status_for(&s.dsh_host, s.dsh_port, running))
}

/// 返回 dsh 的解析方式（供界面展示）。
#[tauri::command]
pub async fn get_resolution(
    state: tauri::State<'_, DshState>,
) -> Result<Resolution, String> {
    Ok(state.resolution().await)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DshVersion {
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
}
