const AUTO = "自动（霞鹜文楷）";

/** What the font pane shows before the user asks to browse the catalog. */
export function currentFontStatus(name: string): string {
  return `当前字体：${name || AUTO}`;
}
