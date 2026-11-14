/**
 * Auto update check throttling.
 *
 * Kept free of Tauri/React imports so it can be unit tested in the node test
 * environment and reused by both the startup scheduler and the manual
 * "Check now" button.
 */

/** At most one automatic check per day. */
export const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

const STORAGE_KEY = "nuncio.updates.lastAutoCheck";

function getStorage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    // Storage can throw when disabled (some private/enterprise policies).
    return null;
  }
}

/**
 * True when a check should run. A missing/invalid timestamp means "never
 * checked"; a timestamp in the future means the clock moved backwards, so
 * checking again is the safer choice.
 */
export function isAutoCheckDue(
  lastCheckedAt: number | null,
  now: number = Date.now(),
  intervalMs: number = AUTO_CHECK_INTERVAL_MS,
): boolean {
  if (lastCheckedAt === null || !Number.isFinite(lastCheckedAt)) return true;
  if (lastCheckedAt > now) return true;
  return now - lastCheckedAt >= intervalMs;
}

/** Timestamp (epoch ms) of the last automatic/manual check, if any. */
export function readLastAutoCheck(): number | null {
  const storage = getStorage();
  if (!storage) return null;
  try {
    const raw = storage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const value = Number(raw);
    return Number.isFinite(value) ? value : null;
  } catch {
    return null;
  }
}

/**
 * Record a check attempt. Called before the network request so a failing
 * endpoint (for example before the first release is published) is retried at
 * most once per interval instead of on every launch/tick.
 */
export function markAutoCheckDone(now: number = Date.now()): void {
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.setItem(STORAGE_KEY, String(now));
  } catch {
    // Losing the throttle is harmless; the next run will check again.
  }
}
