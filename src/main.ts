import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  chromeBtnReadyToShow,
  collapseChromeBeforeOverlay,
  sameBox,
  syncGeometryOnFocus,
  type Box,
} from "./overlay-layout";

const PORT = 3080;
const DEFAULT_APP_URL = "http://127.0.0.1:" + PORT;
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

let settings: AppSettings = {
  source: "local",
  localPath: "H:\\github\\deepseek-harness",
};

function q<T extends HTMLElement>(sel: string): T {
  return document.querySelector(sel) as T;
}

const DSH_LABEL = "dsh";
const CHROME_BTN_LABEL = "chrome-btn";
const CHROME_BTN_SIZE = 36;
const CHROME_BTN_MARGIN = 12;
const CHROME_BTN_GAP = 8;
const CHROME_BTN_FALLBACK_MS = 2500;
const loading = q<HTMLDivElement>("#loading");
const log = q<HTMLPreElement>("#log");
const dot = q<HTMLSpanElement>("#status-dot");
const statusText = q<HTMLSpanElement>("#status-text");
const loadingSpinner = q<HTMLDivElement>("#loading-spinner");
const loadingText = q<HTMLParagraphElement>("#loading-text");
const topbar = q<HTMLElement>("#topbar");

type State = "starting" | "running" | "stopped" | "error";

type DshAvoidRect = {
  x: number;
  y: number;
  w: number;
  h: number;
};

type DshTheme = {
  dark: boolean;
  bg: string;
  fg: string;
  border: string;
  avoid?: DshAvoidRect | null;
};

let upgrading = false;
let cliMode = false;
let ready = false;
let lastDshTheme: DshTheme | null = null;
let themeTimer: number | undefined;
let dshFocusBound = false;
let withdrawn = false;
let chromeBtnUp = false;
let overlaysReadyAt = 0;
let lastDshBox: Box | null = null;
let lastChromeBox: Box | null = null;
let chromeBtnFallbackTimer: number | undefined;

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

/** Nothing consumes a theme or an anchor rect while the overlay is off screen,
 * so every path that hides it stops the poll instead of leaving it running. */
function stopThemeSync(): void {
  if (themeTimer === undefined) return;
  window.clearInterval(themeTimer);
  themeTimer = undefined;
}

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
  showChrome();
  appendLog("> " + message, "err");
  void closeDshWindow();
}

function chromeOpen(): boolean {
  return topbar.classList.contains("open");
}

let overlayOp: Promise<void> = Promise.resolve();

function runOverlayOp(fn: () => Promise<void>): Promise<void> {
  const next = overlayOp.then(fn, fn);
  overlayOp = next.then(
    () => undefined,
    () => undefined,
  );
  return next;
}

function showChrome(): void {
  if (!chromeOpen()) {
    topbar.classList.add("open");
  }
  void (async () => {
    await syncDshBounds();
    await hideChromeBtn();
  })();
}

function collapseChromeBar(): void {
  if (collapseChromeBeforeOverlay(cliMode) && chromeOpen()) {
    topbar.classList.remove("open");
  }
}

function hideChrome(): void {
  if (cliMode) return;
  collapseChromeBar();
  void (async () => {
    await syncDshBounds();
    await raiseChromeBtn();
  })();
}

function setupChrome(): void {
  q("#bar-close").addEventListener("click", () => {
    hideChrome();
  });
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
  if (!wdw || cliMode || !ready) return;
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
    await hideChromeBtn();
    return;
  }
  if (ready && !cliMode) {
    const wdw = await dshWindow();
    if (wdw) await showDshWindow(wdw);
  }
  await syncOverlayBounds();
}

async function onMainFocusChanged(): Promise<void> {
  if (syncGeometryOnFocus()) {
    await onMainGeometryChanged();
    return;
  }
  if (!(await mainAllowsOverlays())) {
    await hideDshWindow();
    await hideChromeBtn();
    return;
  }
  if (ready && !cliMode) {
    const wdw = await dshWindow();
    if (wdw) await showDshWindow(wdw);
  }
}

async function syncOverlayBounds(): Promise<void> {
  await syncDshBounds();
  await syncChromeBtnBounds();
}

async function chromeBtnWindow(): Promise<WebviewWindow | null> {
  return WebviewWindow.getByLabel(CHROME_BTN_LABEL);
}

async function ensureChromeBtn(): Promise<WebviewWindow> {
  let wdw = await chromeBtnWindow();
  if (wdw) return wdw;
  const main = getCurrentWindow();
  wdw = new WebviewWindow(CHROME_BTN_LABEL, {
    url: "chrome-btn.html",
    parent: main,
    decorations: false,
    skipTaskbar: true,
    resizable: false,
    shadow: false,
    focus: false,
    visible: false,
    width: CHROME_BTN_SIZE,
    height: CHROME_BTN_SIZE,
  });
  await new Promise<void>((resolve, reject) => {
    const t = window.setTimeout(() => reject(new Error("创建工具栏按钮超时")), 8000);
    wdw!.once("tauri://created", () => {
      window.clearTimeout(t);
      resolve();
    });
    wdw!.once("tauri://error", (e) => {
      window.clearTimeout(t);
      reject(e.payload ?? e);
    });
  });
  return wdw;
}

async function syncChromeBtnBounds(): Promise<void> {
  const wdw = await chromeBtnWindow();
  if (!wdw) return;
  if (!(await mainAllowsOverlays())) return;
  const frame = await frameOverlayBox();
  if (!frame) return;
  let x = frame.x + frame.w - CHROME_BTN_SIZE - CHROME_BTN_MARGIN;
  let y = frame.y + CHROME_BTN_MARGIN;
  const avoid = lastDshTheme?.avoid;
  if (avoid && avoid.w > 0 && avoid.h > 0) {
    x = frame.x + avoid.x - CHROME_BTN_GAP - CHROME_BTN_SIZE;
    y = frame.y + avoid.y + (avoid.h - CHROME_BTN_SIZE) / 2;
  }
  const minX = frame.x + 4;
  const maxX = frame.x + frame.w - CHROME_BTN_SIZE - 4;
  const minY = frame.y + 4;
  const maxY = frame.y + frame.h - CHROME_BTN_SIZE - 4;
  x = Math.min(maxX, Math.max(minX, x));
  y = Math.min(maxY, Math.max(minY, y));
  const box: Box = { x, y, w: CHROME_BTN_SIZE, h: CHROME_BTN_SIZE };
  if (lastChromeBox && sameBox(lastChromeBox, box)) return;
  lastChromeBox = box;
  await wdw.setPosition(new LogicalPosition(x, y));
  await wdw.setSize(new LogicalSize(CHROME_BTN_SIZE, CHROME_BTN_SIZE));
}

async function hideChromeBtn(): Promise<void> {
  chromeBtnUp = false;
  lastChromeBox = null;
  const wdw = await chromeBtnWindow();
  if (wdw) await wdw.hide();
}

function chromeWaitedMs(): number {
  return overlaysReadyAt === 0 ? 0 : Date.now() - overlaysReadyAt;
}

function tryShowChromeBtn(): void {
  if (cliMode || chromeOpen() || !ready) return;
  if (!chromeBtnReadyToShow(lastDshTheme?.avoid, chromeWaitedMs(), CHROME_BTN_FALLBACK_MS)) {
    return;
  }
  void runOverlayOp(() => raiseChromeBtn());
}

function scheduleChromeBtn(): void {
  tryShowChromeBtn();
  if (chromeBtnFallbackTimer !== undefined) return;
  chromeBtnFallbackTimer = window.setTimeout(() => {
    chromeBtnFallbackTimer = undefined;
    tryShowChromeBtn();
  }, CHROME_BTN_FALLBACK_MS);
}

async function raiseChromeBtn(forceRaise = false): Promise<void> {
  if (cliMode || chromeOpen() || !(await mainAllowsOverlays())) {
    await hideChromeBtn();
    return;
  }
  try {
    const wdw = await ensureChromeBtn();
    await syncChromeBtnBounds();
    const alreadyUp = chromeBtnUp;
    if (!alreadyUp) await wdw.show();
    chromeBtnUp = true;
    if (!alreadyUp || forceRaise) {
      await invoke("raise_overlay", { label: CHROME_BTN_LABEL });
    }
    startThemeSync();
    if (lastDshTheme) void emit("dsh:theme", lastDshTheme);
  } catch (e) {
    appendLog("> 无法显示工具栏按钮：" + String(e), "err");
  }
}

/** Putting the overlay back on screen has to restart the poll that hiding it
 * stopped, so the two always travel together. */
async function showDshWindow(wdw: WebviewWindow): Promise<void> {
  if (!(await wdw.isVisible())) await wdw.show();
  startThemeSync();
}

async function hideDshWindow(): Promise<void> {
  stopThemeSync();
  const wdw = await dshWindow();
  if (wdw) await wdw.hide();
}

async function closeDshWindow(): Promise<void> {
  stopThemeSync();
  dshFocusBound = false;
  lastDshBox = null;
  lastDshTheme = null;
  overlaysReadyAt = 0;
  if (chromeBtnFallbackTimer !== undefined) {
    window.clearTimeout(chromeBtnFallbackTimer);
    chromeBtnFallbackTimer = undefined;
  }
  const wdw = await dshWindow();
  if (wdw) await wdw.close();
}

async function showApp(): Promise<void> {
  loading.style.display = "none";
  if (cliMode || !(await mainAllowsOverlays())) {
    await hideDshWindow();
    await hideChromeBtn();
    return;
  }
  let wdw = await dshWindow();
  if (!wdw) {
    const main = getCurrentWindow();
    const box = (await frameOverlayBox()) ?? { x: 0, y: 0, w: 800, h: 600 };
    wdw = new WebviewWindow(DSH_LABEL, {
      url: appUrl,
      parent: main,
      decorations: false,
      skipTaskbar: true,
      resizable: false,
      shadow: false,
      focus: false,
      visible: false,
      x: Math.round(box.x),
      y: Math.round(box.y),
      width: Math.round(box.w),
      height: Math.round(box.h),
      zoomHotkeysEnabled: true,
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
    // Only on creation: re-arming later would fight a zoom the poll that keeps
    // the setting current has not caught up with yet.
    void invoke("restore_dsh_zoom");
    appendLog("> 已用独立窗口加载 Web UI（避免 iframe 跨站拦截插件）", "sys");
  }
  if (!dshFocusBound) {
    dshFocusBound = true;
    await wdw.onFocusChanged(({ payload: focused }) => {
      if (focused && !cliMode && !chromeOpen() && chromeBtnUp) {
        void raiseChromeBtn(true);
      }
    });
  }
  await syncDshBounds();
  await showDshWindow(wdw);
  overlaysReadyAt = Date.now();
  scheduleChromeBtn();
}

function launchCommandLabel(): string {
  return settings.source === "local"
    ? "node --import tsx/esm apps/cli/src/bin.ts web --no-open  (" + settings.localPath + ")"
    : "npx @deepseek-ai/dsh web --no-open";
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
    setStatus("running", "运行中 · " + publicAppUrl(s.url || appUrl));
  } else {
    appUrl = DEFAULT_APP_URL;
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
        showChrome();
        void hideDshWindow();
        void runOverlayOp(() => hideChromeBtn());
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
  await listen<string>("server:ready", (e) => {
    const next = (e.payload && String(e.payload).trim()) || DEFAULT_APP_URL;
    ready = true;
    const urlChanged = next !== appUrl;
    appUrl = next;
    setStatus("running", "运行中 · " + publicAppUrl(appUrl));
    void (async () => {
      if (urlChanged) await closeDshWindow();
      if (collapseChromeBeforeOverlay(cliMode)) collapseChromeBar();
      await showApp();
    })();
    appendLog("> 就绪：" + publicAppUrl(appUrl), "sys");
    refreshDshVersion();
  });
  await listen("server:timeout", () => {
    showStartupError("启动超时：90 秒内未检测到 Web UI 就绪，请检查 CLI 日志");
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
  await listen("app:withdraw", () => {
    withdrawn = true;
    void hideDshWindow();
    void hideChromeBtn();
  });
  await listen("app:restore", () => {
    withdrawn = false;
    if (ready && !cliMode) void showApp();
  });
  await listen("chrome:show", () => {
    showChrome();
  });
  await listen<DshTheme>("dsh:theme", (e) => {
    lastDshTheme = e.payload;
    if (cliMode || chromeOpen()) return;
    if (!chromeBtnUp) tryShowChromeBtn();
    else void syncChromeBtnBounds();
  });
  await listen("dsh:theme-request", () => {
    if (lastDshTheme) void emit("dsh:theme", lastDshTheme);
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
  setupChrome();
  try {
    settings = await invoke<AppSettings>("get_settings");
  } catch {
    /* keep defaults */
  }
  syncSourceControls();
  q("#version").textContent = "dsh …";
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
    const s = await invoke<any>("server_status");
    if (s.running) {
      appendLog("> 检测到 " + s.url + " 已有端口，等待 Web UI 就绪…", "sys");
    }
  } catch {
    /* start() will report its own errors */
  }
  await start();
});
