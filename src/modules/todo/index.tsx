/**
 * To Do module entry point: smart lists, per-account task lists, a full task
 * list with drag & drop, and a resizable task detail pane.
 */
import { TaskListLtr24Regular } from "@fluentui/react-icons";
import type { ModuleDefinition } from "@/app/moduleTypes";
import { tasks as tasksApi } from "@/ipc/api";
import { toIsoDate } from "@/lib/format";
import type { HotkeyDefinition } from "@/lib/hotkeys";
import { useShellStore } from "@/lib/store";
import type { Task } from "@/ipc/types";
import { deleteTaskWithUndo, findCachedTask, setTaskMyDay } from "./actions";
import { TodoMain } from "./Main";
import { TodoSidebar } from "./Sidebar";
import { useTodoUiStore } from "./uiStore";

function requestAddTask(): void {
  const shell = useShellStore.getState();
  const selection = shell.selection.todo;
  if (selection.smart === "completed" || selection.smart === "flaggedEmail") {
    shell.setSelection("todo", { smart: "myDay", listId: null, taskId: null });
  }
  useTodoUiStore.getState().requestAdd();
}

async function selectedTask(): Promise<Task | null> {
  const id = useShellStore.getState().selection.todo.taskId;
  if (!id) return null;
  const cached = findCachedTask(id);
  if (cached) return cached;
  try {
    return await tasksApi.get(id);
  } catch {
    return null;
  }
}

function deleteSelectedTask(): void {
  void (async () => {
    const task = await selectedTask();
    if (!task) return;
    const deleted = await deleteTaskWithUndo(task);
    if (deleted) {
      useShellStore.getState().setSelection("todo", { taskId: null });
    }
  })();
}

function toggleMyDayForSelected(): void {
  void (async () => {
    const task = await selectedTask();
    if (!task) return;
    const today = toIsoDate(new Date());
    const next = task.myDayDate === today ? null : today;
    await setTaskMyDay(task, next);
  })();
}

const hotkeys: HotkeyDefinition[] = [
  {
    id: "todo.newTask",
    keys: "Mod+N",
    handler: requestAddTask,
    preventDefault: true,
  },
  {
    id: "todo.deleteTask",
    keys: "Delete",
    handler: deleteSelectedTask,
    preventDefault: true,
  },
  {
    id: "todo.toggleMyDay",
    keys: "Mod+T",
    handler: toggleMyDayForSelected,
    preventDefault: true,
  },
];

export const todoModule: ModuleDefinition = {
  id: "todo",
  labelKey: "shell.todo",
  Icon: TaskListLtr24Regular,
  Sidebar: TodoSidebar,
  Main: TodoMain,
  onNew: requestAddTask,
  hotkeys,
};

export default todoModule;
