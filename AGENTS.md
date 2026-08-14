# AGENTS.md

DeepSeek Harness 桌面客户端的构建 / 测试命令。

## 前端（TypeScript + Vite）
- 类型检查：`npx tsc --noEmit`
- 开发服务器：`npm run dev`
- 生产构建：`npm run build`

## 桌面端（Tauri 2 + Rust）
- Rust 快速检查：`cargo check --manifest-path src-tauri/Cargo.toml`
- 开发模式（启动窗口）：`npx tauri dev`
- 发布打包（生成 MSI + NSIS 安装程序）：`npx tauri build`
  - 产物位置：`src-tauri/target/release/bundle/{msi,nsis}/`

## 架构说明
- `src/` —— 控制面板界面（原生 TypeScript），通过 `@tauri-apps/api/core` 的 `invoke` 调用后端命令。
- `src-tauri/src/` —— Rust 后端：
  - `lib.rs` —— 应用引导、Tauri 命令、受管状态、全局 `APP_HANDLE`。
  - `process.rs` —— 发现并拉起 `@deepseek-ai/dsh`（`dsh web`）、健康探测。
  - `proxy.rs` —— 局域网反向代理（axum）：HTTP + WebSocket 双向隧道、令牌鉴权（cookie / 查询参数）。
  - `version.rs` —— npm 仓库版本检查（`semver` 比较）、定时与按需检查。
  - `tray.rs` —— 系统托盘图标及右键菜单、更新徽标图标切换。
  - `settings.rs` —— 存放于用户配置目录的 JSON 配置（`settings::SettingsState`）。

## 运行时依赖
- Node.js 18+（通过官方 npm 包 `@deepseek-ai/dsh` 拉起 `dsh`，需在 PATH 中可被找到）。
- WebView2 运行时（Windows 10/11；缺失时安装程序会自动引导下载安装）。
