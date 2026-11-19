/**
 * To Do main pane: list header with sorting / completed visibility / list
 * actions, the inline "add task" row, a virtualized task list with planned
 * groups and a collapsible completed section, plus the resizable detail pane.
 */
import { useEffect, useMemo, useRef, useState, type DragEvent, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Add20Regular,
  ArrowMove20Regular,
  ArrowSyncRegular,
  CalendarLtr20Regular,
  CalendarLtr24Regular,
  CheckmarkCircle20Regular,
  CheckmarkCircle24Regular,
  ChevronDown16Regular,
  Color20Regular,
  Delete20Regular,
  Edit20Regular,
  Mail20Regular,
  MoreHorizontal20Regular,
  Star20Regular,
  Star24Regular,
  TaskListLtr20Regular,
  TaskListLtr24Regular,
  WeatherSunny20Regular,
  WeatherSunny24Regular,
} from "@fluentui/react-icons";
import type { SmartList, Task, TaskList, TaskQuery } from "@/ipc/types";
import {
  Button,
  EmptyState,
  IconButton,
  Menu,
  ModuleSyncStatus,
  Spinner,
  Splitter,
  VirtualList,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import { accounts as accountsApi, sync as syncApi, tasks as tasksApi } from "@/ipc/api";
import { queryClient } from "@/lib/queryClient";
import { queryKeys } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { useAppSettings, useUpdateSettings } from "@/lib/useSettings";
import { useShellStore } from "@/lib/store";
import { cn } from "@/lib/cn";
import { parseIsoDate } from "@/lib/format";
import {
  addDaysIso,
  applySmartDefaults,
  defaultList,
  emptyTaskInput,
  groupListsByAccount,
  groupPlanned,
  isMyDayToday,
  nextWeekIso,
  plannedBucketLabelKey,
  reorderIds,
  reorderIndexFor,
  smartListLabelKey,
  sortCompletedTasks,
  sortModeLabelKey,
  sortTasks,
  splitByCompletion,
  TASK_SORT_MODES,
} from "./logic";
import {
  createTask,
  deleteTaskList,
  deleteTaskWithUndo,
  errorMessage,
  moveTaskToList,
  reorderTasks,
  setTaskCompleted,
  setTaskImportance,
  setTaskMyDay,
  updateTaskFields,
} from "./actions";
import { hasTaskDrag, readDraggedTaskId, writeDraggedTaskId } from "./dnd";
import { useTaskPollIntervalMs, useToday } from "./hooks";
import { useTodoUiStore } from "./uiStore";
import { TaskRow } from "./TaskRow";
import { TaskDetail } from "./TaskDetail";
import { ColorListDialog, DueDateDialog, RenameListDialog } from "./Dialogs";
import styles from "./Main.module.css";

type ListRow =
  | { kind: "group"; key: string; label: string; count: number }
  | { kind: "completed"; key: string; count: number }
  | { kind: "task"; key: string; task: Task };

interface DropTarget {
  taskId: string;
  position: "before" | "after";
}

/**
 * Completed tasks the view is hiding. The task query leaves completed tasks
 * out when "show completed" is off, so an empty result cannot tell an empty
 * list from an all-completed one; `withCompleted` comes from a fallback query
 * that only runs while the visible list is empty.
 */
export function hiddenCompletedCount(
  visible: Task[],
  withCompleted: Task[] | undefined,
): number {
  if (!withCompleted) return 0;
  const visibleIds = new Set(visible.map((task) => task.id));
  return withCompleted.filter(
    (task) => task.status === "completed" && !visibleIds.has(task.id),
  ).length;
}

function SmartHeaderIcon({ smart }: { smart: SmartList }) {
  switch (smart) {
    case "myDay":
      return <WeatherSunny20Regular />;
    case "important":
      return <Star20Regular />;
    case "planned":
      return <CalendarLtr20Regular />;
    case "completed":
      return <CheckmarkCircle20Regular />;
    case "flaggedEmail":
      return <Mail20Regular />;
    default:
      return <TaskListLtr20Regular />;
  }
}

function EmptyIcon({ smart }: { smart: SmartList | null }) {
  switch (smart) {
    case "myDay":
      return <WeatherSunny24Regular />;
    case "important":
      return <Star24Regular />;
    case "planned":
      return <CalendarLtr24Regular />;
    case "completed":
      return <CheckmarkCircle24Regular />;
    case "flaggedEmail":
      return <Mail20Regular />;
    default:
      return <TaskListLtr24Regular />;
  }
}

export function TodoMain() {
  const t = useT();
  const locale = useLanguage();
  const settings = useAppSettings();
  const updateSettings = useUpdateSettings();
  const selection = useShellStore((state) => state.selection.todo);
  const setSelection = useShellStore((state) => state.setSelection);
  const today = useToday();
  const pollInterval = useTaskPollIntervalMs();

  const sortMode = useTodoUiStore((state) => state.sortMode);
  const setSortMode = useTodoUiStore((state) => state.setSortMode);
  const completedCollapsed = useTodoUiStore((state) => state.completedCollapsed);
  const setCompletedCollapsed = useTodoUiStore((state) => state.setCompletedCollapsed);
  const addRequest = useTodoUiStore((state) => state.addRequest);
  const requestDetailEdit = useTodoUiStore((state) => state.requestDetailEdit);
  const requestCreateList = useTodoUiStore((state) => state.requestCreateList);

  const addInputRef = useRef<HTMLInputElement | null>(null);
  const addBusyRef = useRef(false);
  const lastAddRequest = useRef(addRequest);

  const [addValue, setAddValue] = useState("");
  const [syncBusy, setSyncBusy] = useState(false);
  const [draggedId, setDraggedId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);
  const [headerMenuAnchor, setHeaderMenuAnchor] = useState<HTMLElement | null>(null);
  const [dueDialogTask, setDueDialogTask] = useState<Task | null>(null);
  const [renameOpen, setRenameOpen] = useState(false);
  const [colorOpen, setColorOpen] = useState(false);

  const accountsQuery = useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
    staleTime: 60_000,
  });
  const listsQuery = useQuery({
    queryKey: queryKeys.taskLists(),
    queryFn: tasksApi.lists,
    staleTime: 30_000,
    refetchInterval: pollInterval,
  });

  const accounts = accountsQuery.data ?? [];
  const lists = listsQuery.data ?? [];

  const effectiveSmart: SmartList | null =
    selection.smart ?? (selection.listId ? null : "myDay");
  const effectiveListId = effectiveSmart ? null : selection.listId;
  const showCompleted = settings.tasks.showCompleted;
  const includeCompleted = showCompleted || effectiveSmart === "completed";

  const taskQueryPayload: TaskQuery = {
    listId: effectiveListId,
    smart: effectiveSmart,
    includeCompleted,
    today,
  };

  const taskQuery = useQuery({
    queryKey: queryKeys.tasks(taskQueryPayload),
    queryFn: () => tasksApi.query(taskQueryPayload),
    staleTime: 15_000,
    refetchInterval: pollInterval,
    retry: false,
  });

  const tasks = useMemo(() => taskQuery.data ?? [], [taskQuery.data]);
  const listsById = useMemo(() => {
    const map = new Map<string, TaskList>();
    for (const list of lists) map.set(list.id, list);
    return map;
  }, [lists]);
  const groupData = useMemo(() => groupListsByAccount(lists, accounts), [accounts, lists]);

  const currentList = effectiveListId ? listsById.get(effectiveListId) ?? null : null;
  const addTargetList = effectiveListId ? currentList : defaultList(lists);
  const noLists = listsQuery.isSuccess && lists.length === 0;

  // The normal query drops completed tasks, so an empty list cannot say
  // whether anything is hidden. Ask once (only while the list is empty) how
  // many completed tasks the view is leaving out.
  const hiddenCompletedQuery = useQuery({
    queryKey: queryKeys.tasks({ ...taskQueryPayload, includeCompleted: true }),
    queryFn: () => tasksApi.query({ ...taskQueryPayload, includeCompleted: true }),
    enabled:
      !showCompleted &&
      effectiveSmart !== "completed" &&
      !noLists &&
      taskQuery.isSuccess &&
      tasks.length === 0,
    staleTime: 15_000,
    refetchInterval: pollInterval,
    retry: false,
  });
  const hiddenCompleted = hiddenCompletedCount(tasks, hiddenCompletedQuery.data);

  const currentReadOnly = currentList?.isReadOnly ?? false;
  const smartBlocksAdd =
    effectiveSmart === "completed" || effectiveSmart === "flaggedEmail";
  const canAdd =
    !noLists && !smartBlocksAdd && addTargetList !== null && !addTargetList.isReadOnly;

  useEffect(() => {
    if (addRequest === lastAddRequest.current) return;
    lastAddRequest.current = addRequest;
    if (canAdd) {
      addInputRef.current?.focus();
    } else if (noLists) {
      requestCreateList();
    }
  }, [addRequest, canAdd, noLists, requestCreateList]);

  const { active, completed } = useMemo(() => splitByCompletion(tasks), [tasks]);

  const rows = useMemo<ListRow[]>(() => {
    const result: ListRow[] = [];
    const weekStartsOn = settings.calendar.firstDayOfWeek;

    if (effectiveSmart === "completed") {
      const items = sortCompletedTasks(completed);
      if (items.length > 0) {
        result.push({ kind: "completed", key: "completed-header", count: items.length });
        if (!completedCollapsed) {
          for (const task of items) {
            result.push({ kind: "task", key: task.id, task });
          }
        }
      }
      return result;
    }

    if (effectiveSmart === "planned") {
      const groups = groupPlanned(active, today, weekStartsOn);
      for (const group of groups) {
        result.push({
          kind: "group",
          key: `group-${group.bucket}`,
          label: t(plannedBucketLabelKey(group.bucket)),
          count: group.tasks.length,
        });
        const mode = sortMode === "custom" ? "dueDate" : sortMode;
        for (const task of sortTasks(group.tasks, mode, locale)) {
          result.push({ kind: "task", key: task.id, task });
        }
      }
    } else {
      for (const task of sortTasks(active, sortMode, locale)) {
        result.push({ kind: "task", key: task.id, task });
      }
    }

    if (showCompleted && completed.length > 0) {
      result.push({ kind: "completed", key: "completed-header", count: completed.length });
      if (!completedCollapsed) {
        for (const task of sortCompletedTasks(completed)) {
          result.push({ kind: "task", key: task.id, task });
        }
      }
    }
    return result;
  }, [
    active,
    completed,
    completedCollapsed,
    effectiveSmart,
    locale,
    settings.calendar.firstDayOfWeek,
    showCompleted,
    sortMode,
    t,
    today,
  ]);

  const orderedActiveIds = useMemo(() => {
    if (!effectiveListId) return [] as string[];
    return sortTasks(
      active.filter((task) => task.listId === effectiveListId),
      "custom",
      locale,
    ).map((task) => task.id);
  }, [active, effectiveListId, locale]);

  const completedIdsForList = useMemo(() => {
    if (!effectiveListId) return [] as string[];
    return completed
      .filter((task) => task.listId === effectiveListId)
      .map((task) => task.id);
  }, [completed, effectiveListId]);

  const canReorder = effectiveListId !== null && sortMode === "custom" && !currentReadOnly;

  const selectedTask =
    (selection.taskId ? tasks.find((task) => task.id === selection.taskId) : undefined) ?? null;

  const focusRow = (taskId: string) => {
    window.requestAnimationFrame(() => {
      const element = document.querySelector<HTMLElement>(
        `[data-task-id="${typeof CSS !== "undefined" ? CSS.escape(taskId) : taskId}"]`,
      );
      element?.focus();
    });
  };

  const selectTask = (task: Task) => {
    setSelection("todo", { taskId: task.id });
  };

  const openDetail = (task: Task) => {
    setSelection("todo", { taskId: task.id });
    requestDetailEdit(task.id);
  };

  const moveFocus = (taskId: string, delta: -1 | 1) => {
    const taskRows = rows.filter(
      (row): row is Extract<ListRow, { kind: "task" }> => row.kind === "task",
    );
    const index = taskRows.findIndex((row) => row.task.id === taskId);
    if (index < 0) return;
    const next = taskRows[index + delta];
    if (!next) return;
    setSelection("todo", { taskId: next.task.id });
    focusRow(next.task.id);
  };

  const submitAdd = async () => {
    const title = addValue.trim();
    if (!title || !addTargetList || addBusyRef.current) return;
    addBusyRef.current = true;
    const input = applySmartDefaults(emptyTaskInput(title), effectiveSmart, today);
    const created = await createTask(addTargetList.id, input);
    addBusyRef.current = false;
    if (!created) return;
    setAddValue("");
    setSelection("todo", { taskId: created.id });
    addInputRef.current?.focus();
  };

  const toggleComplete = (task: Task) => {
    void setTaskCompleted(task, task.status !== "completed");
  };

  const toggleImportance = (task: Task) => {
    void setTaskImportance(task, task.importance === "high" ? "normal" : "high");
  };

  const toggleMyDay = (task: Task) => {
    void setTaskMyDay(task, isMyDayToday(task, today) ? null : today);
  };

  const setDue = (task: Task, dueDate: string | null) => {
    void updateTaskFields(task, { dueDate }, { refreshList: true });
  };

  const removeTask = (task: Task) => {
    void deleteTaskWithUndo(task).then((deleted) => {
      if (deleted && useShellStore.getState().selection.todo.taskId === task.id) {
        setSelection("todo", { taskId: null });
      }
    });
  };

  const moveTask = (task: Task, list: TaskList) => {
    void moveTaskToList(task.id, list.id, list.name);
  };

  const moveSubmenu = (task: Task): MenuEntry[] => {
    const entries: MenuEntry[] = [];
    for (const group of groupData) {
      entries.push({ type: "header", id: `move-header-${group.accountId}`, label: group.label });
      for (const list of group.lists) {
        entries.push({
          id: `move-${list.id}`,
          label: list.name,
          icon: (
            <span
              className={styles.menuDot}
              style={{ background: list.color }}
              aria-hidden="true"
            />
          ),
          checked: task.listId === list.id,
          disabled: task.listId === list.id || list.isReadOnly,
          onSelect: () => moveTask(task, list),
        });
      }
    }
    return entries;
  };

  const taskMenu = (task: Task): MenuEntry[] => {
    const myDay = isMyDayToday(task, today);
    const important = task.importance === "high";
    return [
      {
        id: "my-day",
        label: myDay ? t("todo.menu.myDayRemove") : t("todo.menu.myDayAdd"),
        icon: <WeatherSunny20Regular />,
        onSelect: () => toggleMyDay(task),
      },
      {
        id: "importance",
        label: important ? t("todo.menu.notImportant") : t("todo.menu.important"),
        icon: <Star20Regular />,
        onSelect: () => toggleImportance(task),
      },
      { type: "separator" },
      {
        id: "due-today",
        label: t("todo.menu.dueToday"),
        icon: <CalendarLtr20Regular />,
        onSelect: () => setDue(task, today),
      },
      {
        id: "due-tomorrow",
        label: t("todo.menu.dueTomorrow"),
        icon: <CalendarLtr20Regular />,
        onSelect: () => setDue(task, addDaysIso(today, 1)),
      },
      {
        id: "due-next-week",
        label: t("todo.menu.dueNextWeek"),
        icon: <CalendarLtr20Regular />,
        onSelect: () => setDue(task, nextWeekIso(today)),
      },
      {
        id: "due-pick",
        label: t("todo.menu.duePick"),
        icon: <Edit20Regular />,
        onSelect: () => setDueDialogTask(task),
      },
      ...(task.dueDate
        ? [
            {
              id: "due-clear",
              label: t("todo.menu.dueClear"),
              icon: <Delete20Regular />,
              onSelect: () => setDue(task, null),
            } satisfies MenuEntry,
          ]
        : []),
      { type: "separator" },
      {
        id: "move",
        label: t("todo.menu.moveTo"),
        icon: <ArrowMove20Regular />,
        submenu: moveSubmenu(task),
      },
      { type: "separator" },
      {
        id: "delete",
        label: t("todo.menu.delete"),
        icon: <Delete20Regular />,
        danger: true,
        disabled: listsById.get(task.listId)?.isReadOnly ?? false,
        onSelect: () => removeTask(task),
      },
    ];
  };

  const toggleShowCompleted = () => {
    updateSettings.mutate({ tasks: { showCompleted: !showCompleted } });
  };

  /**
   * Refresh this module only. A task provider having a bad day must not drag
   * mail along, and the round trip should not wait for the poll timer.
   */
  const syncNow = async () => {
    if (syncBusy) return;
    setSyncBusy(true);
    // Ask for the backend's view immediately so the header can show the
    // in-flight state instead of waiting out the status query's stale window.
    void queryClient.invalidateQueries({ queryKey: queryKeys.syncStatus() });
    try {
      await syncApi.now(null, "tasks");
    } catch (error) {
      toast.error(t("todo.error.action"), errorMessage(error));
    } finally {
      setSyncBusy(false);
      void queryClient.invalidateQueries({ queryKey: queryKeys.syncStatus() });
      void taskQuery.refetch();
      void listsQuery.refetch();
    }
  };

  const removeCurrentList = async () => {
    if (!currentList) return;
    const ok = await confirm({
      title: t("todo.deleteList.title"),
      message: t("todo.deleteList.message", { name: currentList.name }),
      confirmLabel: t("todo.deleteList.confirm"),
      cancelLabel: t("common.cancel"),
      danger: true,
    });
    if (!ok) return;
    const deleted = await deleteTaskList(currentList.id);
    if (deleted) {
      setSelection("todo", { listId: null, smart: "myDay", taskId: null });
    }
  };

  const headerMenuItems: MenuEntry[] = [
    { type: "header", id: "sort-header", label: t("todo.sort.label") },
    ...TASK_SORT_MODES.map<MenuEntry>((mode) => ({
      id: `sort-${mode}`,
      label: t(sortModeLabelKey(mode)),
      checked: sortMode === mode,
      onSelect: () => setSortMode(mode),
    })),
    { type: "separator" },
    {
      id: "toggle-completed",
      label: showCompleted ? t("todo.hideCompleted") : t("todo.showCompleted"),
      icon: <CheckmarkCircle20Regular />,
      checked: showCompleted,
      onSelect: toggleShowCompleted,
    },
    ...(currentList
      ? ([
          { type: "separator" },
          {
            id: "list-new-task",
            label: t("todo.header.newTask"),
            icon: <Add20Regular />,
            disabled: currentReadOnly,
            onSelect: () => addInputRef.current?.focus(),
          },
          {
            id: "list-rename",
            label: t("todo.list.rename"),
            icon: <Edit20Regular />,
            disabled: currentReadOnly,
            onSelect: () => setRenameOpen(true),
          },
          {
            id: "list-color",
            label: t("todo.list.color"),
            icon: <Color20Regular />,
            disabled: currentReadOnly,
            onSelect: () => setColorOpen(true),
          },
          {
            id: "list-delete",
            label: t("todo.list.delete"),
            icon: <Delete20Regular />,
            danger: true,
            disabled: currentReadOnly || currentList.isDefault,
            onSelect: () => void removeCurrentList(),
          },
        ] satisfies MenuEntry[])
      : []),
  ];

  const title = effectiveSmart ? t(smartListLabelKey(effectiveSmart)) : currentList?.name ?? t("shell.todo");
  const activeCount = active.length;
  const completedCount = completed.length;
  let subtitle: string;
  if (effectiveSmart === "completed") {
    subtitle = t("todo.count.completed", { count: completedCount });
  } else if (effectiveSmart === "myDay") {
    const parsed = parseIsoDate(today);
    const dateText = parsed
      ? new Intl.DateTimeFormat(locale, { month: "long", day: "numeric", weekday: "long" }).format(parsed)
      : today;
    subtitle = `${dateText} · ${t("todo.count.tasks", { count: activeCount })}`;
  } else {
    subtitle = t("todo.count.tasks", { count: activeCount });
  }

  const addPlaceholder = !canAdd
    ? noLists
      ? t("todo.add.noList")
      : smartBlocksAdd
        ? t("todo.add.disabledSmart")
        : listsQuery.isPending
          ? t("todo.add.placeholder")
          : t("todo.add.disabledReadOnly")
    : effectiveSmart === "myDay"
      ? t("todo.add.placeholderMyDay")
      : effectiveSmart === "important"
        ? t("todo.add.placeholderImportant")
        : effectiveSmart === "planned"
          ? t("todo.add.placeholderPlanned")
          : t("todo.add.placeholder");

  const emptyState = (() => {
    if (noLists && tasks.length === 0) {
      return (
        <EmptyState
          icon={<TaskListLtr24Regular />}
          title={t("todo.empty.noLists.title")}
          description={t("todo.empty.noLists.description")}
          actions={
            <Button appearance="primary" icon={<Add20Regular />} onClick={requestCreateList}>
              {t("todo.newList")}
            </Button>
          }
        />
      );
    }
    // Every task in view is completed and the section is hidden: an empty
    // state would wrongly suggest there is nothing here.
    if (hiddenCompleted > 0) {
      return (
        <EmptyState
          icon={<CheckmarkCircle24Regular />}
          title={t("todo.empty.hiddenCompleted.title")}
          description={t("todo.empty.hiddenCompleted.description", { count: hiddenCompleted })}
          actions={
            <Button icon={<CheckmarkCircle20Regular />} onClick={toggleShowCompleted}>
              {t("todo.showCompleted")}
            </Button>
          }
        />
      );
    }
    const icon = <EmptyIcon smart={effectiveSmart} />;
    if (effectiveSmart) {
      const key = `todo.empty.${effectiveSmart}`;
      return (
        <EmptyState
          icon={icon}
          title={t(`${key}.title`)}
          description={t(`${key}.description`)}
          actions={
            canAdd ? (
              <Button icon={<Add20Regular />} onClick={() => addInputRef.current?.focus()}>
                {t("todo.empty.addTask")}
              </Button>
            ) : undefined
          }
        />
      );
    }
    return (
      <EmptyState
        icon={icon}
        title={t("todo.empty.list.title")}
        description={t("todo.empty.list.description")}
        actions={
          canAdd ? (
            <Button icon={<Add20Regular />} onClick={() => addInputRef.current?.focus()}>
              {t("todo.empty.addTask")}
            </Button>
          ) : undefined
        }
      />
    );
  })();

  const handleRowDragStart = (event: DragEvent<HTMLDivElement>, task: Task) => {
    writeDraggedTaskId(event.dataTransfer, task.id);
    setDraggedId(task.id);
    setDropTarget(null);
  };

  const resetDrag = () => {
    setDraggedId(null);
    setDropTarget(null);
  };

  const handleRowDragOver = (
    event: DragEvent<HTMLDivElement>,
    task: Task,
    position: "before" | "after",
  ) => {
    if (!canReorder || !hasTaskDrag(event.dataTransfer)) return;
    if (draggedId === null || draggedId === task.id) return;
    if (task.listId !== effectiveListId) return;
    event.preventDefault();
    event.stopPropagation();
    event.dataTransfer.dropEffect = "move";
    setDropTarget({ taskId: task.id, position });
  };

  const handleRowDrop = (event: DragEvent<HTMLDivElement>, task: Task) => {
    event.preventDefault();
    event.stopPropagation();
    const movedId = readDraggedTaskId(event.dataTransfer) || draggedId;
    const rect = event.currentTarget.getBoundingClientRect();
    const position =
      event.clientY < rect.top + rect.height / 2 ? "before" : "after";
    resetDrag();
    if (!movedId || !canReorder || !effectiveListId) return;
    if (movedId === task.id || task.listId !== effectiveListId) return;
    const insertIndex = reorderIndexFor(orderedActiveIds, movedId, task.id, position);
    if (insertIndex < 0) return;
    const next = reorderIds(orderedActiveIds, movedId, insertIndex);
    if (next.join("\u0000") === orderedActiveIds.join("\u0000")) return;
    void reorderTasks(effectiveListId, [...next, ...completedIdsForList]);
  };

  const handleContainerDrop = (event: DragEvent<HTMLDivElement>) => {
    const movedId = readDraggedTaskId(event.dataTransfer) || draggedId;
    const wasTargeted = dropTarget !== null || draggedId !== null;
    resetDrag();
    if (!movedId || !canReorder || !effectiveListId || !wasTargeted) return;
    if (!hasTaskDrag(event.dataTransfer)) return;
    event.preventDefault();
    if (!orderedActiveIds.includes(movedId)) return;
    const next = reorderIds(orderedActiveIds, movedId, orderedActiveIds.length);
    if (next.join("\u0000") === orderedActiveIds.join("\u0000")) return;
    void reorderTasks(effectiveListId, [...next, ...completedIdsForList]);
  };

  const renderRow = (row: ListRow): ReactNode => {
    if (row.kind === "group") {
      return (
        <div className={styles.groupHeader}>
          <span className={styles.groupLabel}>{row.label}</span>
          <span className={styles.groupCount}>{row.count}</span>
        </div>
      );
    }
    if (row.kind === "completed") {
      return (
        <button
          type="button"
          className={styles.completedHeader}
          aria-expanded={!completedCollapsed}
          onClick={() => setCompletedCollapsed(!completedCollapsed)}
        >
          <ChevronDown16Regular
            className={cn(styles.chevron, completedCollapsed && styles.chevronCollapsed)}
            aria-hidden="true"
          />
          <span className={styles.groupLabel}>{t("todo.completedSection")}</span>
          <span className={styles.groupCount}>{row.count}</span>
        </button>
      );
    }
    const task = row.task;
    const list = listsById.get(task.listId) ?? null;
    const readOnly = list?.isReadOnly ?? false;
    return (
      <TaskRow
        task={task}
        list={list}
        today={today}
        locale={locale}
        t={t}
        selected={selection.taskId === task.id}
        showListName={Boolean(effectiveSmart)}
        readOnly={readOnly}
        menuItems={taskMenu(task)}
        dropPosition={dropTarget?.taskId === task.id ? dropTarget.position : null}
        dragging={draggedId === task.id}
        onSelect={() => selectTask(task)}
        onOpenDetail={() => openDetail(task)}
        onToggleComplete={() => toggleComplete(task)}
        onToggleImportance={() => toggleImportance(task)}
        onDelete={() => removeTask(task)}
        onMoveFocus={(delta) => moveFocus(task.id, delta)}
        onDragStart={(event) => handleRowDragStart(event, task)}
        onDragEnd={resetDrag}
        onDragOver={(event, position) => handleRowDragOver(event, task, position)}
        onDrop={(event) => handleRowDrop(event, task)}
      />
    );
  };

  const detailPane = (
    <TaskDetail
      taskId={selection.taskId}
      seedTask={selectedTask}
      lists={lists}
      accounts={accounts}
      today={today}
      onDeleted={() => setSelection("todo", { taskId: null })}
    />
  );

  return (
    <div className={styles.root}>
      <Splitter
        storageKey="todo.list"
        className={styles.splitter}
        primaryClassName={styles.listPane}
        secondaryClassName={styles.detailPane}
        primary={
          <div className={styles.listColumn}>
            <header className={styles.header}>
              <span className={styles.headerIcon} aria-hidden="true">
                {effectiveSmart ? (
                  <SmartHeaderIcon smart={effectiveSmart} />
                ) : (
                  <span
                    className={styles.headerListDot}
                    style={{ background: currentList?.color ?? "var(--text-tertiary)" }}
                  />
                )}
              </span>
              <div className={styles.headerText}>
                <h1 className={styles.title}>{title}</h1>
                <span className={styles.subtitle}>{subtitle}</span>
              </div>
              <span className={styles.spacer} />
              <ModuleSyncStatus module="tasks" className={styles.headerSync} />
              <IconButton
                icon={syncBusy ? <Spinner size="tiny" /> : <ArrowSyncRegular />}
                label={t("common.syncNow")}
                disabled={syncBusy}
                onClick={() => void syncNow()}
              />
              {taskQuery.isFetching ? <Spinner size="tiny" /> : null}
              <IconButton
                icon={<MoreHorizontal20Regular />}
                label={t("todo.header.menu")}
                aria-haspopup="menu"
                onClick={(event) => setHeaderMenuAnchor(event.currentTarget)}
              />
            </header>

            <div className={styles.addRow}>
              <Add20Regular className={styles.addIcon} aria-hidden="true" />
              <input
                ref={addInputRef}
                className={styles.addInput}
                type="text"
                value={addValue}
                placeholder={addPlaceholder}
                aria-label={addPlaceholder}
                disabled={!canAdd}
                onChange={(event) => setAddValue(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void submitAdd();
                  } else if (event.key === "Escape") {
                    setAddValue("");
                    event.currentTarget.blur();
                  }
                }}
              />
            </div>

            <div
              className={styles.rows}
              onDragOver={(event) => {
                if (!canReorder || draggedId === null) return;
                if (!hasTaskDrag(event.dataTransfer)) return;
                event.preventDefault();
                event.dataTransfer.dropEffect = "move";
              }}
              onDrop={handleContainerDrop}
              onDragLeave={(event) => {
                const next = event.relatedTarget;
                if (next instanceof Node && event.currentTarget.contains(next)) return;
                setDropTarget(null);
              }}
            >
              {taskQuery.isError ? (
                <div className={styles.emptyWrap}>
                  <EmptyState
                    icon={<TaskListLtr24Regular />}
                    title={t("todo.error.load")}
                    description={taskQuery.error ? errorMessage(taskQuery.error) : undefined}
                    actions={
                      <Button size="small" onClick={() => void taskQuery.refetch()}>
                        {t("common.retry")}
                      </Button>
                    }
                  />
                </div>
              ) : taskQuery.isPending ? (
                <div className={styles.loading}>
                  <Spinner label={t("common.loading")} />
                </div>
              ) : (
                <VirtualList
                  items={rows}
                  renderItem={(row) => renderRow(row)}
                  getKey={(row) => row.key}
                  estimateSize={46}
                  overscan={12}
                  className={styles.virtualList}
                  emptyState={<div className={styles.emptyWrap}>{emptyState}</div>}
                  ariaLabel={t("todo.tasks")}
                />
              )}
            </div>
          </div>
        }
        secondary={detailPane}
        defaultSize={520}
        min={340}
        max={900}
        handleLabel={t("common.dragToResize")}
      />

      <Menu
        open={headerMenuAnchor !== null}
        onClose={() => setHeaderMenuAnchor(null)}
        anchor={headerMenuAnchor}
        items={headerMenuItems}
        placement="bottom-end"
        minWidth={240}
        ariaLabel={t("todo.header.menu")}
      />

      <DueDateDialog
        open={dueDialogTask !== null}
        onClose={() => setDueDialogTask(null)}
        value={dueDialogTask?.dueDate ?? null}
        onPick={(date) => {
          if (dueDialogTask) setDue(dueDialogTask, date);
        }}
      />
      <RenameListDialog open={renameOpen} onClose={() => setRenameOpen(false)} list={currentList} />
      <ColorListDialog open={colorOpen} onClose={() => setColorOpen(false)} list={currentList} />
    </div>
  );
}
