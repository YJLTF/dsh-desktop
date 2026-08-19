# AGENTS.md

DSH Desktop（dsh-desktop）的开发指南与注意事项。功能特性见 [README.md](README.md)。

## 构建 / 测试命令

### 前端（TypeScript + Vite）
- 类型检查：`npx tsc --noEmit`
- 开发服务器：`npm run dev`
- 生产构建：`npm run build`

### 桌面端（Tauri 2 + Rust）
- Rust 快速检查：`cargo check --manifest-path src-tauri/Cargo.toml`（会同步 Cargo.lock 版本）
- 开发模式（启动窗口）：`npx tauri dev`
- 发布打包（生成 MSI + NSIS 安装程序）：`npx tauri build`
  - 产物位置：`src-tauri/target/release/bundle/{msi,nsis}/`

## 架构说明
- `src/` —— 控制面板界面（原生 TypeScript），通过 `@tauri-apps/api/core` 的 `invoke` 调用后端命令。
- `src-tauri/src/` —— Rust 后端：
  - `lib.rs` —— 应用引导、Tauri 命令、受管状态、全局 `APP_HANDLE`、Harness 窗口管理。
  - `process.rs` —— 发现并拉起 `@deepseek-ai/dsh`（`dsh web`）、健康探测、遗留进程清理。
  - `proxy.rs` —— 局域网反向代理（axum）：HTTP + WebSocket 双向隧道、令牌鉴权（cookie / 查询参数）、端口回退。
  - `version.rs` —— npm 仓库版本检查（`semver` 比较）、定时与按需检查、托盘徽标联动。
  - `tray.rs` —— 系统托盘图标及右键菜单、更新徽标图标切换。
  - `settings.rs` —— 存放于用户配置目录的 JSON 配置（`settings::SettingsState`），损坏时备份 `.bak` 回退默认。
  - `autostart.rs` —— 开机自启动（HKCU Run 注册表，经 `reg.exe` 实现，无额外 crate 依赖）。
- `src-tauri/nsis/installer-hooks.nsh` —— NSIS 安装钩子：扫描注册表静默卸载旧版 DeepSeek Harness（NSIS / MSI 安装均可识别）。
- `src-tauri/msi/legacy-upgrade.wxs` —— WiX 片段：Upgrade 表登记旧版 UpgradeCode，MSI 安装时自动卸载旧产品。

## 开发注意事项

### 旧版（DeepSeek Harness）迁移
- **productName 变更会导致两套安装器都把新版视为全新应用**（NSIS 卸载键、MSI UpgradeCode 均由产品名派生），旧版卸载迁移依赖三处：
  1. NSIS 钩子（`nsis/installer-hooks.nsh`）按 `DisplayName = "DeepSeek Harness"` 扫描 HKCU/HKLM（64/32 位视图），MSI 走 `msiexec /x {GUID} /qn`（失败退交互式），NSIS 走 `uninstall.exe /S _?=目录`；
  2. WiX 片段（`msi/legacy-upgrade.wxs`）登记旧 UpgradeCode = `uuid v5(DNS, "DeepSeek Harness.exe.app.x64")` = `{3F992021-DF3D-5157-93F7-F1AA21DE57E7}`，靠主模板 `MajorUpgrade` 调度的 `RemoveExistingProducts` 生效；占位组件 `LegacyCleanupGroup` 仅用于把片段链接进 Product，勿删；
  3. `settings.rs::migrate_legacy_settings` 迁移用户配置。
- **MSI 升级路径的孤儿清理靠片段中的 `CleanupLegacy` VBScript 动作**（deferred、`Impersonate="no"` 即 SYSTEM 上下文，排在 `InstallFiles` 后），覆盖：
  - 旧 MSI 升级卸载时主模板跳过 `RemoveShortcuts`（`Installed AND NOT UPGRADINGPRODUCTCODE` 条件，为同名升级保留快捷方式）遗留的孤儿：安装目录内 Uninstall 快捷方式、开始菜单（文件夹与根级 .lnk 两种形态）、公共桌面快捷方式；
  - 旧 **NSIS** 安装（Upgrade 表只认 Windows Installer 产品，管不到它）：枚举各用户单元的卸载键静默拉起 `uninstall.exe /S _?=目录`，再清残留；
  - 各用户自启值与厂商键：**SYSTEM 上下文的 HKCU 是 SYSTEM 自己的单元**，必须枚举 `HKEY_USERS\<SID>` 逐单元清理，用户快捷方式经 `ProfileList\ProfileImagePath` 定位。
- **NSIS `_?=` 的值绝不能加引号**（即使路径含空格）：NSIS 取命令行剩余部分作 `$INSTDIR`，引号会混入路径导致旧卸载器内所有删除操作静默失败（曾引发“只删了 uninstall.exe”的实测问题）。钩子内也不可引用 `$PassiveMode` 等主脚本 `Var`——钩子在 `Var` 声明之前被 include，编译期不可见（unknown variable 警告），需用 `${Silent}` 等编译期标志替代。
- 旧卸载器静默模式下不清理：开机自启值 `DeepSeekHarness`（autostart.rs 实际值名，与卸载器按产品名清理的 `DeepSeek Harness` 不一致）与厂商键 `HKCU\Software\deepseekai\DeepSeek Harness`（仅勾选“删除应用数据”才清），均由钩子兜底清扫。
- **旧版自定义安装目录靠 `CaptureLegacyDir`（immediate，`Before="InstallInitialize"`）捕获**：记录旧目录的注册表值（HKCU 厂商键 InstallDir / HKLM 卸载键 InstallLocation）会被 `RemoveExistingProducts` 随旧产品一起删除，deferred 阶段已读不到。捕获结果写入 `Session.Property("CleanupLegacy")`（同名 deferred 动作的 CustomActionData）传递。
- **安装目录清理是安全语义**：只删已知残留文件（`Uninstall DeepSeek Harness.lnk`、`uninstall.exe`、旧主程序 exe），目录仅在其后为空时删除，且 `Len < 4` 根目录保护——用户可能把应用装在自选目录甚至盘符根附近，绝不能整目录强删。父目录（用户手工创建的层级）也不向上清理。
- 再次更名产品时需同步更新以上三处（升级代码算法：`uuid v5(DNS, "<新名>.exe.app.x64")`）。

### 版本号发布
- 版本号出现在 **5 处**，发布时需同步修改：`package.json`、`package-lock.json`（两处）、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`；`src-tauri/Cargo.lock` 由 `cargo check` 自动同步。安装包版本与面板“桌面端 v”显示均取自 `tauri.conf.json`。

### Windows 特有行为
- **子进程必须加 `CREATE_NO_WINDOW`**（`process::CREATE_NO_WINDOW`）：安装版为 GUI 子系统，拉起 node.exe / npm.cmd / reg.exe / netstat 等控制台程序若不加该标志，每次调用都会弹出终端窗口。新增任何 `Command` 调用时勿遗漏。
- **还原窗口须先 `unminimize()` 再 `show()`**：Windows 上对最小化窗口 `show()` 是空操作。所有唤起窗口的路径（托盘菜单、二次启动 single-instance）都已遵循；新增唤起路径时同样处理。
- **npm 查找顺序**：node 同目录 `npm.cmd` → PATH → `%APPDATA%\npm\npm.cmd`；npm 完全不可用时全局目录回退 `%APPDATA%\npm\node_modules`。默认路径检查以 `npm root -g` 动态解析为准（跟随用户自定义 prefix），不要写死绝对路径。
- **`npm root -g` 必须在 `spawn_blocking` 中执行**：npm 冷启动可达秒级，直接在 async 上下文调用会阻塞 tokio 工作线程。

### 代理模块（proxy.rs）
- **启停必须持 `ProxyRuntime::op_lock`**：开机自动恢复与用户手动开关可能并发，分段锁（shutdown/join）会造成双绑定。新增修改代理生命周期的命令时先取该锁。
- **端口可能回退**：Win10 上 Hyper-V/WSL/VPN 会随机保留端口段（bind 报 WSAEACCES 10013），`bind_listener` 失败重试后回退系统分配端口。因此**实际监听端口以 `ProxyRuntime::port` 记录为准**，`get_proxy_info` 已按此回报；不要假设监听端口等于 `settings.lan_proxy_port`。
- **令牌是启动时克隆进 `ProxyState` 的**：修改令牌（`regenerate_token`）必须重启运行中的代理才生效，否则新令牌被 401、旧令牌继续可用。调整令牌相关逻辑时保持这一联动。
- **请求体流式透传**：`forward_http` 用 `BodyStream` + `wrap_stream` 转发请求体（GET/HEAD 跳过），不要改回 `collect().to_bytes()` 整包缓冲——大附件上传时内存峰值会与请求体等大。
- **`lan_ip()` 已带 10 秒缓存**（含失败结果缓存），可放心在命令中调用；探测链为多目标 UDP connect → 主机名解析兜底。

### 设置与前端
- **端口范围双重校验**：前端 `clampPort` / `clampHours`（1024–65535 / 1–168）与后端 `update_settings` 校验必须同时维护——越界值会让 serde 对 `u16` 反序列化失败，导致**所有**设置都保存不了。
- **外部数据进 innerHTML 前必须 `esc()` 转义**（版本号等来自 npm registry）。新增模板拼接时遵循。
- **前端 init 逐项容错**：单项 `invoke` 失败不应中断初始化（否则事件监听挂不上、页面假死）；新增初始化调用保持 try/catch 包裹。
- **`Settings` 结构变更**：新字段必须加 `#[serde(default ...)]` 并同步 `Default` 实现、前端 `Settings` 接口与 `collectSettings`，否则旧配置文件反序列化失败会触发 `.bak` 备份回退。

### dsh 一键更新（version.rs::update_dsh）
- **仅支持"全局安装"来源**：自定义路径 / 本地 node_modules（安装目录可能不可写，或不在 npm 管辖内）会拒绝并提示手动升级，判定依据是 `Resolution.source == "全局安装"`。
- **更新流程**：停 dsh（若运行中）→ `npm install -g @deepseek-ai/dsh@latest`（CREATE_NO_WINDOW、10 分钟超时）→ 恢复运行 → `gather()` 刷新横幅与托盘徽标；npm 失败时旧包通常完好，尽量恢复旧版运行后再报错。
- **防重入**：`UPDATING` AtomicBool（npm 安装可达分钟级）；阶段进度经 `update-progress` 事件推送，message 直接用作前端按钮文案。

### 其他
- **CSP 已收紧**（`tauri.conf.json`）：新增需要外部连接/内联脚本的功能时须同步调整 `security.csp`。
- **退出清理**：`RunEvent::Exit` 中调用 `DshState::shutdown_blocking()` 结束 dsh 子进程；`app.exit()` 不执行受管状态析构，删掉该钩子会遗留孤儿 node 进程占用端口。
- **遗留进程清理只杀白名单映像**（node / 解析出的自定义可执行文件名），避免误杀无关服务；扩展启动方式时同步更新 `kill_if_stale`。
- **dsh 重启后刷新 Harness 窗口**：`start_dsh` 成功且 `harness-ui` 指向本服务时会 `location.reload()`，旧页面的 WebSocket / 会话已失效，不要移除该刷新。
- **托盘左键行为依赖 dsh 运行状态**（运行中开 Harness / 未运行唤起控制面板），通过 `DshState::is_running()` 同步判断。
