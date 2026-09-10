export type Box = { x: number; y: number; w: number; h: number };

export function sameBox(a: Box, b: Box, eps = 0.5): boolean {
  return (
    Math.abs(a.x - b.x) < eps &&
    Math.abs(a.y - b.y) < eps &&
    Math.abs(a.w - b.w) < eps &&
    Math.abs(a.h - b.h) < eps
  );
}

/** Hide the toolbar before the dsh overlay exists so it is created at final size. */
export function collapseChromeBeforeOverlay(cliMode: boolean): boolean {
  return !cliMode;
}

/** The × button is App-only; CLI keeps the toolbar so the tab switch stays reachable. */
export function chromeCloseHidesBar(cliMode: boolean): boolean {
  return !cliMode;
}

/** Focus changes must not move/resize overlays; that is what made startup flash. */
export function syncGeometryOnFocus(): boolean {
  return false;
}
