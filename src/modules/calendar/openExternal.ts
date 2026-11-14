import { isTauri } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { toast } from "@/components";
import { t } from "@/lib/i18n";

/** Schemes the calendar is allowed to hand to the OS. */
const ALLOWED_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);

/**
 * Normalize a user-supplied link into a safe absolute URL.
 *
 * Only `http:`, `https:` and `mailto:` URLs are accepted; a bare host such as
 * "www.example.com" is treated as HTTPS. Anything else (javascript:, file:,
 * data:, protocol-relative junk, …) returns null so callers can ignore it.
 */
export function normalizeExternalUrl(raw: string | null | undefined): string | null {
  const trimmed = (raw ?? "").trim();
  if (!trimmed) return null;

  // Add HTTPS when the string has no scheme. The explicit "www." case is the
  // common one (e.g. a pasted meeting link without a protocol).
  const withScheme = /^[a-z][a-z0-9+.-]*:/i.test(trimmed) ? trimmed : `https://${trimmed}`;

  let url: URL;
  try {
    url = new URL(withScheme);
  } catch {
    return null;
  }

  if (!ALLOWED_PROTOCOLS.has(url.protocol)) return null;
  if (url.protocol === "mailto:") {
    // `mailto:` with no recipient is useless; keep the rest of the address.
    return url.pathname.trim() ? url.toString() : null;
  }
  if (!url.hostname) return null;
  return url.toString();
}

/** Open a URL in the system browser, with a browser-only fallback. */
export function openExternal(url: string | null | undefined): void {
  const trimmed = (url ?? "").trim();
  if (!trimmed) return;

  const normalized = normalizeExternalUrl(trimmed);
  if (!normalized) {
    toast.error(t("calendar.toast.unsupportedLink"));
    return;
  }

  if (isTauri()) {
    openUrl(normalized).catch(() => {
      try {
        window.open(normalized, "_blank", "noopener,noreferrer");
      } catch {
        // Popups can be blocked; nothing else to do.
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

/** Web maps link for a free-form location string. */
export function mapUrlFor(location: string): string {
  return `https://www.bing.com/maps?q=${encodeURIComponent(location)}`;
}
