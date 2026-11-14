/**
 * Open external URLs (websites, maps) in the OS browser with a browser-only
 * fallback so the module can be smoke-tested outside Tauri.
 */
import { isTauri } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { safeExternalUrl } from "./logic";

/**
 * Open a URL in the OS browser with a browser-only fallback so the module can
 * be smoke-tested outside Tauri. Only http(s) and mailto links are forwarded;
 * anything else (file:, javascript:, custom schemes from synced vCards) is
 * silently ignored so untrusted contact data cannot launch local handlers.
 */
export function openExternalUrl(url: string | null | undefined): void {
  const normalized = safeExternalUrl(url, { allowMailto: true });
  if (!normalized) return;
  if (isTauri()) {
    openUrl(normalized).catch(() => {
      try {
        window.open(normalized, "_blank", "noopener,noreferrer");
      } catch {
        // Popup blocked; nothing else we can do.
      }
    });
    return;
  }
  try {
    window.open(normalized, "_blank", "noopener,noreferrer");
  } catch {
    // Ignore popup blockers in the browser preview.
  }
}

/** Copy text to the clipboard, returning false when unavailable. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const copied = document.execCommand("copy");
    document.body.removeChild(area);
    return copied;
  } catch {
    return false;
  }
}
