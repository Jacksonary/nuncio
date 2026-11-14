/**
 * Recurrence editing model ↔ IPC `RecurrenceRule` mapping.
 *
 * The editor exposes a small, human-friendly subset of RRULE (the same one
 * Windows Calendar offers). These pure functions convert between the editor
 * state and the contract type, and produce a readable summary.
 */
import type { RecurrenceRule, Weekday } from "@/ipc/types";
import { addDaysLocal, shortWeekday, startOfDayLocal } from "./dates";
import { toIsoDate } from "@/lib/format";

export type RepeatMode =
  | "none"
  | "daily"
  | "weekdays"
  | "weekly"
  | "monthly"
  | "monthlyNth"
  | "yearly";

export type RepeatEndMode = "never" | "after" | "until";

export interface RepeatState {
  mode: RepeatMode;
  interval: number;
  byWeekday: Weekday[];
  monthlyMode: "date" | "nth";
  /** 1..5 → first..fifth weekday of the month. */
  nth: number;
  endMode: RepeatEndMode;
  count: number;
  until: string | null;
}

export const WEEKDAYS: Weekday[] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
const WEEKDAY_BY_JS_DAY: Weekday[] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

export function weekdayOf(date: Date): Weekday {
  return WEEKDAY_BY_JS_DAY[date.getDay()] ?? "MO";
}

export function nthWeekdayOfMonth(date: Date): number {
  return Math.min(5, Math.floor((date.getDate() - 1) / 7) + 1);
}

export function defaultRepeatState(): RepeatState {
  return {
    mode: "none",
    interval: 1,
    byWeekday: [],
    monthlyMode: "date",
    nth: 1,
    endMode: "never",
    count: 10,
    until: null,
  };
}

function isWeekdaysRule(rule: RecurrenceRule): boolean {
  if (rule.freq !== "weekly" || rule.interval !== 1) return false;
  const days = new Set(rule.byWeekday);
  if (days.size !== 5) return false;
  return ["MO", "TU", "WE", "TH", "FR"].every((day) => days.has(day as Weekday));
}

/** Build the editor state from a stored rule. */
export function repeatFromRule(rule: RecurrenceRule | null, start: Date): RepeatState {
  const base = defaultRepeatState();
  if (!rule) return base;

  const interval = Math.max(1, rule.interval || 1);
  const endMode: RepeatEndMode =
    rule.count !== null && rule.count > 0 ? "after" : rule.until ? "until" : "never";

  const shared: RepeatState = {
    ...base,
    interval,
    endMode,
    count: rule.count ?? base.count,
    until: rule.until ?? null,
  };

  if (rule.freq === "daily") {
    return { ...shared, mode: "daily" };
  }

  if (rule.freq === "weekly") {
    if (isWeekdaysRule(rule)) {
      return { ...shared, mode: "weekdays", byWeekday: ["MO", "TU", "WE", "TH", "FR"] };
    }
    const byWeekday =
      rule.byWeekday.length > 0 ? [...rule.byWeekday] : [weekdayOf(start)];
    return { ...shared, mode: "weekly", byWeekday };
  }

  if (rule.freq === "monthly") {
    if (rule.bySetPos.length > 0 && rule.byWeekday.length > 0) {
      return {
        ...shared,
        mode: "monthlyNth",
        monthlyMode: "nth",
        byWeekday: [...rule.byWeekday],
        nth: Math.max(1, Math.min(5, Math.abs(rule.bySetPos[0] ?? 1))),
      };
    }
    return { ...shared, mode: "monthly", monthlyMode: "date" };
  }

  return { ...shared, mode: "yearly" };
}

/** Convert editor state back to a contract rule (null = does not repeat). */
export function ruleFromRepeat(state: RepeatState, start: Date): RecurrenceRule | null {
  if (state.mode === "none") return null;

  const interval = Math.max(1, Math.round(state.interval) || 1);
  const count = state.endMode === "after" ? Math.max(1, Math.round(state.count) || 1) : null;
  const until = state.endMode === "until" ? ensureDateString(state.until) : null;
  const end = { count, until };

  switch (state.mode) {
    case "daily":
      return {
        freq: "daily",
        interval,
        byWeekday: [],
        bySetPos: [],
        byMonthDay: [],
        byMonth: [],
        ...end,
      };
    case "weekdays":
      return {
        freq: "weekly",
        interval: 1,
        byWeekday: ["MO", "TU", "WE", "TH", "FR"],
        bySetPos: [],
        byMonthDay: [],
        byMonth: [],
        ...end,
      };
    case "weekly": {
      const byWeekday =
        state.byWeekday.length > 0 ? [...state.byWeekday] : [weekdayOf(start)];
      return {
        freq: "weekly",
        interval,
        byWeekday,
        bySetPos: [],
        byMonthDay: [],
        byMonth: [],
        ...end,
      };
    }
    case "monthly":
      return {
        freq: "monthly",
        interval,
        byWeekday: [],
        bySetPos: [],
        byMonthDay: [start.getDate()],
        byMonth: [],
        ...end,
      };
    case "monthlyNth":
      return {
        freq: "monthly",
        interval,
        byWeekday: [weekdayOf(start)],
        bySetPos: [Math.max(1, Math.min(5, state.nth))],
        byMonthDay: [],
        byMonth: [],
        ...end,
      };
    case "yearly":
      return {
        freq: "yearly",
        interval,
        byWeekday: [],
        bySetPos: [],
        byMonthDay: [start.getDate()],
        byMonth: [start.getMonth() + 1],
        ...end,
      };
    default:
      return null;
  }
}

function ensureDateString(value: string | null): string | null {
  if (!value) return null;
  return /^\d{4}-\d{2}-\d{2}$/.test(value) ? value : toIsoDate(new Date(value));
}

export interface RepeatSummaryContext {
  t: (key: string, vars?: Record<string, string | number>) => string;
  locale: string;
}

/** Reference dates so weekday labels can be rendered in the current locale. */
export function weekdayLabel(weekday: Weekday, locale: string): string {
  const index = WEEKDAYS.indexOf(weekday);
  // 2024-01-01 is a Monday.
  const date = addDaysLocal(new Date(2024, 0, 1, 12, 0, 0, 0), index);
  return shortWeekday(date, locale);
}

/** Human summary used below the repeat controls and in the peek. */
export function describeRepeat(
  state: RepeatState,
  start: Date,
  ctx: RepeatSummaryContext,
  ruleOverride?: RecurrenceRule | null,
): string {
  const rule = ruleOverride !== undefined ? ruleOverride : ruleFromRepeat(state, start);
  if (!rule) return ctx.t("calendar.repeat.none");
  const t = ctx.t;
  const every = rule.interval || 1;

  const day = startOfDayLocal(start);
  let base: string;
  switch (state.mode) {
    case "weekdays":
      base = t("calendar.repeat.summary.weekdays");
      break;
    case "weekly": {
      const days = (rule.byWeekday.length > 0 ? rule.byWeekday : [weekdayOf(day)])
        .map((weekday) => weekdayLabel(weekday, ctx.locale))
        .join(", ");
      base =
        every > 1
          ? t("calendar.repeat.summary.weeklyEvery", { interval: every, days })
          : t("calendar.repeat.summary.weekly", { days });
      break;
    }
    case "monthly":
      base =
        every > 1
          ? t("calendar.repeat.summary.monthlyEvery", {
              interval: every,
              day: day.getDate(),
            })
          : t("calendar.repeat.summary.monthly", { day: day.getDate() });
      break;
    case "monthlyNth": {
      const weekday = weekdayLabel(rule.byWeekday[0] ?? weekdayOf(day), ctx.locale);
      base =
        every > 1
          ? t("calendar.repeat.summary.monthlyNthEvery", {
              interval: every,
              nth: rule.bySetPos[0] ?? 1,
              weekday,
            })
          : t("calendar.repeat.summary.monthlyNth", {
              nth: rule.bySetPos[0] ?? 1,
              weekday,
            });
      break;
    }
    case "yearly": {
      const month = new Intl.DateTimeFormat(ctx.locale, { month: "long" }).format(day);
      base =
        every > 1
          ? t("calendar.repeat.summary.yearlyEvery", {
              interval: every,
              month,
              day: day.getDate(),
            })
          : t("calendar.repeat.summary.yearly", { month, day: day.getDate() });
      break;
    }
    case "daily":
      base =
        every > 1
          ? t("calendar.repeat.summary.dailyEvery", { interval: every })
          : t("calendar.repeat.summary.daily");
      break;
    default:
      base = t("calendar.repeat.none");
  }

  if (rule.count !== null) {
    return t("calendar.repeat.summary.withCount", { summary: base, count: rule.count });
  }
  if (rule.until) {
    return t("calendar.repeat.summary.withUntil", { summary: base, date: rule.until });
  }
  return base;
}
