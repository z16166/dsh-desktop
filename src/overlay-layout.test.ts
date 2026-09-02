import assert from "node:assert/strict";
import test from "node:test";
import {
  chromeBtnReadyToShow,
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

test("chrome button waits for the Session export capsule, then falls back", () => {
  assert.equal(chromeBtnReadyToShow(null, 0, 2500), false);
  assert.equal(chromeBtnReadyToShow({ w: 0, h: 0 }, 400, 2500), false);
  assert.equal(chromeBtnReadyToShow({ w: 111, h: 32 }, 100, 2500), true);
  assert.equal(chromeBtnReadyToShow(null, 2500, 2500), true);
});
