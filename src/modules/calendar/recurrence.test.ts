import { describe, expect, it } from "vitest";
import type { RecurrenceRule } from "@/ipc/types";
import {
  defaultRepeatState,
  describeRepeat,
  repeatFromRule,
  ruleFromRepeat,
  weekdayOf,
  type RepeatState,
} from "@/modules/calendar/recurrence";

const START = new Date(2026, 10, 12, 9, 0); // Thursday Nov 12 2026

function roundTrip(state: RepeatState, start = START): RecurrenceRule | null {
  const rule = ruleFromRepeat(state, start);
  if (!rule) return null;
  const back = repeatFromRule(rule, start);
  return ruleFromRepeat(back, start);
}

describe("ruleFromRepeat", () => {
  it("returns null for none", () => {
    expect(ruleFromRepeat(defaultRepeatState(), START)).toBeNull();
  });

  it("maps daily with interval", () => {
    expect(
      ruleFromRepeat({ ...defaultRepeatState(), mode: "daily", interval: 3 }, START),
    ).toEqual({
      freq: "daily",
      interval: 3,
      byWeekday: [],
      bySetPos: [],
      byMonthDay: [],
      byMonth: [],
      count: null,
      until: null,
    });
  });

  it("maps weekdays to a Mon-Fri weekly rule", () => {
    expect(
      ruleFromRepeat({ ...defaultRepeatState(), mode: "weekdays" }, START),
    ).toMatchObject({
      freq: "weekly",
      interval: 1,
      byWeekday: ["MO", "TU", "WE", "TH", "FR"],
    });
  });

  it("uses the start weekday when weekly has no days selected", () => {
    expect(
      ruleFromRepeat({ ...defaultRepeatState(), mode: "weekly" }, START)?.byWeekday,
    ).toEqual(["TH"]);
  });

  it("maps monthly by date and by nth weekday", () => {
    const byDate = ruleFromRepeat({ ...defaultRepeatState(), mode: "monthly" }, START);
    expect(byDate).toMatchObject({ freq: "monthly", byMonthDay: [12], bySetPos: [] });

    const nth = ruleFromRepeat(
      { ...defaultRepeatState(), mode: "monthlyNth", nth: 2 },
      START,
    );
    expect(nth).toMatchObject({
      freq: "monthly",
      byWeekday: ["TH"],
      bySetPos: [2],
      byMonthDay: [],
    });
  });

  it("maps yearly using the start month/day", () => {
    expect(ruleFromRepeat({ ...defaultRepeatState(), mode: "yearly" }, START)).toMatchObject({
      freq: "yearly",
      byMonth: [11],
      byMonthDay: [12],
    });
  });

  it("encodes the end condition", () => {
    const after = ruleFromRepeat(
      { ...defaultRepeatState(), mode: "daily", endMode: "after", count: 7 },
      START,
    );
    expect(after?.count).toBe(7);
    expect(after?.until).toBeNull();

    const until = ruleFromRepeat(
      { ...defaultRepeatState(), mode: "daily", endMode: "until", until: "2026-12-31" },
      START,
    );
    expect(until?.until).toBe("2026-12-31");
    expect(until?.count).toBeNull();
  });

  it("clamps invalid inputs", () => {
    const rule = ruleFromRepeat(
      { ...defaultRepeatState(), mode: "daily", interval: 0, endMode: "after", count: 0 },
      START,
    );
    expect(rule?.interval).toBe(1);
    expect(rule?.count).toBe(1);
  });
});

describe("repeatFromRule", () => {
  it("detects the weekday preset", () => {
    const rule: RecurrenceRule = {
      freq: "weekly",
      interval: 1,
      byWeekday: ["MO", "TU", "WE", "TH", "FR"],
      bySetPos: [],
      byMonthDay: [],
      byMonth: [],
      count: null,
      until: null,
    };
    expect(repeatFromRule(rule, START).mode).toBe("weekdays");
  });

  it("detects monthly-by-weekday rules", () => {
    const rule: RecurrenceRule = {
      freq: "monthly",
      interval: 2,
      byWeekday: ["TU"],
      bySetPos: [3],
      byMonthDay: [],
      byMonth: [],
      count: null,
      until: null,
    };
    const state = repeatFromRule(rule, START);
    expect(state.mode).toBe("monthlyNth");
    expect(state.nth).toBe(3);
    expect(state.interval).toBe(2);
  });

  it("reads count and until back", () => {
    const rule = ruleFromRepeat(
      { ...defaultRepeatState(), mode: "weekly", endMode: "after", count: 4 },
      START,
    )!;
    expect(repeatFromRule(rule, START).endMode).toBe("after");
    expect(repeatFromRule(rule, START).count).toBe(4);
  });
});

describe("round trips", () => {
  const modes: RepeatState["mode"][] = [
    "daily",
    "weekdays",
    "weekly",
    "monthly",
    "monthlyNth",
    "yearly",
  ];
  for (const mode of modes) {
    it(`preserves ${mode}`, () => {
      const state: RepeatState = {
        ...defaultRepeatState(),
        mode,
        interval: 2,
        byWeekday: ["MO", "WE"],
        nth: 2,
      };
      expect(roundTrip(state)).toEqual(ruleFromRepeat(state, START));
    });
  }

  it("preserves end conditions", () => {
    for (const endMode of ["never", "after", "until"] as const) {
      const state: RepeatState = {
        ...defaultRepeatState(),
        mode: "monthly",
        endMode,
        count: 5,
        until: "2027-01-31",
      };
      expect(roundTrip(state)).toEqual(ruleFromRepeat(state, START));
    }
  });
});

describe("weekdayOf", () => {
  it("uses the local weekday", () => {
    expect(weekdayOf(new Date(2026, 10, 12))).toBe("TH");
    expect(weekdayOf(new Date(2026, 10, 15))).toBe("SU");
  });
});

describe("describeRepeat", () => {
  const t = (key: string, vars?: Record<string, string | number>) => {
    const varsText = vars
      ? Object.entries(vars)
          .map(([name, value]) => `${name}=${value}`)
          .join(",")
      : "";
    return `${key}|${varsText}`;
  };

  it("describes daily rules with count", () => {
    const state: RepeatState = {
      ...defaultRepeatState(),
      mode: "daily",
      endMode: "after",
      count: 3,
    };
    expect(describeRepeat(state, START, { t, locale: "en-US" })).toContain("count=3");
  });

  it("describes weekly rules with localized weekday names", () => {
    const state: RepeatState = { ...defaultRepeatState(), mode: "weekly", byWeekday: ["MO", "WE"] };
    const summary = describeRepeat(state, START, { t, locale: "en-US" });
    expect(summary).toContain("Mon");
    expect(summary).toContain("Wed");
  });
});
