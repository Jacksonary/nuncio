/**
 * Tiny i18n runtime (no external dependency).
 *
 * - `t(key, vars)` returns the current language's string with `{var}`
 *   interpolation, falling back to zh-CN and finally the key itself.
 * - `useT()` returns a t function that re-renders the component when the
 *   language changes.
 * - `setLanguage("system" | "zh-CN" | "en-US")` resolves "system" against
 *   `navigator.language` and persists the preference.
 */
import { useCallback, useMemo, useSyncExternalStore } from "react";
import { dictionaries, type UiLanguage } from "@/i18n";

const STORAGE_KEY = "nuncio.language";

export type LanguageInput = UiLanguage | "system" | null | undefined;

const listeners = new Set<() => void>();
let current: UiLanguage = readInitialLanguage();

function readStorage(): string | null {
  try {
    if (typeof localStorage === "undefined") return null;
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function navigatorLanguage(): UiLanguage {
  const raw =
    typeof navigator !== "undefined"
      ? navigator.language || (navigator.languages && navigator.languages[0])
      : "";
  return raw && raw.toLowerCase().startsWith("zh") ? "zh-CN" : "en-US";
}

function readInitialLanguage(): UiLanguage {
  const stored = readStorage();
  if (stored === "zh-CN" || stored === "en-US") return stored;
  if (stored === "system") return navigatorLanguage();
  return navigatorLanguage();
}

/** Resolve a settings language value to a concrete UI language. */
export function resolveLanguage(language: LanguageInput): UiLanguage {
  if (language === "zh-CN" || language === "en-US") return language;
  return navigatorLanguage();
}

export function getLanguage(): UiLanguage {
  return current;
}

/** Locale identifier suitable for Intl formatters. */
export function getLocale(): string {
  return current;
}

export function setLanguage(language: LanguageInput): void {
  const next = resolveLanguage(language);
  try {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem(
        STORAGE_KEY,
        language === "system" ? "system" : next,
      );
    }
  } catch {
    // Storage can be unavailable (private mode); language still applies.
  }
  const changed = next !== current;
  current = next;
  if (typeof document !== "undefined") {
    document.documentElement.lang = next;
  }
  if (changed) {
    for (const listener of listeners) listener();
  }
}

function interpolate(template: string, vars?: Record<string, string | number>): string {
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : match,
  );
}

function translate(
  language: UiLanguage,
  key: string,
  vars?: Record<string, string | number>,
): string {
  const value =
    dictionaries[language][key] ?? dictionaries["zh-CN"][key] ?? key;
  return interpolate(value, vars);
}

/** Translate a key in the current language. */
export function t(key: string, vars?: Record<string, string | number>): string {
  return translate(current, key, vars);
}

/**
 * Translate a value that may be an i18n key.
 *
 * Backend discovery results carry a hint that is either plain text or an i18n
 * key prefixed with `discovery.` (see `DiscoveryResult.note`). Keys that are
 * not present in the dictionaries are returned unchanged so raw text from the
 * backend is never swallowed.
 */
export function translateMaybeKey(textOrKey: string | null | undefined): string {
  if (!textOrKey) return "";
  if (!textOrKey.startsWith("discovery.")) return textOrKey;
  const languageValue =
    (Object.prototype.hasOwnProperty.call(dictionaries[current], textOrKey)
      ? dictionaries[current][textOrKey]
      : undefined) ??
    (Object.prototype.hasOwnProperty.call(dictionaries["zh-CN"], textOrKey)
      ? dictionaries["zh-CN"][textOrKey]
      : undefined);
  return languageValue ?? textOrKey;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getSnapshot(): UiLanguage {
  return current;
}

/** React hook: a t() bound to the current language, re-rendering on change. */
export function useT(): (key: string, vars?: Record<string, string | number>) => string {
  const language = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return useMemo(
    () => (key: string, vars?: Record<string, string | number>) =>
      translate(language, key, vars),
    [language],
  );
}

/** React hook for the active language identifier. */
export function useLanguage(): UiLanguage {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** Stable callback form for hot paths outside render. */
export function useTranslateStable(): typeof t {
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return useCallback(t, []);
}
