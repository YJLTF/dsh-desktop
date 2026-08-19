mod autostart;
mod process;
mod proxy;
mod settings;
mod tray;
mod version;

use std::sync::OnceLock;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;

use settings::Settings;

/// 全局应用句柄，供后台任务和模块发送事件 / 通知使用。
pub static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// 发送一条系统通知。
pub fn notify(title: &str, body: &str) -> Result<(), ()> {
    if let Some(handle) = APP_HANDLE.get() {
        handle
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|_| ())
    } else {
        Err(())
    }
}

/// 将文本写入系统剪贴板。
/// 不走前端 navigator.clipboard：隐藏 / 未聚焦的 WebView 无法执行该 API，托盘操作会静默失败。
pub fn copy_text(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .map_err(|e| e.to_string())
}

/// Harness 窗口打开后将控制面板隐藏到托盘（不在任务栏占位），仅显示 Harness 窗口。
/// 恢复路径（托盘“显示控制面板”、二次启动唤起）均已先 unminimize 再 show，
/// 因此即使隐藏前处于最小化状态也能正常还原。
fn hide_control_panel(handle: &AppHandle) {
    if let Some(panel) = handle.get_webview_window("control-panel") {
        let _ = panel.hide();
    }
}

/// 打开（或聚焦）加载 dsh Web 界面的专用窗口。
pub fn open_harness_window_inner(handle: &AppHandle) {
    let h = handle.clone();
    tauri::async_runtime::spawn(async move {
        open_harness_window_checked(&h).await;
    });
}

/// 判断 WebView 当前地址是否指向 dsh 服务（localhost 与 127.0.0.1 视为等同）。
pub fn url_points_to_dsh(current: &url::Url, dsh_host: &str, dsh_port: u16) -> bool {
    let host_ok = match current.host_str() {
        Some(h) => {
            h == dsh_host
                || (h == "localhost" && dsh_host == "127.0.0.1")
                || (h == "127.0.0.1" && dsh_host == "localhost")
        }
        None => false,
    };
    host_ok && current.port().unwrap_or(80) == dsh_port
}

/// 打开 Harness 窗口的实际流程：先等待 dsh 服务可达再导航。
/// 若在服务未就绪时导航，WebView2 会停留在连接错误页（表现为白屏）且不会自动重试。
pub async fn open_harness_window_checked(handle: &AppHandle) {
    let s = settings::current(handle);
    let dsh_url = format!("http://{}:{}", s.dsh_host, s.dsh_port);

    // 等待服务就绪（最长约 10 秒），避免撞上启动 / 重启空窗期。
    if !process::probe_server(&s.dsh_host, s.dsh_port, std::time::Duration::from_secs(10)).await {
        log::warn!("无法打开 Harness 窗口：dsh 服务未就绪（{dsh_url}）");
        let _ = notify(
            "无法打开 Harness 界面",
            "dsh 服务未就绪，请稍后重试或先在控制面板启动 dsh",
        );
        return;
    }

    if let Some(w) = handle.get_webview_window("harness-ui") {
        // 窗口已存在：若页面停留在错误页或地址不符，重新导航后再显示。
        let needs_nav = w
            .url()
            .map(|u| !url_points_to_dsh(&u, &s.dsh_host, s.dsh_port))
            .unwrap_or(true);
        if needs_nav {
            let _ = w.eval(&format!("window.location.href = '{dsh_url}';"));
        }
        // Windows 上最小化的窗口 show() 是空操作，须先 unminimize 才能还原。
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        hide_control_panel(handle);
        return;
    }

    let builder = WebviewWindowBuilder::new(
        handle,
        "harness-ui",
        WebviewUrl::External(url::Url::parse(&dsh_url).expect("dsh URL 合法")),
    )
    .title("DSH Desktop")
    .inner_size(1280.0, 840.0)
    .min_inner_size(720.0, 480.0)
    .on_navigation(|url| {
        match url.host_str() {
            Some(h) => h == "127.0.0.1" || h == "localhost" || h == "tauri.localhost",
            None => false,
        }
    });

    #[cfg(debug_assertions)]
    let builder = builder.devtools(true);

    match builder.build() {
        Ok(w) => {
            // 用 eval 再触发一次导航：部分 WebView2 环境下首次 External 加载会黑屏，
            // 二次导航可强制渲染。
            let _ = w.eval(&format!("window.location.href = '{dsh_url}';"));
            hide_control_panel(handle);
        }
        Err(e) => {
            log::error!("创建 Harness 窗口失败：{e}");
            let _ = notify("打开 Harness 窗口失败", &e.to_string());
        }
    }
}

#[tauri::command]
fn get_settings(handle: AppHandle) -> Result<Settings, String> {
    Ok(settings::current(&handle))
}

/// 替换设置（若传入的令牌为空则保留原令牌）。
#[tauri::command]
fn update_settings(handle: AppHandle, incoming: Settings) -> Result<Settings, String> {
    let state = handle.state::<settings::SettingsState>();
    let mut guard = state.0.lock().expect("设置锁已中毒");
    let mut next = incoming;

    // 端口范围校验：越界值会让 serde 反序列化失败，导致任何设置都保存不了。
    if !(1024..=65535).contains(&next.lan_proxy_port) {
        return Err(format!("代理端口必须在 1024–65535 之间：{}", next.lan_proxy_port));
    }
    if !(1024..=65535).contains(&next.dsh_port) {
        return Err(format!("dsh 端口必须在 1024–65535 之间：{}", next.dsh_port));
    }

    if next.lan_proxy_token.trim().is_empty() {
        next.lan_proxy_token = guard.lan_proxy_token.clone();
    }
    // 开机自启动变化时同步注册表。
    if next.auto_launch_app != guard.auto_launch_app {
        autostart::set_enabled(next.auto_launch_app)?;
    }
    *guard = next.clone();
    drop(guard);
    settings::persist_and_sync(&next);
    Ok(next)
}

/// 重新生成局域网访问令牌；代理运行中则重启代理使新令牌立即生效
///（ProxyState 持有旧令牌的副本，不重启的话新令牌会被 401 拒绝、旧令牌继续可用）。
#[tauri::command]
async fn regenerate_token(handle: AppHandle) -> Result<String, String> {
    let state = handle.state::<settings::SettingsState>();
    let updated = {
        let mut guard = state.0.lock().expect("设置锁已中毒");
        guard.lan_proxy_token = settings::generate_token();
        guard.clone()
    };
    let new_token = updated.lan_proxy_token.clone();
    settings::persist_and_sync(&updated);

    let runtime = handle.state::<proxy::ProxyRuntime>();
    if proxy::is_running(&runtime).await {
        let _ = proxy::stop_proxy(handle.clone(), runtime.clone()).await;
        if let Err(e) = proxy::start_proxy(handle.clone(), runtime).await {
            log::error!("重置令牌后重启代理失败：{e}");
        }
    }
    Ok(new_token)
}

#[tauri::command]
fn get_app_version(handle: AppHandle) -> Result<String, String> {
    Ok(handle.package_info().version.to_string())
}

#[tauri::command]
async fn open_harness_window(handle: AppHandle) {
    open_harness_window_checked(&handle).await;
}

/// 打开文件选择窗口，让用户挑选自定义 dsh 入口（bin.js / 可执行文件）。
/// 返回 null 表示用户取消。blocking API 不可在主线程调用；
/// 异步命令运行在独立线程上，满足该约束。
#[tauri::command]
async fn pick_dsh_path(handle: AppHandle) -> Result<Option<String>, String> {
    let mut builder = handle
        .dialog()
        .file()
        .set_title("选择 dsh 入口文件")
        .add_filter(
            "dsh 入口（脚本 / 可执行文件）",
            &["js", "mjs", "cmd", "bat", "exe"],
        );
    if let Some(w) = handle.get_webview_window("control-panel") {
        builder = builder.set_parent(&w);
    }
    Ok(builder
        .blocking_pick_file()
        .and_then(|f: tauri_plugin_dialog::FilePath| f.into_path().ok())
        .map(|p: std::path::PathBuf| p.to_string_lossy().into_owned()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("control-panel") {
                // 面板可能在最小化状态下被隐藏到托盘（打开 Harness 后），
                // Windows 上对最小化窗口 show() 是空操作，须先 unminimize 才能还原。
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            let handle = app.handle().clone();
            let _ = APP_HANDLE.set(handle.clone());

            // 加载并注册设置状态。
            let loaded = settings::Settings::load().unwrap_or_default();
            app.manage(settings::SettingsState(std::sync::Mutex::new(loaded.clone())));

            // 开机自启动：以设置为准同步注册表（纠正用户手动改注册表造成的不一致）。
            autostart::sync_from_settings(&loaded);

            // 注册长期存活的运行时状态。
            app.manage(process::DshState::default());
            app.manage(proxy::ProxyRuntime::default());

            // 构建系统托盘与菜单。
            tray::build_tray(app);

            // 向控制面板推送初始状态。
            process::emit_status(&handle, false);

            // 若已配置则自动启动 dsh。
            let auto_start = loaded.auto_start_dsh;
            if auto_start {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    match process::start_dsh(h.clone(), h.state::<process::DshState>()).await {
                        Ok(_) => log::info!("dsh 已自动启动"),
                        Err(e) => {
                            log::warn!("dsh 自动启动失败：{e}");
                            let _ = notify("无法启动 dsh", &e);
                        }
                    }
                });
            }

            // 若上次会话结束时代理处于开启状态则自动恢复。
            if loaded.lan_proxy_enabled {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let runtime = h.state::<proxy::ProxyRuntime>();
                    if let Err(e) = proxy::start_proxy(h.clone(), runtime).await {
                        log::warn!("代理自动启动失败：{e}");
                    }
                });
            }

            // 启动定期更新检查。
            version::spawn_periodic_check();

            // 启动后稍作延迟执行一次更新检查。
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if settings::current(&h).auto_check_updates {
                    let _ = version::check_for_updates(h.clone()).await;
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // 控制面板窗口：最小化到托盘而非退出。
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "control-panel" {
                    let handle = window.app_handle();
                    if settings::current(handle).minimize_to_tray {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            // 进程
            process::start_dsh,
            process::stop_dsh,
            process::restart_dsh,
            process::get_dsh_status,
            process::get_resolution,
            process::validate_dsh_path,
            // 版本
            version::check_for_updates,
            version::update_dsh,
            // 代理
            proxy::start_proxy,
            proxy::stop_proxy,
            proxy::get_proxy_info,
            proxy::get_proxy_token,
            // 设置
            get_settings,
            update_settings,
            regenerate_token,
            get_app_version,
            open_harness_window,
            pick_dsh_path,
        ])
        .build(tauri::generate_context!())
        .expect("初始化 Tauri 应用时出错")
        .run(|app, event| {
            // 应用退出前结束 dsh 子进程：app.exit() 不执行受管状态的析构，
            // 不在此显式结束会遗留孤儿 node 进程占用端口。
            if let tauri::RunEvent::Exit = event {
                app.state::<process::DshState>().shutdown_blocking();
            }
        });
}
