import assert from "node:assert/strict";
import test from "node:test";
import { filterFontFamilies } from "./font-filter.ts";

test("empty query keeps every family", () => {
  assert.deepEqual(filterFontFamilies(["Arial", "宋体"], "  "), ["Arial", "宋体"]);
});

test("filter is a case-insensitive substring", () => {
  assert.deepEqual(
    filterFontFamilies(["Consolas", "Comic Sans MS", "宋体"], "con"),
    ["Consolas"],
  );
  assert.deepEqual(filterFontFamilies(["Arial", "宋体"], "宋"), ["宋体"]);
});
