# DeepSeek Harness 桌面客户端

[@deepseek-ai/dsh](https://www.npmjs.com/package/@deepseek-ai/dsh)（DeepSeek Harness）的轻量 Windows 桌面客户端：系统托盘驻留、一键拉起 `dsh web` 服务、版本升级检测与局域网共享代理。

基于 [Tauri 2](https://v2.tauri.app/) 构建，前端为原生 TypeScript（Vite），后端为 Rust（axum 反向代理）。

## 功能特性

### dsh 服务管理
- **自动发现与拉起** —— 自动发现已安装的 `@deepseek-ai/dsh`（自定义路径 → 本地 node_modules → `npm root -g` 全局安装，三级查找），拉起 `dsh web` 子进程并做健康探测；支持启动 / 停止 / 重启，异常退出自动检测
- **版本与来源展示** —— 面板实时显示已安装的 dsh 版本号与入口来源（全局安装 / 本地安装 / 自定义路径）
- **自定义 dsh 入口** —— 支持通过文件选择窗指定自定义的 `bin.js` 脚本或可执行文件，路径无效时面板显式警告
- **孤儿进程清理** —— 上次会话被强杀残留的 dsh 进程会在下次启动前自动清理，避免端口冲突导致启动失败；退出应用时自动回收子进程
- **白屏防护** —— 打开 Harness 窗口前等待服务就绪；dsh 重启空窗期内不会导航到错误页，重启成功后已打开的 Harness 窗口自动刷新

### 系统托盘
- **托盘驻留** —— 关闭窗口即最小化到托盘；打开 Harness 界面后控制面板自动隐藏到托盘，屏幕只保留 Harness 窗口
- **托盘菜单** —— 打开 Harness 界面、显示控制面板、开关局域网代理、复制局域网链接、检查更新、退出
- **智能左键** —— dsh 运行中左键托盘直接打开 Harness；未运行时左键唤起控制面板
- **更新徽标** —— 检测到新版本时托盘图标切换为徽标样式，升级完成检查后自动复原

### 局域网共享代理
- **反向代理** —— 内置 axum 反向代理（HTTP + WebSocket 双向隧道、请求体流式透传），可将本机 dsh 共享给局域网内其他设备；默认关闭
- **令牌鉴权** —— 以 32 字节随机令牌鉴权（cookie / 查询参数），支持 URL 携带令牌访问后自动下发 cookie；令牌可随时重置且立即生效（运行中的代理自动应用新令牌）
- **Win10 端口兼容** —— 配置端口被系统保留（Hyper-V / WSL2 / VPN 常见的保留端口段，绑定报 10013）时自动重试并回退到系统分配端口，面板提示实际监听端口
- **局域网 IP 识别** —— 多级探测本机局域网 IPv4（默认路由 UDP 探测 + 主机名解析兜底），纯内网环境也能给出可访问地址；识别失败时面板提示手动访问格式
- **防火墙提醒** —— 首次启用代理时提醒放行 Windows 防火墙端口，避免拒绝授权后局域网访问静默失败

### 设置
- 关闭窗口最小化到托盘 / 启动时自动运行 dsh / 自动检查 dsh 更新（间隔可调）
- **开机自动启动** —— Windows 登录后自动运行并驻留托盘（HKCU Run 注册表项）
- 代理端口（运行中保护性禁改）、访问令牌（缩略展示，点击复制全串）

## 环境要求

- [Node.js](https://nodejs.org/) 18+（`node` / `npm` 需在 PATH 中；dsh 通过官方 npm 包运行）
- [Rust](https://www.rust-lang.org/) 1.77+（仅构建时需要）
- WebView2 运行时（Windows 10/11 通常已内置；缺失时安装程序会自动引导下载安装）

## 从源码构建

```bash
npm install
npx tauri build
```

产物位于 `src-tauri/target/release/bundle/`：

| 格式 | 路径 |
|------|------|
| MSI | `msi/DeepSeek Harness_<版本>_x64_zh-CN.msi` |
| NSIS | `nsis/DeepSeek Harness_<版本>_x64-setup.exe` |

MSI 文件名末尾的 `zh-CN` 是安装器界面语言（`bundle.windows.wix.language`），非系统区域问题。

开发调试命令见 [AGENTS.md](AGENTS.md)。

## 使用

### 窗口结构
- `control-panel` —— 控制面板（主窗口，关闭即最小化到托盘）
- `harness-ui` —— 加载 `dsh web` 界面（`http://127.0.0.1:3080`）的专用窗口

### 局域网访问

1. 控制面板 → 局域网代理 → 启用
2. 托盘菜单 → 复制局域网访问链接（含令牌），或点击面板链接框的复制按钮
3. 局域网设备浏览器打开链接即可访问 dsh；令牌泄露时可随时在面板中重置

### 配置

运行时配置存放于用户配置目录，均可在控制面板中修改：

```
%APPDATA%\deepseekai\DeepSeek Harness\config\settings.json
```

包含：托盘最小化、dsh 自动启动、开机自启动应用、更新检查开关与间隔、局域网代理开关 / 端口 / 访问令牌、自定义 dsh 路径等。配置文件损坏时会自动备份为 `.json.bak` 并回退默认值。
