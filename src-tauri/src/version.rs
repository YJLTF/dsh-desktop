use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::process::{self, DshVersion, Resolution};
use crate::settings;
use crate::APP_HANDLE;

const REGISTRY_LATEST: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh/latest";

#[derive(Debug, Deserialize)]
struct NpmLatest {
    version: String,
}

/// 通过运行 `<入口> --version` 获取已安装的 dsh 版本号。
async fn installed_version(resolution: &Resolution) -> Option<String> {
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
fn installed_version_from_pkg(resolution: &Resolution) -> Option<String> {
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
    if info.update_available {
        if let Some(main) = APP_HANDLE.get() {
            crate::tray::set_update_badge(main, true);
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
                if info.update_available {
                    crate::tray::set_update_badge(handle, true);
                    let _ = crate::notify(
                        "发现 DeepSeek Harness 新版本",
                        &format!(
                            "dsh {} 已发布（当前版本 {}）。升级命令：npm install -g @deepseek-ai/dsh",
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
