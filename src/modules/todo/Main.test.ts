import { describe, expect, it } from "vitest";
import type { Task } from "@/ipc/types";
import { hiddenCompletedCount } from "./Main";

function makeTask(id: string, status: Task["status"] = "completed"): Task {
  return {
    id,
    listId: "list-1",
    accountId: "local",
    title: id,
    notes: "",
    status,
    importance: "normal",
    dueDate: null,
    reminderAt: null,
    recurrence: null,
    myDayDate: null,
    completedAt: status === "completed" ? "2026-01-01T00:00:00Z" : null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    steps: [],
    sortOrder: 0,
    linkedMessageId: null,
  };
}

describe("hiddenCompletedCount", () => {
  it("is zero until the fallback query has data", () => {
    expect(hiddenCompletedCount([], undefined)).toBe(0);
  });

  it("is zero when the fallback returns no completed tasks", () => {
    expect(hiddenCompletedCount([], [makeTask("a", "notStarted")])).toBe(0);
  });

  it("counts completed tasks the visible query left out", () => {
    const completed = [makeTask("a"), makeTask("b")];
    expect(hiddenCompletedCount([], completed)).toBe(2);
    expect(hiddenCompletedCount([makeTask("active", "notStarted")], completed)).toBe(2);
  });

  it("ignores completed tasks that are already visible", () => {
    const visible = [makeTask("a")];
    const withCompleted = [makeTask("a"), makeTask("b")];
    expect(hiddenCompletedCount(visible, withCompleted)).toBe(1);
  });
});
