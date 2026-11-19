/**
 * Task detail pane: editable title, steps with drag reordering, My Day,
 * reminder, due date, recurrence, autosaving notes, linked e-mail and metadata.
 * Providers that cannot round-trip a feature still let the user edit it, with a
 * subtle "仅保存在本机" hint.
 */
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
} from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Add16Regular,
  ArrowRepeatAll20Regular,
  CalendarLtr20Regular,
  Checkmark16Regular,
  ChevronRight16Regular,
  Clock20Regular,
  Delete20Regular,
  Dismiss16Regular,
  Mail20Regular,
  Reorder20Regular,
  Star20Filled,
  Star20Regular,
  TaskListLtr24Regular,
  WeatherSunny20Regular,
} from "@fluentui/react-icons";
import type { Account, IsoDate, RecurrenceRule, Task, TaskInput, TaskList, TaskStep } from "@/ipc/types";
import {
  Badge,
  Button,
  DatePicker,
  EmptyState,
  IconButton,
  Menu,
  Spinner,
  TextArea,
  TimePicker,
  type MenuEntry,
} from "@/components";
import { app, tasks as tasksApi } from "@/ipc/api";
import { queryKeys } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { formatDateTime } from "@/lib/format";
import {
  RECURRENCE_PRESETS,
  addDaysIso,
  anyFeatureUnsupported,
  combineReminder,
  createStep,
  defaultReminder,
  describeRecurrence,
  formatDueLong,
  formatReminderLabel,
  groupListsByAccount,
  isMyDayToday,
  isOverdue,
  listFeatureSupport,
  moveStep,
  nextWeekIso,
  presetRecurrence,
  reorderIndexFor,
  splitReminder,
  stepsProgress,
  taskToInput,
  type RecurrencePreset,
} from "./logic";
import {
  deleteTaskWithUndo,
  errorMessage,
  moveTaskToList,
  saveTask,
  setTaskCompleted,
  setTaskImportance,
  setTaskMyDay,
} from "./actions";
import { DueDateDialog, RecurrenceDialog } from "./Dialogs";
import { useTodoUiStore } from "./uiStore";
import styles from "./TaskDetail.module.css";

const STEP_DRAG_TYPE = "application/x-nuncio-step";

export interface TaskDetailProps {
  taskId: string | null;
  seedTask: Task | null;
  lists: TaskList[];
  accounts: Account[];
  today: IsoDate;
  onDeleted: () => void;
}

interface ActionRowProps {
  icon: ReactNode;
  label: ReactNode;
  value?: ReactNode;
  hint?: ReactNode;
  disabled?: boolean;
  accent?: boolean;
  menu?: boolean;
  onClick?: (event: MouseEvent<HTMLButtonElement>) => void;
}

function ActionRow({ icon, label, value, hint, disabled, accent, menu, onClick }: ActionRowProps) {
  return (
    <button
      type="button"
      className={styles.actionRow}
      disabled={disabled}
      aria-haspopup={menu ? "menu" : undefined}
      onClick={onClick}
    >
      <span className={accent ? `${styles.actionIcon} ${styles.actionIconAccent}` : styles.actionIcon}>
        {icon}
      </span>
      <span className={styles.actionLabel}>{label}</span>
      {hint ? <span className={styles.localHint}>{hint}</span> : null}
      {value ? <span className={styles.actionValue}>{value}</span> : null}
      {menu && !disabled ? (
        <ChevronRight16Regular className={styles.actionChevron} aria-hidden="true" />
      ) : null}
    </button>
  );
}

export function TaskDetail({ taskId, seedTask, lists, accounts, today, onDeleted }: TaskDetailProps) {
  const t = useT();
  const locale = useLanguage();
  const editRequest = useTodoUiStore((state) => state.editRequest);

  const detailQuery = useQuery({
    queryKey: queryKeys.task(taskId ?? "__none__"),
    queryFn: () => tasksApi.get(taskId ?? ""),
    enabled: taskId !== null,
    initialData: seedTask && seedTask.id === taskId ? seedTask : undefined,
    staleTime: 10_000,
    retry: false,
  });

  const task = detailQuery.data ?? (seedTask && seedTask.id === taskId ? seedTask : null);

  const [title, setTitle] = useState(task?.title ?? "");
  const [notes, setNotes] = useState(task?.notes ?? "");
  const [steps, setSteps] = useState<TaskStep[]>(task?.steps ?? []);
  const [newStep, setNewStep] = useState("");
  const [reminderOpen, setReminderOpen] = useState(false);
  const [reminderDate, setReminderDate] = useState<IsoDate | null>(null);
  const [reminderTime, setReminderTime] = useState<string | null>(null);
  const [dueDialogOpen, setDueDialogOpen] = useState(false);
  const [recurrenceOpen, setRecurrenceOpen] = useState(false);
  const [dueMenuAnchor, setDueMenuAnchor] = useState<HTMLElement | null>(null);
  const [repeatMenuAnchor, setRepeatMenuAnchor] = useState<HTMLElement | null>(null);
  const [moveMenuAnchor, setMoveMenuAnchor] = useState<HTMLElement | null>(null);
  const [stepDragId, setStepDragId] = useState<string | null>(null);
  const [stepDrop, setStepDrop] = useState<{ id: string; position: "before" | "after" } | null>(null);

  const titleRef = useRef<HTMLTextAreaElement | null>(null);
  const titleTimer = useRef<number | null>(null);
  const notesTimer = useRef<number | null>(null);

  const clearTimers = () => {
    if (titleTimer.current !== null) {
      window.clearTimeout(titleTimer.current);
      titleTimer.current = null;
    }
    if (notesTimer.current !== null) {
      window.clearTimeout(notesTimer.current);
      notesTimer.current = null;
    }
  };

  useEffect(() => {
    clearTimers();
    setTitle(task?.title ?? "");
    setNotes(task?.notes ?? "");
    setSteps(task?.steps ?? []);
    setNewStep("");
    setReminderOpen(false);
    setDueDialogOpen(false);
    setRecurrenceOpen(false);
    setDueMenuAnchor(null);
    setRepeatMenuAnchor(null);
    setMoveMenuAnchor(null);
    setStepDragId(null);
    setStepDrop(null);
    // Only reset editing buffers when switching to another task.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [task?.id]);

  useEffect(() => {
    setTitle(task?.title ?? "");
  }, [task?.id, task?.title]);

  useEffect(() => {
    setNotes(task?.notes ?? "");
  }, [task?.id, task?.notes]);

  useEffect(() => {
    setSteps(task?.steps ?? []);
  }, [task?.id, task?.steps]);

  useEffect(() => {
    const parts = splitReminder(task?.reminderAt ?? null);
    setReminderDate(parts.date);
    setReminderTime(parts.time);
  }, [task?.id, task?.reminderAt]);

  useEffect(() => clearTimers, []);

  useLayoutEffect(() => {
    const element = titleRef.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, 120)}px`;
  }, [title]);

  useEffect(() => {
    if (!task || !editRequest || editRequest.taskId !== task.id) return;
    const element = titleRef.current;
    if (!element) return;
    element.focus();
    const end = element.value.length;
    element.setSelectionRange(end, end);
  }, [editRequest, task?.id]);

  const orderedSteps = useMemo(
    () => [...steps].sort((a, b) => a.sortOrder - b.sortOrder),
    [steps],
  );
  const progress = stepsProgress(orderedSteps);
  const recurrenceInitial = useMemo(
    () => task?.recurrence ?? presetRecurrence("custom", task?.dueDate ?? today),
    [task?.recurrence, task?.dueDate, today],
  );

  const list = task ? lists.find((item) => item.id === task.listId) ?? null : null;
  const account = task ? accounts.find((item) => item.id === task.accountId) ?? null : null;
  const support = listFeatureSupport(list, account);
  const readOnly = list?.isReadOnly ?? false;

  const buildInput = (current: Task, patch: Partial<TaskInput>): TaskInput => {
    const trimmed = title.trim() || current.title;
    const nextSteps = orderedSteps.map((step, index) => ({ ...step, sortOrder: index }));
    return {
      ...taskToInput(current),
      title: trimmed,
      notes,
      steps: nextSteps,
      ...patch,
    };
  };

  const saveField = (patch: Partial<TaskInput>, refreshList = false) => {
    if (!task) return;
    void saveTask(task.id, buildInput(task, patch), { optimistic: patch as Partial<Task>, refreshList });
  };

  const commitTitle = (value?: string) => {
    if (titleTimer.current !== null) {
      window.clearTimeout(titleTimer.current);
      titleTimer.current = null;
    }
    if (!task) return;
    const next = (value ?? title).trim();
    if (!next) {
      setTitle(task.title);
      return;
    }
    if (next === task.title) return;
    saveField({ title: next });
  };

  const changeTitle = (value: string) => {
    setTitle(value);
    if (titleTimer.current !== null) window.clearTimeout(titleTimer.current);
    titleTimer.current = window.setTimeout(() => commitTitle(value), 600);
  };

  const commitNotes = (value?: string) => {
    if (notesTimer.current !== null) {
      window.clearTimeout(notesTimer.current);
      notesTimer.current = null;
    }
    if (!task) return;
    const next = value ?? notes;
    if (next === task.notes) return;
    saveField({ notes: next });
  };

  const changeNotes = (value: string) => {
    setNotes(value);
    if (notesTimer.current !== null) window.clearTimeout(notesTimer.current);
    notesTimer.current = window.setTimeout(() => commitNotes(value), 700);
  };

  const commitSteps = (next: TaskStep[]) => {
    const renumbered = next.map((step, index) => ({ ...step, sortOrder: index }));
    setSteps(renumbered);
    saveField({ steps: renumbered });
  };

  const toggleStep = (step: TaskStep) => {
    commitSteps(
      orderedSteps.map((item) =>
        item.id === step.id ? { ...item, isCompleted: !item.isCompleted } : item,
      ),
    );
  };

  const renameStep = (step: TaskStep, element: HTMLInputElement) => {
    const value = element.value.trim();
    if (!value) {
      element.value = step.title;
      return;
    }
    if (value === step.title) return;
    commitSteps(
      orderedSteps.map((item) => (item.id === step.id ? { ...item, title: value } : item)),
    );
  };

  const removeStep = (step: TaskStep) => {
    commitSteps(orderedSteps.filter((item) => item.id !== step.id));
  };

  const addStep = () => {
    const value = newStep.trim();
    if (!value) return;
    setNewStep("");
    commitSteps([...orderedSteps, createStep(value, orderedSteps.length)]);
  };

  const moveStepTo = (stepId: string, index: number) => {
    const next = moveStep(orderedSteps, stepId, index);
    if (next === orderedSteps) return;
    commitSteps(next);
  };

  const handleStepDragStart = (event: DragEvent<HTMLElement>, step: TaskStep) => {
    event.dataTransfer.setData(STEP_DRAG_TYPE, step.id);
    event.dataTransfer.effectAllowed = "move";
    setStepDragId(step.id);
  };

  const handleStepDragOver = (event: DragEvent<HTMLDivElement>, step: TaskStep) => {
    if (!stepDragId || stepDragId === step.id) return;
    event.preventDefault();
    event.stopPropagation();
    const rect = event.currentTarget.getBoundingClientRect();
    const position = event.clientY < rect.top + rect.height / 2 ? "before" : "after";
    setStepDrop({ id: step.id, position });
  };

  const handleStepDrop = (event: DragEvent<HTMLDivElement>, step: TaskStep) => {
    event.preventDefault();
    event.stopPropagation();
    const movedId = event.dataTransfer.getData(STEP_DRAG_TYPE) || stepDragId;
    const position = stepDrop?.id === step.id ? stepDrop.position : "before";
    setStepDragId(null);
    setStepDrop(null);
    if (!movedId || movedId === step.id) return;
    const ids = orderedSteps.map((item) => item.id);
    const insert = reorderIndexFor(ids, movedId, step.id, position);
    if (insert < 0) return;
    moveStepTo(movedId, insert);
  };

  const handleStepKeyDown = (event: KeyboardEvent<HTMLElement>, step: TaskStep) => {
    if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
    event.preventDefault();
    const index = orderedSteps.findIndex((item) => item.id === step.id);
    if (index < 0) return;
    const target = event.key === "ArrowUp" ? index - 1 : index + 1;
    if (target < 0 || target >= orderedSteps.length) return;
    moveStepTo(step.id, target);
  };

  const applyRecurrence = (rule: RecurrenceRule | null) => {
    saveField({ recurrence: rule }, true);
  };

  const applyReminder = (date: IsoDate, time: string) => {
    setReminderDate(date);
    setReminderTime(time);
    saveField({ reminderAt: combineReminder(date, time) });
  };

  const clearReminder = () => {
    setReminderOpen(false);
    saveField({ reminderAt: null });
  };

  const openReminderEditor = () => {
    if (reminderOpen) {
      setReminderOpen(false);
      return;
    }
    if (!reminderDate || !reminderTime) {
      const defaults = defaultReminder(new Date());
      setReminderDate(defaults.date);
      setReminderTime(defaults.time);
    }
    setReminderOpen(true);
  };

  const dueMenuItems: MenuEntry[] = [
    { id: "due-today", label: t("todo.menu.dueToday"), onSelect: () => saveField({ dueDate: today }, true) },
    {
      id: "due-tomorrow",
      label: t("todo.menu.dueTomorrow"),
      onSelect: () => saveField({ dueDate: addDaysIso(today, 1) }, true),
    },
    {
      id: "due-next-week",
      label: t("todo.menu.dueNextWeek"),
      onSelect: () => saveField({ dueDate: nextWeekIso(today) }, true),
    },
    {
      id: "due-pick",
      label: t("todo.menu.duePick"),
      onSelect: () => setDueDialogOpen(true),
    },
    ...(task?.dueDate
      ? ([
          { type: "separator" },
          {
            id: "due-clear",
            label: t("todo.menu.dueClear"),
            danger: true,
            onSelect: () => saveField({ dueDate: null }, true),
          },
        ] satisfies MenuEntry[])
      : []),
  ];

  const repeatMenuItems: MenuEntry[] = RECURRENCE_PRESETS.map<MenuEntry>((preset) => ({
    id: `repeat-${preset}`,
    label:
      preset === "none"
        ? t("todo.detail.repeatNone")
        : preset === "custom"
          ? t("todo.repeat.custom.title")
          : t(presetLabelKey(preset)),
    checked: preset === "none" ? !task?.recurrence : false,
    onSelect: () => {
      if (preset === "custom") {
        setRecurrenceOpen(true);
        return;
      }
      applyRecurrence(presetRecurrence(preset, task?.dueDate ?? today));
    },
  }));

  const moveMenuItems: MenuEntry[] = [];
  for (const group of groupListsByAccount(lists, accounts)) {
    moveMenuItems.push({ type: "header", id: `move-header-${group.accountId}`, label: group.label });
    for (const target of group.lists) {
      moveMenuItems.push({
        id: `move-${target.id}`,
        label: target.name,
        icon: (
          <span
            className={styles.menuDot}
            style={{ background: target.color }}
            aria-hidden="true"
          />
        ),
        checked: target.id === task?.listId,
        disabled: target.id === task?.listId || target.isReadOnly,
        onSelect: () => {
          if (task) void moveTaskToList(task.id, target.id, target.name);
        },
      });
    }
  }

  const openLinkedEmail = () => {
    if (!task?.linkedMessageId) return;
    const route = `/message/${task.linkedMessageId}`;
    app.openWindow(route, task.title || t("shell.window.message")).catch((error: unknown) => {
      console.warn("[todo] open linked message failed", error);
      if (typeof window !== "undefined") window.location.hash = `#${route}`;
    });
  };

  const handleDelete = async () => {
    if (!task) return;
    const deleted = await deleteTaskWithUndo(task);
    if (deleted) onDeleted();
  };

  if (!taskId) {
    return (
      <div className={styles.emptyPane}>
        <EmptyState
          icon={<TaskListLtr24Regular />}
          title={t("todo.empty.selectTask.title")}
          description={t("todo.empty.selectTask.description")}
        />
      </div>
    );
  }

  if (!task) {
    if (detailQuery.isError) {
      return (
        <div className={styles.emptyPane}>
          <EmptyState
            icon={<TaskListLtr24Regular />}
            title={t("todo.error.load")}
            description={
              detailQuery.error ? errorMessage(detailQuery.error) : t("todo.error.taskMissing")
            }
            actions={
              <Button size="small" onClick={onDeleted}>
                {t("common.close")}
              </Button>
            }
          />
        </div>
      );
    }
    return (
      <div className={styles.loadingPane}>
        <Spinner label={t("common.loading")} />
      </div>
    );
  }

  const myDay = isMyDayToday(task, today);
  const dueOverdue = isOverdue(task.dueDate, today);
  const featureHint = (supported: boolean) =>
    supported ? undefined : t("todo.detail.localOnly");

  return (
    <div className={styles.detail}>
      <div className={styles.titleRow}>
        <button
          type="button"
          className={
            task.status === "completed"
              ? `${styles.completeButton} ${styles.completeButtonOn}`
              : styles.completeButton
          }
          aria-label={
            task.status === "completed" ? t("todo.row.uncomplete") : t("todo.row.complete")
          }
          aria-pressed={task.status === "completed"}
          disabled={readOnly}
          onClick={() => void setTaskCompleted(task, task.status !== "completed")}
        >
          <Checkmark16Regular aria-hidden="true" />
        </button>
        <textarea
          ref={titleRef}
          className={
            task.status === "completed"
              ? `${styles.titleInput} ${styles.titleCompleted}`
              : styles.titleInput
          }
          rows={1}
          value={title}
          placeholder={t("todo.detail.titlePlaceholder")}
          aria-label={t("todo.detail.titlePlaceholder")}
          disabled={readOnly}
          onChange={(event) => changeTitle(event.target.value)}
          onBlur={() => commitTitle()}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              event.currentTarget.blur();
            }
          }}
        />
        <IconButton
          className={styles.starButton}
          icon={task.importance === "high" ? <Star20Filled /> : <Star20Regular />}
          label={
            task.importance === "high" ? t("todo.row.notImportant") : t("todo.row.important")
          }
          selected={task.importance === "high"}
          disabled={readOnly}
          tooltip={support.importance ? undefined : t("todo.detail.localOnly")}
          onClick={() =>
            void setTaskImportance(task, task.importance === "high" ? "normal" : "high")
          }
        />
      </div>

      <div className={styles.metaRow}>
        <button
          type="button"
          className={styles.listChip}
          disabled={readOnly || lists.length === 0}
          aria-haspopup="menu"
          onClick={(event) => setMoveMenuAnchor(event.currentTarget)}
        >
          <span
            className={styles.listDot}
            style={{ background: list?.color ?? "var(--text-tertiary)" }}
            aria-hidden="true"
          />
          <span className={styles.listName}>{list?.name ?? t("todo.lists")}</span>
          <ChevronRight16Regular aria-hidden="true" />
        </button>
        {readOnly ? <Badge appearance="outline">{t("todo.list.readOnly")}</Badge> : null}
        {anyFeatureUnsupported(support) ? (
          <span className={styles.localHint} title={t("todo.detail.localOnlyHint")}>
            {t("todo.detail.localOnly")}
          </span>
        ) : null}
      </div>

      <div className={styles.content}>
        <section className={styles.section} aria-label={t("todo.detail.steps")}>
          <div className={styles.sectionHeader}>
            <span className={styles.sectionTitle}>{t("todo.detail.steps")}</span>
            {progress.total > 0 ? (
              <span className={styles.sectionMeta}>
                {progress.done}/{progress.total}
              </span>
            ) : null}
            {!support.steps ? <span className={styles.localHint}>{t("todo.detail.localOnly")}</span> : null}
          </div>
          <div className={styles.stepList}>
            {orderedSteps.map((step) => (
              <div
                key={step.id}
                className={styles.stepRow}
                data-drop={stepDrop?.id === step.id ? stepDrop.position : undefined}
                onDragOver={(event) => handleStepDragOver(event, step)}
                onDrop={(event) => handleStepDrop(event, step)}
              >
                <button
                  type="button"
                  className={
                    step.isCompleted
                      ? `${styles.stepCircle} ${styles.stepCircleOn}`
                      : styles.stepCircle
                  }
                  aria-label={
                    step.isCompleted ? t("todo.row.uncomplete") : t("todo.row.complete")
                  }
                  aria-pressed={step.isCompleted}
                  disabled={readOnly}
                  onClick={() => toggleStep(step)}
                >
                  <Checkmark16Regular aria-hidden="true" />
                </button>
                <input
                  className={
                    step.isCompleted
                      ? `${styles.stepInput} ${styles.stepInputDone}`
                      : styles.stepInput
                  }
                  defaultValue={step.title}
                  aria-label={t("todo.detail.stepTitle")}
                  disabled={readOnly}
                  onBlur={(event) => renameStep(step, event.currentTarget)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      event.currentTarget.blur();
                    } else if (event.key === "Escape") {
                      event.currentTarget.value = step.title;
                      event.currentTarget.blur();
                    }
                  }}
                />
                {readOnly ? null : (
                  <span
                    className={styles.stepHandle}
                    role="button"
                    tabIndex={0}
                    aria-label={t("todo.detail.stepMove")}
                    draggable
                    onDragStart={(event) => handleStepDragStart(event, step)}
                    onDragEnd={() => {
                      setStepDragId(null);
                      setStepDrop(null);
                    }}
                    onKeyDown={(event) => handleStepKeyDown(event, step)}
                  >
                    <Reorder20Regular aria-hidden="true" />
                  </span>
                )}
                {readOnly ? null : (
                  <IconButton
                    size="small"
                    className={styles.stepDelete}
                    icon={<Dismiss16Regular />}
                    label={t("todo.detail.stepRemove")}
                    onClick={() => removeStep(step)}
                  />
                )}
              </div>
            ))}
            {readOnly ? null : (
              <div className={styles.stepAddRow}>
                <Add16Regular aria-hidden="true" />
                <input
                  className={styles.stepAddInput}
                  value={newStep}
                  placeholder={t("todo.detail.stepPlaceholder")}
                  aria-label={t("todo.detail.stepPlaceholder")}
                  onChange={(event) => setNewStep(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      addStep();
                    } else if (event.key === "Escape") {
                      setNewStep("");
                    }
                  }}
                />
              </div>
            )}
          </div>
        </section>

        <section className={styles.section} aria-label={t("todo.detail.actions")}>
          <ActionRow
            icon={<WeatherSunny20Regular />}
            label={myDay ? t("todo.detail.removeFromMyDay") : t("todo.detail.addToMyDay")}
            value={myDay ? t("todo.detail.myDayOn") : undefined}
            accent={myDay}
            disabled={readOnly}
            onClick={() => void setTaskMyDay(task, myDay ? null : today)}
          />

          <ActionRow
            icon={<Clock20Regular />}
            label={t("todo.detail.reminder")}
            value={
              task.reminderAt
                ? formatReminderLabel(task.reminderAt, today, t, locale)
                : t("todo.detail.addReminder")
            }
            hint={featureHint(support.reminders)}
            disabled={readOnly}
            onClick={openReminderEditor}
          />
          {reminderOpen && !readOnly ? (
            <div className={styles.reminderEditor}>
              {/* Only the explicit remove button below clears a reminder; a
                  per-field clear used to delete the whole reminder and close
                  the editor. */}
              <DatePicker
                value={reminderDate}
                onChange={(date) => {
                  if (date) applyReminder(date, reminderTime ?? "09:00");
                }}
                label={t("todo.reminder.date")}
              />
              <TimePicker
                value={reminderTime}
                onChange={(time) => {
                  if (time) applyReminder(reminderDate ?? today, time);
                }}
                label={t("todo.reminder.time")}
              />
              {task.reminderAt ? (
                <Button
                  size="small"
                  appearance="subtle"
                  icon={<Delete20Regular />}
                  onClick={clearReminder}
                >
                  {t("todo.reminder.clear")}
                </Button>
              ) : null}
            </div>
          ) : null}

          <ActionRow
            icon={<CalendarLtr20Regular />}
            label={t("todo.detail.due")}
            value={
              task.dueDate ? (
                dueOverdue ? (
                  <span className={styles.overdue}>{formatDueLong(task.dueDate, locale)}</span>
                ) : (
                  formatDueLong(task.dueDate, locale)
                )
              ) : (
                t("todo.detail.addDue")
              )
            }
            disabled={readOnly}
            menu
            onClick={(event) => setDueMenuAnchor(event.currentTarget)}
          />

          <ActionRow
            icon={<ArrowRepeatAll20Regular />}
            label={t("todo.detail.repeat")}
            value={
              task.recurrence
                ? describeRecurrence(task.recurrence, t)
                : t("todo.detail.repeatNone")
            }
            hint={featureHint(support.recurrence)}
            disabled={readOnly}
            menu
            onClick={(event) => setRepeatMenuAnchor(event.currentTarget)}
          />

          {task.linkedMessageId ? (
            <ActionRow
              icon={<Mail20Regular />}
              label={t("todo.detail.linkedEmail")}
              value={t("todo.detail.openEmail")}
              onClick={openLinkedEmail}
            />
          ) : null}
        </section>

        <section className={styles.section} aria-label={t("todo.detail.notes")}>
          <TextArea
            label={t("todo.detail.notes")}
            placeholder={t("todo.detail.notesPlaceholder")}
            value={notes}
            rows={4}
            disabled={readOnly}
            onChange={(event) => changeNotes(event.target.value)}
            onBlur={() => commitNotes()}
          />
        </section>

        <div className={styles.infoBlock}>
          <span>{t("todo.detail.created", { date: formatDateTime(task.createdAt) })}</span>
          {task.completedAt ? (
            <span>{t("todo.detail.completed", { date: formatDateTime(task.completedAt) })}</span>
          ) : null}
        </div>

        {readOnly ? null : (
          <div className={styles.footer}>
            <Button appearance="danger" icon={<Delete20Regular />} onClick={() => void handleDelete()}>
              {t("todo.detail.delete")}
            </Button>
          </div>
        )}
      </div>

      <Menu
        open={dueMenuAnchor !== null}
        onClose={() => setDueMenuAnchor(null)}
        anchor={dueMenuAnchor}
        items={dueMenuItems}
        placement="bottom-end"
        minWidth={220}
        ariaLabel={t("todo.detail.dueDate")}
      />
      <Menu
        open={repeatMenuAnchor !== null}
        onClose={() => setRepeatMenuAnchor(null)}
        anchor={repeatMenuAnchor}
        items={repeatMenuItems}
        placement="bottom-end"
        minWidth={220}
        ariaLabel={t("todo.detail.repeat")}
      />
      <Menu
        open={moveMenuAnchor !== null}
        onClose={() => setMoveMenuAnchor(null)}
        anchor={moveMenuAnchor}
        items={moveMenuItems}
        placement="bottom-start"
        minWidth={240}
        ariaLabel={t("todo.menu.moveTo")}
      />

      <DueDateDialog
        open={dueDialogOpen}
        onClose={() => setDueDialogOpen(false)}
        value={task.dueDate}
        onPick={(date) => saveField({ dueDate: date }, true)}
      />
      <RecurrenceDialog
        open={recurrenceOpen}
        onClose={() => setRecurrenceOpen(false)}
        initial={recurrenceInitial}
        onSave={(rule) => applyRecurrence(rule)}
      />
    </div>
  );
}

function presetLabelKey(preset: RecurrencePreset): string {
  switch (preset) {
    case "daily":
      return "todo.repeat.daily";
    case "weekdays":
      return "todo.repeat.weekdays";
    case "weekly":
      return "todo.repeat.weekly";
    case "monthly":
      return "todo.repeat.monthly";
    case "yearly":
      return "todo.repeat.yearly";
    default:
      return "todo.detail.repeatNone";
  }
}
