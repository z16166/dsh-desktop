import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// A font's own metrics must not change a row's border box. The list applies
// each family only when its row scrolls into view; a taller row changes
// scrollHeight mid-drag, and the scrollbar thumb then lags the pointer.
test("font rows keep a fixed height while their family is applied", () => {
  const css = readFileSync(new URL("./settings.css", import.meta.url), "utf8");
  const rule = css.match(/button\.font-row\s*\{([^}]*)\}/)?.[1];
  assert.ok(rule, "button.font-row rule");
  const px = (prop: string) => {
    const match = rule.match(new RegExp(`${prop}:\\s*(\\d+(?:\\.\\d+)?)px`));
    assert.ok(match, prop);
    return Number(match[1]);
  };
  const padding = rule.match(/padding:\s*(\d+(?:\.\d+)?)px\s+(\d+(?:\.\d+)?)px/);
  assert.ok(padding, "vertical padding");
  const border = rule.match(/border-bottom:\s*(\d+(?:\.\d+)?)px/);
  assert.ok(border, "border-bottom");
  // border-box height includes padding and the bottom border. The line box
  // has to fit in what remains, or a tall font clips and the next one does not.
  assert.equal(
    px("height"),
    Number(padding[1]) * 2 + px("line-height") + Number(border[1]),
  );
  assert.match(rule, /overflow:\s*hidden/);
  assert.match(rule, /white-space:\s*nowrap/);
});
