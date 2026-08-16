import { emit } from "@tauri-apps/api/event";

document.getElementById("btn")?.addEventListener("click", () => {
  void emit("chrome:show");
});
