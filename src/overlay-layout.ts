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

/** Focus changes must not move/resize overlays; that is what made startup flash. */
export function syncGeometryOnFocus(): boolean {
  return false;
}

/** Show the chrome button once the Session export capsule is measured, or after fallback. */
export function chromeBtnReadyToShow(
  avoid: { w: number; h: number } | null | undefined,
  waitedMs: number,
  fallbackMs: number,
): boolean {
  if (avoid && avoid.w > 0 && avoid.h > 0) return true;
  return waitedMs >= fallbackMs;
}
