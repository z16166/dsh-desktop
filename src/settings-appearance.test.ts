import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import {
  bootTheme,
  resolvedTheme,
  settingsFontFamily,
  themeSourceFromProbe,
} from "./settings-appearance.ts";

test("the settings window uses the selected family, or the bundled face when none is chosen", () => {
  assert.equal(
    settingsFontFamily(""),
    '"霞鹜文楷等宽 GB 屏幕阅读版", "LXGW WenKai Mono GB Screen", "Microsoft YaHei", sans-serif',
  );
  assert.equal(
    settingsFontFamily("Consolas"),
    '"Consolas", "Microsoft YaHei", sans-serif',
  );
});

test("the settings theme follows the harness preference, including follow-system", () => {
  assert.equal(themeSourceFromProbe("dark", false), "dark");
  assert.equal(themeSourceFromProbe("light", true), "light");
  assert.equal(themeSourceFromProbe("system", true), "system");
  assert.equal(themeSourceFromProbe("", true), "dark");
  assert.equal(themeSourceFromProbe("", false), "light");
  assert.equal(resolvedTheme("dark", false), "dark");
  assert.equal(resolvedTheme("light", true), "light");
  assert.equal(resolvedTheme("system", true), "dark");
  assert.equal(resolvedTheme("system", false), "light");
  assert.equal(bootTheme("dark", false), "dark");
  assert.equal(bootTheme("light", true), "light");
  assert.equal(bootTheme(undefined, true), "dark");
  assert.equal(bootTheme(undefined, false), "light");
});

test("settings paints the harness palette before the stylesheet arrives", () => {
  const html = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  const head = html.split("<body>")[0];
  assert.match(head, /__DSH_SETTINGS_THEME__/);
  assert.match(head, /background:\s*#0e1116/);
  assert.match(head, /data-theme="light"/);
  const styleAt = head.indexOf("<style>");
  const moduleAt = head.indexOf('type="module"');
  assert.ok(styleAt >= 0 && moduleAt > styleAt, "the first paint color is inline, ahead of the module");
});
