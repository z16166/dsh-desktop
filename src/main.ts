import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { sameBox, syncGeometryOnFocus, type Box } from "./overlay-layout";
import { kindForProcessLine } from "./log-kind";
import {
  type LogEntry,
  type LogKind,
  type StatusEvent,
  type StatusState,
} from "./shell-events";

const PORT = 3080;
const DEFAULT_APP_URL = "http://127.0.0.1:" + PORT;
const LOG_LIMIT = 2000;
const ERROR_HINT = "可在托盘菜单的「设置」中查看 CLI 日志，安装好依赖后可重试。";
const STOPPED_HINT = "从托盘菜单打开「设置」可以查看日志、更换字体或重新启动。";
let appUrl = DEFAULT_APP_URL;

function publicAppUrl(u: string): string {
  try {
    const parsed = new URL(u);
    parsed.search = "";
    parsed.hash = "";
    return parsed.toString().replace(/\/$/, "");
  } catch {
    return u;
  }
}

type LaunchSource = "npx" | "local";

type AppSettings = {
  source: LaunchSource;
  localPath: string;
};

type ServerStatus = {
  running: boolean;
  port: number;
  url: string;
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
const loadingSpinner = q<HTMLDivElement>("#loading-spinner");
const loadingText = q<HTMLParagraphElement>("#loading-text");

type Phase = "starting" | "running" | "stopped";

let phase: Phase = "starting";
let ready = false;
let stopRequested = false;
let themeTimer: number | undefined;
let withdrawn = false;
let lastDshBox: Box | null = null;
let logSeq = 0;
const logHistory: LogEntry[] = [];

function launchCommandLabel(): string {
  return settings.source === "local"
    ? "node --import tsx/esm apps/cli/src/bin.ts web --no-open  (" + settings.localPath + ")"
    : "npx @deepseek-ai/dsh web --no-open";
}

function publishStatus(state: StatusState, text: string): void {
  const payload: StatusEvent = { state, text };
  void emit("shell:status", payload);
}

function appendLog(line: string, kind: LogKind): void {
  const entry: LogEntry = { id: ++logSeq, kind, text: line };
  logHistory.push(entry);
  while (logHistory.length > LOG_LIMIT) logHistory.shift();
  void emit("shell:log", entry);
}

function clearLogHistory(): void {
  logHistory.length = 0;
  void emit("shell:log-cleared");
}

async function serverStillRunning(): Promise<boolean> {
  try {
    const status = await invoke<ServerStatus>("server_status");
    return status.running;
  } catch {
    return false;
  }
}

async function mainAllowsOverlays(): Promise<boolean> {
  if (withdrawn) return false;
  try {
    const main = getCurrentWindow();
    if (await main.isMinimized()) return false;
    if (!(await main.isVisible())) return false;
    return true;
  } catch {
    return false;
  }
}

function startThemeSync(): void {
  if (themeTimer !== undefined) return;
  const tick = () => {
    void invoke("sync_dsh_theme");
  };
  tick();
  themeTimer = window.setInterval(tick, 1500);
}

/** Font and zoom probes only matter while the overlay is on screen. */
function stopThemeSync(): void {
  if (themeTimer === undefined) return;
  window.clearInterval(themeTimer);
  themeTimer = undefined;
}

function setLoading(
  message: string,
  opts: { error?: boolean; retry?: boolean; spinner?: boolean; hint?: string } = {},
): void {
  loading.style.display = "";
  loadingSpinner.style.display = opts.error || opts.spinner === false ? "none" : "";
  loadingText.textContent = message;
  loadingText.classList.toggle("error-text", !!opts.error);
  const hint = q<HTMLParagraphElement>("#loading-hint");
  hint.hidden = !opts.hint;
  hint.textContent = opts.hint ?? "";
  q<HTMLButtonElement>("#btn-retry").hidden = !opts.retry;
}

function showStartupError(message: string): void {
  phase = "stopped";
  ready = false;
  setLoading(message, { error: true, retry: true, hint: ERROR_HINT });
  publishStatus("error", "启动失败");
  appendLog("> " + message, "err");
  void closeDshWindow();
}

function showStopped(label: string): void {
  phase = "stopped";
  ready = false;
  const message = label === "已退出" ? "服务已退出" : "服务已停止";
  setLoading(message, { spinner: false, retry: true, hint: STOPPED_HINT });
  publishStatus("stopped", label);
  void closeDshWindow();
}

async function dshWindow(): Promise<WebviewWindow | null> {
  return WebviewWindow.getByLabel(DSH_LABEL);
}

async function frameOverlayBox(): Promise<Box | null> {
  try {
    const main = getCurrentWindow();
    const scale = await main.scaleFactor();
    const origin = await main.innerPosition();
    const frame = q<HTMLDivElement>("#app-frame");
    const rect = frame.getBoundingClientRect();
    return {
      x: origin.x / scale + rect.left,
      y: origin.y / scale + rect.top,
      w: Math.max(80, rect.width),
      h: Math.max(80, rect.height),
    };
  } catch {
    return null;
  }
}

async function syncDshBounds(): Promise<void> {
  const wdw = await dshWindow();
  if (!wdw || !ready) return;
  if (!(await mainAllowsOverlays())) return;
  const box = await frameOverlayBox();
  if (!box) return;
  if (lastDshBox && sameBox(lastDshBox, box)) return;
  lastDshBox = box;
  await wdw.setPosition(new LogicalPosition(box.x, box.y));
  await wdw.setSize(new LogicalSize(box.w, box.h));
}

async function onMainGeometryChanged(): Promise<void> {
  if (!(await mainAllowsOverlays())) {
    await hideDshWindow();
    return;
  }
  if (ready) {
    const wdw = await dshWindow();
    if (wdw) await showDshWindow();
  }
  await syncDshBounds();
}

async function onMainFocusChanged(): Promise<void> {
  if (syncGeometryOnFocus()) {
    await onMainGeometryChanged();
    return;
  }
  if (!(await mainAllowsOverlays())) {
    await hideDshWindow();
    return;
  }
  if (ready) {
    const wdw = await dshWindow();
    if (wdw) await showDshWindow();
  }
}

/** Putting the overlay back on screen has to restart the poll that hiding it
 * stopped, so the two always travel together. */
async function showDshWindow(): Promise<void> {
  // Both halves of the overlay's visibility go through Rust rather than Tauri's own
  // show()/hide(): show() is `ShowWindow(SW_SHOW)` and hands the foreground to this
  // taskbar-less tool window (so the shell looks like a background window), while hide()
  // silently does nothing once tao's cached visibility flag is stale (so the harness
  // page stays painted on top). The commands go by what the OS reports instead.
  await invoke("show_dsh_window");
  startThemeSync();
}

async function hideDshWindow(): Promise<void> {
  stopThemeSync();
  // Through Rust rather than `wdw.hide()`: tao skips `ShowWindow(SW_HIDE)` once its
  // cached visibility flag says hidden, which is the state Windows leaves behind when
  // it re-shows this owned window on its own. A skipped hide paints the harness page
  // over whatever is underneath, so every hide goes through the verified path.
  await invoke("hide_dsh_window");
}

async function closeDshWindow(): Promise<void> {
  stopThemeSync();
  lastDshBox = null;
  const wdw = await dshWindow();
  if (wdw) await wdw.close();
}

async function showApp(): Promise<void> {
  if (!(await mainAllowsOverlays())) {
    await hideDshWindow();
    return;
  }
  let wdw = await dshWindow();
  if (!wdw) {
    const box = (await frameOverlayBox()) ?? { x: 0, y: 0, w: 800, h: 600 };
    await invoke("create_dsh_window", {
      url: appUrl,
      x: Math.round(box.x),
      y: Math.round(box.y),
      w: Math.round(box.w),
      h: Math.round(box.h),
    });
    wdw = await dshWindow();
    if (!wdw) throw new Error("创建 dsh 窗口失败");
    // Only on creation: re-arming later would fight a zoom the poll that keeps
    // the setting current has not caught up with yet.
    void invoke("restore_dsh_zoom");
    appendLog("> 已用独立窗口加载 Web UI（避免 iframe 跨站拦截插件）", "sys");
  }
  await syncDshBounds();
  await showDshWindow();
  loading.style.display = "none";
}

async function reloadSettings(): Promise<void> {
  try {
    settings = await invoke<AppSettings>("get_settings");
  } catch {
    /* keep the last copy */
  }
}

async function start(): Promise<void> {
  await reloadSettings();
  phase = "starting";
  ready = false;
  const label = launchCommandLabel();
  setLoading("正在启动 " + label + " …");
  publishStatus("starting", "启动中…");
  appendLog("$ " + label, "sys");
  try {
    const status = await invoke<ServerStatus>("start_server");
    appendLog("> 等待端口 " + status.port + " 就绪…", "sys");
  } catch (e) {
    showStartupError("启动失败：" + String(e));
  }
}

async function setupEvents(): Promise<void> {
  await listen<{ kind: LogKind; text: string }>("shell:note", (e) => {
    appendLog(e.payload.text, e.payload.kind);
  });
  await listen("shell:clear", () => {
    clearLogHistory();
  });
  await listen("settings:hello", () => {
    void emit("shell:log-history", logHistory.slice());
  });
  await listen<{ message?: string }>("shell:starting", (e) => {
    // A late copy of this event must not pull the harness back down after it is already up.
    if (ready && phase === "running") return;
    phase = "starting";
    ready = false;
    const message = e.payload?.message || "正在启动 " + launchCommandLabel() + " …";
    setLoading(message);
    publishStatus("starting", "启动中…");
  });
  await listen("shell:stopping", () => {
    stopRequested = true;
  });
  await listen<{ message: string }>("shell:start-failed", (e) => {
    showStartupError(e.payload.message);
  });

  await listen<string>("server:stdout", (e) => {
    appendLog(e.payload, kindForProcessLine("out", e.payload));
  });
  await listen<string>("server:stderr", (e) => {
    appendLog(e.payload, kindForProcessLine("err", e.payload));
  });
  await listen<string>("server:ready", (e) => {
    const next = (e.payload && String(e.payload).trim()) || DEFAULT_APP_URL;
    const urlChanged = next !== appUrl;
    appUrl = next;
    phase = "running";
    ready = true;
    publishStatus("running", "运行中 · " + publicAppUrl(appUrl));
    void (async () => {
      if (urlChanged) await closeDshWindow();
      await showApp();
    })();
    appendLog("> 就绪：" + publicAppUrl(appUrl), "sys");
  });
  await listen("server:timeout", () => {
    showStartupError("启动超时：90 秒内未检测到 Web UI 就绪，请检查 CLI 日志");
  });
  await listen("shell:stop-cancelled", () => {
    stopRequested = false;
  });
  await listen<number | null>("server:exited", async (e) => {
    appendLog("> 进程已退出 (code=" + e.payload + ")", "sys");
    if (await serverStillRunning()) {
      stopRequested = false;
      return;
    }
    if (stopRequested) {
      stopRequested = false;
      showStopped("已退出");
      return;
    }
    if (phase === "starting") {
      showStartupError("启动失败：进程提前退出（退出码 " + e.payload + "），请检查 CLI 日志");
      return;
    }
    showStopped("已退出");
  });
  await listen("server:stopped", async () => {
    if (await serverStillRunning()) {
      stopRequested = false;
      return;
    }
    stopRequested = false;
    showStopped("已停止");
  });
  await listen<string>("upgrade:stdout", (e2) => {
    appendLog(e2.payload, kindForProcessLine("out", e2.payload));
  });
  await listen<string>("upgrade:stderr", (e2) => {
    appendLog(e2.payload, kindForProcessLine("err", e2.payload));
  });
  await listen("app:withdraw", () => {
    withdrawn = true;
    void hideDshWindow();
  });
  await listen("app:restore", () => {
    withdrawn = false;
    if (ready) void showApp();
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  q("#btn-retry").addEventListener("click", () => {
    void start();
  });
  await reloadSettings();
  const main = getCurrentWindow();
  await main.onResized(() => {
    void onMainGeometryChanged();
  });
  await main.onMoved(() => {
    void onMainGeometryChanged();
  });
  await main.onFocusChanged(() => {
    void onMainFocusChanged();
  });
  await setupEvents();
  try {
    const status = await invoke<ServerStatus>("server_status");
    if (status.running) {
      appendLog("> 检测到 " + status.url + " 已有端口，等待 Web UI 就绪…", "sys");
    }
  } catch {
    /* start() will report its own errors */
  }
  await start();
});
