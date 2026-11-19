import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Task } from "@/ipc/types";

const mocks = vi.hoisted(() => ({
  create: vi.fn(),
  setCompleted: vi.fn(),
  deleteTask: vi.fn(),
  show: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
}));

vi.mock("@/ipc/api", () => ({
  isIpcError: (error: unknown): error is { code: string; message: string } =>
    typeof error === "object" && error !== null && "code" in error && "message" in error,
  tasks: {
    create: mocks.create,
    setCompleted: mocks.setCompleted,
    delete: mocks.deleteTask,
  },
}));

vi.mock("@/components", () => ({
  toast: {
    show: mocks.show,
    success: mocks.success,
    error: mocks.error,
  },
}));

import { t } from "@/lib/i18n";
import { deleteTaskWithUndo } from "./actions";

function makeTask(overrides: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    listId: "list-1",
    accountId: "local",
    title: "Buy milk",
    notes: "",
    status: "completed",
    importance: "normal",
    dueDate: null,
    reminderAt: null,
    recurrence: null,
    myDayDate: null,
    completedAt: "2026-01-01T00:00:00Z",
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    steps: [],
    sortOrder: 0,
    linkedMessageId: null,
    ...overrides,
  };
}

interface UndoToast {
  type?: string;
  title: string;
  actions?: { onClick?: () => void }[];
}

/** Delete a task, then invoke the Undo action of the toast it produced. */
async function deleteThenUndo(task: Task): Promise<void> {
  mocks.deleteTask.mockResolvedValue(undefined);
  await deleteTaskWithUndo(task);
  const calls = mocks.show.mock.calls;
  const options = calls[calls.length - 1]?.[0] as UndoToast | undefined;
  options?.actions?.[0]?.onClick?.();
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe("undo of a completed task", () => {
  it("re-creates the task, completes it and reports a clean restore", async () => {
    const restored = makeTask({ id: "task-2" });
    mocks.create.mockResolvedValue(restored);
    mocks.setCompleted.mockResolvedValue({ ...restored, status: "completed" });

    await deleteThenUndo(makeTask());
    await vi.waitFor(() => expect(mocks.success).toHaveBeenCalled());

    expect(mocks.create).toHaveBeenCalledTimes(1);
    expect(mocks.setCompleted).toHaveBeenCalledWith("task-2", true);
    expect(mocks.success).toHaveBeenCalledWith(t("todo.toast.restored"), "Buy milk");
    expect(mocks.error).not.toHaveBeenCalled();
  });

  it("reports the partial restore and offers a retry when completion fails", async () => {
    const restored = makeTask({ id: "task-2" });
    mocks.create.mockResolvedValue(restored);
    mocks.setCompleted
      .mockRejectedValueOnce(new Error("provider offline"))
      .mockResolvedValueOnce({ ...restored, status: "completed" });

    await deleteThenUndo(makeTask());
    await vi.waitFor(() => expect(mocks.show).toHaveBeenCalledTimes(2));

    const partial = mocks.show.mock.calls[1][0] as UndoToast;
    expect(partial.type).toBe("error");
    expect(partial.title).toBe(
      t("todo.toast.restoredPartial", { title: "Buy milk" }),
    );
    expect(partial.actions).toHaveLength(1);
    // The task is back, so a plain success toast would be a lie.
    expect(mocks.success).not.toHaveBeenCalled();

    partial.actions?.[0]?.onClick?.();
    await vi.waitFor(() => expect(mocks.success).toHaveBeenCalledTimes(1));

    expect(mocks.setCompleted).toHaveBeenCalledTimes(2);
    expect(mocks.success).toHaveBeenCalledWith(t("todo.toast.restored"), "Buy milk");
  });
});
