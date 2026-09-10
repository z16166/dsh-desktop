import assert from "node:assert/strict";
import test from "node:test";
import {
  collapseChromeBeforeOverlay,
  sameBox,
  syncGeometryOnFocus,
} from "./overlay-layout.ts";

test("sameBox ignores sub-pixel jitter", () => {
  assert.equal(
    sameBox({ x: 10, y: 20, w: 100, h: 50 }, { x: 10.2, y: 20.1, w: 100.3, h: 49.8 }),
    true,
  );
  assert.equal(
    sameBox({ x: 10, y: 20, w: 100, h: 50 }, { x: 12, y: 20, w: 100, h: 50 }),
    false,
  );
});

test("app tab collapses chrome before creating the overlay", () => {
  assert.equal(collapseChromeBeforeOverlay(false), true);
  assert.equal(collapseChromeBeforeOverlay(true), false);
});

test("focus must not retrigger overlay geometry", () => {
  assert.equal(syncGeometryOnFocus(), false);
});
