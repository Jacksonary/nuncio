import { describe, expect, it } from "vitest";
import {
  AUTO_CHECK_INTERVAL_MS,
  isAutoCheckDue,
  readLastAutoCheck,
} from "../lib/updateThrottle";

describe("auto update throttle", () => {
  const now = Date.UTC(2026, 10, 13, 12, 0, 0);

  it("checks when no previous attempt was recorded", () => {
    expect(isAutoCheckDue(null, now)).toBe(true);
    expect(isAutoCheckDue(Number.NaN, now)).toBe(true);
  });

  it("waits until the 24 hour window has elapsed", () => {
    expect(isAutoCheckDue(now - 60 * 60 * 1000, now)).toBe(false);
    expect(isAutoCheckDue(now - AUTO_CHECK_INTERVAL_MS + 1000, now)).toBe(false);
    expect(isAutoCheckDue(now - AUTO_CHECK_INTERVAL_MS, now)).toBe(true);
    expect(isAutoCheckDue(now - 48 * 60 * 60 * 1000, now)).toBe(true);
  });

  it("checks again when the clock moved backwards", () => {
    expect(isAutoCheckDue(now + 60 * 60 * 1000, now)).toBe(true);
  });

  it("supports a custom interval", () => {
    expect(isAutoCheckDue(now - 5000, now, 10_000)).toBe(false);
    expect(isAutoCheckDue(now - 10_000, now, 10_000)).toBe(true);
  });

  it("returns null outside a browser storage environment", () => {
    // The vitest node environment has no localStorage.
    expect(readLastAutoCheck()).toBeNull();
  });
});
