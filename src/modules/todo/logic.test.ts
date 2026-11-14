import { describe, expect, it } from "vitest";
import type { Account, RecurrenceRule, Task, TaskList } from "@/ipc/types";
import { todo } from "@/i18n/todo";
import {
  applySmartDefaults,
  defaultList,
  describeRecurrence,
  dueLabel,
  emptyTaskInput,
  filterBySmart,
  groupPlanned,
  listFeatureSupport,
  moveStep,
  nextWeekIso,
  plannedBucketFor,
  presetRecurrence,
  reorderIds,
  reorderIndexFor,
  sortCompletedTasks,
  sortTasks,
  splitByCompletion,
  stepsProgress,
  combineReminder,
  splitReminder,
  type RecurrencePreset,
  type Translate,
} from "@/modules/todo/logic";

const dict = todo["zh-CN"];

const t: Translate = (key, vars) => {
  const raw = dict[key] ?? key;
  return raw.replace(/\{(\w+)\}/g, (match, name: string) =>
    vars && Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : match,
  );
};

let idCounter = 0;

function makeTask(partial: Partial<Task> & { id?: string } = {}): Task {
  idCounter += 1;
  const id = partial.id ?? `task-${idCounter}`;
  const { id: _partialId, ...rest } = partial;
  const base: Omit<Task, "id"> = {
    listId: "list-1",
    accountId: "local",
    title: `Task ${idCounter}`,
    notes: "",
    status: "notStarted",
    importance: "normal",
    dueDate: null,
    reminderAt: null,
    recurrence: null,
    myDayDate: null,
    completedAt: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    steps: [],
    sortOrder: 0,
    linkedMessageId: null,
    ...rest,
  };
  return { ...base, id };
}

function makeList(partial: Partial<TaskList> & { id: string }): TaskList {
  const { id, ...rest } = partial;
  const base: Omit<TaskList, "id"> = {
    accountId: "local",
    name: id,
    color: "#0f6cbd",
    isDefault: false,
    isReadOnly: false,
    incompleteCount: 0,
    sortOrder: 0,
    ...rest,
  };
  return { ...base, id };
}

function makeRule(partial: Partial<RecurrenceRule>): RecurrenceRule {
  return {
    freq: "daily",
    interval: 1,
    byWeekday: [],
    bySetPos: [],
    byMonthDay: [],
    byMonth: [],
    count: null,
    until: null,
    ...partial,
  };
}

const TODAY = "2026-11-12"; // Thursday

describe("sortTasks", () => {
  it("uses sortOrder for the custom order", () => {
    const tasks = [
      makeTask({ id: "b", sortOrder: 2 }),
      makeTask({ id: "a", sortOrder: 0 }),
      makeTask({ id: "c", sortOrder: 1 }),
    ];
    expect(sortTasks(tasks, "custom").map((task) => task.id)).toEqual(["a", "c", "b"]);
  });

  it("sorts high importance first", () => {
    const tasks = [
      makeTask({ id: "low", importance: "low", sortOrder: 0 }),
      makeTask({ id: "high", importance: "high", sortOrder: 5 }),
      makeTask({ id: "normal", importance: "normal", sortOrder: 1 }),
    ];
    expect(sortTasks(tasks, "importance").map((task) => task.id)).toEqual([
      "high",
      "normal",
      "low",
    ]);
  });

  it("sorts by due date with undated tasks last", () => {
    const tasks = [
      makeTask({ id: "none", dueDate: null, sortOrder: 0 }),
      makeTask({ id: "later", dueDate: "2026-11-20", sortOrder: 1 }),
      makeTask({ id: "soon", dueDate: "2026-11-13", sortOrder: 2 }),
    ];
    expect(sortTasks(tasks, "dueDate").map((task) => task.id)).toEqual([
      "soon",
      "later",
      "none",
    ]);
  });

  it("sorts alphabetically using the locale", () => {
    const tasks = [
      makeTask({ id: "c", title: "Charlie" }),
      makeTask({ id: "a", title: "alpha" }),
      makeTask({ id: "b", title: "Bravo" }),
    ];
    expect(sortTasks(tasks, "alphabetical", "en-US").map((task) => task.id)).toEqual([
      "a",
      "b",
      "c",
    ]);
  });

  it("sorts newest-created first", () => {
    const tasks = [
      makeTask({ id: "old", createdAt: "2026-01-01T00:00:00Z" }),
      makeTask({ id: "new", createdAt: "2026-03-01T00:00:00Z" }),
    ];
    expect(sortTasks(tasks, "createdAt").map((task) => task.id)).toEqual(["new", "old"]);
  });

  it("sorts completed tasks by completion time", () => {
    const tasks = [
      makeTask({ id: "a", status: "completed", completedAt: "2026-02-01T00:00:00Z" }),
      makeTask({ id: "b", status: "completed", completedAt: "2026-04-01T00:00:00Z" }),
    ];
    expect(sortCompletedTasks(tasks).map((task) => task.id)).toEqual(["b", "a"]);
  });
});

describe("splitByCompletion / stepsProgress", () => {
  it("separates active and completed tasks", () => {
    const split = splitByCompletion([
      makeTask({ id: "a" }),
      makeTask({ id: "b", status: "completed" }),
    ]);
    expect(split.active.map((task) => task.id)).toEqual(["a"]);
    expect(split.completed.map((task) => task.id)).toEqual(["b"]);
  });

  it("counts completed steps", () => {
    const progress = stepsProgress([
      { id: "1", title: "a", isCompleted: true, sortOrder: 0 },
      { id: "2", title: "b", isCompleted: false, sortOrder: 1 },
    ]);
    expect(progress).toEqual({ done: 1, total: 2 });
  });
});

describe("smart list grouping", () => {
  const tasks = [
    makeTask({ id: "myday", myDayDate: TODAY }),
    makeTask({ id: "important", importance: "high" }),
    makeTask({ id: "planned", dueDate: "2026-11-20" }),
    makeTask({ id: "completed", status: "completed" }),
    makeTask({ id: "email", linkedMessageId: "m1" }),
    makeTask({ id: "plain" }),
  ];

  it("filters each smart list from the same task set", () => {
    expect(filterBySmart(tasks, "myDay", TODAY).map((task) => task.id)).toEqual(["myday"]);
    expect(filterBySmart(tasks, "important", TODAY).map((task) => task.id)).toEqual([
      "important",
    ]);
    expect(filterBySmart(tasks, "planned", TODAY).map((task) => task.id)).toEqual(["planned"]);
    expect(filterBySmart(tasks, "completed", TODAY).map((task) => task.id)).toEqual([
      "completed",
    ]);
    expect(filterBySmart(tasks, "flaggedEmail", TODAY).map((task) => task.id)).toEqual([
      "email",
    ]);
    expect(filterBySmart(tasks, "all", TODAY)).toHaveLength(tasks.length);
  });

  it("only counts My Day tasks from today", () => {
    expect(filterBySmart([makeTask({ id: "old", myDayDate: "2026-11-11" })], "myDay", TODAY)).toEqual(
      [],
    );
  });
});

describe("planned grouping", () => {
  it("maps dates into the documented buckets", () => {
    expect(plannedBucketFor(null, TODAY)).toBeNull();
    expect(plannedBucketFor("2026-11-11", TODAY)).toBe("overdue");
    expect(plannedBucketFor("2026-11-12", TODAY)).toBe("today");
    expect(plannedBucketFor("2026-11-13", TODAY)).toBe("tomorrow");
    expect(plannedBucketFor("2026-11-14", TODAY)).toBe("thisWeek");
    expect(plannedBucketFor("2026-11-15", TODAY)).toBe("thisWeek");
    expect(plannedBucketFor("2026-11-16", TODAY)).toBe("later");
  });

  it("treats Sunday as the end of a Monday-first week", () => {
    expect(plannedBucketFor("2026-11-16", "2026-11-15")).toBe("tomorrow");
    expect(plannedBucketFor("2026-11-22", "2026-11-15")).toBe("later");
  });

  it("groups tasks in display order and skips undated tasks", () => {
    const tasks = [
      makeTask({ id: "later", dueDate: "2026-12-01" }),
      makeTask({ id: "today", dueDate: TODAY }),
      makeTask({ id: "overdue", dueDate: "2026-11-01" }),
      makeTask({ id: "none", dueDate: null }),
      makeTask({ id: "tomorrow", dueDate: "2026-11-13" }),
    ];
    const groups = groupPlanned(tasks, TODAY, 1);
    expect(groups.map((group) => group.bucket)).toEqual([
      "overdue",
      "today",
      "tomorrow",
      "later",
    ]);
    expect(groups.flatMap((group) => group.tasks.map((task) => task.id))).toEqual([
      "overdue",
      "today",
      "tomorrow",
      "later",
    ]);
  });
});

describe("drag reordering", () => {
  it("moves an item forward and backward", () => {
    const ids = ["a", "b", "c"];
    expect(reorderIds(ids, "a", 2)).toEqual(["b", "c", "a"]);
    expect(reorderIds(ids, "c", 0)).toEqual(["c", "a", "b"]);
    expect(reorderIds(ids, "b", 1)).toEqual(["a", "b", "c"]);
  });

  it("computes insertion indexes for before/after drops", () => {
    const ids = ["a", "b", "c"];
    expect(reorderIndexFor(ids, "a", "b", "after")).toBe(1);
    expect(reorderIndexFor(ids, "c", "b", "before")).toBe(1);
    expect(reorderIndexFor(ids, "a", "c", "before")).toBe(1);
    expect(reorderIndexFor(ids, "c", "a", "after")).toBe(1);
    expect(reorderIndexFor(ids, "a", "a", "after")).toBe(-1);
  });

  it("reorders steps and renumbers sortOrder", () => {
    const steps = [
      { id: "1", title: "one", isCompleted: false, sortOrder: 0 },
      { id: "2", title: "two", isCompleted: false, sortOrder: 1 },
      { id: "3", title: "three", isCompleted: false, sortOrder: 2 },
    ];
    expect(moveStep(steps, "3", 0).map((step) => [step.id, step.sortOrder])).toEqual([
      ["3", 0],
      ["1", 1],
      ["2", 2],
    ]);
  });
});

describe("recurrence labels", () => {
  it("describes daily and weekly rules", () => {
    expect(describeRecurrence(makeRule({ freq: "daily" }), t)).toBe("每天");
    expect(describeRecurrence(makeRule({ freq: "daily", interval: 3 }), t)).toBe("每 3 天");
    expect(
      describeRecurrence(
        makeRule({ freq: "weekly", byWeekday: ["MO", "TU", "WE", "TH", "FR"] }),
        t,
      ),
    ).toBe("每个工作日");
    expect(
      describeRecurrence(makeRule({ freq: "weekly", byWeekday: ["MO", "WE"] }), t),
    ).toBe("每周一、三");
    expect(
      describeRecurrence(makeRule({ freq: "weekly", byWeekday: ["TU"] }), t),
    ).toBe("每周二");
    expect(
      describeRecurrence(makeRule({ freq: "weekly", interval: 2, byWeekday: ["TU"] }), t),
    ).toBe("每 2 周的周二");
  });

  it("describes monthly and yearly rules", () => {
    expect(
      describeRecurrence(makeRule({ freq: "monthly", byMonthDay: [15] }), t),
    ).toBe("每月 15 日");
    expect(
      describeRecurrence(
        makeRule({ freq: "monthly", bySetPos: [2], byWeekday: ["TU"] }),
        t,
      ),
    ).toBe("每月第 2 个星期二");
    expect(
      describeRecurrence(
        makeRule({ freq: "yearly", byMonth: [11], byMonthDay: [12] }),
        t,
      ),
    ).toBe("每年 11 月 12 日");
    expect(describeRecurrence(makeRule({ freq: "yearly" }), t)).toBe("每年");
  });

  it("appends end conditions", () => {
    expect(describeRecurrence(makeRule({ freq: "daily", count: 5 }), t)).toBe("每天，共 5 次");
    expect(
      describeRecurrence(makeRule({ freq: "daily", until: "2026-12-01" }), t).startsWith(
        "每天，直到",
      ),
    ).toBe(true);
  });

  it("builds preset rules from an anchor date", () => {
    const weekdays = presetRecurrence("weekdays", TODAY);
    expect(weekdays?.byWeekday).toEqual(["MO", "TU", "WE", "TH", "FR"]);
    const weekly = presetRecurrence("weekly", TODAY);
    expect(weekly?.byWeekday).toEqual(["TH"]);
    const monthly = presetRecurrence("monthly", TODAY);
    expect(monthly?.byMonthDay).toEqual([12]);
    const yearly = presetRecurrence("yearly", TODAY);
    expect(yearly?.byMonth).toEqual([11]);
    expect(yearly?.byMonthDay).toEqual([12]);
    expect(presetRecurrence("none", TODAY)).toBeNull();
  });

  it("supports every preset without throwing", () => {
    const presets: RecurrencePreset[] = [
      "none",
      "daily",
      "weekdays",
      "weekly",
      "monthly",
      "yearly",
      "custom",
    ];
    for (const preset of presets) {
      const rule = presetRecurrence(preset, TODAY);
      if (rule) {
        expect(typeof describeRecurrence(rule, t)).toBe("string");
      }
    }
  });
});

describe("smart list defaults", () => {
  it("inherits smart-list attributes for new tasks", () => {
    const base = emptyTaskInput("New");
    expect(applySmartDefaults(base, "myDay", TODAY).myDayDate).toBe(TODAY);
    expect(applySmartDefaults(base, "important", TODAY).importance).toBe("high");
    expect(applySmartDefaults(base, "planned", TODAY).dueDate).toBe(TODAY);
    expect(applySmartDefaults(base, "all", TODAY).dueDate).toBeNull();
  });

  it("picks a writable default list", () => {
    const readOnlyDefault = makeList({ id: "ro", isDefault: true, isReadOnly: true });
    const local = makeList({ id: "local-list", accountId: "local" });
    const other = makeList({ id: "other", accountId: "acc" });
    expect(defaultList([readOnlyDefault, other, local])?.id).toBe("local-list");
    expect(defaultList([readOnlyDefault])).toBeNull();
    expect(defaultList([])).toBeNull();
  });
});

describe("provider feature support", () => {
  const account = (overrides: Partial<Account["capabilities"]>): Account => ({
    id: "acc",
    provider: "gmail",
    email: "a@example.com",
    displayName: "A",
    color: "#000000",
    authKind: "oauth",
    oauthProvider: "google",
    imap: null,
    smtp: null,
    pop3: null,
    caldavUrl: null,
    carddavUrl: null,
    syncWindowDays: 90,
    mailEnabled: true,
    calendarEnabled: true,
    contactsEnabled: true,
    tasksEnabled: true,
    notificationsEnabled: true,
    includeInUnified: true,
    sortOrder: 0,
    state: "ok",
    stateMessage: null,
    createdAt: "2026-01-01T00:00:00Z",
    capabilities: {
      idle: true,
      condstore: true,
      qresync: false,
      move: true,
      uidplus: true,
      gmailExtensions: true,
      specialUse: true,
      serverSearch: true,
      autoSavesSent: true,
      calendar: true,
      contacts: true,
      tasks: true,
      taskImportance: false,
      taskReminders: false,
      taskSteps: true,
      taskRecurrence: false,
      ...overrides,
    },
  });

  it("mirrors provider capabilities", () => {
    const support = listFeatureSupport(
      makeList({ id: "l", accountId: "acc" }),
      account({}),
    );
    expect(support).toEqual({
      importance: false,
      reminders: false,
      steps: true,
      recurrence: false,
    });
  });

  it("treats local lists as fully capable", () => {
    const support = listFeatureSupport(
      makeList({ id: "l", accountId: "local" }),
      account({ taskImportance: false }),
    );
    expect(support).toEqual({
      importance: true,
      reminders: true,
      steps: true,
      recurrence: true,
    });
  });
});

describe("date labels", () => {
  it("labels today, tomorrow and yesterday", () => {
    expect(dueLabel(TODAY, TODAY, t, "zh-CN")).toEqual({ text: "今天", overdue: false });
    expect(dueLabel("2026-11-13", TODAY, t, "zh-CN")).toEqual({
      text: "明天",
      overdue: false,
    });
    expect(dueLabel("2026-11-11", TODAY, t, "zh-CN")).toEqual({
      text: "昨天",
      overdue: true,
    });
  });

  it("uses weekdays for the next few days", () => {
    expect(dueLabel("2026-11-15", TODAY, t, "en-US").text).toBe("Sun");
  });

  it("computes next week as the following Monday", () => {
    expect(nextWeekIso(TODAY)).toBe("2026-11-16");
    expect(nextWeekIso("2026-11-16")).toBe("2026-11-23");
    expect(nextWeekIso("2026-11-15")).toBe("2026-11-16");
  });

  it("round-trips reminder date and time in local time", () => {
    const iso = combineReminder("2026-11-12", "14:30");
    expect(splitReminder(iso)).toEqual({ date: "2026-11-12", time: "14:30" });
    expect(splitReminder(null)).toEqual({ date: null, time: null });
  });
});
