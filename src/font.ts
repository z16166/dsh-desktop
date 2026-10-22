import { invoke } from "@tauri-apps/api/core";
import { filterFontFamilies } from "./font-filter";
import { currentFontStatus } from "./font-status";

const AUTO_LABEL = "（自动：霞鹜文楷）";
const PREVIEW_SAMPLE = "DeepSeek Harness 字体预览 0123456789 The quick brown fox";

export function mountFontPicker(opts: { onApplied?: (name: string) => void } = {}): void {
  const search = document.getElementById("font-search") as HTMLInputElement;
  const status = document.getElementById("font-status") as HTMLParagraphElement;
  const list = document.getElementById("font-list") as HTMLDivElement;
  const preview = document.getElementById("font-preview") as HTMLDivElement;
  const applyBtn = document.getElementById("font-apply") as HTMLButtonElement;
  const loadBtn = document.getElementById("font-load") as HTMLButtonElement;

  let families: string[] = [];
  let selected = "";
  let observer: IntersectionObserver | null = null;
  let listed = false;
  let listing = false;

  function previewFamily(name: string): void {
    preview.style.fontFamily = name ? JSON.stringify(name) : "";
    preview.textContent = name ? PREVIEW_SAMPLE : `${AUTO_LABEL}\n${PREVIEW_SAMPLE}`;
  }

  function paint(scrollSelected: boolean): void {
    observer?.disconnect();
    list.replaceChildren();
    const shown = filterFontFamilies(families, search.value);

    const auto = document.createElement("button");
    auto.type = "button";
    auto.className = "font-row";
    auto.setAttribute("role", "option");
    auto.textContent = AUTO_LABEL;
    auto.dataset.font = "";
    auto.setAttribute("aria-selected", selected === "" ? "true" : "false");
    auto.addEventListener("click", () => pick(""));
    list.appendChild(auto);

    observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const el = entry.target as HTMLElement;
          const name = el.dataset.font;
          if (!name) continue;
          el.style.fontFamily = entry.isIntersecting ? JSON.stringify(name) : "";
        }
      },
      { root: list, rootMargin: "80px 0px" },
    );

    for (const name of shown) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "font-row";
      row.setAttribute("role", "option");
      row.textContent = name;
      row.dataset.font = name;
      row.setAttribute("aria-selected", selected === name ? "true" : "false");
      row.addEventListener("click", () => pick(name));
      list.appendChild(row);
      observer.observe(row);
    }

    if (families.length === 0) {
      status.textContent = "无法读取系统字体";
    } else if (shown.length === 0) {
      status.textContent = "没有匹配的字体";
    } else {
      status.textContent = `共 ${families.length} 种字体`;
    }

    if (scrollSelected) {
      list.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
    }
  }

  function pick(name: string): void {
    selected = name;
    previewFamily(name);
    for (const row of list.querySelectorAll<HTMLElement>("button.font-row")) {
      row.setAttribute("aria-selected", (row.dataset.font ?? "") === name ? "true" : "false");
    }
  }

  function showCurrent(): void {
    status.textContent = currentFontStatus(selected);
    previewFamily(selected);
  }

  async function loadFontList(): Promise<void> {
    if (listed || listing) return;
    listing = true;
    loadBtn.disabled = true;
    status.textContent = "正在读取系统字体…";
    try {
      families = await invoke<string[]>("list_system_fonts");
      listed = true;
      loadBtn.hidden = true;
      search.hidden = false;
      list.hidden = false;
      paint(true);
    } catch (e) {
      status.textContent = "无法读取系统字体：" + String(e);
      loadBtn.disabled = false;
    } finally {
      listing = false;
    }
  }

  search.addEventListener("input", () => {
    if (listed) paint(false);
  });
  loadBtn.addEventListener("click", () => void loadFontList());
  applyBtn.addEventListener("click", () => {
    void (async () => {
      applyBtn.disabled = true;
      try {
        await invoke("set_dsh_font", { name: selected });
        status.textContent = selected ? `已应用 ${selected}` : "已应用自动字体";
        opts.onApplied?.(selected);
      } catch (e) {
        status.textContent = "应用字体失败：" + String(e);
      } finally {
        applyBtn.disabled = false;
      }
    })();
  });

  void (async () => {
    try {
      selected = await invoke<string>("get_dsh_font");
    } catch (e) {
      if (!listed && !listing) {
        status.textContent = "无法读取当前字体：" + String(e);
        previewFamily("");
      }
      return;
    }
    if (listed) paint(true);
    else if (!listing) showCurrent();
    else previewFamily(selected);
  })();
}
