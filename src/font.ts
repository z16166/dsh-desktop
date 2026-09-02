import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { filterFontFamilies } from "./font-filter";

const AUTO_LABEL = "（自动：霞鹜文楷）";
const PREVIEW_SAMPLE = "DeepSeek Harness 字体预览 0123456789 The quick brown fox";

const search = document.getElementById("search") as HTMLInputElement;
const status = document.getElementById("status") as HTMLParagraphElement;
const list = document.getElementById("list") as HTMLDivElement;
const preview = document.getElementById("preview") as HTMLDivElement;
const applyBtn = document.getElementById("apply") as HTMLButtonElement;
const cancelBtn = document.getElementById("cancel") as HTMLButtonElement;

let families: string[] = [];
let selected = "";
let observer: IntersectionObserver | null = null;

function previewFamily(name: string): void {
  preview.style.fontFamily = name ? JSON.stringify(name) : "";
  preview.textContent = name ? PREVIEW_SAMPLE : `${AUTO_LABEL}\n${PREVIEW_SAMPLE}`;
}

function paint(): void {
  observer?.disconnect();
  list.replaceChildren();
  const shown = filterFontFamilies(families, search.value);

  const auto = document.createElement("button");
  auto.type = "button";
  auto.className = "row";
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
    row.className = "row";
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
}

function pick(name: string): void {
  selected = name;
  previewFamily(name);
  for (const row of list.querySelectorAll<HTMLElement>("button.row")) {
    row.setAttribute("aria-selected", (row.dataset.font ?? "") === name ? "true" : "false");
  }
}

async function closePicker(): Promise<void> {
  await getCurrentWindow().close();
}

search.addEventListener("input", () => paint());
cancelBtn.addEventListener("click", () => {
  void closePicker();
});
applyBtn.addEventListener("click", () => {
  void (async () => {
    await invoke("set_dsh_font", { name: selected });
    await closePicker();
  })();
});

void (async () => {
  try {
    selected = await invoke<string>("get_dsh_font");
    families = await invoke<string[]>("list_system_fonts");
  } catch (e) {
    status.textContent = "无法读取系统字体：" + String(e);
    previewFamily(selected);
    return;
  }
  previewFamily(selected);
  paint();
})();
