/**
 * Theme bootstrap + runtime application.
 *
 * The shell calls `bootstrapTheme()` synchronously in main.tsx *before* React
 * renders (from the last known settings cached in localStorage) so the first
 * paint already has the correct palette. Settings sync later re-applies the
 * authoritative values and mirrors them into the native window theme.
 */
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { ThemeMode } from "@/ipc/types";

export const THEME_STORAGE_KEY = "nuncio.theme";
export const ACCENT_STORAGE_KEY = "nuncio.accent";
export const DEFAULT_ACCENT = "#0f6cbd";

export type ResolvedTheme = "light" | "dark";

function storage(): Storage | null {
  try {
    if (typeof localStorage === "undefined") return null;
    return localStorage;
  } catch {
    return null;
  }
}

export function systemPrefersDark(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
    return false;
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

export function resolveTheme(mode: ThemeMode): ResolvedTheme {
  if (mode === "system") return systemPrefersDark() ? "dark" : "light";
  return mode === "dark" ? "dark" : "light";
}

/** Apply a theme mode to <html data-theme> and cache it for next launch. */
export function applyTheme(mode: ThemeMode): ResolvedTheme {
  const resolved = resolveTheme(mode);
  if (typeof document !== "undefined") {
    document.documentElement.dataset.theme = resolved;
  }
  storage()?.setItem(THEME_STORAGE_KEY, mode);
  return resolved;
}

export function readCachedThemeMode(): ThemeMode {
  const value = storage()?.getItem(THEME_STORAGE_KEY);
  return value === "light" || value === "dark" || value === "system" ? value : "system";
}

export function normalizeHexColor(value: string | null | undefined): string | null {
  if (!value) return null;
  const hex = value.trim();
  if (/^#[0-9a-fA-F]{6}$/.test(hex)) return hex.toLowerCase();
  if (/^#[0-9a-fA-F]{3}$/.test(hex)) {
    return `#${hex
      .slice(1)
      .split("")
      .map((c) => c + c)
      .join("")}`.toLowerCase();
  }
  return null;
}

function hexToRgb(hex: string): { r: number; g: number; b: number } {
  return {
    r: Number.parseInt(hex.slice(1, 3), 16),
    g: Number.parseInt(hex.slice(3, 5), 16),
    b: Number.parseInt(hex.slice(5, 7), 16),
  };
}

/** WCAG relative luminance, used to pick white or black text on the accent. */
export function accentContrastColor(hex: string): "#000000" | "#ffffff" {
  const { r, g, b } = hexToRgb(hex);
  const linear = [r, g, b].map((value) => {
    const channel = value / 255;
    return channel <= 0.04045
      ? channel / 12.92
      : Math.pow((channel + 0.055) / 1.055, 2.4);
  });
  const luminance = 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
  return luminance > 0.42 ? "#000000" : "#ffffff";
}

/** Apply the accent color (null = default Nuncio blue) and cache it. */
export function applyAccent(accent: string | null | undefined): string {
  const normalized = normalizeHexColor(accent) ?? DEFAULT_ACCENT;
  if (typeof document !== "undefined") {
    const root = document.documentElement;
    root.style.setProperty("--accent", normalized);
    root.style.setProperty("--accent-text", accentContrastColor(normalized));
    root.style.setProperty("--text-on-accent", accentContrastColor(normalized));
  }
  if (normalized === DEFAULT_ACCENT && !accent) {
    storage()?.removeItem(ACCENT_STORAGE_KEY);
  } else {
    storage()?.setItem(ACCENT_STORAGE_KEY, normalized);
  }
  return normalized;
}

/** Run once from main.tsx before the first React paint. */
export function bootstrapTheme(): void {
  applyTheme(readCachedThemeMode());
  applyAccent(storage()?.getItem(ACCENT_STORAGE_KEY));
}

/**
 * Mirror the theme into the native window so the webview's
 * prefers-color-scheme (and OS chrome) follow the app setting, including
 * inside mail iframes. Errors (e.g. running in a plain browser) are ignored.
 */
export async function syncNativeTheme(mode: ThemeMode): Promise<void> {
  if (!isTauri()) return;
  try {
    await getCurrentWindow().setTheme(resolveTheme(mode));
  } catch {
    // Not fatal: the CSS theme is already applied.
  }
}

/** Subscribe to OS theme changes (only meaningful for theme === "system"). */
export function watchSystemTheme(listener: () => void): () => void {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
    return () => undefined;
  }
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const handler = () => listener();
  media.addEventListener("change", handler);
  return () => media.removeEventListener("change", handler);
}
