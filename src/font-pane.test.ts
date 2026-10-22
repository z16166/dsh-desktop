import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

test("the closed font pane names the current family and does not list fonts", async () => {
  const { currentFontStatus } = await import("./font-status.ts");
  assert.equal(currentFontStatus(""), "当前字体：自动（霞鹜文楷）");
  assert.equal(currentFontStatus("Consolas"), "当前字体：Consolas");
});

test("settings markup keeps the font catalog hidden until the user asks", () => {
  const html = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  assert.match(html, /id="font-load"/);
  assert.match(html, /id="font-search"[^>]*\bhidden\b/);
  assert.match(html, /id="font-list"[^>]*\bhidden\b/);
  assert.doesNotMatch(html, /正在读取系统字体/);
});

test("opening the font pane does not ask for the system font catalog", () => {
  const src = readFileSync(new URL("./font.ts", import.meta.url), "utf8");
  const beforeLoad = src.split("async function loadFontList(")[0];
  assert.ok(beforeLoad, "loadFontList");
  assert.doesNotMatch(beforeLoad, /list_system_fonts/);
  assert.match(src, /list_system_fonts/);
});
