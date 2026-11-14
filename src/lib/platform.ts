/**
 * Platform detection. `navigator` is enough to pick the shortcut modifier
 * before the backend answers; app.info refines it at runtime.
 */
import type { AppInfo } from "@/ipc/types";

export type Platform = AppInfo["platform"];

let current: Platform = detectPlatform();

function detectPlatform(): Platform {
  if (typeof navigator === "undefined") return "windows";
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  const raw = (
    nav.userAgentData?.platform ||
    nav.platform ||
    nav.userAgent ||
    ""
  ).toLowerCase();
  if (raw.includes("mac")) return "macos";
  if (raw.includes("win")) return "windows";
  if (raw.includes("linux") || raw.includes("x11")) return "linux";
  return "windows";
}

export function getPlatform(): Platform {
  return current;
}

/** Called by the settings sync hook once `app.info` resolves. */
export function setPlatform(platform: Platform | null | undefined): void {
  if (platform === "windows" || platform === "macos" || platform === "linux") {
    current = platform;
  }
}

export function isMac(): boolean {
  return current === "macos";
}

export function isWindows(): boolean {
  return current === "windows";
}

export function isLinux(): boolean {
  return current === "linux";
}

/** Human label for the primary modifier key. */
export function modKeyLabel(): string {
  return isMac() ? "⌘" : "Ctrl";
}
