use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Emitter, Manager};

use crate::APP_HANDLE;

const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 点击窗口关闭按钮时最小化到托盘而非退出程序。
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    /// 应用启动时自动运行 dsh web 服务。
    #[serde(default = "default_true")]
    pub auto_start_dsh: bool,
    /// 定期查询 npm 仓库以检查是否有更新的 dsh 版本。
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
    /// 检查更新的轮询间隔（小时）。
    #[serde(default = "default_interval")]
    pub update_check_interval_hours: u32,
    /// 局域网代理是否处于启用状态（默认关闭）。
    #[serde(default)]
    pub lan_proxy_enabled: bool,
    /// 局域网代理监听的端口。
    #[serde(default = "default_proxy_port")]
    pub lan_proxy_port: u16,
    /// 是否已展示过“防火墙放行”提示（每个安装生命周期只提示一次）。
    #[serde(default)]
    pub firewall_hint_shown: bool,
    /// Windows 登录时自动启动本应用（HKCU Run 注册表项，随设置开启/关闭）。
    #[serde(default)]
    pub auto_launch_app: bool,
    /// 从局域网访问代理所需的共享密钥。
    #[serde(default)]
    pub lan_proxy_token: String,
    /// 可选：自定义 dsh 入口的绝对路径（node 脚本或 dsh 可执行文件）。
    #[serde(default)]
    pub dsh_custom_path: Option<String>,
    /// dsh web 服务绑定的主机地址（始终为环回地址）。
    #[serde(default = "default_dsh_host")]
    pub dsh_host: String,
    /// dsh web 服务监听的端口。
    #[serde(default = "default_dsh_port")]
    pub dsh_port: u16,
}

fn default_true() -> bool {
    true
}
fn default_interval() -> u32 {
    24
}
fn default_proxy_port() -> u16 {
    3081
}
fn default_dsh_host() -> String {
    "127.0.0.1".to_string()
}
fn default_dsh_port() -> u16 {
    3080
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            minimize_to_tray: true,
            auto_start_dsh: true,
            auto_check_updates: true,
            update_check_interval_hours: 24,
            lan_proxy_enabled: false,
            lan_proxy_port: 3081,
            firewall_hint_shown: false,
            auto_launch_app: false,
            lan_proxy_token: String::new(),
            dsh_custom_path: None,
            dsh_host: default_dsh_host(),
            dsh_port: default_dsh_port(),
        }
    }
}

impl Settings {
    /// 解析 settings.json 在用户配置目录中的位置。
    pub fn config_dir() -> PathBuf {
        let base = directories::ProjectDirs::from("com", "deepseekai", "DeepSeek Harness")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        base
    }

    pub fn settings_path() -> PathBuf {
        Self::config_dir().join(SETTINGS_FILE)
    }

    /// 从磁盘加载设置；若文件缺失则使用默认值创建，
    /// 并确保代理访问令牌已生成。
    pub fn load() -> io::Result<Self> {
        let path = Self::settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let raw = fs::read_to_string(&path)?;
            let mut s: Settings = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    // 解析失败：保留备份再回退默认值，避免用户自定义路径、令牌无迹可寻地丢失。
                    let backup = path.with_extension("json.bak");
                    let _ = fs::copy(&path, &backup);
                    log::error!(
                        "settings.json 解析失败（{e}），已备份至 {} 并使用默认设置",
                        backup.display()
                    );
                    Settings::default()
                }
            };
            if s.lan_proxy_token.is_empty() {
                s.lan_proxy_token = generate_token();
                let _ = s.save();
            }
            Ok(s)
        } else {
            let mut s = Settings::default();
            s.lan_proxy_token = generate_token();
            let _ = s.save();
            Ok(s)
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }
}

/// 存放在 Tauri 受管状态中的可共享、可变设置。
pub struct SettingsState(pub Mutex<Settings>);

/// 生成密码学安全的 32 字节十六进制令牌。
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 持久化当前设置，并向前端推送事件以便用新值重新渲染。
pub fn persist_and_sync(settings: &Settings) {
    if let Err(e) = settings.save() {
        log::error!("保存设置失败：{e}");
    }
    if let Some(handle) = APP_HANDLE.get() {
        let _ = handle.emit("settings-changed", settings.clone());
    }
}

/// 便捷访问器：在锁的保护下获取当前设置的副本。
pub fn current(handle: &tauri::AppHandle) -> Settings {
    let state = handle.state::<SettingsState>();
    let guard = state.0.lock().expect("设置锁已中毒");
    guard.clone()
}
