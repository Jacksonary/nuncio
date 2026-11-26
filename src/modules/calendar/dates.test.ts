import { describe, expect, it } from "vitest";
import {
  DEFAULT_CALENDAR_SETTINGS,
  addDaysLocal,
  calendarDayDiff,
  combineDateAndMinutes,
  createRange,
  formatRangeTitle,
  isoWeekNumber,
  monthGridDays,
  rangeForView,
  shiftView,
  snapMinutes,
  snapDelta,
  startOfDayLocal,
  toOffsetIsoString,
  weekdayHeaders,
  type CalendarSettings,
} from "@/modules/calendar/dates";

const settings: CalendarSettings = { ...DEFAULT_CALENDAR_SETTINGS };

function iso(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(
    date.getDate(),
  ).padStart(2, "0")}`;
}

describe("rangeForView", () => {
  const anchor = new Date(2026, 10, 12, 15, 30); // Thursday, Nov 12 2026

  it("returns one local day for the day view", () => {
    const range = rangeForView("day", anchor, settings);
    expect(iso(range.from)).toBe("2026-11-12");
    expect(range.from.getHours()).toBe(0);
    expect(iso(range.to)).toBe("2026-11-13");
    expect(range.days).toHaveLength(1);
  });

  it("starts the week on the configured weekday", () => {
    const monday = rangeForView("week", anchor, { ...settings, firstDayOfWeek: 1 });
    expect(iso(monday.from)).toBe("2026-11-09");
    expect(iso(monday.to)).toBe("2026-11-16");
    expect(monday.days).toHaveLength(7);

    const sunday = rangeForView("week", anchor, { ...settings, firstDayOfWeek: 0 });
    expect(iso(sunday.from)).toBe("2026-11-08");
    expect(iso(sunday.to)).toBe("2026-11-15");
  });

  it("uses the configured work days for the work-week view", () => {
    const range = rangeForView("workWeek", anchor, {
      ...settings,
      firstDayOfWeek: 0,
      workDays: [1, 2, 3, 4, 5],
    });
    expect(iso(range.from)).toBe("2026-11-09");
    expect(iso(range.to)).toBe("2026-11-14");
    expect(range.days.map(iso)).toEqual([
      "2026-11-09",
      "2026-11-10",
      "2026-11-11",
      "2026-11-12",
      "2026-11-13",
    ]);
  });

  it("supports a Sunday-through-Thursday working week", () => {
    const range = rangeForView("workWeek", anchor, {
      ...settings,
      firstDayOfWeek: 0,
      workDays: [0, 1, 2, 3, 4],
    });
    expect(range.days.map(iso)).toEqual([
      "2026-11-08",
      "2026-11-09",
      "2026-11-10",
      "2026-11-11",
      "2026-11-12",
    ]);
  });

  it("builds a six-week month grid", () => {
    const range = rangeForView("month", anchor, settings);
    expect(range.days).toHaveLength(42);
    expect(range.grid).toBe(true);
    expect(range.days[0]?.getDay()).toBe(settings.firstDayOfWeek);
    expect(iso(range.from)).toBe("2026-10-26");
    expect(iso(range.to)).toBe("2026-12-07");
  });

  it("covers the whole year", () => {
    const range = rangeForView("year", anchor, settings);
    expect(iso(range.from)).toBe("2026-01-01");
    expect(iso(range.to)).toBe("2027-01-01");
  });

  it("covers the next 30 days for the agenda", () => {
    const range = rangeForView("agenda", anchor, settings);
    expect(iso(range.from)).toBe("2026-11-12");
    expect(iso(range.to)).toBe("2026-12-12");
  });
});

describe("shiftView", () => {
  const anchor = new Date(2026, 10, 12, 15, 30);

  it("moves one day / week / month / year", () => {
    expect(iso(shiftView("day", anchor, 1, settings))).toBe("2026-11-13");
    expect(iso(shiftView("week", anchor, -1, settings))).toBe("2026-11-05");
    expect(iso(shiftView("month", anchor, 1, settings))).toBe("2026-12-01");
    expect(shiftView("year", anchor, 1, settings).getFullYear()).toBe(2027);
    expect(iso(shiftView("agenda", anchor, 1, settings))).toBe("2026-12-12");
  });

  it("clamps month navigation at the end of short months", () => {
    const jan31 = new Date(2026, 0, 31);
    expect(iso(shiftView("month", jan31, 1, settings))).toBe("2026-02-01");
  });
});

describe("local date math", () => {
  it("adds calendar days preserving the local time across DST", () => {
    const start = new Date(2026, 2, 7, 10, 30);
    const result = addDaysLocal(start, 7);
    expect(result.getHours()).toBe(10);
    expect(result.getMinutes()).toBe(30);
    expect(iso(result)).toBe("2026-03-14");
  });

  it("computes calendar-day differences without DST drift", () => {
    const a = new Date(2026, 2, 10, 0, 30);
    const b = new Date(2026, 2, 8, 23, 30);
    expect(calendarDayDiff(a, b)).toBe(2);
  });

  it("snaps minutes to the nearest quarter hour", () => {
    expect(snapMinutes(7)).toBe(0);
    expect(snapMinutes(8)).toBe(15);
    expect(snapMinutes(1439)).toBe(1440);
  });

  it("snaps signed drag deltas without clamping", () => {
    expect(snapDelta(-20)).toBe(-15);
    expect(snapDelta(20)).toBe(15);
    expect(snapDelta(52)).toBe(45);
    expect(snapDelta(-52)).toBe(-45);
    expect(snapDelta(7)).toBe(0);
  });

  it("combines a day with minutes since midnight", () => {
    const date = combineDateAndMinutes(new Date(2026, 10, 12), 8 * 60 + 45);
    expect(date.getHours()).toBe(8);
    expect(date.getMinutes()).toBe(45);
  });

  it("formats an offset ISO string", () => {
    const value = toOffsetIsoString(new Date(2026, 10, 12, 9, 5));
    expect(value.startsWith("2026-11-12T09:05:00")).toBe(true);
    expect(value).toMatch(/[+-]\d{2}:\d{2}$/);
  });

  it("normalizes the start of the day", () => {
    const date = startOfDayLocal(new Date(2026, 10, 12, 23, 59, 59));
    expect(date.getHours()).toBe(0);
    expect(iso(date)).toBe("2026-11-12");
  });
});

describe("calendar grids", () => {
  it("returns ISO week numbers", () => {
    expect(isoWeekNumber(new Date(2026, 0, 1))).toBe(1);
    expect(isoWeekNumber(new Date(2025, 11, 29))).toBe(1);
  });

  it("builds 42 day cells for any month, in order", () => {
    const days = monthGridDays(new Date(2026, 1, 1), 1);
    expect(days).toHaveLength(42);
    expect(days[0]?.getDay()).toBe(1);
    expect(calendarDayDiff(days[41]!, days[0]!)).toBe(41);
  });

  it("produces seven weekday headers", () => {
    const headers = weekdayHeaders("en-US", 1);
    expect(headers).toHaveLength(7);
    expect(headers[0]).toBe("M");
    expect(weekdayHeaders("en-US", 0)[0]).toBe("S");
  });
});

describe("formatRangeTitle", () => {
  const anchor = new Date(2026, 10, 12);

  it("formats each view", () => {
    expect(formatRangeTitle("month", anchor, rangeForView("month", anchor, settings), "en-US")).toBe(
      "November 2026",
    );
    expect(formatRangeTitle("year", anchor, rangeForView("year", anchor, settings), "en-US")).toBe(
      "2026",
    );
    expect(
      formatRangeTitle("week", anchor, rangeForView("week", anchor, settings), "en-US"),
    ).toBe("November 9 – 15, 2026");
  });
});

describe("createRange", () => {
  it("keeps a real drag", () => {
    expect(createRange(660, 720, 30)).toEqual({ start: 660, end: 720 });
    expect(createRange(720, 660, 30)).toEqual({ start: 660, end: 720 });
  });

  it("gives a click the default duration instead of a sliver", () => {
    // A click (no movement) must preview and create the same block.
    expect(createRange(660, 660, 30)).toEqual({ start: 660, end: 690 });
    // A drag shorter than one snap step counts as a click.
    expect(createRange(660, 665, 30)).toEqual({ start: 660, end: 690 });
  });

  it("never creates less than one snap step", () => {
    expect(createRange(660, 660, 5)).toEqual({ start: 660, end: 675 });
  });

  it("clamps the block inside the day", () => {
    expect(createRange(1435, 1435, 30)).toEqual({ start: 1410, end: 1440 });
  });
});
