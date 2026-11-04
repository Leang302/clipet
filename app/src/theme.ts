export type ThemeChoice = "system" | "light" | "dark";
const KEY = "clipflow-theme";

export function loadTheme(): ThemeChoice {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark" || v === "system") return v;
  } catch {
    // localStorage can throw in some webview configurations; fall back silently.
  }
  return "system";
}

export function applyTheme(theme: ThemeChoice) {
  const root = document.documentElement;
  if (theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", theme);
}

export function saveTheme(theme: ThemeChoice) {
  applyTheme(theme);
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // Best-effort persistence only.
  }
}

export const THEME_STORAGE_KEY = KEY;
