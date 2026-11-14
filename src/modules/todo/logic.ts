/**
 * Pure To Do logic shared by the sidebar, task list and detail pane.
 *
 * Everything here is free of React so it can be unit-tested in isolation:
 * sorting, smart-list grouping, recurrence labels, due/reminder formatting and
 * Task <-> TaskInput conversion.
 */
import type {
  Account,
  IsoDate,
  IsoDateTime,
  RecurrenceRule,
  SmartList,
  SmartListCounts,
  Task,
  TaskInput,
  TaskList,
  TaskStep,
  Weekday,
} from "@/ipc/types";
import {
  addDays,
  formatDate,
  formatTime,
  formatWeekday,
  parseIsoDate,
  parseTimeOfDay,
  toIsoDate,
} from "@/lib/format";

export type Translate = (key: string, vars?: Record<string, string | number>) => string;

/* ─────────────────────────────── Sorting ─────────────────────────────── */

export type TaskSortMode = "custom" | "importance" | "dueDate" | "alphabetical" | "createdAt";

export const TASK_SORT_MODES: TaskSortMode[] = [
  "custom",
  "importance",
  "dueDate",
  "alphabetical",
  "createdAt",
];

export function sortModeLabelKey(mode: TaskSortMode): string {
  return `todo.sort.${mode}`;
}

const IMPORTANCE_RANK: Record<Task["importance"], number> = {
  high: 0,
  normal: 1,
  low: 2,
};

function compareSortOrder(a: Task, b: Task): number {
  if (a.sortOrder !== b.sortOrder) return a.sortOrder - b.sortOrder;
  if (a.createdAt !== b.createdAt) return a.createdAt < b.createdAt ? -1 : 1;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/** Stable sort for the active list. Returns a new array. */
export function sortTasks(tasks: Task[], mode: TaskSortMode, locale?: string): Task[] {
  const copy = [...tasks];
  switch (mode) {
    case "custom":
      return copy.sort(compareSortOrder);
    case "importance":
      return copy.sort(
        (a, b) => IMPORTANCE_RANK[a.importance] - IMPORTANCE_RANK[b.importance] || compareSortOrder(a, b),
      );
    case "dueDate":
      return copy.sort((a, b) => {
        if (a.dueDate === b.dueDate) return compareSortOrder(a, b);
        if (a.dueDate === null) return 1;
        if (b.dueDate === null) return -1;
        return a.dueDate < b.dueDate ? -1 : 1;
      });
    case "alphabetical":
      return copy.sort(
        (a, b) =>
          a.title.localeCompare(b.title, locale, { sensitivity: "base", numeric: true }) ||
          compareSortOrder(a, b),
      );
    case "createdAt":
      return copy.sort((a, b) => {
        if (a.createdAt !== b.createdAt) return a.createdAt < b.createdAt ? 1 : -1;
        return a.id < b.id ? 1 : -1;
      });
    default:
      return copy.sort(compareSortOrder);
  }
}

/** Completed section: most recently completed first. */
export function sortCompletedTasks(tasks: Task[]): Task[] {
  return [...tasks].sort((a, b) => {
    const aTime = a.completedAt ?? a.updatedAt;
    const bTime = b.completedAt ?? b.updatedAt;
    if (aTime !== bTime) return aTime < bTime ? 1 : -1;
    return a.id < b.id ? -1 : 1;
  });
}

export interface TaskSplit {
  active: Task[];
  completed: Task[];
}

export function splitByCompletion(tasks: Task[]): TaskSplit {
  const active: Task[] = [];
  const completed: Task[] = [];
  for (const task of tasks) {
    if (task.status === "completed") completed.push(task);
    else active.push(task);
  }
  return { active, completed };
}

/* ───────────────────────────── Smart lists ───────────────────────────── */

export const SMART_LISTS: SmartList[] = [
  "myDay",
  "important",
  "planned",
  "all",
  "completed",
  "flaggedEmail",
];

export function smartListLabelKey(smart: SmartList): string {
  return `todo.smart.${smart}`;
}

export function smartCount(counts: SmartListCounts | undefined, smart: SmartList): number {
  return counts ? counts[smart] : 0;
}

/* ─────────────────────────── Planned grouping ────────────────────────── */

export type PlannedBucket = "overdue" | "today" | "tomorrow" | "thisWeek" | "later";

export const PLANNED_BUCKETS: PlannedBucket[] = [
  "overdue",
  "today",
  "tomorrow",
  "thisWeek",
  "later",
];

export function plannedBucketLabelKey(bucket: PlannedBucket): string {
  return `todo.planned.${bucket}`;
}

const WEEKDAYS: Weekday[] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/** Integer day difference between two "YYYY-MM-DD" values (b - a). */
export function isoDayDiff(a: IsoDate, b: IsoDate): number {
  const left = parseIsoDate(a);
  const right = parseIsoDate(b);
  if (!left || !right) return 0;
  return Math.round((right.getTime() - left.getTime()) / 86_400_000);
}

export function addDaysIso(date: IsoDate, days: number): IsoDate {
  const parsed = parseIsoDate(date);
  if (!parsed) return date;
  return toIsoDate(addDays(parsed, days));
}

export function weekdayFromIso(date: IsoDate): Weekday {
  const parsed = parseIsoDate(date);
  if (!parsed) return "MO";
  const index = parsed.getDay(); // 0 = Sunday
  return (["SU", "MO", "TU", "WE", "TH", "FR", "SA"] as Weekday[])[index] ?? "MO";
}

/** Last day of the week containing `today`, as configured by weekStartsOn. */
export function endOfWeekIso(today: IsoDate, weekStartsOn = 1): IsoDate {
  const parsed = parseIsoDate(today);
  if (!parsed) return today;
  const jsDay = parsed.getDay();
  const offset = weekStartsOn === 1 ? (jsDay + 6) % 7 : jsDay;
  return addDaysIso(today, 6 - offset);
}

export function plannedBucketFor(
  dueDate: IsoDate | null,
  today: IsoDate,
  weekStartsOn = 1,
): PlannedBucket | null {
  if (!dueDate) return null;
  if (dueDate < today) return "overdue";
  if (dueDate === today) return "today";
  if (dueDate === addDaysIso(today, 1)) return "tomorrow";
  if (dueDate <= endOfWeekIso(today, weekStartsOn)) return "thisWeek";
  return "later";
}

export interface PlannedGroup {
  bucket: PlannedBucket;
  tasks: Task[];
}

/** Group planned tasks into 已过期 / 今天 / 明天 / 本周 / 以后 (empty groups dropped). */
export function groupPlanned(
  tasks: Task[],
  today: IsoDate,
  weekStartsOn = 1,
): PlannedGroup[] {
  const buckets = new Map<PlannedBucket, Task[]>();
  for (const task of tasks) {
    const bucket = plannedBucketFor(task.dueDate, today, weekStartsOn);
    if (!bucket) continue;
    const list = buckets.get(bucket);
    if (list) list.push(task);
    else buckets.set(bucket, [task]);
  }
  return PLANNED_BUCKETS.map((bucket) => ({
    bucket,
    tasks: buckets.get(bucket) ?? [],
  })).filter((group) => group.tasks.length > 0);
}

/* ──────────────────────────── Drag helpers ───────────────────────────── */

/**
 * Move `movedId` so that it lands at `targetIndex` in the resulting array.
 * `targetIndex` is expressed in the array without the moved item (0 = first).
 */
export function reorderIds(ids: string[], movedId: string, targetIndex: number): string[] {
  if (!ids.includes(movedId)) return [...ids];
  const without = ids.filter((id) => id !== movedId);
  const clamped = Math.max(0, Math.min(without.length, targetIndex));
  without.splice(clamped, 0, movedId);
  return without;
}

/**
 * Insertion index (in the array without the moved item) for dropping
 * `movedId` before/after `targetId`.
 */
export function reorderIndexFor(
  ids: string[],
  movedId: string,
  targetId: string,
  position: "before" | "after",
): number {
  const from = ids.indexOf(movedId);
  const target = ids.indexOf(targetId);
  if (from < 0 || target < 0 || movedId === targetId) return -1;
  let insert = target + (position === "after" ? 1 : 0);
  if (from < insert) insert -= 1;
  return insert;
}

/** Reorder steps and renumber sortOrder. Returns a new array. */
export function moveStep(steps: TaskStep[], fromId: string, toIndex: number): TaskStep[] {
  const from = steps.findIndex((step) => step.id === fromId);
  if (from < 0) return steps;
  const without = steps.filter((_, index) => index !== from);
  const clamped = Math.max(0, Math.min(without.length, toIndex));
  without.splice(clamped, 0, steps[from]);
  return without.map((step, index) => ({ ...step, sortOrder: index }));
}

/* ───────────────────────────── Steps helpers ─────────────────────────── */

let stepSequence = 0;

/** Stable client id for a new step; the backend may replace it on save. */
export function newStepId(): string {
  const cryptoApi = typeof globalThis !== "undefined" ? globalThis.crypto : undefined;
  if (cryptoApi && typeof cryptoApi.randomUUID === "function") {
    return cryptoApi.randomUUID();
  }
  stepSequence += 1;
  return `step-${Date.now().toString(36)}-${stepSequence}`;
}

export function createStep(title: string, sortOrder: number): TaskStep {
  return { id: newStepId(), title, isCompleted: false, sortOrder };
}

export interface StepProgress {
  done: number;
  total: number;
}

export function stepsProgress(steps: TaskStep[]): StepProgress {
  let done = 0;
  for (const step of steps) if (step.isCompleted) done += 1;
  return { done, total: steps.length };
}

/* ────────────────────────── Task input conversion ────────────────────── */

export interface TaskToInputOptions {
  /** Undo/restore must not reuse step ids: generate fresh ones. */
  freshStepIds?: boolean;
}

export function taskToInput(task: Task, options?: TaskToInputOptions): TaskInput {
  const steps = [...(task.steps ?? [])]
    .sort((a, b) => a.sortOrder - b.sortOrder)
    .map((step, index) => ({
      id: options?.freshStepIds ? newStepId() : step.id,
      title: step.title,
      isCompleted: step.isCompleted,
      sortOrder: index,
    }));
  return {
    title: task.title,
    notes: task.notes,
    importance: task.importance,
    dueDate: task.dueDate,
    reminderAt: task.reminderAt,
    recurrence: task.recurrence,
    myDayDate: task.myDayDate,
    steps,
  };
}

export function emptyTaskInput(title: string): TaskInput {
  return {
    title,
    notes: "",
    importance: "normal",
    dueDate: null,
    reminderAt: null,
    recurrence: null,
    myDayDate: null,
    steps: [],
  };
}

/**
 * New tasks created inside a smart list inherit the attributes that make them
 * appear in that list (Outlook/To Do behavior).
 */
export function applySmartDefaults(
  input: TaskInput,
  smart: SmartList | null,
  today: IsoDate,
): TaskInput {
  if (!smart) return input;
  switch (smart) {
    case "myDay":
      return { ...input, myDayDate: today };
    case "important":
      return { ...input, importance: "high" };
    case "planned":
      return { ...input, dueDate: today };
    default:
      return input;
  }
}

/* ───────────────────────────── Recurrence ────────────────────────────── */

export type RecurrencePreset =
  | "none"
  | "daily"
  | "weekdays"
  | "weekly"
  | "monthly"
  | "yearly"
  | "custom";

export type RecurrenceFrequency = RecurrenceRule["freq"];

/** Monday-first weekday order used by pickers and labels. */
export const WEEKDAY_ORDER: Weekday[] = [...WEEKDAYS];

export function clampRecurrenceInterval(value: number): number {
  if (!Number.isFinite(value)) return 1;
  return Math.min(99, Math.max(1, Math.round(value)));
}

export const RECURRENCE_PRESETS: RecurrencePreset[] = [
  "none",
  "daily",
  "weekdays",
  "weekly",
  "monthly",
  "yearly",
  "custom",
];

function emptyRule(): RecurrenceRule {
  return {
    freq: "daily",
    interval: 1,
    byWeekday: [],
    bySetPos: [],
    byMonthDay: [],
    byMonth: [],
    count: null,
    until: null,
  };
}

/** Build a preset rule anchored on the task's due date (or today). */
export function presetRecurrence(preset: RecurrencePreset, anchor: IsoDate): RecurrenceRule | null {
  if (preset === "none") return null;
  const date = parseIsoDate(anchor) ?? new Date();
  const day = date.getDate();
  const month = date.getMonth() + 1;
  const weekday = weekdayFromIso(anchor);
  const base = emptyRule();
  switch (preset) {
    case "daily":
      return { ...base, freq: "daily" };
    case "weekdays":
      return { ...base, freq: "weekly", byWeekday: ["MO", "TU", "WE", "TH", "FR"] };
    case "weekly":
      return { ...base, freq: "weekly", byWeekday: [weekday] };
    case "monthly":
      return { ...base, freq: "monthly", byMonthDay: [day] };
    case "yearly":
      return { ...base, freq: "yearly", byMonth: [month], byMonthDay: [day] };
    case "custom":
      return {
        ...base,
        freq: "weekly",
        byWeekday: [weekday],
      };
  }
}

function clampInterval(rule: RecurrenceRule): number {
  const value = Math.round(rule.interval);
  return Number.isFinite(value) && value > 0 ? value : 1;
}

function nthLabel(nth: number, t: Translate): string {
  if (nth >= 1 && nth <= 4) return t(`todo.repeat.nth.${nth}`);
  if (nth === -1) return t("todo.repeat.nth.last");
  return String(nth);
}

function weekdayList(days: Weekday[], t: Translate, style: "short" | "full" = "short"): string {
  const sorted = WEEKDAYS.filter((day) => days.includes(day));
  const prefix = style === "short" ? "todo.weekdayShort" : "todo.weekdayFull";
  return sorted.map((day) => t(`${prefix}.${day}`)).join(t("todo.repeat.daySeparator"));
}

function isWorkdayRule(days: Weekday[]): boolean {
  return (
    days.length === 5 &&
    (["MO", "TU", "WE", "TH", "FR"] as Weekday[]).every((day) => days.includes(day))
  );
}

/** Localized human label for a recurrence rule ("每周一", "Every 2 weeks", …). */
export function describeRecurrence(rule: RecurrenceRule, t: Translate): string {
  const interval = clampInterval(rule);
  const byWeekday = rule.byWeekday ?? [];
  const bySetPos = rule.bySetPos ?? [];
  const byMonthDay = rule.byMonthDay ?? [];
  const byMonth = rule.byMonth ?? [];
  let base = "";

  if (rule.freq === "daily") {
    base = interval === 1 ? t("todo.repeat.daily") : t("todo.repeat.everyNDays", { n: interval });
  } else if (rule.freq === "weekly") {
    const days = byWeekday;
    if (interval === 1 && isWorkdayRule(days)) {
      base = t("todo.repeat.weekdays");
    } else if (days.length === 0) {
      base = interval === 1 ? t("todo.repeat.weekly") : t("todo.repeat.everyNWeeks", { n: interval });
    } else {
      const list = weekdayList(days, t);
      base =
        interval === 1
          ? t("todo.repeat.weeklyOn", { days: list })
          : t("todo.repeat.everyNWeeksOn", { n: interval, days: list });
    }
  } else if (rule.freq === "monthly") {
    const day = byMonthDay[0];
    if (day !== undefined) {
      base =
        interval === 1
          ? t("todo.repeat.monthlyOn", { day })
          : t("todo.repeat.everyNMonthsOn", { n: interval, day });
    } else if (bySetPos.length > 0 && byWeekday.length > 0) {
      const nth = nthLabel(bySetPos[0], t);
      const weekday = weekdayList(byWeekday.slice(0, 1), t, "full");
      base =
        interval === 1
          ? t("todo.repeat.monthlyNth", { nth, weekday })
          : t("todo.repeat.everyNMonthsNth", { n: interval, nth, weekday });
    } else {
      base = interval === 1 ? t("todo.repeat.monthly") : t("todo.repeat.everyNMonths", { n: interval });
    }
  } else {
    const month = byMonth[0];
    const day = byMonthDay[0];
    if (month !== undefined && day !== undefined) {
      base =
        interval === 1
          ? t("todo.repeat.yearlyOn", { month, day })
          : t("todo.repeat.everyNYearsOn", { n: interval, month, day });
    } else {
      base = interval === 1 ? t("todo.repeat.yearly") : t("todo.repeat.everyNYears", { n: interval });
    }
  }

  if (rule.until) {
    const date = parseIsoDate(rule.until);
    return `${base}${t("todo.repeat.suffixSeparator")}${t("todo.repeat.until", {
      date: date ? formatDate(date) : rule.until,
    })}`;
  }
  if (rule.count !== null && rule.count > 0) {
    return `${base}${t("todo.repeat.suffixSeparator")}${t("todo.repeat.countTimes", {
      count: rule.count,
    })}`;
  }
  return base;
}

/* ─────────────────────────── Date formatting ─────────────────────────── */

export interface DueLabel {
  text: string;
  overdue: boolean;
}

/** Compact due-date chip text ("今天", "明天", "周三", "11月20日"). */
export function dueLabel(
  dueDate: IsoDate,
  today: IsoDate,
  t: Translate,
  locale?: string,
): DueLabel {
  const diff = isoDayDiff(today, dueDate);
  const parsed = parseIsoDate(dueDate);
  if (diff === 0) return { text: t("todo.due.today"), overdue: false };
  if (diff === 1) return { text: t("todo.due.tomorrow"), overdue: false };
  if (diff === -1) return { text: t("todo.due.yesterday"), overdue: true };
  if (diff > 1 && diff <= 6 && parsed) {
    return { text: formatWeekday(parsed, { locale }), overdue: false };
  }
  return {
    text: parsed ? formatDate(parsed, { locale }) : dueDate,
    overdue: diff < 0,
  };
}

/** Full due date used in the detail pane: "11月20日 · 周五". */
export function formatDueLong(dueDate: IsoDate, locale?: string): string {
  const parsed = parseIsoDate(dueDate);
  if (!parsed) return dueDate;
  return `${formatDate(parsed, { locale })} · ${formatWeekday(parsed, { locale })}`;
}

export function isOverdue(dueDate: IsoDate | null, today: IsoDate): boolean {
  return dueDate !== null && dueDate < today;
}

/* ───────────────────────────── Reminders ─────────────────────────────── */

export interface ReminderParts {
  date: IsoDate | null;
  time: string | null;
}

export function splitReminder(reminderAt: IsoDateTime | null): ReminderParts {
  if (!reminderAt) return { date: null, time: null };
  const date = new Date(reminderAt);
  if (Number.isNaN(date.getTime())) return { date: null, time: null };
  const hours = String(date.getHours()).padStart(2, "0");
  const minutes = String(date.getMinutes()).padStart(2, "0");
  return { date: toIsoDate(date), time: `${hours}:${minutes}` };
}

/** Combine a local date and "HH:mm" into a UTC ISO instant. */
export function combineReminder(date: IsoDate, time: string): IsoDateTime {
  const parsedDate = parseIsoDate(date);
  const minutes = parseTimeOfDay(time);
  if (!parsedDate || minutes === null) {
    return new Date().toISOString();
  }
  const result = new Date(
    parsedDate.getFullYear(),
    parsedDate.getMonth(),
    parsedDate.getDate(),
    Math.floor(minutes / 60),
    minutes % 60,
    0,
    0,
  );
  return result.toISOString();
}

/** Default reminder: the next full hour, at least one hour from now. */
export function defaultReminder(now: Date): { date: IsoDate; time: string } {
  const next = new Date(now.getTime() + 60 * 60_000);
  next.setMinutes(0, 0, 0);
  if (next.getTime() <= now.getTime()) next.setHours(next.getHours() + 1);
  return {
    date: toIsoDate(next),
    time: `${String(next.getHours()).padStart(2, "0")}:00`,
  };
}

export function formatReminderLabel(
  reminderAt: IsoDateTime,
  today: IsoDate,
  t: Translate,
  locale?: string,
): string {
  const parts = splitReminder(reminderAt);
  if (!parts.date || !parts.time) return "";
  const diff = isoDayDiff(today, parts.date);
  let dateText: string;
  if (diff === 0) dateText = t("todo.due.today");
  else if (diff === 1) dateText = t("todo.due.tomorrow");
  else if (diff === -1) dateText = t("todo.due.yesterday");
  else {
    const parsed = parseIsoDate(parts.date);
    dateText = parsed ? formatDate(parsed, { locale }) : parts.date;
  }
  return `${dateText} ${parts.time}`;
}

/** "14:30" for an ISO reminder instant. */
export function formatReminderTime(reminderAt: IsoDateTime): string {
  const date = new Date(reminderAt);
  if (Number.isNaN(date.getTime())) return "";
  return formatTime(date);
}

/* ────────────────────────────── Misc ─────────────────────────────────── */

export function isMyDayToday(task: Task, today: IsoDate): boolean {
  return task.myDayDate !== null && task.myDayDate === today;
}

/** The list new tasks go to when the view is a smart list. */
export function defaultList(lists: TaskList[]): TaskList | null {
  const writable = lists.filter((list) => !list.isReadOnly);
  return (
    writable.find((list) => list.isDefault) ??
    writable.find((list) => list.accountId === "local") ??
    writable[0] ??
    null
  );
}

export interface ListGroup {
  accountId: string;
  label: string;
  color: string;
  lists: TaskList[];
}

/** Lists grouped by owning account, in account sort order. */
export function groupListsByAccount(lists: TaskList[], accounts: Account[]): ListGroup[] {
  const byAccount = new Map<string, TaskList[]>();
  for (const list of lists) {
    const bucket = byAccount.get(list.accountId);
    if (bucket) bucket.push(list);
    else byAccount.set(list.accountId, [list]);
  }
  for (const bucket of byAccount.values()) {
    bucket.sort((a, b) => a.sortOrder - b.sortOrder || a.name.localeCompare(b.name));
  }
  const ordered: ListGroup[] = [];
  const sortedAccounts = [...accounts].sort((a, b) => a.sortOrder - b.sortOrder);
  for (const account of sortedAccounts) {
    const bucket = byAccount.get(account.id);
    if (!bucket || bucket.length === 0) continue;
    ordered.push({
      accountId: account.id,
      label: account.displayName || account.email,
      color: account.color,
      lists: bucket,
    });
    byAccount.delete(account.id);
  }
  for (const [accountId, bucket] of byAccount) {
    ordered.push({ accountId, label: accountId, color: "var(--text-tertiary)", lists: bucket });
  }
  return ordered;
}

export interface FeatureSupport {
  importance: boolean;
  reminders: boolean;
  steps: boolean;
  recurrence: boolean;
}

const ALL_SUPPORTED: FeatureSupport = {
  importance: true,
  reminders: true,
  steps: true,
  recurrence: true,
};

/**
 * Providers advertise which task features they can round-trip. Local lists
 * always support everything; unknown accounts are treated as capable so the UI
 * never hides controls unnecessarily.
 */
export function listFeatureSupport(list: TaskList | null, account: Account | null): FeatureSupport {
  if (!list) return ALL_SUPPORTED;
  if (list.accountId === "local" || !account || account.provider === "local") {
    return ALL_SUPPORTED;
  }
  const capabilities = account.capabilities;
  if (!capabilities) return ALL_SUPPORTED;
  return {
    importance: capabilities.taskImportance,
    reminders: capabilities.taskReminders,
    steps: capabilities.taskSteps,
    recurrence: capabilities.taskRecurrence,
  };
}

export function anyFeatureUnsupported(support: FeatureSupport): boolean {
  return (
    !support.importance || !support.reminders || !support.steps || !support.recurrence
  );
}

export function nextWeekIso(today: IsoDate): IsoDate {
  const parsed = parseIsoDate(today);
  if (!parsed) return addDaysIso(today, 7);
  const jsDay = parsed.getDay(); // 0 = Sunday, 1 = Monday
  const daysUntilMonday = (8 - jsDay) % 7;
  return addDaysIso(today, daysUntilMonday === 0 ? 7 : daysUntilMonday);
}

export function filterBySmart(tasks: Task[], smart: SmartList, today: IsoDate): Task[] {
  switch (smart) {
    case "myDay":
      return tasks.filter((task) => isMyDayToday(task, today));
    case "important":
      return tasks.filter((task) => task.importance === "high");
    case "planned":
      return tasks.filter((task) => task.dueDate !== null);
    case "completed":
      return tasks.filter((task) => task.status === "completed");
    case "flaggedEmail":
      return tasks.filter((task) => task.linkedMessageId !== null);
    default:
      return tasks;
  }
}
