import { emit, listen } from "@tauri-apps/api/event";

type DshTheme = {
  dark: boolean;
  bg: string;
  fg: string;
  border: string;
};

function applyTheme(theme: DshTheme): void {
  const root = document.documentElement;
  root.style.setProperty("--bg", theme.bg);
  root.style.setProperty("--fg", theme.fg);
  root.style.setProperty("--border", theme.border);
}

document.getElementById("btn")?.addEventListener("click", () => {
  void emit("chrome:show");
});

void listen<DshTheme>("dsh:theme", (e) => {
  applyTheme(e.payload);
});
void emit("dsh:theme-request");
