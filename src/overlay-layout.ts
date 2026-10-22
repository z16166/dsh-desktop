export type Box = { x: number; y: number; w: number; h: number };

export function sameBox(a: Box, b: Box, eps = 0.5): boolean {
  return (
    Math.abs(a.x - b.x) < eps &&
    Math.abs(a.y - b.y) < eps &&
    Math.abs(a.w - b.w) < eps &&
    Math.abs(a.h - b.h) < eps
  );
}

/** Focus changes must not move/resize overlays; that is what made startup flash. */
export function syncGeometryOnFocus(): boolean {
  return false;
}
