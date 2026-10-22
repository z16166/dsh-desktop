import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { mountFontPicker } from "./font";
import { type LogEntry, type LogKind, type StatusEvent, type StatusState } from "./shell-events";
import { bootTheme, resolvedTheme, themeSourceFromProbe, type ThemeSource } from "./settings-appearance";

const DEFAULT_APP_URL = "http://127.0.0.1:3080";

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

type UpgradeResult = {
  ok: boolean;
  version: string;
  restarted: boolean;
  message: string;
};

let settings: AppSettings = {
  source: "local",
  localPath: "H:\\github\\deepseek-harness",
};

let upgrading = false;
let lastState: StatusState | "" = "";

function q<T extends HTMLElement>(sel: string): T {
  return document.querySelector(sel) as T;
}

const log = q<HTMLPreElement>("#log");
const dot = q<HTMLSpanElement>("#status-dot");
const statusText = q<HTMLSpanElement>("#status-text");

function launchCommandLabel(): string {
  return settings.source === "local"
    ? "node --import tsx/esm apps/cli/src/bin.ts web --no-open  (" + settings.localPath + ")"
    : "npx @deepseek-ai/dsh web --no-open";
}

function setStatus(state: StatusState, text: string): void {
  dot.className = "dot " + state;
  statusText.textContent = text;
}

function onStatus(state: StatusState, text: string): void {
  const becameRunning = state === "running" && lastState !== "running";
  lastState = state;
  setStatus(state, text);
  if (becameRunning) void refreshDshVersion();
}

async function note(text: string, kind: LogKind = "sys"): Promise<void> {
  await emit("shell:note", { kind, text });
}

function syncSourceControls(): void {
  const select = q<HTMLSelectElement>("#source-select");
  const path = q<HTMLInputElement>("#local-path");
  select.value = settings.source;
  path.value = settings.localPath;
  path.hidden = settings.source !== "local";
}

function setSourceDisabled(disabled: boolean): void {
  q<HTMLButtonElement>("#btn-upgrade").disabled = disabled;
  q<HTMLSelectElement>("#source-select").disabled = disabled;
  q<HTMLInputElement>("#local-path").disabled = disabled;
}

async function refreshDshVersion(): Promise<void> {
  const el = q<HTMLSpanElement>("#version");
  try {
    const version = await invoke<string>("dsh_version");
    const suffix = settings.source === "local" ? " · 本地" : "";
    el.textContent = version && version !== "unknown" ? "dsh v" + version + suffix : "dsh 未知";
  } catch {
    el.textContent = "dsh 未知";
  }
}

async function start(): Promise<void> {
  let already = false;
  try {
    already = (await invoke<ServerStatus>("server_status")).running;
  } catch {
    already = false;
  }
  const label = launchCommandLabel();
  await note("$ " + label);
  if (already) {
    await note("> 服务已在运行");
    return;
  }
  await emit("shell:starting", { message: "正在启动 " + label + " …" });
  try {
    const status = await invoke<ServerStatus>("start_server");
    await note("> 等待端口 " + status.port + " 就绪…");
  } catch (e) {
    await emit("shell:start-failed", { message: "启动失败：" + String(e) });
  }
}

async function stop(): Promise<void> {
  await note("$ 停止服务…");
  await emit("shell:stopping");
  try {
    const status = await invoke<ServerStatus>("stop_server");
    if (status.running) {
      await emit("shell:stop-cancelled");
      await note("> 该服务非本应用启动，未停止");
    }
  } catch (e) {
    await emit("shell:stop-cancelled");
    await note("> 停止失败：" + String(e), "err");
  }
}

async function restart(): Promise<void> {
  await stop();
  await start();
}

async function upgrade(): Promise<void> {
  if (upgrading) return;
  upgrading = true;
  setSourceDisabled(true);
  await note(
    settings.source === "local"
      ? "> 正在从本地仓库升级（git pull → pnpm install → pnpm run build）…"
      : "> 正在检查最新版本并安装…",
  );
  try {
    const result = await invoke<UpgradeResult>("upgrade_dsh");
    if (result.ok) {
      await note("> 已安装版本 " + result.version + " · " + result.message);
      void refreshDshVersion();
    } else {
      await note("> 升级失败：" + result.message, "err");
    }
  } catch (e) {
    await note("> 升级失败：" + String(e), "err");
  } finally {
    upgrading = false;
    setSourceDisabled(false);
  }
}

async function persistSettings(next: AppSettings, restartAfter: boolean): Promise<void> {
  try {
    settings = await invoke<AppSettings>("set_settings", { settings: next });
    syncSourceControls();
    await note(
      "> 启动源已设为 " +
        (settings.source === "local" ? "本地源码 · " + settings.localPath : "npx"),
    );
    if (restartAfter) await restart();
  } catch (e) {
    await note("> 保存启动源失败：" + String(e), "err");
    syncSourceControls();
  }
}

function setupSourceControls(): void {
  const select = q<HTMLSelectElement>("#source-select");
  const path = q<HTMLInputElement>("#local-path");
  select.addEventListener("change", () => {
    if (upgrading) {
      syncSourceControls();
      return;
    }
    const next: AppSettings = {
      source: select.value === "local" ? "local" : "npx",
      localPath: path.value.trim() || settings.localPath,
    };
    void persistSettings(next, true);
  });
  const applyPath = () => {
    if (upgrading || settings.source !== "local") return;
    const localPath = path.value.trim();
    if (!localPath || localPath === settings.localPath) return;
    void persistSettings({ source: "local", localPath }, true);
  };
  path.addEventListener("change", applyPath);
  path.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      applyPath();
    }
  });
}

const seen = new Set<number>();
const pending: LogEntry[] = [];
let historyApplied = false;

function addEntry(entry: LogEntry): void {
  if (!entry || seen.has(entry.id)) return;
  seen.add(entry.id);
  const span = document.createElement("span");
  span.className = entry.kind;
  span.dataset.id = String(entry.id);
  span.textContent = entry.text + "\n";
  log.appendChild(span);
  while (log.childElementCount > 2000) {
    const first = log.firstElementChild as HTMLElement | null;
    if (!first) break;
    const id = Number(first.dataset.id);
    if (Number.isFinite(id)) seen.delete(id);
    first.remove();
  }
  log.scrollTop = log.scrollHeight;
}

function clearView(): void {
  seen.clear();
  pending.length = 0;
  log.textContent = "";
}

async function copyLog(): Promise<void> {
  try {
    await navigator.clipboard.writeText(log.textContent || "");
    await note("> 已复制到剪贴板");
  } catch {
    await note("> 复制失败", "err");
  }
}

let themeSource: ThemeSource = "system";
const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");

function paintTheme(): void {
  document.documentElement.dataset.theme = resolvedTheme(themeSource, systemTheme.matches);
}

function startThemeMirror(): void {
  const preset = (window as Window & { __DSH_SETTINGS_THEME__?: unknown }).__DSH_SETTINGS_THEME__;
  // The head script already painted this. Resolving the default "system"
  // preference here would replace a dark harness with a light OS scheme
  // until the first probe came back.
  let holdPalette = preset === "light" || preset === "dark";
  if (holdPalette) {
    document.documentElement.dataset.theme = bootTheme(preset, systemTheme.matches);
  }
  systemTheme.addEventListener("change", () => {
    if (!holdPalette) paintTheme();
  });
  void listen<{ source?: string; dark?: boolean }>("shell:theme", (e) => {
    themeSource = themeSourceFromProbe(e.payload.source ?? "", !!e.payload.dark);
    holdPalette = false;
    paintTheme();
  }).catch(() => {
    /* outside the shell there is no event bus */
  });
  const pull = () => {
    void invoke("sync_dsh_theme").catch(() => {});
  };
  pull();
  window.setInterval(pull, 1500);
}

async function setupLog(): Promise<void> {
  await listen<LogEntry>("shell:log", (e) => {
    if (!historyApplied) {
      pending.push(e.payload);
      return;
    }
    addEntry(e.payload);
  });
  await listen<LogEntry[]>("shell:log-history", (e) => {
    if (historyApplied) return;
    historyApplied = true;
    log.replaceChildren();
    seen.clear();
    for (const entry of e.payload ?? []) addEntry(entry);
    for (const entry of pending) addEntry(entry);
    pending.length = 0;
  });
  await listen("shell:log-cleared", () => {
    clearView();
    historyApplied = true;
  });
  await listen<StatusEvent>("shell:status", (e) => {
    onStatus(e.payload.state, e.payload.text);
  });

  const ask = () => {
    void emit("settings:hello");
  };
  ask();
  const retry = window.setInterval(() => {
    if (historyApplied) {
      window.clearInterval(retry);
      return;
    }
    ask();
  }, 250);
  window.setTimeout(() => window.clearInterval(retry), 4000);
}

async function loadSnapshot(): Promise<void> {
  try {
    settings = await invoke<AppSettings>("get_settings");
  } catch {
    /* keep defaults; the dialog still has to render outside the shell */
  }
  syncSourceControls();
  try {
    const status = await invoke<ServerStatus>("server_status");
    if (!lastState) {
      if (status.running) onStatus("running", "运行中 · " + publicAppUrl(status.url || DEFAULT_APP_URL));
      else onStatus("stopped", "已停止");
    }
  } catch {
    if (!lastState) onStatus("stopped", "未连接");
  }
  void refreshDshVersion();
}

window.addEventListener("DOMContentLoaded", () => {
  startThemeMirror();
  setupSourceControls();
  q("#btn-start").addEventListener("click", () => void start());
  q("#btn-stop").addEventListener("click", () => void stop());
  q("#btn-restart").addEventListener("click", () => void restart());
  q("#btn-upgrade").addEventListener("click", () => void upgrade());
  q("#btn-clear").addEventListener("click", () => void emit("shell:clear"));
  q("#btn-copy").addEventListener("click", () => void copyLog());
  mountFontPicker({
    onApplied(name) {
      void note("> 字体已设为 " + (name || "自动（霞鹜文楷）"));
    },
  });
  void setupLog().catch(() => {
    /* outside the shell there is no event bus */
  });
  void loadSnapshot();
});
