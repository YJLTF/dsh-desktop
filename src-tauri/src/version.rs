use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::process::{self, DshState, DshVersion, Resolution};
use crate::settings;
use crate::APP_HANDLE;

const REGISTRY_LATEST: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh/latest";
const PKG_SPEC: &str = "@deepseek-ai/dsh@latest";

/// 一键安装 / 一键更新共用的防重入标志：两者都是分钟级的 `npm install -g`，
/// 并发执行会互相破坏（npm 全局目录锁、进度文案错乱）。
static UPDATING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
struct NpmLatest {
    version: String,
}

/// 通过运行 `<入口> --version` 获取已安装的 dsh 版本号。
pub(crate) async fn installed_version(resolution: &Resolution) -> Option<String> {
    let (program, args): (String, Vec<String>) = match resolution {
        Resolution::Node { node, script, .. } => (node.clone(), vec![script.clone(), "--version".into()]),
        Resolution::Executable { exe, .. } => (exe.clone(), vec!["--version".into()]),
        Resolution::NotFound { .. } => return None,
    };

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).stdin(std::process::Stdio::null());
    // 安装版为 GUI 子系统，运行 `dsh --version` 也会弹出 node.exe 终端窗口，需显式隐藏。
    #[cfg(windows)]
    cmd.creation_flags(process::CREATE_NO_WINDOW);
    let output = cmd.output().await.ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let v = text.trim().split_whitespace().next()?;
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// 兜底方案：直接从解析得到的 package.json 中读取版本号。
pub(crate) fn installed_version_from_pkg(resolution: &Resolution) -> Option<String> {
    let script = match resolution {
        Resolution::Node { script, .. } => PathBuf::from(script),
        _ => return None,
    };
    // bin.js 位于 <包>/lib/bin.js，package.json 在其上两级目录。
    let pkg_json = script.parent()?.parent()?.join("package.json");
    let raw = std::fs::read_to_string(&pkg_json).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("version")?.as_str().map(|s| s.to_string())
}

/// 查询 npm 仓库以获取最新发布的版本号。
async fn latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let resp: NpmLatest = client.get(REGISTRY_LATEST).send().await.ok()?.json().await.ok()?;
    Some(resp.version)
}

/// 使用 semver 比较两个版本号字符串，并正确处理预发布标签。
fn is_newer(latest: &str, installed: &str) -> bool {
    match (semver::Version::parse(latest), semver::Version::parse(installed)) {
        (Ok(l), Ok(i)) => l > i,
        _ => latest != installed,
    }
}

/// 汇总已安装版本、最新版本，以及是否存在可用更新。
pub async fn gather(handle: &AppHandle) -> DshVersion {
    let settings = settings::current(handle);
    let resolution = process::resolve_dsh(&settings.dsh_custom_path).await;

    let installed = installed_version(&resolution)
        .await
        .or_else(|| installed_version_from_pkg(&resolution));

    let latest = latest_version().await;

    let update_available = match (&installed, &latest) {
        (Some(i), Some(l)) => is_newer(l, i),
        _ => false,
    };

    DshVersion {
        installed,
        latest,
        update_available,
    }
}

/// Tauri 命令：执行一次按需检查并返回结果。
#[tauri::command]
pub async fn check_for_updates(handle: AppHandle) -> Result<DshVersion, String> {
    let info = gather(&handle).await;
    let _ = handle.emit("version-info", info.clone());
    // 无条件按最新结果设置徽标：升级后发现“已是最新”也要把徽标切回普通图标。
    if let Some(main) = APP_HANDLE.get() {
        crate::tray::set_update_badge(main, info.update_available);
    }
    Ok(info)
}

/// 推送更新进度到前端（message 直接用作按钮文案）。
fn emit_progress(handle: &AppHandle, message: &str) {
    #[derive(Clone, Serialize)]
    struct UpdateProgress<'a> {
        message: &'a str,
    }
    let _ = handle.emit("update-progress", UpdateProgress { message });
}

/// 一键更新：`npm install -g @deepseek-ai/dsh@latest`。
/// 仅作用于全局安装来源；自定义路径 / 本地 node_modules 无法安全地自动替换
/// （安装目录可能不可写，或根本不在 npm 管辖内），返回错误提示手动升级。
#[tauri::command]
pub async fn update_dsh(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<DshVersion, String> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return Err("已有安装或更新任务正在进行，请稍候".into());
    }
    let result = run_update(&handle, &state).await;
    UPDATING.store(false, Ordering::SeqCst);
    result
}

/// 提取 npm stderr 的末尾片段（最多 200 字符）拼进错误信息——npm 报错的关键行在尾部。
fn stderr_tail(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let tail: String = text.chars().rev().take(200).collect();
    tail.chars().rev().collect()
}

/// 执行 `npm install -g @deepseek-ai/dsh@latest`（10 分钟超时）。
/// 供一键更新与一键安装共用；进度文案由调用方负责推送。
async fn npm_install_global(npm: &std::path::Path) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(npm);
    cmd.args(["install", "-g", PKG_SPEC])
        .stdin(std::process::Stdio::null());
    // GUI 子系统拉起 npm.cmd 必须隐藏终端窗口。
    #[cfg(windows)]
    cmd.creation_flags(process::CREATE_NO_WINDOW);

    let output = tokio::time::timeout(Duration::from_secs(600), cmd.output())
        .await
        .map_err(|_| "npm 安装超时（10 分钟），请检查网络后重试".to_string())?
        .map_err(|e| format!("执行 npm 失败：{e}"))?;

    if !output.status.success() {
        return Err(format!("npm 安装失败：{}", stderr_tail(&output)));
    }
    Ok(())
}

async fn run_update(
    handle: &AppHandle,
    state: &tauri::State<'_, DshState>,
) -> Result<DshVersion, String> {
    let settings = settings::current(handle);
    let resolution = process::resolve_dsh(&settings.dsh_custom_path).await;

    match &resolution {
        Resolution::Node { source, .. } | Resolution::Executable { source, .. } => {
            if source != "全局安装" {
                return Err(format!(
                    "当前 dsh 来源为“{source}”，一键更新仅支持全局安装的 dsh，请手动升级"
                ));
            }
        }
        Resolution::NotFound { message } => return Err(message.clone()),
    }

    let npm = process::npm_exe().ok_or(
        "未找到 npm，无法自动更新。请安装 Node.js 后重试，或手动运行 npm install -g @deepseek-ai/dsh",
    )?;

    // 更新期间停止运行中的 dsh：既避免文件替换冲突，也确保重启后加载的是新版本。
    let was_running = state.is_running();
    if was_running {
        emit_progress(handle, "正在停止 dsh…");
        let _ = process::stop_dsh(handle.clone(), state.clone()).await;
    }

    emit_progress(handle, "正在下载并安装新版本…");
    let installed = npm_install_global(&npm).await;
    if installed.is_err() && was_running {
        // npm 安装失败时旧包通常完好，尽量恢复 dsh 运行。
        emit_progress(handle, "安装失败，正在恢复 dsh…");
        let _ = process::start_dsh(handle.clone(), state.clone()).await;
    }
    installed?;

    if was_running {
        emit_progress(handle, "正在以新版本重启 dsh…");
        process::start_dsh(handle.clone(), state.clone()).await.map_err(|e| {
            // 新版本已装好，只是重启失败——不算更新失败，但要明确告知。
            format!("dsh 已更新到最新版，但重启失败：{e}")
        })?;
    }

    // 重新汇总版本并刷新横幅、托盘徽标。
    let info = gather(handle).await;
    let _ = handle.emit("version-info", info.clone());
    if let Some(main) = APP_HANDLE.get() {
        crate::tray::set_update_badge(main, info.update_available);
    }
    Ok(info)
}

/// 一键安装前的环境检查，返回可用的 npm 路径。dsh 经 npm 分发，
/// 机器上必须先有 node / npm；缺失时给出可操作的指引而非笼统报错
/// （node/npm 查找是纯文件系统探测，同步执行即可）。
fn resolve_npm_for_install() -> Result<std::path::PathBuf, String> {
    if process::node_exe().is_none() {
        return Err(
            "未找到 Node.js。dsh 通过 npm 分发，请先安装 Node.js（https://nodejs.org），完成后回到本面板重试".into(),
        );
    }
    process::npm_exe().ok_or_else(|| {
        "未找到 npm。请确认 Node.js 已完整安装（含 npm）后重试，或手动运行 npm install -g @deepseek-ai/dsh".into()
    })
}

/// 一键安装：机器上找不到 dsh 时执行 `npm install -g @deepseek-ai/dsh@latest`，
/// 成功后刷新版本信息与托盘徽标；若启用了“启动时自动运行 dsh”则随即拉起服务。
#[tauri::command]
pub async fn install_dsh(
    handle: AppHandle,
    state: tauri::State<'_, DshState>,
) -> Result<DshVersion, String> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return Err("已有安装或更新任务正在进行，请稍候".into());
    }
    let result = run_install(&handle, &state).await;
    UPDATING.store(false, Ordering::SeqCst);
    result
}

async fn run_install(
    handle: &AppHandle,
    state: &tauri::State<'_, DshState>,
) -> Result<DshVersion, String> {
    let settings = settings::current(handle);

    // 幂等保护：已能解析到 dsh 就不再跑 npm（用户可能在点击前配置了自定义路径、
    // 或环境刚被外部装好），直接按“已是当前状态”返回当前信息供前端收起横幅。
    if !matches!(
        process::resolve_dsh(&settings.dsh_custom_path).await,
        Resolution::NotFound { .. }
    ) {
        let info = gather(handle).await;
        let _ = handle.emit("version-info", info.clone());
        return Ok(info);
    }

    let npm = resolve_npm_for_install()?;

    emit_progress(handle, "正在下载并安装 dsh…");
    npm_install_global(&npm).await?;

    emit_progress(handle, "正在确认安装结果…");
    let info = gather(handle).await;
    if info.installed.is_none() {
        // npm 报成功却仍解析不到：多为全局 prefix 指向非常规目录等环境问题，
        // 如实告知比让横幅永远挂着更好。
        return Err(
            "npm 已执行成功，但仍未定位到已安装的 dsh；请重启应用重试，或检查 npm 全局目录（npm root -g）".into(),
        );
    }
    let _ = handle.emit("version-info", info.clone());
    if let Some(main) = APP_HANDLE.get() {
        crate::tray::set_update_badge(main, info.update_available);
    }

    // 与开机自启同一开关语义：允许自动运行则装完立即拉起。启动失败不算安装失败
    // （包已就位），记日志即可，面板的未运行状态会引导用户手动启动看具体错误。
    if settings.auto_start_dsh {
        emit_progress(handle, "正在启动 dsh…");
        if let Err(e) = process::start_dsh(handle.clone(), state.clone()).await {
            log::warn!("安装后自动启动 dsh 失败：{e}");
        }
    }
    Ok(info)
}

/// 启动后台任务，在启用时定期检查更新。
pub fn spawn_periodic_check() {
    tauri::async_runtime::spawn(async move {
        loop {
            let handle = match APP_HANDLE.get() {
                Some(h) => h,
                None => break,
            };
            let settings = settings::current(handle);
            if settings.auto_check_updates {
                let info = gather(handle).await;
                let _ = handle.emit("version-info", info.clone());
                crate::tray::set_update_badge(handle, info.update_available);
                if info.update_available {
                    let _ = crate::notify(
                        "发现 DSH 新版本",
                        &format!(
                            "dsh {} 已发布（当前版本 {}）。打开控制面板，点击“立即更新”即可升级。",
                            info.latest.as_deref().unwrap_or("?"),
                            info.installed.as_deref().unwrap_or("?")
                        ),
                    );
                }
            }
            let hours = settings.update_check_interval_hours.max(1) as u64;
            tokio::time::sleep(Duration::from_secs(60 * 60 * hours)).await;
        }
    });
}
