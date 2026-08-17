//! Windows 开机自启动（HKCU Run 注册表项）。
//! 通过 reg.exe 实现，避免为此引入额外 crate；仅写当前用户注册表，无需管理员权限。

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use crate::process::CREATE_NO_WINDOW;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "DeepSeekHarness";

/// 查询自启动是否已启用。
pub fn is_enabled() -> bool {
    let Ok(out) = reg(&["query", RUN_KEY, "/v", VALUE_NAME]) else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).to_lowercase().contains(VALUE_NAME.to_lowercase().as_str())
}

/// 开启或关闭自启动；返回需要向用户提示的错误信息。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled == is_enabled() {
        return Ok(());
    }
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("获取应用路径失败：{e}"))?;
        let value = format!("\"{}\"", exe.display());
        reg(&[
            "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
        ])
        .map(|_| ())
        .map_err(|e| format!("写入自启动注册表失败：{e}"))
    } else {
        // 值不存在时 delete 会报错，is_enabled 已确保存在才走到这里。
        reg(&["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
            .map(|_| ())
            .map_err(|e| format!("移除自启动注册表失败：{e}"))
    }
}

/// 按当前设置同步注册表（应用启动时调用，纠正用户手动改注册表造成的不一致）。
pub fn sync_from_settings(settings: &crate::settings::Settings) {
    if let Err(e) = set_enabled(settings.auto_launch_app) {
        log::warn!("同步开机自启动设置失败：{e}");
    }
}

fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new("reg");
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.output()
}
