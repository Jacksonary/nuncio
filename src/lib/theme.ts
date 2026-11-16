/**
 * Theme bootstrap + runtime application.
 *
 * The shell calls `bootstrapTheme()` synchronously in main.tsx *before* React
 * renders (from the last known settings cached in localStorage) so the first
 * paint already has the correct palette. Settings sync later re-applies the
 * authoritative values and mirrors them into the native window theme.
 */
import { isTauri } from "@tauri-apps/api/core";
import type { UnlistenFn } from "@tauri-apps/api/event";
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

/**
 * Apply a theme mode to <html data-theme> and cache it for next launch.
 *
 * `resolved` overrides the matchMedia lookup. Callers pass it when they
 * already know the effective theme from a more authoritative source — the
 * native window, or the payload of a Tauri theme-change event — because
 * `prefers-color-scheme` inside the webview can lag behind an OS change (or
 * still report the value it was pinned to).
 */
export function applyTheme(mode: ThemeMode, resolved?: ResolvedTheme): ResolvedTheme {
  const value = resolved ?? resolveTheme(mode);
  if (typeof document !== "undefined") {
    document.documentElement.dataset.theme = value;
  }
  storage()?.setItem(THEME_STORAGE_KEY, mode);
  return value;
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
 *
 * IMPORTANT: pass `null` for "system", never the *resolved* light/dark value.
 * `setTheme(null)` is what tells the runtime "follow the OS"; a concrete value
 * pins the window (and on Windows `ICoreWebView2Profile::SetPreferredColorScheme`)
 * to that scheme permanently. Once pinned, tao short-circuits the
 * `WM_SETTINGCHANGE` handler (`preferred_theme.is_some()` → return) so it never
 * emits `ThemeChanged`, and the webview's `prefers-color-scheme` media query is
 * frozen at the pinned value. That silently kills every theme-change listener,
 * including our own `watchSystemTheme`, and the UI stops following the OS while
 * the theme mode still reads "system" — a self-lock. Resolve the *display*
 * value, never the *native* value.
 */
export async function syncNativeTheme(mode: ThemeMode): Promise<void> {
  if (!isTauri()) return;
  try {
    await getCurrentWindow().setTheme(mode === "system" ? null : mode);
  } catch {
    // Not fatal: the CSS theme is already applied.
  }
}

/**
 * The OS theme as the native layer sees it right now, or null when unavailable
 * (plain browser, or the platform has no notion of a window theme).
 *
 * Preferred over `matchMedia` when un-pinning: a webview that was just released
 * from a forced scheme may keep reporting the old `prefers-color-scheme` for a
 * while, whereas the native window value is read straight from the OS.
 */
export async function nativeSystemTheme(): Promise<ResolvedTheme | null> {
  if (!isTauri()) return null;
  try {
    const theme = await getCurrentWindow().theme();
    return theme === "dark" ? "dark" : theme === "light" ? "light" : null;
  } catch {
    return null;
  }
}

/**
 * Resolve the effective theme after the native window has been switched back
 * to "follow system", preferring the native value and falling back to
 * matchMedia for the browser dev build.
 */
export async function resolveNativeSystemTheme(): Promise<ResolvedTheme> {
  const native = await nativeSystemTheme();
  return native ?? resolveTheme("system");
}

/**
 * Subscribe to OS theme changes (only meaningful for theme === "system").
 *
 * The Tauri window event is the primary source: once the window is no longer
 * pinned, the runtime emits it on every OS theme flip and its payload is the
 * authoritative new theme. `matchMedia` is only a fallback for the browser dev
 * build and for a failed native subscription — deliberately not both at once.
 * On Windows the runtime re-applies the OS scheme to the webview on each
 * `ThemeChanged` (`SetPreferredColorScheme`), so a `change` event can arrive a
 * tick later still describing the previous scheme and would overwrite the
 * correct value. One source, no race.
 *
 * Returns a cleanup function synchronously even though subscribing is async:
 * `onThemeChanged` resolves to an `UnlistenFn`, so disposal is tracked with a
 * flag and the handle is released as soon as it arrives after an early
 * cleanup. Without that, a consumer that unmounts before the IPC round-trip
 * finishes would leak a listener for the lifetime of the window.
 */
export function watchSystemTheme(listener: (theme?: ResolvedTheme) => void): () => void {
  let disposed = false;
  let unlistenTheme: UnlistenFn | null = null;
  let unlistenMedia: (() => void) | null = null;

  const subscribeMedia = () => {
    if (disposed) return;
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    if (unlistenMedia) return;
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = () => {
      if (!disposed) listener();
    };
    media.addEventListener("change", handler);
    unlistenMedia = () => media.removeEventListener("change", handler);
  };

  if (isTauri()) {
    getCurrentWindow()
      .onThemeChanged(({ payload }) => {
        if (!disposed) listener(payload === "dark" ? "dark" : "light");
      })
      .then((unlisten) => {
        if (disposed) {
          // The subscription outlived its caller: drop it immediately.
          unlisten();
          return;
        }
        unlistenTheme = unlisten;
      })
      .catch(() => {
        // Unsupported platform: fall through to matchMedia.
        subscribeMedia();
      });
  } else {
    subscribeMedia();
  }

  return () => {
    disposed = true;
    unlistenMedia?.();
    unlistenMedia = null;
    unlistenTheme?.();
    unlistenTheme = null;
  };
}
