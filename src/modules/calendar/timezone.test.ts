import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  formatIsoWithZoneOffset,
  formatZoneOffset,
  isValidTimeZone,
  wallClockDate,
  zoneOffsetMs,
  zonedDateTimeToDate,
  zonedParts,
} from "@/modules/calendar/dates";

type NodeProcess = { env: Record<string, string | undefined> };

// `@types/node` is not part of the frontend tsconfig, so reach the process
// env through a narrow cast (tests run under Node via Vitest).
const nodeProcess = (globalThis as unknown as { process?: NodeProcess }).process;
const originalTimeZone = nodeProcess?.env.TZ;

function setProcessTimeZone(zone: string | undefined): void {
  if (!nodeProcess) return;
  if (zone === undefined) delete nodeProcess.env.TZ;
  else nodeProcess.env.TZ = zone;
}

describe("IANA time zone helpers", () => {
  beforeAll(() => {
    setProcessTimeZone("UTC");
  });

  afterAll(() => {
    setProcessTimeZone(originalTimeZone);
  });

  it("converts a Shanghai wall clock to the matching instant", () => {
    const instant = zonedDateTimeToDate("Asia/Shanghai", 2026, 11, 2, 9, 0);
    expect(instant.toISOString()).toBe("2026-11-02T01:00:00.000Z");
    expect(formatIsoWithZoneOffset(instant, "Asia/Shanghai")).toBe(
      "2026-11-02T09:00:00+08:00",
    );
    expect(zoneOffsetMs(instant, "Asia/Shanghai")).toBe(8 * 3_600_000);
  });

  it("uses the correct offset on both sides of a DST transition", () => {
    const summer = zonedDateTimeToDate("America/New_York", 2026, 7, 1, 12, 0);
    const winter = zonedDateTimeToDate("America/New_York", 2026, 1, 15, 12, 0);
    expect(formatIsoWithZoneOffset(summer, "America/New_York")).toBe(
      "2026-07-01T12:00:00-04:00",
    );
    expect(formatIsoWithZoneOffset(winter, "America/New_York")).toBe(
      "2026-01-15T12:00:00-05:00",
    );
  });

  it("round-trips wall-clock parts in a zone", () => {
    const instant = zonedDateTimeToDate("Europe/Berlin", 2026, 10, 25, 9, 30);
    expect(zonedParts(instant, "Europe/Berlin")).toMatchObject({
      year: 2026,
      month: 10,
      day: 25,
      hour: 9,
      minute: 30,
    });
    const wall = wallClockDate(instant, "Europe/Berlin");
    expect(wall.getHours()).toBe(9);
    expect(wall.getMinutes()).toBe(30);
  });

  it("labels offsets and validates zone ids", () => {
    const instant = zonedDateTimeToDate("Asia/Shanghai", 2026, 11, 2, 9, 0);
    expect(formatZoneOffset("Asia/Shanghai", instant)).toBe("UTC+08:00");
    expect(isValidTimeZone("Asia/Shanghai")).toBe(true);
    expect(isValidTimeZone("Not/AZone")).toBe(false);
    expect(isValidTimeZone(null)).toBe(false);
  });
});
