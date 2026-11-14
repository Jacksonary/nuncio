/**
 * Write-side helpers for the To Do module. Components and module-level hotkeys
 * call these instead of issuing IPC directly, so cache updates, optimistic
 * patches and error toasts stay consistent everywhere.
 */
import { isIpcError, tasks as tasksApi } from "@/ipc/api";
import type { Task, TaskInput, TaskList } from "@/ipc/types";
import { toast } from "@/components";
import { t } from "@/lib/i18n";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { queryClient } from "@/lib/queryClient";
import { taskToInput } from "./logic";

export function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/* ─────────────────────────── Cache helpers ───────────────────────────── */

export function invalidateTaskQueries(): void {
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.tasks });
}

export function invalidateDerivedData(): void {
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.taskLists });
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.smartCounts });
}

export function invalidateAllTaskData(): void {
  invalidateTaskQueries();
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.task });
  invalidateDerivedData();
}

/** Latest known copy of a task from any cache (detail or list queries). */
export function findCachedTask(id: string): Task | null {
  const direct = queryClient.getQueryData<Task>(queryKeys.task(id));
  if (direct) return direct;
  const queries = queryClient.getQueriesData<Task[]>({ queryKey: queryKeyPrefixes.tasks });
  for (const [, data] of queries) {
    const found = data?.find((task) => task.id === id);
    if (found) return found;
  }
  return null;
}

export function patchTaskCaches(id: string, update: (task: Task) => Task): void {
  queryClient.setQueriesData<Task[]>({ queryKey: queryKeyPrefixes.tasks }, (current) => {
    if (!current) return current;
    let changed = false;
    const next = current.map((task) => {
      if (task.id !== id) return task;
      const updated = update(task);
      if (updated !== task) changed = true;
      return updated;
    });
    return changed ? next : current;
  });
  const cached = queryClient.getQueryData<Task>(queryKeys.task(id));
  if (cached) {
    queryClient.setQueryData<Task>(queryKeys.task(id), update(cached));
  }
}

export function replaceTaskInCaches(task: Task): void {
  patchTaskCaches(task.id, () => task);
}

export function removeTaskFromCaches(id: string): void {
  queryClient.setQueriesData<Task[]>({ queryKey: queryKeyPrefixes.tasks }, (current) =>
    current ? current.filter((task) => task.id !== id) : current,
  );
  queryClient.removeQueries({ queryKey: queryKeys.task(id) });
}

function snapshotTaskQueries(): Array<[readonly unknown[], Task[] | undefined]> {
  return queryClient.getQueriesData<Task[]>({ queryKey: queryKeyPrefixes.tasks });
}

function restoreTaskQueries(snapshot: Array<[readonly unknown[], Task[] | undefined]>): void {
  for (const [key, data] of snapshot) {
    queryClient.setQueryData(key, data);
  }
}

/* ───────────────────────────── Mutations ─────────────────────────────── */

export async function setTaskCompleted(task: Task, completed: boolean): Promise<void> {
  const previous = findCachedTask(task.id) ?? task;
  const desired = completed ? "completed" : "notStarted";
  if (previous.status === desired) return;
  const optimistic: Partial<Task> = {
    status: completed ? "completed" : "notStarted",
    completedAt: completed ? new Date().toISOString() : null,
  };
  patchTaskCaches(task.id, (current) => ({ ...current, ...optimistic }));
  try {
    const saved = await tasksApi.setCompleted(task.id, completed);
    replaceTaskInCaches(saved);
    invalidateDerivedData();
  } catch (error) {
    replaceTaskInCaches(previous);
    toast.error(t("todo.error.action"), errorMessage(error));
  }
}

export async function setTaskImportance(task: Task, importance: Task["importance"]): Promise<void> {
  const previous = findCachedTask(task.id) ?? task;
  if (previous.importance === importance) return;
  patchTaskCaches(task.id, (current) => ({ ...current, importance }));
  try {
    const saved = await tasksApi.setImportance(task.id, importance);
    replaceTaskInCaches(saved);
    invalidateDerivedData();
    invalidateTaskQueries();
  } catch (error) {
    replaceTaskInCaches(previous);
    toast.error(t("todo.error.action"), errorMessage(error));
  }
}

export async function setTaskMyDay(task: Task, date: Task["myDayDate"]): Promise<void> {
  const previous = findCachedTask(task.id) ?? task;
  if (previous.myDayDate === date) return;
  patchTaskCaches(task.id, (current) => ({ ...current, myDayDate: date }));
  try {
    const saved = await tasksApi.setMyDay(task.id, date);
    replaceTaskInCaches(saved);
    invalidateDerivedData();
    invalidateTaskQueries();
  } catch (error) {
    replaceTaskInCaches(previous);
    toast.error(t("todo.error.action"), errorMessage(error));
  }
}

export async function moveTaskToList(
  taskId: string,
  listId: string,
  listName: string,
): Promise<boolean> {
  const previous = findCachedTask(taskId);
  if (previous?.listId === listId) return false;
  if (previous) patchTaskCaches(taskId, (current) => ({ ...current, listId }));
  try {
    const saved = await tasksApi.move(taskId, listId);
    replaceTaskInCaches(saved);
    invalidateDerivedData();
    invalidateTaskQueries();
    toast.success(t("todo.toast.movedToList", { name: listName }));
    return true;
  } catch (error) {
    if (previous) replaceTaskInCaches(previous);
    toast.error(t("todo.error.move"), errorMessage(error));
    return false;
  }
}

const saveQueues = new Map<string, Promise<Task | null>>();

export interface SaveTaskOptions {
  /** Fields to apply to cached tasks immediately (the API call follows). */
  optimistic?: Partial<Task>;
  /** Membership may have changed (due date, my day, list) -> refetch list. */
  refreshList?: boolean;
}

/**
 * Serialize writes per task: rapid title/notes edits cannot overtake each
 * other, so the last user edit always wins on the backend.
 */
export function saveTask(id: string, input: TaskInput, options?: SaveTaskOptions): Promise<Task | null> {
  const previous = findCachedTask(id);
  if (options?.optimistic && previous) {
    patchTaskCaches(id, (current) => ({ ...current, ...options.optimistic }));
  }
  const tail = saveQueues.get(id) ?? Promise.resolve(null);
  const next = tail.catch(() => null).then(async () => {
    try {
      const saved = await tasksApi.update(id, input);
      replaceTaskInCaches(saved);
      invalidateDerivedData();
      if (options?.refreshList) invalidateTaskQueries();
      return saved;
    } catch (error) {
      if (previous) replaceTaskInCaches(previous);
      toast.error(t("todo.error.update"), errorMessage(error));
      invalidateTaskQueries();
      return null;
    }
  });
  saveQueues.set(id, next);
  void next.finally(() => {
    if (saveQueues.get(id) === next) saveQueues.delete(id);
  });
  return next;
}

/** Update one or more TaskInput fields, optimistically. */
export function updateTaskFields(
  task: Task,
  patch: Partial<TaskInput>,
  options?: { refreshList?: boolean },
): Promise<Task | null> {
  const latest = findCachedTask(task.id) ?? task;
  const input: TaskInput = { ...taskToInput(latest), ...patch };
  return saveTask(task.id, input, {
    optimistic: patch as Partial<Task>,
    refreshList: options?.refreshList,
  });
}

export async function createTask(listId: string, input: TaskInput): Promise<Task | null> {
  try {
    const task = await tasksApi.create(listId, input);
    invalidateAllTaskData();
    return task;
  } catch (error) {
    toast.error(t("todo.error.create"), errorMessage(error));
    return null;
  }
}

export async function reorderTasks(listId: string, ids: string[]): Promise<void> {
  const snapshot = snapshotTaskQueries();
  const order = new Map(ids.map((id, index) => [id, index]));
  for (const [key, data] of snapshot) {
    if (!data) continue;
    if (!data.some((task) => order.has(task.id))) continue;
    queryClient.setQueryData(
      key,
      data.map((task) => {
        const index = order.get(task.id);
        return index === undefined ? task : { ...task, sortOrder: index };
      }),
    );
  }
  try {
    await tasksApi.reorder(listId, ids);
    invalidateTaskQueries();
  } catch (error) {
    restoreTaskQueries(snapshot);
    toast.error(t("todo.error.reorder"), errorMessage(error));
  }
}

async function restoreDeletedTask(task: Task): Promise<void> {
  const input = taskToInput(task, { freshStepIds: true });
  const created = await createTask(task.listId, input);
  if (!created) return;
  if (task.status === "completed") {
    try {
      const saved = await tasksApi.setCompleted(created.id, true);
      replaceTaskInCaches(saved);
      invalidateDerivedData();
    } catch (error) {
      toast.error(t("todo.error.action"), errorMessage(error));
    }
  }
  toast.success(t("todo.toast.restored"), task.title);
}

export async function deleteTaskWithUndo(task: Task): Promise<boolean> {
  const snapshot = findCachedTask(task.id) ?? task;
  removeTaskFromCaches(task.id);
  try {
    await tasksApi.delete(task.id);
    invalidateDerivedData();
    toast.show({
      type: "info",
      title: t("todo.toast.deleted"),
      message: snapshot.title,
      duration: 8000,
      actions: [
        {
          label: t("todo.toast.undo"),
          onClick: () => {
            void restoreDeletedTask(snapshot);
          },
        },
      ],
    });
    return true;
  } catch (error) {
    toast.error(t("todo.error.delete"), errorMessage(error));
    invalidateAllTaskData();
    return false;
  }
}

/* ───────────────────────────── Task lists ────────────────────────────── */

export async function createTaskList(
  accountId: string,
  name: string,
  color: string,
): Promise<TaskList | null> {
  try {
    const list = await tasksApi.createList(accountId, name, color);
    void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.taskLists });
    return list;
  } catch (error) {
    toast.error(t("todo.error.list"), errorMessage(error));
    return null;
  }
}

export async function updateTaskList(
  id: string,
  name: string | null,
  color: string | null,
): Promise<TaskList | null> {
  try {
    const list = await tasksApi.updateList(id, name, color);
    void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.taskLists });
    return list;
  } catch (error) {
    toast.error(t("todo.error.list"), errorMessage(error));
    return null;
  }
}

export async function deleteTaskList(id: string): Promise<boolean> {
  try {
    await tasksApi.deleteList(id);
    invalidateAllTaskData();
    toast.success(t("todo.toast.listDeleted"));
    return true;
  } catch (error) {
    toast.error(t("todo.error.list"), errorMessage(error));
    return false;
  }
}
