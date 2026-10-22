export type ThemeSource = "light" | "dark" | "system";

/** Same faces the harness injection probes when the user leaves the font on automatic. */
export const AUTO_FONT_FAMILY =
  '"霞鹜文楷等宽 GB 屏幕阅读版", "LXGW WenKai Mono GB Screen", "Microsoft YaHei", sans-serif';

export function settingsFontFamily(name: string): string {
  if (!name) return AUTO_FONT_FAMILY;
  return `${JSON.stringify(name)}, "Microsoft YaHei", sans-serif`;
}

/** Harness publishes `light`, `dark`, or `system` on `html[data-ds-theme-source]`. */
export function themeSourceFromProbe(source: string, dark: boolean): ThemeSource {
  if (source === "light" || source === "dark" || source === "system") return source;
  return dark ? "dark" : "light";
}

/**
 * Color for the first paint. A harness palette already resolved to light or
 * dark wins over this machine, so the window does not open in the OS scheme
 * and then repaint.
 */
export function bootTheme(preset: unknown, systemDark: boolean): "light" | "dark" {
  if (preset === "light" || preset === "dark") return preset;
  return systemDark ? "dark" : "light";
}

/** `system` follows this machine, the same way the harness page resolves it. */
export function resolvedTheme(source: ThemeSource, systemDark: boolean): "light" | "dark" {
  if (source === "dark") return "dark";
  if (source === "light") return "light";
  return systemDark ? "dark" : "light";
}
