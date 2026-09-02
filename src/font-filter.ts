/** Substring match, case-insensitive. Empty query keeps the whole list. */
export function filterFontFamilies(names: string[], query: string): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return names;
  return names.filter((name) => name.toLowerCase().includes(q));
}
