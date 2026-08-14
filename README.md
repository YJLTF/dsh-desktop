# DeepSeek Harness 桌面客户端

[@deepseek-ai/dsh](https://www.npmjs.com/package/@deepseek-ai/dsh)（DeepSeek Harness）的轻量 Windows 桌面客户端：系统托盘驻留、一键拉起 `dsh web` 服务、版本升级检测与局域网共享代理。

基于 [Tauri 2](https://v2.tauri.app/) 构建，前端为原生 TypeScript（Vite），后端为 Rust（axum 反向代理）。

## 功能

- **dsh 生命周期管理** —— 自动发现已安装的 `@deepseek-ai/dsh`（本地 / 全局 npm 安装），拉起 `dsh web` 子进程并做健康探测；支持启动 / 停止 / 重启，异常退出自动检测
- **系统托盘驻留** —— 关闭窗口即最小化到托盘，托盘菜单可打开 Harness 界面、复制局域网链接、退出（退出时自动回收 dsh 子进程，不遗留孤儿进程）
- **白屏防护** —— 打开 Harness 窗口前等待服务就绪；dsh 重启空窗期内不会导航到错误页
- **版本升级检测** —— 定期对比 npm 仓库最新版本，有更新时托盘图标加徽标并在面板横幅提示
- **局域网共享代理** —— 内置 axum 反向代理（HTTP + WebSocket 双向隧道），以访问令牌鉴权，可将本机 dsh 安全共享给局域网内其他设备；默认关闭
- **自定义 dsh 入口** —— 支持通过文件选择窗指定自定义的 `bin.js` 脚本或可执行文件，路径无效时面板会显式警告

## 环境要求

- [Node.js](https://nodejs.org/) 18+（`node` / `npm` 需在 PATH 中；dsh 通过官方 npm 包运行）
- [Rust](https://www.rust-lang.org/) 1.77+（仅构建时需要）
- WebView2 运行时（Windows 10/11 通常已内置；缺失时安装程序会自动引导下载安装）

## 开发

```bash
npm install        # 安装前端依赖
npm run dev        # 仅启动前端 Vite 开发服务器
npx tauri dev      # 启动桌面端开发模式（自动带起前端）
```

类型检查：

```bash
npx tsc --noEmit
cargo check --manifest-path src-tauri/Cargo.toml
```

## 构建

```bash
npx tauri build
```

产物位于 `src-tauri/target/release/bundle/`：

| 格式 | 路径 |
|------|------|
| MSI | `msi/DeepSeek Harness_<版本>_x64_zh-CN.msi` |
| NSIS | `nsis/DeepSeek Harness_<版本>_x64-setup.exe` |

MSI 文件名末尾的 `zh-CN` 是安装器界面语言（`bundle.windows.wix.language`），非系统区域问题。

## 架构

```
src/                    控制面板前端（原生 TypeScript）
  main.ts                 面板 UI 与 Tauri 命令调用
src-tauri/src/          Rust 后端
  lib.rs                  应用引导、Tauri 命令、受管状态、全局句柄
  process.rs              dsh 发现 / 拉起 / 健康探测 / 端口清理
  proxy.rs                局域网反向代理（axum）：HTTP + WebSocket 隧道、令牌鉴权
  version.rs              npm 仓库版本检查（semver 比较）、定时轮询
  tray.rs                 系统托盘图标与菜单
  settings.rs             JSON 配置（持久化于用户配置目录）
```

窗口结构：

- `control-panel` —— 控制面板（主窗口，关闭即最小化到托盘）
- `harness-ui` —— 加载 `dsh web` 界面（`http://127.0.0.1:3080`）的专用窗口

## 配置

运行时配置存放于用户配置目录：

```
%APPDATA%\deepseekai\DeepSeek Harness\config\settings.json
```

包含：开机自启 dsh、托盘最小化、更新检查间隔、局域网代理开关 / 端口 / 访问令牌、自定义 dsh 路径等，均可在控制面板中修改。

## 局域网访问

1. 控制面板 → 局域网代理 → 启用
2. 托盘菜单 → 复制局域网访问链接（含令牌）
3. 局域网设备浏览器打开链接即可访问 dsh；令牌泄露时可随时在面板中重置
