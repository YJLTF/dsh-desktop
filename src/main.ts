import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

interface Settings {
  minimize_to_tray: boolean;
  auto_start_dsh: boolean;
  auto_check_updates: boolean;
  update_check_interval_hours: number;
  lan_proxy_enabled: boolean;
  lan_proxy_port: number;
  lan_proxy_token: string;
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
          <h1>DeepSeek Harness</h1>
          <div class="ver" id="app-ver">桌面端 v—</div>
        </div>
      </div>

      <div id="update-banner" class="card banner" style="display:none">
        <div class="banner-row">
          <span class="badge">新版本</span>
          <div class="banner-body" id="update-body"></div>
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
          <div>
            <div class="label">启用局域网访问</div>
            <div class="desc">通过反向代理在局域网内共享 dsh</div>
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
  (document.getElementById("btn-open") as HTMLButtonElement).disabled = !s.running;
  (document.getElementById("btn-stop") as HTMLButtonElement).disabled = !s.running;
}

function renderProxy(info: ProxyInfo) {
  const box = document.getElementById("proxy-url-box")!;
  if (info.running && info.url) {
    box.innerHTML = `<span id="proxy-url">${info.url}<span id="token-suffix"></span></span><span class="copy" id="copy-url" title="复制完整链接">⧉</span>`;
  } else if (info.running) {
    // 代理已启动但未能确定局域网 IP（常见于无外网路由的纯内网环境）。
    box.innerHTML = `<span id="proxy-url">已启用（端口 ${info.port}），但未能确定局域网 IP；可在其他设备访问 http://&lt;本机IP&gt;:${info.port}/?token=&lt;令牌&gt;</span>`;
  } else {
    box.innerHTML = `<span class="off">代理未启用</span>`;
  }
  (document.getElementById("proxy-toggle") as HTMLInputElement).checked = info.running;
}

function renderVersion(v: VersionInfo) {
  const banner = document.getElementById("update-banner")!;
  const body = document.getElementById("update-body")!;
  if (v.update_available) {
    banner.style.display = "";
    body.innerHTML =
      `dsh <strong>${v.latest}</strong> 可用（当前 ${v.installed ?? "?"}）。运行 ` +
      `<code>npm install -g @deepseek-ai/dsh</code> 升级。`;
  } else {
    banner.style.display = "none";
  }
}

function renderSettings(s: Settings) {
  settings = s;
  (document.getElementById("dsh-path") as HTMLInputElement).value = s.dsh_custom_path ?? "";
  (document.getElementById("proxy-port") as HTMLInputElement).value = String(s.lan_proxy_port);
  (document.getElementById("proxy-token") as HTMLElement).textContent = s.lan_proxy_token || "—";
  (document.getElementById("set-minimize") as HTMLInputElement).checked = s.minimize_to_tray;
  (document.getElementById("set-autostart") as HTMLInputElement).checked = s.auto_start_dsh;
  (document.getElementById("set-autoupdate") as HTMLInputElement).checked = s.auto_check_updates;
  (document.getElementById("set-interval") as HTMLInputElement).value = String(s.update_check_interval_hours);
  refreshPathWarning();
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
    lan_proxy_port: parseInt((document.getElementById("proxy-port") as HTMLInputElement).value, 10) || settings.lan_proxy_port,
    update_check_interval_hours: parseInt((document.getElementById("set-interval") as HTMLInputElement).value, 10) || settings.update_check_interval_hours,
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

  // 初始数据。
  const s = await invoke<Settings>("get_settings");
  renderSettings(s);
  const status = await invoke<DshStatus>("get_dsh_status");
  renderDshStatus(status);
  const proxy = await invoke<ProxyInfo>("get_proxy_info");
  renderProxy(proxy);

  // 事件绑定。
  document.getElementById("btn-open")!.addEventListener("click", () => {
    invoke("open_harness_window" as any).catch(() => {
      /* open_harness_window 是 Rust 辅助方法，已通过托盘打开；以下是兜底 */
      window.open(`http://127.0.0.1:3080`, "_blank");
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

  document.getElementById("proxy-toggle")!.addEventListener("change", (e) => {
    toggleProxy((e.target as HTMLInputElement).checked);
  });
  document.getElementById("btn-regen")!.addEventListener("click", async () => {
    try {
      const token = await invoke<string>("regenerate_token");
      (document.getElementById("proxy-token")!).textContent = token;
      toast("访问令牌已重置");
      const info = await invoke<ProxyInfo>("get_proxy_info");
      renderProxy(info);
    } catch (e) {
      toast(`重置失败: ${e}`);
    }
  });
  document.getElementById("proxy-url-box")!.addEventListener("click", async (e) => {
    const target = e.target as HTMLElement;
    if (target.id === "copy-url") {
      const token = (await invoke<string>("get_proxy_token")) ?? "";
      const info = await invoke<ProxyInfo>("get_proxy_info");
      const full = info.url ? info.url + token : "";
      if (full) await navigator.clipboard.writeText(full);
      toast("已复制完整访问链接");
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
  for (const id of ["set-minimize", "set-autostart", "set-autoupdate"]) {
    document.getElementById(id)!.addEventListener("change", saveSettings);
  }

  // Tauri 事件监听。
  const unlisteners: UnlistenFn[] = [];
  unlisteners.push(await listen<DshStatus>("dsh-status", (e) => renderDshStatus(e.payload)));
  unlisteners.push(await listen<ProxyInfo>("proxy-info", (e) => renderProxy(e.payload)));
  unlisteners.push(await listen<VersionInfo>("version-info", (e) => renderVersion(e.payload)));
  unlisteners.push(await listen<Settings>("settings-changed", (e) => renderSettings(e.payload)));

  // 每隔几秒轮询 dsh 状态，以便捕获进程意外退出。
  setInterval(async () => {
    try {
      const st = await invoke<DshStatus>("get_dsh_status");
      renderDshStatus(st);
    } catch { /* 忽略 */ }
  }, 5000);
}

init();
