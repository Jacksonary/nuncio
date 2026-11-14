/**
 * One task row: completion circle, title, context chips (list, steps, due,
 * reminder, my day, repeat, linked mail), importance star and the context menu.
 * The row is draggable for reorder / move-to-list and fully keyboard operable.
 */
import { useState, type DragEvent, type KeyboardEvent } from "react";
import {
  Alert16Regular,
  ArrowRepeatAll16Regular,
  CalendarLtr16Regular,
  Checkmark16Regular,
  Mail16Regular,
  MoreHorizontal16Regular,
  Star16Filled,
  Star16Regular,
  TaskListSquareLtr16Regular,
  WeatherSunny16Regular,
} from "@fluentui/react-icons";
import type { IsoDate, Task, TaskList } from "@/ipc/types";
import { ContextMenu, IconButton, Menu, type MenuEntry } from "@/components";
import { cn } from "@/lib/cn";
import {
  dueLabel,
  formatReminderLabel,
  isMyDayToday,
  stepsProgress,
  type Translate,
} from "./logic";
import styles from "./TaskRow.module.css";

export interface TaskRowProps {
  task: Task;
  list: TaskList | null;
  today: IsoDate;
  locale: string;
  t: Translate;
  selected: boolean;
  showListName: boolean;
  readOnly: boolean;
  menuItems: MenuEntry[];
  /** Visual drop indicator while dragging another task over this row. */
  dropPosition: "before" | "after" | null;
  dragging: boolean;
  onSelect: () => void;
  onOpenDetail: () => void;
  onToggleComplete: () => void;
  onToggleImportance: () => void;
  onDelete: () => void;
  onMoveFocus: (delta: -1 | 1) => void;
  onDragStart: (event: DragEvent<HTMLDivElement>) => void;
  onDragEnd: () => void;
  onDragOver: (event: DragEvent<HTMLDivElement>, position: "before" | "after") => void;
  onDrop: (event: DragEvent<HTMLDivElement>) => void;
}

export function TaskRow({
  task,
  list,
  today,
  locale,
  t,
  selected,
  showListName,
  readOnly,
  menuItems,
  dropPosition,
  dragging,
  onSelect,
  onOpenDetail,
  onToggleComplete,
  onToggleImportance,
  onDelete,
  onMoveFocus,
  onDragStart,
  onDragEnd,
  onDragOver,
  onDrop,
}: TaskRowProps) {
  const [menuAnchor, setMenuAnchor] = useState<HTMLElement | null>(null);
  const completed = task.status === "completed";
  const progress = stepsProgress(task.steps ?? []);
  const due = task.dueDate ? dueLabel(task.dueDate, today, t, locale) : null;
  const reminderText = task.reminderAt
    ? formatReminderLabel(task.reminderAt, today, t, locale)
    : null;
  const myDay = isMyDayToday(task, today);

  const handleKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.target !== event.currentTarget) return;
    switch (event.key) {
      case "Enter":
        event.preventDefault();
        onOpenDetail();
        break;
      case " ":
        if (readOnly) break;
        event.preventDefault();
        onToggleComplete();
        break;
      case "Delete":
      case "Backspace":
        if (readOnly) break;
        event.preventDefault();
        event.stopPropagation();
        onDelete();
        break;
      case "ArrowDown":
        event.preventDefault();
        onMoveFocus(1);
        break;
      case "ArrowUp":
        event.preventDefault();
        onMoveFocus(-1);
        break;
      default:
        break;
    }
  };

  return (
    <ContextMenu items={menuItems} disabled={readOnly}>
      <div
        className={cn(styles.row, selected && styles.rowSelected)}
        data-task-id={task.id}
        data-dragging={dragging || undefined}
        data-drop={dropPosition ?? undefined}
        role="button"
        tabIndex={0}
        aria-current={selected || undefined}
        aria-label={task.title || t("common.untitled")}
        draggable={!readOnly}
        onClick={onSelect}
        onDoubleClick={(event) => {
          if ((event.target as HTMLElement).closest("button")) return;
          onOpenDetail();
        }}
        onKeyDown={handleKeyDown}
        onDragStart={onDragStart}
        onDragEnd={onDragEnd}
        onDragOver={(event) => {
          const rect = event.currentTarget.getBoundingClientRect();
          const position = event.clientY < rect.top + rect.height / 2 ? "before" : "after";
          onDragOver(event, position);
        }}
        onDrop={onDrop}
      >
        <button
          type="button"
          className={cn(styles.completeButton, completed && styles.completed)}
          aria-label={completed ? t("todo.row.uncomplete") : t("todo.row.complete")}
          aria-pressed={completed}
          disabled={readOnly}
          onClick={(event) => {
            event.stopPropagation();
            onToggleComplete();
          }}
        >
          <Checkmark16Regular className={styles.check} aria-hidden="true" />
        </button>

        <div className={styles.body} onClick={onSelect}>
          <div
            className={cn(styles.title, completed && styles.titleCompleted)}
            title={task.title}
          >
            {task.title || t("common.untitled")}
          </div>
          <div className={styles.chips}>
            {showListName && list ? (
              <span className={styles.chip} title={list.name}>
                <span
                  className={styles.listDot}
                  style={{ background: list.color }}
                  aria-hidden="true"
                />
                <span className={styles.chipText}>{list.name}</span>
              </span>
            ) : null}
            {progress.total > 0 ? (
              <span
                className={styles.chip}
                title={t("todo.chip.stepsLabel", { done: progress.done, total: progress.total })}
              >
                <TaskListSquareLtr16Regular className={styles.chipIcon} aria-hidden="true" />
                <span className={styles.chipNumbers}>
                  {progress.done}/{progress.total}
                </span>
              </span>
            ) : null}
            {due ? (
              <span
                className={cn(styles.chip, due.overdue && styles.chipOverdue)}
                title={t("todo.detail.dueDate")}
              >
                <CalendarLtr16Regular className={styles.chipIcon} aria-hidden="true" />
                <span>{due.text}</span>
              </span>
            ) : null}
            {reminderText ? (
              <span className={styles.chip} title={t("todo.detail.reminder")}>
                <Alert16Regular className={styles.chipIcon} aria-hidden="true" />
                <span>{reminderText}</span>
              </span>
            ) : null}
            {myDay ? (
              <span className={cn(styles.chip, styles.chipAccent)} title={t("todo.chip.myDay")}>
                <WeatherSunny16Regular className={styles.chipIcon} aria-hidden="true" />
              </span>
            ) : null}
            {task.recurrence ? (
              <span className={styles.chip} title={t("todo.chip.repeat")}>
                <ArrowRepeatAll16Regular className={styles.chipIcon} aria-hidden="true" />
              </span>
            ) : null}
            {task.linkedMessageId ? (
              <span className={styles.chip} title={t("todo.chip.email")}>
                <Mail16Regular className={styles.chipIcon} aria-hidden="true" />
              </span>
            ) : null}
          </div>
        </div>

        <IconButton
          size="small"
          className={cn(styles.star, task.importance === "high" && styles.starActive)}
          icon={task.importance === "high" ? <Star16Filled /> : <Star16Regular />}
          label={
            task.importance === "high" ? t("todo.row.notImportant") : t("todo.row.important")
          }
          selected={task.importance === "high"}
          disabled={readOnly}
          onClick={(event) => {
            event.stopPropagation();
            onToggleImportance();
          }}
        />

        <IconButton
          size="small"
          className={styles.more}
          icon={<MoreHorizontal16Regular />}
          label={t("todo.row.menu")}
          disabled={readOnly}
          onClick={(event) => {
            event.stopPropagation();
            setMenuAnchor(event.currentTarget);
          }}
        />

        <Menu
          open={menuAnchor !== null}
          onClose={() => setMenuAnchor(null)}
          anchor={menuAnchor}
          items={menuItems}
          placement="bottom-end"
          minWidth={230}
          ariaLabel={t("todo.row.menu")}
        />
      </div>
    </ContextMenu>
  );
}
