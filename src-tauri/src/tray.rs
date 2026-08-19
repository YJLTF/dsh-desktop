use std::sync::Mutex;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent, TrayIconId};
use tauri::{App, AppHandle, Emitter, Manager, Wry};

use crate::settings;

/// 持有代理菜单项的句柄，使其文本能反映实时状态。
pub struct TrayItems {
    pub proxy: Mutex<Option<MenuItem<Wry>>>,
}

pub fn build_tray(app: &App) {
    let open_harness = MenuItem::with_id(app, "open-harness", "打开 Harness 界面", true, None::<&str>).unwrap();
    let show_panel = MenuItem::with_id(app, "show-panel", "显示控制面板", true, None::<&str>).unwrap();
    let toggle_proxy = MenuItem::with_id(app, "toggle-proxy", "启用局域网代理", true, None::<&str>).unwrap();
    let open_url = MenuItem::with_id(app, "open-proxy-url", "复制局域网访问链接", true, None::<&str>).unwrap();
    let check_updates = MenuItem::with_id(app, "check-updates", "检查更新", true, None::<&str>).unwrap();
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>).unwrap();

    let menu = MenuBuilder::new(app)
        .item(&open_harness)
        .item(&show_panel)
        .separator()
        .item(&toggle_proxy)
        .item(&open_url)
        .separator()
        .item(&check_updates)
        .separator()
        .item(&quit)
        .build()
        .unwrap();

    // 复用 tauri.conf.json 中声明的托盘；否则新建一个。
    let tray = match app.tray_by_id(&TrayIconId::new("main-tray")) {
        Some(t) => {
            let _ = t.set_menu(Some(menu));
            t
        }
        None => {
            tauri::tray::TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DSH Desktop")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .build(app)
                .unwrap()
        }
    };

    app.manage(TrayItems {
        proxy: Mutex::new(Some(toggle_proxy)),
    });

    set_proxy_label(app.handle(), false);

    tray.on_menu_event(|app, event| match event.id().as_ref() {
        "open-harness" => {
            crate::open_harness_window_inner(app);
        }
        "show-panel" => {
            if let Some(w) = app.get_webview_window("control-panel") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        "toggle-proxy" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                toggle_proxy_from_tray(&app).await;
            });
        }
        "open-proxy-url" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let runtime = app.state::<crate::proxy::ProxyRuntime>();
                let info = crate::proxy::get_proxy_info(app.clone(), runtime)
                    .await
                    .unwrap_or_else(|_| crate::proxy::ProxyInfo {
                        running: false,
                        port: settings::current(&app).lan_proxy_port,
                        lan_ip: None,
                        url: None,
                    });
                if let Some(base) = info.url {
                    let token = settings::current(&app).lan_proxy_token;
                    let full = format!("{base}{token}");
                    match crate::copy_text(&full) {
                        Ok(()) => {
                            let _ = crate::notify("已复制局域网访问链接", &full);
                        }
                        Err(e) => {
                            let _ = crate::notify("复制到剪贴板失败", &e);
                        }
                    }
                } else {
                    let _ = crate::notify("局域网代理未启用", "请先启用代理以获取访问链接。");
                }
            });
        }
        "check-updates" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                match crate::version::check_for_updates(app.clone()).await {
                    Ok(info) if !info.update_available => {
                        let v = info.installed.as_deref().unwrap_or("?");
                        let _ = crate::notify("dsh 已是最新版本", &format!("当前版本：{v}"));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = crate::notify("检查更新失败", &e);
                    }
                }
            });
        }
        "quit" => {
            app.exit(0);
        }
        _ => {}
    });

    tray.on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let app = tray.app_handle();
            // dsh 运行中左键打开 Harness；未运行时左键只会得到“服务未就绪”通知，
            // 不如直接唤起控制面板让用户先启动服务。
            let running = app
                .try_state::<crate::process::DshState>()
                .map(|s| s.is_running())
                .unwrap_or(false);
            if running {
                crate::open_harness_window_inner(app);
            } else if let Some(w) = app.get_webview_window("control-panel") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
    });
}

/// 在普通托盘图标与“有更新”徽标图标之间切换。
pub fn set_update_badge(handle: &AppHandle, available: bool) {
    if let Some(tray) = handle.tray_by_id(&TrayIconId::new("main-tray")) {
        let icon = if available {
            tauri::include_image!("icons/icon-update.png")
        } else {
            tauri::include_image!("icons/icon.png")
        };
        let _ = tray.set_icon(Some(icon));
    }
    let _ = handle.emit("update-badge", available);
}

/// 开启或关闭代理，并相应更新菜单文本。
async fn toggle_proxy_from_tray(app: &AppHandle) {
    let runtime = app.state::<crate::proxy::ProxyRuntime>();
    let info = crate::proxy::get_proxy_info(app.clone(), runtime.clone())
        .await
        .unwrap_or_else(|_| crate::proxy::ProxyInfo {
            running: false,
            port: settings::current(app).lan_proxy_port,
            lan_ip: None,
            url: None,
        });

    let now_running = if info.running {
        let _ = crate::proxy::stop_proxy(app.clone(), runtime.clone()).await;
        false
    } else {
        match crate::proxy::start_proxy(app.clone(), runtime.clone()).await {
            Ok(new_info) => {
                if let Some(base) = new_info.url {
                    let token = settings::current(app).lan_proxy_token;
                    let _ = crate::notify(
                        "局域网代理已启用",
                        &format!("访问地址：{base}<令牌>\n令牌：{token}"),
                    );
                }
                true
            }
            Err(e) => {
                let _ = crate::notify("启用局域网代理失败", &e);
                false
            }
        }
    };
    set_proxy_label(app, now_running);
}

/// 更新代理菜单项的文本。
fn set_proxy_label(app: &AppHandle, running: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        if let Some(item) = items.proxy.lock().unwrap().as_ref() {
            let _ = item.set_text(if running {
                "关闭局域网代理"
            } else {
                "启用局域网代理"
            });
        }
    }
}
