/**
 * To Do sidebar: smart lists with live counts, then per-account task lists with
 * colors, plus list context menus (rename / color / delete) and drop targets
 * for "drag a task onto a list to move it".
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Add20Regular,
  CalendarToday20Regular,
  CheckmarkCircle20Regular,
  Color20Regular,
  Delete20Regular,
  Edit20Regular,
  Mail20Regular,
  MoreHorizontal16Regular,
  Star20Regular,
  TaskListLtr20Regular,
  WeatherSunny20Regular,
} from "@fluentui/react-icons";
import type { SmartList, TaskList } from "@/ipc/types";
import {
  Button,
  ContextMenu,
  IconButton,
  Menu,
  confirm,
} from "@/components";
import { accounts as accountsApi, tasks as tasksApi } from "@/ipc/api";
import { queryKeys } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { cn } from "@/lib/cn";
import { formatDate, parseIsoDate } from "@/lib/format";
import { deleteTaskList, moveTaskToList } from "./actions";
import { hasTaskDrag, readDraggedTaskId } from "./dnd";
import { useTaskPollIntervalMs, useToday } from "./hooks";
import { SMART_LISTS, groupListsByAccount, smartCount, smartListLabelKey } from "./logic";
import { ColorListDialog, CreateListDialog, RenameListDialog } from "./Dialogs";
import { useTodoUiStore } from "./uiStore";
import styles from "./Sidebar.module.css";

function SmartListIcon({ smart }: { smart: SmartList }) {
  switch (smart) {
    case "myDay":
      return <WeatherSunny20Regular />;
    case "important":
      return <Star20Regular />;
    case "planned":
      return <CalendarToday20Regular />;
    case "completed":
      return <CheckmarkCircle20Regular />;
    case "flaggedEmail":
      return <Mail20Regular />;
    default:
      return <TaskListLtr20Regular />;
  }
}

type ListDialog =
  | { kind: "create" }
  | { kind: "rename"; list: TaskList }
  | { kind: "color"; list: TaskList }
  | null;

export function TodoSidebar() {
  const t = useT();
  const locale = useLanguage();
  const selection = useShellStore((state) => state.selection.todo);
  const setSelection = useShellStore((state) => state.setSelection);
  const today = useToday();
  const pollInterval = useTaskPollIntervalMs();
  const createListRequest = useTodoUiStore((state) => state.createListRequest);
  const requestAdd = useTodoUiStore((state) => state.requestAdd);

  const [dialog, setDialog] = useState<ListDialog>(null);
  const [rowMenu, setRowMenu] = useState<{ list: TaskList; anchor: HTMLElement } | null>(null);
  const [dropListId, setDropListId] = useState<string | null>(null);
  const lastCreateRequest = useRef(createListRequest);

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
  const countsQuery = useQuery({
    queryKey: queryKeys.smartCounts(today),
    queryFn: () => tasksApi.smartCounts(today),
    staleTime: 30_000,
    refetchInterval: pollInterval,
  });

  const accounts = accountsQuery.data ?? [];
  const lists = listsQuery.data ?? [];
  const counts = countsQuery.data;

  useEffect(() => {
    if (createListRequest === lastCreateRequest.current) return;
    lastCreateRequest.current = createListRequest;
    setDialog({ kind: "create" });
  }, [createListRequest]);

  // Normalize the persisted selection once the list set is known.
  useEffect(() => {
    if (!listsQuery.isSuccess) return;
    const available = listsQuery.data ?? [];
    if (selection.smart && selection.listId) {
      setSelection("todo", { listId: null });
      return;
    }
    if (!selection.smart && !selection.listId) {
      setSelection("todo", { smart: "myDay" });
      return;
    }
    if (selection.listId && !selection.smart && !available.some((list) => list.id === selection.listId)) {
      setSelection("todo", { listId: null, smart: "myDay", taskId: null });
    }
  }, [listsQuery.isSuccess, listsQuery.data, selection.smart, selection.listId, setSelection]);

  const groups = useMemo(() => groupListsByAccount(lists, accounts), [accounts, lists]);

  const currentList = lists.find((list) => list.id === selection.listId) ?? null;
  const defaultAccountId =
    currentList?.accountId ??
    accounts.find((account) => account.id === "local")?.id ??
    accounts.find((account) => account.tasksEnabled)?.id ??
    null;

  const selectSmart = (smart: SmartList) => {
    setSelection("todo", { smart, listId: null, taskId: null });
  };

  const selectList = (listId: string) => {
    setSelection("todo", { listId, smart: null, taskId: null });
  };

  const removeList = useCallback(
    async (list: TaskList) => {
      const ok = await confirm({
        title: t("todo.deleteList.title"),
        message: t("todo.deleteList.message", { name: list.name }),
        confirmLabel: t("todo.deleteList.confirm"),
        cancelLabel: t("common.cancel"),
        danger: true,
      });
      if (!ok) return;
      const deleted = await deleteTaskList(list.id);
      if (deleted && useShellStore.getState().selection.todo.listId === list.id) {
        useShellStore.getState().setSelection("todo", {
          listId: null,
          smart: "myDay",
          taskId: null,
        });
      }
    },
    [t],
  );

  const listMenu = (list: TaskList) => [
    {
      id: "new-task",
      label: t("todo.header.newTask"),
      icon: <Add20Regular />,
      disabled: list.isReadOnly,
      onSelect: () => {
        selectList(list.id);
        requestAdd();
      },
    },
    { type: "separator" as const },
    {
      id: "rename",
      label: t("todo.list.rename"),
      icon: <Edit20Regular />,
      disabled: list.isReadOnly,
      onSelect: () => setDialog({ kind: "rename", list }),
    },
    {
      id: "color",
      label: t("todo.list.color"),
      icon: <Color20Regular />,
      disabled: list.isReadOnly,
      onSelect: () => setDialog({ kind: "color", list }),
    },
    { type: "separator" as const },
    {
      id: "delete",
      label: t("todo.list.delete"),
      icon: <Delete20Regular />,
      danger: true,
      disabled: list.isReadOnly || list.isDefault,
      onSelect: () => void removeList(list),
    },
  ];

  const myDayDate = (() => {
    const parsed = parseIsoDate(today);
    return parsed ? formatDate(parsed, { locale }) : today;
  })();

  return (
    <div className={styles.sidebar}>
      <nav className={styles.section} aria-label={t("todo.smartLists")}>
        {SMART_LISTS.map((smart) => {
          const active = selection.smart === smart && !selection.listId;
          const count = smartCount(counts, smart);
          return (
            <button
              key={smart}
              type="button"
              className={cn(styles.row, styles.smartRow, active && styles.rowActive)}
              aria-current={active ? "page" : undefined}
              onClick={() => selectSmart(smart)}
            >
              <span className={styles.icon} aria-hidden="true">
                <SmartListIcon smart={smart} />
              </span>
              <span className={styles.label}>{t(smartListLabelKey(smart))}</span>
              {smart === "myDay" ? <span className={styles.date}>{myDayDate}</span> : null}
              {count > 0 ? (
                <span className={styles.count} aria-label={t("todo.count.tasks", { count })}>
                  {count > 99 ? "99+" : count}
                </span>
              ) : null}
            </button>
          );
        })}
      </nav>

      <section className={styles.listSection} aria-label={t("todo.lists")}>
        <h2 className={styles.sectionTitle}>{t("todo.lists")}</h2>
        <div className={styles.listScroll}>
          {listsQuery.isPending ? (
            <p className={styles.placeholder}>{t("common.loading")}</p>
          ) : listsQuery.isError ? (
            <div className={styles.errorBox} role="alert">
              <span>{t("todo.error.loadLists")}</span>
              <Button size="small" onClick={() => void listsQuery.refetch()}>
                {t("common.retry")}
              </Button>
            </div>
          ) : lists.length === 0 ? (
            <p className={styles.placeholder}>{t("todo.empty.noLists.title")}</p>
          ) : (
            groups.map((group) => (
              <div key={group.accountId} className={styles.group}>
                <div className={styles.groupTitle}>
                  <span
                    className={styles.groupDot}
                    style={{ background: group.color }}
                    aria-hidden="true"
                  />
                  <span className={styles.groupName}>{group.label}</span>
                </div>
                {group.lists.map((list) => {
                  const active = selection.listId === list.id && !selection.smart;
                  return (
                    <ContextMenu key={list.id} items={listMenu(list)}>
                      <div
                        role="button"
                        tabIndex={0}
                        className={cn(
                          styles.row,
                          styles.listRow,
                          active && styles.rowActive,
                          dropListId === list.id && styles.dropTarget,
                        )}
                        aria-current={active ? "page" : undefined}
                        aria-label={list.name}
                        onClick={() => selectList(list.id)}
                        onKeyDown={(event) => {
                          if (event.target !== event.currentTarget) return;
                          if (event.key === "Enter" || event.key === " ") {
                            event.preventDefault();
                            selectList(list.id);
                          }
                        }}
                        onDragOver={(event) => {
                          if (!hasTaskDrag(event.dataTransfer)) return;
                          event.preventDefault();
                          event.dataTransfer.dropEffect = "move";
                          setDropListId(list.id);
                        }}
                        onDragLeave={(event) => {
                          const next = event.relatedTarget;
                          if (next instanceof Node && event.currentTarget.contains(next)) return;
                          setDropListId((current) => (current === list.id ? null : current));
                        }}
                        onDrop={(event) => {
                          if (!hasTaskDrag(event.dataTransfer)) return;
                          event.preventDefault();
                          const taskId = readDraggedTaskId(event.dataTransfer);
                          setDropListId(null);
                          if (taskId) {
                            void moveTaskToList(taskId, list.id, list.name);
                          }
                        }}
                      >
                        <span
                          className={styles.listDot}
                          style={{ background: list.color }}
                          aria-hidden="true"
                        />
                        <span className={styles.listName}>{list.name}</span>
                        {list.isDefault ? (
                          <span className={styles.defaultTag}>{t("todo.list.defaultShort")}</span>
                        ) : null}
                        {list.incompleteCount > 0 ? (
                          <span className={styles.count}>{list.incompleteCount}</span>
                        ) : null}
                        <IconButton
                          size="small"
                          className={styles.rowMore}
                          icon={<MoreHorizontal16Regular />}
                          label={t("todo.list.menu")}
                          onClick={(event) => {
                            event.stopPropagation();
                            setRowMenu({ list, anchor: event.currentTarget });
                          }}
                        />
                      </div>
                    </ContextMenu>
                  );
                })}
              </div>
            ))
          )}
        </div>

        <div className={styles.addRow}>
          <Button
            appearance="transparent"
            size="small"
            icon={<Add20Regular />}
            fullWidth
            className={styles.addButton}
            onClick={() => setDialog({ kind: "create" })}
          >
            {t("todo.newList")}
          </Button>
        </div>
      </section>

      <Menu
        open={rowMenu !== null}
        onClose={() => setRowMenu(null)}
        anchor={rowMenu?.anchor ?? null}
        items={rowMenu ? listMenu(rowMenu.list) : []}
        placement="bottom-start"
        minWidth={220}
        ariaLabel={t("todo.list.menu")}
      />

      <CreateListDialog
        open={dialog?.kind === "create"}
        onClose={() => setDialog(null)}
        accounts={accounts}
        defaultAccountId={defaultAccountId}
        onCreated={(list) => {
          setSelection("todo", { listId: list.id, smart: null, taskId: null });
        }}
      />
      <RenameListDialog
        open={dialog?.kind === "rename"}
        onClose={() => setDialog(null)}
        list={dialog?.kind === "rename" ? dialog.list : null}
      />
      <ColorListDialog
        open={dialog?.kind === "color"}
        onClose={() => setDialog(null)}
        list={dialog?.kind === "color" ? dialog.list : null}
      />
    </div>
  );
}
