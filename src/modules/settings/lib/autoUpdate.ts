/**
 * Background update checks.
 *
 * `updates.autoCheck` used to be a persisted switch without a consumer. The
 * settings module entry point (imported by the shell at startup) calls
 * `startAutoUpdateCheck()`, so a check runs even while the settings flyout is
 * closed. Only the main window schedules checks; message/compose windows share
 * the same JS process but must not query the updater.
 *
 * Policy (PLAN.md §10, round-2 fix list):
 *  - while `updates.autoCheck` is enabled: check ~30 s after app start and
 *    every 24 h afterwards;
 *  - an available release surfaces a toast with "Update now"
 *    (download + install + relaunch) and "Later";
 *  - every failure path is silent (console only): automatic checks are
 *    unrequested background work and must not nag the user.
 *
 * Manual "check now" stays in GeneralPage because it reports inline progress;
 * it shares the throttle timestamp with this module so the two paths do not
 * duplicate work.
 */
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { Update } from "@tauri-apps/plugin-updater";
import { toast } from "@/components";
import { settings as settingsApi } from "@/ipc/api";
import { t } from "@/lib/i18n";
import { relaunchApp } from "./desktop";
import {
  AUTO_CHECK_INTERVAL_MS,
  isAutoCheckDue,
  markAutoCheckDone,
  readLastAutoCheck,
} from "./updateThrottle";

/** Wait for startup work (settings, accounts, sync) before hitting the network. */
export const AUTO_CHECK_START_DELAY_MS = 30_000;
/** Re-evaluate the 24 h window while the app sits in the tray. */
const RECHECK_INTERVAL_MS = AUTO_CHECK_INTERVAL_MS;
/** Tauri window label of the primary window (tauri.conf.json). */
export const MAIN_WINDOW_LABEL = "main";

interface AutoUpdateWindow extends Window {
  __nuncioAutoUpdateStarted?: boolean;
}

export interface AutoUpdateCheckOptions {
  /**
   * The caller just enabled the setting and knows the user wants a check, so
   * skip reading settings (which may still contain the previous value).
   */
  assumeEnabled?: boolean;
  /** Ignore the 24 h throttle. */
  force?: boolean;
}

let activeCheck: Promise<void> | null = null;
let notifiedVersion: string | null = null;

/**
 * True in the primary window only. Secondary windows (message, compose) must
 * not install updates, so every entry point checks this.
 */
export function isMainWindow(): boolean {
  if (!isTauri()) return false;
  try {
    return getCurrentWindow().label === MAIN_WINDOW_LABEL;
  } catch {
    return false;
  }
}

function logAutoCheckFailure(error: unknown): void {
  // Deliberately not a toast: automatic checks are unrequested background
  // work, and a missing latest.json is normal before the first release.
  console.warn("[settings] automatic update check failed", error);
}

async function installAndRelaunch(update: Update): Promise<void> {
  const progressToast = toast.info(
    t("settings.general.autoUpdateDownloading"),
    t("settings.general.autoUpdateDownloadingDesc"),
    { duration: 0 },
  );
  try {
    await update.downloadAndInstall();
    toast.dismiss(progressToast);
    await relaunchApp();
  } catch (error) {
    toast.dismiss(progressToast);
    // Silent on errors; the app stays open and usable.
    logAutoCheckFailure(error);
  }
}

function showAvailable(update: Update): void {
  if (notifiedVersion === update.version) {
    void update.close().catch(() => undefined);
    return;
  }
  notifiedVersion = update.version;
  toast.info(
    t("settings.general.autoUpdateAvailable", { version: update.version }),
    t("settings.general.autoUpdateAvailableDesc"),
    {
      duration: 0,
      actions: [
        {
          label: t("settings.general.autoUpdateLater"),
          onClick: () => {
            // Let a later check surface the same release again.
            notifiedVersion = null;
            void update.close().catch(() => undefined);
          },
        },
        {
          label: t("settings.general.autoUpdateUpdateNow"),
          primary: true,
          onClick: () => {
            void installAndRelaunch(update);
          },
        },
      ],
    },
  );
}

async function runCheck(options: AutoUpdateCheckOptions): Promise<void> {
  if (!isMainWindow()) return;

  if (!options.assumeEnabled) {
    try {
      const current = await settingsApi.get();
      if (!current.updates.autoCheck) return;
    } catch (error) {
      // Settings are unavailable early in startup; do not record an attempt so
      // the next launch (or the next recheck tick) can try again.
      logAutoCheckFailure(error);
      return;
    }
  }

  if (!options.force && !isAutoCheckDue(readLastAutoCheck(), Date.now(), AUTO_CHECK_INTERVAL_MS)) {
    return;
  }

  // Record the attempt before the request so an unreachable endpoint is not
  // hammered on every launch.
  markAutoCheckDone();

  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const update = await check({ timeout: 30_000 });
    if (update) showAvailable(update);
  } catch (error) {
    logAutoCheckFailure(error);
  }
}

/**
 * Run a throttled check. Concurrent callers share one in-flight request.
 * Resolves when the attempt finishes; never rejects.
 */
export function runAutoUpdateCheck(options: AutoUpdateCheckOptions = {}): Promise<void> {
  if (activeCheck) return activeCheck;
  activeCheck = runCheck(options)
    .catch(logAutoCheckFailure)
    .finally(() => {
      activeCheck = null;
    });
  return activeCheck;
}

/**
 * Schedule the startup check plus the 24 h recheck timer. Safe to call multiple
 * times (the shell may be mounted once, but HMR re-evaluates modules); only the
 * first call in the main window starts timers. Secondary windows are no-ops.
 *
 * @returns cleanup function that stops the timers.
 */
export function startAutoUpdateCheck(delayMs: number = AUTO_CHECK_START_DELAY_MS): () => void {
  if (typeof window === "undefined" || !isMainWindow()) return () => undefined;
  const host = window as AutoUpdateWindow;
  if (host.__nuncioAutoUpdateStarted) return () => undefined;
  host.__nuncioAutoUpdateStarted = true;

  const initialTimer = window.setTimeout(() => {
    void runAutoUpdateCheck();
  }, Math.max(0, delayMs));
  const recheckTimer = window.setInterval(() => {
    void runAutoUpdateCheck();
  }, RECHECK_INTERVAL_MS);

  return () => {
    window.clearTimeout(initialTimer);
    window.clearInterval(recheckTimer);
    host.__nuncioAutoUpdateStarted = false;
  };
}
