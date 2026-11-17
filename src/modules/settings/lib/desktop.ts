/**
 * Thin wrappers around Tauri plugins. They are dynamically imported and only
 * called inside the desktop runtime so the UI still renders in a plain browser.
 */
import { isTauri } from "@tauri-apps/api/core";
import { t } from "@/lib/i18n";

export function isDesktop(): boolean {
  return isTauri();
}

export async function openExternal(url: string): Promise<void> {
  if (isTauri()) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
    return;
  }
  if (typeof window !== "undefined") {
    window.open(url, "_blank", "noopener,noreferrer");
  }
}

export async function copyText(value: string): Promise<boolean> {
  try {
    if (typeof navigator !== "undefined" && navigator.clipboard) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    return false;
  }
  return false;
}

export async function pickSavePath(defaultPath: string, title: string): Promise<string | null> {
  if (!isTauri()) return null;
  const { save } = await import("@tauri-apps/plugin-dialog");
  const path = await save({
    title,
    defaultPath,
    filters: [
      { name: t("settings.data.backupFilter"), extensions: ["nuncio-backup", "zip", "json"] },
      { name: t("settings.data.allFilesFilter"), extensions: ["*"] },
    ],
  });
  return path ?? null;
}

export async function pickOpenPath(title: string): Promise<string | null> {
  if (!isTauri()) return null;
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selected = await open({
    title,
    multiple: false,
    directory: false,
    filters: [
      { name: t("settings.data.backupFilter"), extensions: ["nuncio-backup", "zip", "json"] },
      { name: t("settings.data.allFilesFilter"), extensions: ["*"] },
    ],
  });
  return typeof selected === "string" ? selected : null;
}

export async function setAutostart(enabled: boolean): Promise<void> {
  if (!isTauri()) return;
  const { enable, disable } = await import("@tauri-apps/plugin-autostart");
  if (enabled) await enable();
  else await disable();
}

export async function relaunchApp(): Promise<void> {
  if (!isTauri()) return;
  const { relaunch } = await import("@tauri-apps/plugin-process");
  await relaunch();
}
