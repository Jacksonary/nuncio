/**
 * Human-friendly messages for backend `IpcError`s and updater failures.
 *
 * The backend error codes are part of the IPC contract (`IpcError.code`).
 * Mapped codes get a localized explanation; `invalidInput` additionally shows
 * the backend detail because it is usually actionable (e.g. "invalid port").
 */
import { isIpcError } from "@/ipc/api";

export type Translate = (key: string, vars?: Record<string, string | number>) => string;

const CODE_KEYS: Record<string, string> = {
  auth: "settings.errors.auth",
  network: "settings.errors.network",
  protocol: "settings.errors.protocol",
  invalidInput: "settings.errors.invalidInput",
  conflict: "settings.errors.conflict",
  unsupported: "settings.errors.unsupported",
  cancelled: "settings.errors.cancelled",
  notFound: "settings.errors.notFound",
  io: "settings.errors.io",
  db: "settings.errors.db",
  internal: "settings.errors.internal",
};

/** Localized description of any thrown value. */
export function describeError(error: unknown, t: Translate): string {
  if (isIpcError(error)) {
    const key = CODE_KEYS[error.code];
    if (key) {
      const base = t(key);
      return error.code === "invalidInput" && error.message
        ? `${base}：${error.message}`
        : base;
    }
    return error.message || t("settings.errors.generic");
  }
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string" && error) return error;
  return t("settings.errors.generic");
}

/** True when an IpcError uses the given code. */
export function isErrorCode(error: unknown, code: string): boolean {
  return isIpcError(error) && error.code === code;
}

/**
 * Updater failures are not IpcErrors: they surface as strings from the Tauri
 * updater plugin (e.g. when no public key was configured at build time).
 */
export function describeUpdaterError(error: unknown, t: Translate): string {
  const message = error instanceof Error ? error.message : String(error ?? "");
  const lower = message.toLowerCase();
  if (
    lower.includes("pubkey") ||
    lower.includes("public key") ||
    lower.includes("publickey") ||
    lower.includes("signature")
  ) {
    return t("settings.general.updaterNoPubkey");
  }
  if (lower.includes("endpoint") || lower.includes("404") || lower.includes("network")) {
    return t("settings.general.updaterNetwork");
  }
  return message || t("settings.general.updaterFailed");
}
