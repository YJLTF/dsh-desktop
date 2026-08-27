import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface Settings {
  minimize_to_tray: boolean;
  auto_start_dsh: boolean;
  auto_check_updates: boolean;
  update_check_interval_hours: number;
  lan_proxy_enabled: boolean;
  lan_proxy_port: number;
  lan_proxy_token: string;
  firewall_hint_shown: boolean;
  auto_launch_app: boolean;
  dsh_custom_path: string | null;
  dsh_host: string;
  dsh_port: number;
}

interface DshStatus {
  running: boolean;
  host: string;
  port: number;
  url: string;
}

interface ProxyInfo {
  running: boolean;
  port: number;
  lan_ip: string | null;
  url: string | null;
}

interface VersionInfo {
  installed: string | null;
  latest: string | null;
  update_available: boolean;
}

type Resolution =
  | { kind: "Node"; node: string; script: string; source: string }
  | { kind: "Executable"; exe: string; source: string }
  | { kind: "NotFound"; message: string };

/// 转义进 innerHTML 模板的动态文本（版本号等来自外部数据源）。
function esc(s: string | number | null | undefined): string {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" } as Record<string, string>)[c]
  );
}

/// 将端口钳制到 1024–65535（越界值会导致后端 serde 反序列化失败，所有设置都保存不了）。
function clampPort(value: string, fallback: number): number {
  const n = parseInt(value, 10);
  if (!Number.isFinite(n)) return fallback;
  return Math.min(65535, Math.max(1024, Math.round(n)));
}

/// 将小时数钳制到 1–168。
function clampHours(value: string, fallback: number): number {
  const n = parseInt(value, 10);
  if (!Number.isFinite(n)) return fallback;
  return Math.min(168, Math.max(1, Math.round(n)));
}

let settings: Settings | null = null;

function el(html: string): HTMLElement {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild as HTMLElement;
}

function toast(msg: string) {
  let t = document.querySelector(".toast") as HTMLElement;
  if (!t) {
    t = el('<div class="toast"></div>') as HTMLElement;
    document.body.appendChild(t);
  }
  t.textContent = msg;
  t.classList.add("show");
  clearTimeout((t as any)._timer);
  (t as any)._timer = setTimeout(() => t.classList.remove("show"), 2200);
}

function buildApp(): HTMLElement {
  return el(`
    <div class="app-inner">
      <div class="header">
        <div class="logo">h</div>
        <div>
          <h1>DSH Desktop</h1>
          <div class="ver" id="app-ver">桌面端 v—</div>
        </div>
      </div>

      <div id="install-banner" class="card banner" style="display:none">
        <div class="banner-row">
          <span class="badge">未安装</span>
          <div class="banner-body">
            未检测到 dsh 环境<br />
            <span class="subtle">点击一键安装，将执行 <code>npm install -g @deepseek-ai/dsh</code>（需已安装 Node.js）</span>
          </div>
          <button class="btn btn-update" id="btn-install">一键安装</button>
        </div>
      </div>

      <div id="update-banner" class="card banner" style="display:none">
        <div class="banner-row">
          <span class="badge">新版本</span>
          <div class="banner-body" id="update-body"></div>
          <button class="btn btn-update" id="btn-update">立即更新</button>
        </div>
      </div>

      <div class="card">
        <h2>dsh 服务</h2>
        <div class="row">
          <div style="display:flex;align-items:center;gap:8px">
            <span class="status-dot" id="dsh-dot"></span>
            <span class="status-text" id="dsh-status-text">未运行</span>
          </div>
          <span class="muted" id="dsh-url" style="font-size:11px"></span>
        </div>
        <div class="btn-row">
          <button class="btn primary" id="btn-open">打开 Harness</button>
          <button class="btn" id="btn-restart">重启</button>
          <button class="btn danger" id="btn-stop">停止</button>
        </div>
        <div class="row" style="margin-top:12px">
          <div>
            <div class="label">版本信息</div>
            <div class="desc" id="dsh-version">dsh v—</div>
          </div>
          <span class="muted" id="dsh-source" style="font-size:11px"></span>
        </div>
        <div class="row" style="margin-top:12px">
          <div>
            <div class="label">自定义 dsh 路径</div>
            <div class="desc">选择入口文件（bin.js / 可执行文件）；留空则自动查找已安装的 @deepseek-ai/dsh</div>
          </div>
        </div>
        <div class="row" style="margin-top:14px;align-items:stretch">
          <input type="text" id="dsh-path" placeholder="C:\\path\\to\\bin.js" style="flex:1;min-width:0;width:auto" />
          <button class="btn" id="btn-browse" style="flex-shrink:0">浏览…</button>
        </div>
        <div class="warn" id="dsh-path-warning" style="display:none"></div>
      </div>

      <div class="card">
        <h2>局域网代理</h2>
        <div class="row">
          <div style="display:flex;align-items:center;gap:8px">
            <span class="status-dot" id="proxy-dot"></span>
            <div>
              <div class="label">启用局域网访问</div>
              <div class="desc">通过反向代理在局域网内共享 dsh</div>
            </div>
          </div>
          <label class="switch">
            <input type="checkbox" id="proxy-toggle" />
            <span class="slider"></span>
          </label>
        </div>
        <div class="row">
          <div><div class="label">代理端口</div></div>
          <input type="number" id="proxy-port" min="1024" max="65535" />
        </div>
        <div class="hint" id="proxy-port-hint" style="display:none"></div>
        <div class="url-box" id="proxy-url-box">
          <span class="off">代理未启用</span>
        </div>
        <div class="row" style="margin-top:10px">
          <div>
            <div class="label">访问令牌</div>
            <div class="token" id="proxy-token">—</div>
          </div>
          <button class="btn" id="btn-regen">重置</button>
        </div>
      </div>

      <div class="card">
        <h2>设置</h2>
        <div class="row">
          <div><div class="label">关闭窗口时最小化到托盘</div></div>
          <label class="switch">
            <input type="checkbox" id="set-minimize" />
            <span class="slider"></span>
          </label>
        </div>
        <div class="row">
          <div><div class="label">启动时自动运行 dsh</div></div>
          <label class="switch">
            <input type="checkbox" id="set-autostart" />
            <span class="slider"></span>
          </label>
        </div>
        <div class="row">
          <div><div class="label">自动检查 dsh 更新</div></div>
          <label class="switch">
            <input type="checkbox" id="set-autoupdate" />
            <span class="slider"></span>
          </label>
        </div>
        <div class="row">
          <div><div class="label">检查间隔（小时）</div></div>
          <input type="number" id="set-interval" min="1" max="168" />
        </div>
        <div class="row">
          <div>
            <div class="label">开机自动启动</div>
            <div class="desc">Windows 登录后自动运行并驻留托盘</div>
          </div>
          <label class="switch">
            <input type="checkbox" id="set-autolaunch" />
            <span class="slider"></span>
          </label>
        </div>
      </div>
    </div>
  `) as HTMLElement;
}

function renderDshStatus(s: DshStatus) {
  const dot = document.getElementById("dsh-dot")!;
  const text = document.getElementById("dsh-status-text")!;
  const url = document.getElementById("dsh-url")!;
  dot.classList.toggle("running", s.running);
  text.classList.toggle("running", s.running);
  text.textContent = s.running ? "运行中" : "未运行";
  url.textContent = s.url;
  const btnOpen = document.getElementById("btn-open") as HTMLButtonElement;
  const btnStop = document.getElementById("btn-stop") as HTMLButtonElement;
  btnOpen.disabled = !s.running;
  btnStop.disabled = !s.running;
  // 禁用态给出原因，避免“按钮怎么是灰的”困惑。
  btnOpen.title = s.running ? "" : "dsh 未运行，请先启动（托盘左键或等待自动启动）";
  btnStop.title = s.running ? "" : "dsh 未运行";
}

function renderProxy(info: ProxyInfo) {
  const box = document.getElementById("proxy-url-box")!;
  if (info.running && info.url) {
    box.innerHTML = `<span id="proxy-url">${esc(info.url)}<span id="token-suffix"></span></span><span class="copy" id="copy-url" title="复制完整链接">⧉</span>`;
  } else if (info.running) {
    // 代理已启动但未能确定局域网 IP（常见于无外网路由的纯内网环境）。
    box.innerHTML = `<span id="proxy-url">已启用（端口 ${esc(info.port)}），但未能确定局域网 IP；可在其他设备访问 http://&lt;本机IP&gt;:${esc(info.port)}/?token=&lt;令牌&gt;</span>`;
  } else {
    box.innerHTML = `<span class="off">代理未启用</span>`;
  }
  (document.getElementById("proxy-toggle") as HTMLInputElement).checked = info.running;

  document.getElementById("proxy-dot")!.classList.toggle("running", info.running);

  // 运行中不允许改端口（改动不会作用于运行中的代理），并用提示说明实际端口。
  const portInput = document.getElementById("proxy-port") as HTMLInputElement;
  const hint = document.getElementById("proxy-port-hint")!;
  portInput.disabled = info.running;
  if (info.running) {
    if (settings && info.port !== settings.lan_proxy_port) {
      hint.textContent = `端口 ${settings.lan_proxy_port} 不可用，当前实际监听 ${info.port}；关闭代理后可修改端口`;
    } else {
      hint.textContent = "代理运行中，端口设置暂不可修改（关闭后可更改）";
    }
    hint.style.display = "";
  } else {
    hint.style.display = "none";
  }
}

/// 是否有更新正在进行（控制横幅按钮文案 / 禁用态）。
let updating = false;

/// 是否有安装任务正在进行（一键安装与一键更新共用进度事件通道，
/// 两者横幅互斥展示——未安装时只显示安装横幅，装好有新版才显示更新横幅）。
let installing = false;

/// 安装横幅显隐。进行中（installing）时保持按钮的禁用与文案，避免状态轮询重置。
function renderInstallBanner(visible: boolean) {
  const banner = document.getElementById("install-banner")!;
  if (!visible) {
    banner.style.display = "none";
    return;
  }
  banner.style.display = "";
  const btn = document.getElementById("btn-install") as HTMLButtonElement;
  if (!installing) {
    btn.disabled = false;
    btn.textContent = "一键安装";
  }
}

function renderVersion(v: VersionInfo) {
  const banner = document.getElementById("update-banner")!;
  const body = document.getElementById("update-body")!;
  if (v.update_available) {
    banner.style.display = "";
    body.innerHTML =
      `dsh <strong>${esc(v.latest)}</strong> 可用（当前 ${esc(v.installed ?? "?")}）`;
    // 上次更新失败后按钮可能停留在禁用态，重新渲染时恢复。
    const btn = document.getElementById("btn-update") as HTMLButtonElement;
    if (btn && !updating) {
      btn.disabled = false;
      btn.textContent = "立即更新";
    }
  } else {
    banner.style.display = "none";
  }
  // 同步 dsh 服务卡片中的版本号。
  const ver = document.getElementById("dsh-version");
  if (ver && v.installed) ver.textContent = `dsh v${v.installed}`;
}

/// 上次渲染时的自定义路径；变化时才重新解析入口来源（解析可能触发 npm root -g）。
let lastCustomPath: string | null | undefined = undefined;

function renderSettings(s: Settings) {
  settings = s;
  if (lastCustomPath === undefined || s.dsh_custom_path !== lastCustomPath) {
    lastCustomPath = s.dsh_custom_path;
    refreshResolution();
  }
  (document.getElementById("dsh-path") as HTMLInputElement).value = s.dsh_custom_path ?? "";
  (document.getElementById("proxy-port") as HTMLInputElement).value = String(s.lan_proxy_port);
  renderToken(s.lan_proxy_token);
  (document.getElementById("set-minimize") as HTMLInputElement).checked = s.minimize_to_tray;
  (document.getElementById("set-autostart") as HTMLInputElement).checked = s.auto_start_dsh;
  (document.getElementById("set-autoupdate") as HTMLInputElement).checked = s.auto_check_updates;
  (document.getElementById("set-interval") as HTMLInputElement).value = String(s.update_check_interval_hours);
  (document.getElementById("set-autolaunch") as HTMLInputElement).checked = s.auto_launch_app;
  refreshPathWarning();
}

/// 令牌缩略展示（悬停可见全串），避免 64 位十六进制换行铺满卡片。
function renderToken(token: string) {
  const el = document.getElementById("proxy-token")!;
  el.textContent = token ? `${token.slice(0, 8)}…${token.slice(-4)}` : "—";
  el.title = token ? `${token}（点击复制）` : "";
}

/// 刷新 dsh 入口来源显示（全局安装 / 本地安装 / 自定义路径 / 未找到）。
/// 后端在 dsh 未运行时会按当前设置实时解析（可能涉及 npm root -g，秒级），
/// 因此调用处不 await，避免阻塞页面初始化与事件绑定。
/// 同时承担 dsh 环境检查：NotFound 时展示一键安装横幅，
/// 该路径不受“自动检查更新”开关控制，面板每次打开都会执行。
async function refreshResolution() {
  try {
    const r = await invoke<Resolution>("get_resolution");
    const src = document.getElementById("dsh-source");
    if (src) src.textContent = r.kind === "NotFound" ? "未找到 dsh" : r.source;
    renderInstallBanner(r.kind === "NotFound");
  } catch { /* 忽略 */ }
}

/// 校验自定义 dsh 路径输入框当前内容，并在输入框下方展示警告（无问题则隐藏）。
async function refreshPathWarning() {
  const warnEl = document.getElementById("dsh-path-warning");
  if (!warnEl) return;
  const val = (document.getElementById("dsh-path") as HTMLInputElement).value.trim();
  try {
    const warn = await invoke<string | null>("validate_dsh_path", { path: val || null });
    if (warn) {
      warnEl.textContent = warn;
      warnEl.style.display = "";
    } else {
      warnEl.style.display = "none";
    }
  } catch {
    warnEl.style.display = "none";
  }
}

function collectSettings(): Settings | null {
  if (!settings) return null;
  return {
    ...settings,
    dsh_custom_path: (document.getElementById("dsh-path") as HTMLInputElement).value.trim() || null,
    lan_proxy_port: clampPort((document.getElementById("proxy-port") as HTMLInputElement).value, settings.lan_proxy_port),
    update_check_interval_hours: clampHours((document.getElementById("set-interval") as HTMLInputElement).value, settings.update_check_interval_hours),
    auto_launch_app: (document.getElementById("set-autolaunch") as HTMLInputElement).checked,
  };
}

async function saveSettings() {
  const next = collectSettings();
  if (!next) return;
  try {
    const saved = await invoke<Settings>("update_settings", { incoming: next });
    renderSettings(saved);
  } catch (e) {
    toast(`保存失败: ${e}`);
  }
}

async function toggleProxy(on: boolean) {
  try {
    if (on) {
      const info = await invoke<ProxyInfo>("start_proxy");
      renderProxy(info);
      if (settings && info.port !== settings.lan_proxy_port) {
        // 配置端口不可用（如 Win10 Hyper-V/WSL 保留端口段），后端已回退为系统分配端口。
        toast(`端口 ${settings.lan_proxy_port} 不可用，已改用端口 ${info.port}`);
      } else {
        toast("局域网代理已启用");
      }
    } else {
      await invoke("stop_proxy");
      toast("局域网代理已关闭");
    }
    if (settings) {
      settings.lan_proxy_enabled = on;
      await saveSettings();
    }
  } catch (e) {
    toast(`操作失败: ${e}`);
    // 刷新真实状态
    const info = await invoke<ProxyInfo>("get_proxy_info");
    renderProxy(info);
  }
}

async function init() {
  const app = buildApp();
  document.getElementById("app")!.appendChild(app);

  // 版本标签。
  try {
    const ver = await invoke<string>("get_app_version");
    document.getElementById("app-ver")!.textContent = `桌面端 v${ver}`;
  } catch {
    /* 忽略 */
  }

  // 初始数据：逐项容错——单个 invoke 失败（后端瞬时错误）不应中断初始化，
  // 否则事件监听挂不上、按钮全部无响应，页面表现为“假死”。
  try {
    renderSettings(await invoke<Settings>("get_settings"));
  } catch (e) {
    toast(`读取设置失败: ${e}`);
  }
  try { renderDshStatus(await invoke<DshStatus>("get_dsh_status")); } catch { /* 忽略 */ }
  try { renderProxy(await invoke<ProxyInfo>("get_proxy_info")); } catch { /* 忽略 */ }

  bindEvents();
  await listenEvents();

  // dsh 入口来源：后端可能需实时解析（npm root -g 秒级），异步刷新不阻塞初始化。
  refreshResolution();

  // 每隔几秒轮询 dsh 状态，以便捕获进程意外退出。
  setInterval(async () => {
    try {
      const st = await invoke<DshStatus>("get_dsh_status");
      renderDshStatus(st);
    } catch { /* 忽略 */ }
  }, 5000);
}

function bindEvents() {
  // 事件绑定。
  document.getElementById("btn-open")!.addEventListener("click", () => {
    invoke("open_harness_window" as any).catch(() => {
      /* 后端命令失败时的兜底：用设置中的地址，而非硬编码 127.0.0.1:3080 */
      const url = settings
        ? `http://${settings.dsh_host}:${settings.dsh_port}`
        : "http://127.0.0.1:3080";
      window.open(url, "_blank");
    });
  });
  document.getElementById("btn-restart")!.addEventListener("click", async () => {
    toast("正在重启 dsh…");
    try {
      await invoke("restart_dsh");
      toast("dsh 已重启");
    } catch (e) {
      toast(`重启失败: ${e}`);
    }
  });
  document.getElementById("btn-stop")!.addEventListener("click", async () => {
    try {
      await invoke("stop_dsh");
      toast("dsh 已停止");
    } catch (e) {
      toast(`停止失败: ${e}`);
    }
  });

  document.getElementById("btn-update")!.addEventListener("click", async () => {
    const btn = document.getElementById("btn-update") as HTMLButtonElement;
    if (updating || btn.disabled) return;
    updating = true;
    btn.disabled = true;
    btn.textContent = "正在更新…";
    try {
      const v = await invoke<VersionInfo>("update_dsh");
      renderVersion(v);
      toast(`已更新到 dsh v${v.installed ?? "最新版"}`);
    } catch (e) {
      toast(`更新失败: ${e}`);
      btn.disabled = false;
      btn.textContent = "立即更新";
    } finally {
      updating = false;
    }
  });

  document.getElementById("btn-install")!.addEventListener("click", async () => {
    const btn = document.getElementById("btn-install") as HTMLButtonElement;
    if (installing || btn.disabled) return;
    installing = true;
    btn.disabled = true;
    btn.textContent = "正在准备安装…";
    try {
      const v = await invoke<VersionInfo>("install_dsh");
      renderVersion(v);
      toast(v.installed ? `dsh v${v.installed} 已就绪` : "dsh 已就绪");
      // 重新解析入口：横幅据此收起，来源标签同步为实际结果（如“全局安装”）。
      await refreshResolution();
      renderInstallBanner(false);
    } catch (e) {
      toast(`安装失败: ${e}`);
      btn.disabled = false;
      btn.textContent = "一键安装";
    } finally {
      installing = false;
    }
  });

  document.getElementById("proxy-toggle")!.addEventListener("change", (e) => {
    toggleProxy((e.target as HTMLInputElement).checked);
  });
  document.getElementById("btn-regen")!.addEventListener("click", async () => {
    try {
      const token = await invoke<string>("regenerate_token");
      renderToken(token);
      toast(
        settings?.lan_proxy_enabled
          ? "访问令牌已重置（运行中的代理已用新令牌重启）"
          : "访问令牌已重置"
      );
      const info = await invoke<ProxyInfo>("get_proxy_info");
      renderProxy(info);
    } catch (e) {
      toast(`重置失败: ${e}`);
    }
  });

  // 点击令牌复制全串（展示为缩略形式）。
  document.getElementById("proxy-token")!.addEventListener("click", async () => {
    try {
      const token = await invoke<string>("get_proxy_token");
      if (token) {
        await navigator.clipboard.writeText(token);
        toast("访问令牌已复制");
      }
    } catch { /* 忽略 */ }
  });
  document.getElementById("proxy-url-box")!.addEventListener("click", async (e) => {
    const target = e.target as HTMLElement;
    if (target.id === "copy-url") {
      try {
        const token = (await invoke<string>("get_proxy_token")) ?? "";
        const info = await invoke<ProxyInfo>("get_proxy_info");
        const full = info.url ? info.url + token : "";
        if (!full) {
          toast("未能获取访问链接（代理未启用或无法确定局域网 IP）");
          return;
        }
        await navigator.clipboard.writeText(full);
        toast("已复制完整访问链接");
      } catch {
        toast("复制失败");
      }
    }
  });

  document.getElementById("btn-browse")!.addEventListener("click", async () => {
    try {
      const picked = await invoke<string | null>("pick_dsh_path");
      if (picked) {
        (document.getElementById("dsh-path") as HTMLInputElement).value = picked;
        await saveSettings();
      }
    } catch (e) {
      toast(`选择文件失败: ${e}`);
    }
  });

  for (const id of ["dsh-path", "proxy-port", "set-interval"]) {
    document.getElementById(id)!.addEventListener("change", saveSettings);
  }
  for (const id of ["set-minimize", "set-autostart", "set-autoupdate", "set-autolaunch"]) {
    document.getElementById(id)!.addEventListener("change", saveSettings);
  }
}

async function listenEvents() {
  // Tauri 事件监听（单条失败不阻断其余监听）。
  const subs: [string, (e: { payload: any }) => void][] = [
    ["dsh-status", (e) => renderDshStatus(e.payload)],
    ["proxy-info", (e) => renderProxy(e.payload)],
    ["version-info", (e) => renderVersion(e.payload)],
    ["settings-changed", (e) => renderSettings(e.payload)],
    // 一键安装 / 更新的阶段进度（后端文案直接显示在对应按钮上，按当前操作路由）。
    ["update-progress", (e) => {
      const msg = e.payload?.message;
      if (!msg) return;
      const btn = document.getElementById(installing ? "btn-install" : "btn-update");
      if (btn) btn.textContent = String(msg);
    }],
  ];
  for (const [event, handler] of subs) {
    try {
      await listen(event, handler);
    } catch { /* 忽略 */ }
  }
}

init();
