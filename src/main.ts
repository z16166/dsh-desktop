import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

const PORT = 3080;
const APP_URL = "http://127.0.0.1:" + PORT;

type LaunchSource = "npx" | "local";

type AppSettings = {
  source: LaunchSource;
  localPath: string;
};

let settings: AppSettings = {
  source: "local",
  localPath: "H:\\github\\deepseek-harness",
};

function q<T extends HTMLElement>(sel: string): T {
  return document.querySelector(sel) as T;
}

const DSH_LABEL = "dsh";
const loading = q<HTMLDivElement>("#loading");
const log = q<HTMLPreElement>("#log");
const dot = q<HTMLSpanElement>("#status-dot");
const statusText = q<HTMLSpanElement>("#status-text");
const loadingSpinner = q<HTMLDivElement>("#loading-spinner");
const loadingText = q<HTMLParagraphElement>("#loading-text");

type State = "starting" | "running" | "stopped" | "error";

let upgrading = false;
let cliMode = false;
let ready = false;

function setStatus(state: State, text: string): void {
  dot.className = "dot " + state;
  statusText.textContent = text;
}

function appendLog(line: string, kind: "out" | "err" | "sys"): void {
  const span = document.createElement("span");
  span.className = kind;
  span.textContent = line + "\n";
  log.appendChild(span);
  while (log.childElementCount > 2000) {
    if (log.firstElementChild) log.removeChild(log.firstElementChild);
  }
  log.scrollTop = log.scrollHeight;
}

/** Switch the loading overlay between "starting", "error" and "stopped" states. */
function setLoading(
  message: string,
  opts: { error?: boolean; retry?: boolean; spinner?: boolean } = {},
): void {
  loadingSpinner.style.display = opts.error || opts.spinner === false ? "none" : "";
  loadingText.textContent = message;
  loadingText.classList.toggle("error-text", !!opts.error);
  q<HTMLParagraphElement>("#loading-hint").hidden = !opts.error;
  q<HTMLButtonElement>("#btn-retry").hidden = !opts.retry;
}

function showStartupError(message: string): void {
  ready = false;
  setStatus("error", "启动失败");
  setLoading(message, { error: true, retry: true });
  appendLog("> " + message, "err");
  void closeDshWindow();
}

async function dshWindow(): Promise<WebviewWindow | null> {
  return WebviewWindow.getByLabel(DSH_LABEL);
}

async function syncDshBounds(): Promise<void> {
  const wdw = await dshWindow();
  if (!wdw || cliMode || !ready) return;
  const main = getCurrentWindow();
  const scale = await main.scaleFactor();
  const origin = await main.innerPosition();
  const frame = q<HTMLDivElement>("#app-frame");
  const rect = frame.getBoundingClientRect();
  const x = origin.x / scale + rect.left;
  const y = origin.y / scale + rect.top;
  const width = Math.max(80, rect.width);
  const height = Math.max(80, rect.height);
  await wdw.setPosition(new LogicalPosition(x, y));
  await wdw.setSize(new LogicalSize(width, height));
}

async function hideDshWindow(): Promise<void> {
  const wdw = await dshWindow();
  if (wdw) await wdw.hide();
}

async function closeDshWindow(): Promise<void> {
  const wdw = await dshWindow();
  if (wdw) await wdw.close();
}

async function showApp(): Promise<void> {
  loading.style.display = "none";
  if (cliMode) {
    await hideDshWindow();
    return;
  }
  let wdw = await dshWindow();
  if (!wdw) {
    const main = getCurrentWindow();
    wdw = new WebviewWindow(DSH_LABEL, {
      url: APP_URL,
      parent: main,
      decorations: false,
      skipTaskbar: true,
      resizable: false,
      shadow: false,
      focus: true,
    });
    await new Promise<void>((resolve, reject) => {
      const t = window.setTimeout(() => reject(new Error("创建 dsh 窗口超时")), 8000);
      wdw!.once("tauri://created", () => {
        window.clearTimeout(t);
        resolve();
      });
      wdw!.once("tauri://error", (e) => {
        window.clearTimeout(t);
        reject(e.payload ?? e);
      });
    });
    appendLog("> 已用独立窗口加载 Web UI（避免 iframe 跨站拦截插件）", "sys");
  } else {
    await wdw.show();
  }
  await syncDshBounds();
}

function launchCommandLabel(): string {
  return settings.source === "local"
    ? "node --import tsx/esm apps/cli/src/bin.ts web  (" + settings.localPath + ")"
    : "npx @deepseek-ai/dsh web";
}

function syncSourceControls(): void {
  const select = q<HTMLSelectElement>("#source-select");
  const path = q<HTMLInputElement>("#local-path");
  select.value = settings.source;
  path.value = settings.localPath;
  path.hidden = settings.source !== "local";
}

async function start(): Promise<void> {
  ready = false;
  setStatus("starting", "启动中…");
  setLoading("正在启动 " + launchCommandLabel() + " …");
  appendLog("$ " + launchCommandLabel(), "sys");
  try {
    const s = await invoke<any>("start_server");
    appendLog("> 等待端口 " + s.port + " 就绪…", "sys");
  } catch (e) {
    showStartupError("启动失败：" + String(e));
  }
}

async function stop(): Promise<void> {
  appendLog("$ 停止服务…", "sys");
  const s = await invoke<any>("stop_server");
  if (s.running) {
    appendLog("> 该服务非本应用启动，未停止", "sys");
    setStatus("running", "运行中 · " + APP_URL);
  } else {
    setStatus("stopped", "已停止");
    await closeDshWindow();
  }
}

async function restart(): Promise<void> {
  await stop();
  await start();
}

async function upgrade(): Promise<void> {
  if (upgrading) return;
  upgrading = true;
  const btn = q<HTMLButtonElement>("#btn-upgrade");
  btn.disabled = true;
  q<HTMLSelectElement>("#source-select").disabled = true;
  q<HTMLInputElement>("#local-path").disabled = true;
  appendLog(
    settings.source === "local"
      ? "> 正在从本地仓库升级（git pull → pnpm install → pnpm run build）…"
      : "> 正在检查最新版本并安装…",
    "sys",
  );
  try {
    const r = await invoke<any>("upgrade_dsh");
    if (r.ok) {
      appendLog("> 已安装版本 " + r.version + " · " + r.message, "sys");
      if (r.restarted) {
        await closeDshWindow();
        await showApp();
      }
    } else {
      appendLog("> 升级失败：" + r.message, "err");
    }
  } catch (e2) {
    appendLog("> 升级失败：" + String(e2), "err");
  } finally {
    upgrading = false;
    btn.disabled = false;
    q<HTMLSelectElement>("#source-select").disabled = false;
    q<HTMLInputElement>("#local-path").disabled = false;
  }
}

function clearLog(): void {
  log.textContent = "";
}

async function copyLog(): Promise<void> {
  try {
    await navigator.clipboard.writeText(log.textContent || "");
    appendLog("> 已复制到剪贴板", "sys");
  } catch {
    appendLog("> 复制失败", "err");
  }
}

function setupTabs(): void {
  const tabs = document.querySelectorAll<HTMLButtonElement>(".tab");
  const panels: Record<string, HTMLElement> = {
    app: q("#panel-app"),
    cli: q("#panel-cli"),
  };
  tabs.forEach((t) => {
    t.addEventListener("click", () => {
      tabs.forEach((x) => x.classList.remove("active"));
      t.classList.add("active");
      const key = t.dataset.tab || "app";
      Object.keys(panels).forEach((k) => panels[k].classList.toggle("active", k === key));
      if (key === "cli") {
        cliMode = true;
        void hideDshWindow();
      } else {
        cliMode = false;
        if (ready) void showApp();
      }
    });
  });
}

async function setupEvents(): Promise<void> {
  await listen<string>("server:stdout", (e) => appendLog(e.payload, "out"));
  await listen<string>("server:stderr", (e) => appendLog(e.payload, "err"));
  await listen("server:ready", () => {
    ready = true;
    setStatus("running", "运行中 · " + APP_URL);
    void showApp();
    appendLog("> 就绪：" + APP_URL, "sys");
    refreshDshVersion();
  });
  await listen("server:timeout", () => {
    showStartupError("启动超时：90 秒内未检测到端口监听，请检查 CLI 日志");
  });
  await listen<number | null>("server:exited", (e) => {
    if (ready) {
      setStatus("stopped", "已退出");
      appendLog("> 进程已退出 (code=" + e.payload + ")", "sys");
      void closeDshWindow();
    } else {
      showStartupError("启动失败：进程提前退出（退出码 " + e.payload + "），请检查 CLI 日志");
    }
  });
  await listen("server:stopped", () => {
    if (!ready) setLoading("服务已停止", { spinner: false });
    setStatus("stopped", "已停止");
  });
  await listen<string>("upgrade:stdout", (e2) => appendLog(e2.payload, "out"));
  await listen<string>("upgrade:stderr", (e2) => appendLog(e2.payload, "err"));
  await listen("app:restore", () => {
    if (ready && !cliMode) void showApp();
  });
}

async function persistSettings(next: AppSettings, restartAfter: boolean): Promise<boolean> {
  try {
    settings = await invoke<AppSettings>("set_settings", { settings: next });
    syncSourceControls();
    appendLog(
      "> 启动源已设为 " +
        (settings.source === "local" ? "本地源码 · " + settings.localPath : "npx"),
      "sys",
    );
    if (restartAfter) await restart();
    return true;
  } catch (e) {
    appendLog("> 保存启动源失败：" + String(e), "err");
    syncSourceControls();
    return false;
  }
}

function setupSourceControls(): void {
  const select = q<HTMLSelectElement>("#source-select");
  const path = q<HTMLInputElement>("#local-path");
  select.addEventListener("change", async () => {
    if (upgrading) {
      syncSourceControls();
      return;
    }
    const next: AppSettings = {
      source: select.value === "local" ? "local" : "npx",
      localPath: path.value.trim() || settings.localPath,
    };
    await persistSettings(next, true);
  });
  const applyPath = async () => {
    if (upgrading || settings.source !== "local") return;
    const localPath = path.value.trim();
    if (!localPath || localPath === settings.localPath) return;
    await persistSettings({ source: "local", localPath }, true);
  };
  path.addEventListener("change", applyPath);
  path.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void applyPath();
    }
  });
}

function setupButtons(): void {
  q("#btn-start").addEventListener("click", start);
  q("#btn-stop").addEventListener("click", stop);
  q("#btn-restart").addEventListener("click", restart);
  q("#btn-upgrade").addEventListener("click", upgrade);
  q("#btn-clear").addEventListener("click", clearLog);
  q("#btn-copy").addEventListener("click", copyLog);
  q("#btn-retry").addEventListener("click", start);
}

/** Show the DeepSeek Harness (dsh) version in the topbar, not the app's own. */
async function refreshDshVersion(): Promise<void> {
  const el = q<HTMLSpanElement>("#version");
  try {
    const v = await invoke<string>("dsh_version");
    const suffix = settings.source === "local" ? " · 本地" : "";
    el.textContent = v && v !== "unknown" ? "dsh v" + v + suffix : "dsh 未知";
  } catch {
    el.textContent = "dsh 未知";
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  setupTabs();
  setupButtons();
  setupSourceControls();
  try {
    settings = await invoke<AppSettings>("get_settings");
  } catch {
    /* keep defaults */
  }
  syncSourceControls();
  q("#version").textContent = "dsh …";
  const main = getCurrentWindow();
  await main.onResized(() => {
    void syncDshBounds();
  });
  await main.onMoved(() => {
    void syncDshBounds();
  });
  await setupEvents();
  try {
    const s = await invoke<any>("server_status");
    if (s.running) {
      appendLog("> 检测到 " + s.url + " 已有端口，等待 Web UI 就绪…", "sys");
    }
  } catch {
    /* start() will report its own errors */
  }
  await start();
});
