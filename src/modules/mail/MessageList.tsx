/**
 * Middle pane: search + filters, date-grouped virtualized message list,
 * multi-selection, batch bar, thread expansion, drag source and the full
 * message context menu.
 */
import {
  useCallback,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type CSSProperties,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { useInfiniteQuery, useQuery, useQueryClient } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  Archive16Regular,
  ArrowForward16Regular,
  ArrowMove20Regular,
  ArrowReply16Regular,
  ArrowReplyAll16Regular,
  ArrowSync16Regular,
  Attach16Regular,
  ChatMultiple16Regular,
  CheckboxChecked16Regular,
  Delete16Regular,
  DocumentEdit16Regular,
  Flag16Regular,
  Important16Regular,
  Mail16Regular,
  MailRead16Regular,
  MailUnread16Regular,
  Options16Regular,
  Open16Regular,
  Print16Regular,
  Save16Regular,
  Search16Regular,
  SelectAllOn16Regular,
  Send16Regular,
  TaskListSquareLtr16Regular,
  Warning16Regular,
} from "@fluentui/react-icons";
import {
  Avatar,
  Button,
  Checkbox,
  DatePicker,
  EmptyState,
  IconButton,
  Input,
  Menu,
  MenuButton,
  SearchBox,
  Spinner,
  Switch,
  ToggleButton,
  Tooltip,
  VirtualList,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import {
  app,
  isIpcError,
  messages as messagesApi,
  outbox as outboxApi,
  sync as syncApi,
} from "@/ipc/api";
import type {
  Folder,
  FolderRole,
  HoverAction,
  MailFilter,
  MailScope,
  MessageListQuery,
  MessageSummary,
  ReplyMode,
  SearchQuery,
} from "@/ipc/types";
import { formatListDate } from "@/lib/format";
import { formatShortcut } from "@/lib/hotkeys";
import { t, useT } from "@/lib/i18n";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useUpdateSettings } from "@/lib/useSettings";
import {
  isPermanentDelete,
  messageActionToast,
  undoLastMessageAction,
  useMessageActions,
} from "./actions";
import { onMailCommand } from "./commands";
import { useMailCompose } from "./composeIntegration";
import { setMessageDragData } from "./dnd";
import {
  EMPTY_SELECTION,
  actionTargets,
  buildMessageRows,
  messageIds,
  nextAfterRemoval,
  isSupportedSwipeAction,
  parseUnifiedFolderId,
  safeFilename,
  selectionReducer,
  type DateGroupKey,
  type MessageRow,
} from "./logic";
import {
  folderRoleKey,
  useAllFolders,
  useFolderChoices,
  useMailAccounts,
  useMailSettings,
  useMailSyncing,
  useUnifiedAccounts,
} from "./queries";
import { isOutboxMessageId, outboxItemId, outboxStatusKey, parseOutboxStatus } from "./outbox";
import { SwipeableRow, type SwipeActionDescriptor } from "./SwipeRow";
import styles from "./mail.module.css";

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function folderTitle(folder: Folder | null, unifiedRole: FolderRole | null, tt: ReturnType<typeof useT>): string {
  if (unifiedRole) {
    const key = folderRoleKey(unifiedRole);
    const value = tt(key);
    return value === key ? unifiedRole : value;
  }
  if (!folder) return "";
  const key = folderRoleKey(folder.role);
  const value = tt(key);
  if (value !== key && folder.role !== "other") return value;
  return folder.name || value;
}

function groupLabelKey(group: DateGroupKey): string {
  return `mail.list.group.${group}`;
}

function senderName(summary: MessageSummary): string {
  const from = summary.from;
  if (!from) return t("mail.list.unknownSender");
  return from.name?.trim() || from.email;
}

function summaryStyle(density: "compact" | "normal" | "spacious"): CSSProperties {
  if (density === "compact") return { ["--row-min" as string]: "52px" };
  if (density === "spacious") return { ["--row-min" as string]: "92px" };
  return { ["--row-min" as string]: "70px" };
}

function openMessageWindow(messageId: string, subject: string, print = false): void {
  const route = print ? `/message/${messageId}?print=1` : `/message/${messageId}`;
  if (!isTauri()) {
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
    return;
  }
  app.openWindow(route, subject || t("shell.window.message")).catch((error: unknown) => {
    console.warn("[mail] open message window failed", error);
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
  });
}

interface ThreadMembersProps {
  accountId: string;
  threadId: string;
  selectedId: string | null;
  onSelect: (summary: MessageSummary) => void;
  showAvatars: boolean;
}

function ThreadMembers({ accountId, threadId, selectedId, onSelect, showAvatars }: ThreadMembersProps) {
  const tt = useT();
  const query = useQuery({
    queryKey: queryKeys.thread(accountId, threadId),
    queryFn: () => messagesApi.thread(accountId, threadId),
    staleTime: 15_000,
  });
  if (query.isLoading) {
    return (
      <div className={styles.threadLoading}>
        <Spinner size="tiny" />
      </div>
    );
  }
  const members = query.data ?? [];
  return (
    <div className={styles.threadMembers} role="group" aria-label={tt("mail.list.threadMembers")}>
      {members.map((member) => (
        <div
          key={member.id}
          className={[styles.threadMember, !member.seen ? styles.threadMemberUnread : ""]
            .filter(Boolean)
            .join(" ")}
          role="option"
          aria-selected={member.id === selectedId}
          tabIndex={-1}
          onClick={(event) => {
            event.stopPropagation();
            onSelect(member);
          }}
        >
          {showAvatars ? (
            <Avatar
              name={member.from?.name}
              email={member.from?.email}
              size={20}
              className={styles.threadAvatar}
            />
          ) : null}
          <span className={styles.threadSender}>{senderName(member)}</span>
          <span className={styles.threadSubject}>
            {member.subject || tt("mail.list.noSubject")}
          </span>
          {member.hasAttachments ? <AttachMark /> : null}
          <span className={styles.threadDate}>{formatListDate(member.date)}</span>
        </div>
      ))}
    </div>
  );
}

function AttachMark() {
  return <span className={styles.inlineIcon} aria-hidden="true"><Attach16Regular /></span>;
}

export function MessageList() {
  const tt = useT();
  const selection = useShellStore((state) => state.selection.mail);
  const setSelection = useShellStore((state) => state.setSelection);
  const closeInlineCompose = useShellStore((state) => state.closeInlineCompose);

  const accounts = useMailAccounts();
  const unifiedAccounts = useUnifiedAccounts();
  const folders = useAllFolders(accounts);
  const folderChoices = useFolderChoices(accounts, folders);
  const mailSettings = useMailSettings();
  const updateSettings = useUpdateSettings();
  const { apply } = useMessageActions();
  const compose = useMailCompose();
  const queryClient = useQueryClient();

  const folderId = selection.folderId;
  const messageId = selection.messageId;
  const folder = folderId ? folders.byId.get(folderId) ?? null : null;
  const unifiedRole = parseUnifiedFolderId(folderId);
  const outboxView = folder?.role === "outbox" || unifiedRole === "outbox";

  const [searchInput, setSearchInput] = useState("");
  const [searchText, setSearchText] = useState("");
  const [searchScope, setSearchScope] = useState<"folder" | "all">("folder");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [advanced, setAdvanced] = useState({
    from: "",
    to: "",
    subject: "",
    hasAttachment: false,
    unread: false,
    flagged: false,
    dateFrom: null as string | null,
    dateTo: null as string | null,
    includeServer: false,
  });
  const [filter, setFilter] = useState<MailFilter>("all");
  const [selectionMode, setSelectionMode] = useState(false);
  const [selected, dispatchSelection] = useReducer(selectionReducer, EMPTY_SELECTION);
  const [expandedThreads, setExpandedThreads] = useState<Set<string>>(new Set());
  const [contextMenu, setContextMenu] = useState<{
    summary: MessageSummary;
    point: { x: number; y: number };
  } | null>(null);
  const [hoverMenu, setHoverMenu] = useState<{
    summary: MessageSummary;
    point: { x: number; y: number };
  } | null>(null);
  const [now, setNow] = useState(() => new Date());
  const searchRef = useRef<HTMLInputElement | null>(null);
  const listWrapRef = useRef<HTMLDivElement | null>(null);

  const conversation = mailSettings.conversationView;
  const density = mailSettings.density;

  useEffect(() => {
    const timer = window.setInterval(() => setNow(new Date()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => setSearchText(searchInput.trim()), 220);
    return () => window.clearTimeout(timer);
  }, [searchInput]);

  const canScopeFolder = folder !== null;
  const effectiveSearchScope: "folder" | "all" = canScopeFolder ? searchScope : "all";
  const effectiveFolder = effectiveSearchScope === "folder" ? folder : null;
  const subscriptionAccounts = useMemo(() => {
    if (folder) return [folder.accountId];
    if (unifiedRole) return unifiedAccounts.map((account) => account.id);
    return accounts.map((account) => account.id);
  }, [accounts, folder, unifiedAccounts, unifiedRole]);
  const syncing = useMailSyncing(subscriptionAccounts);

  const scope = useMemo<MailScope | null>(() => {
    if (searchText) {
      const query: SearchQuery = {
        text: searchText,
        accountId: effectiveFolder?.accountId ?? null,
        folderId: effectiveFolder?.id ?? null,
        from: advanced.from.trim() || null,
        to: advanced.to.trim() || null,
        subject: advanced.subject.trim() || null,
        hasAttachment: advanced.hasAttachment ? true : null,
        unread: advanced.unread ? true : null,
        flagged: advanced.flagged ? true : null,
        dateFrom: advanced.dateFrom,
        dateTo: advanced.dateTo,
        includeServer: advanced.includeServer,
      };
      return { kind: "search", query };
    }
    if (unifiedRole) return { kind: "unified", role: unifiedRole };
    if (folder) return { kind: "folder", folderId: folder.id };
    return null;
  }, [advanced, effectiveFolder, folder, searchText, unifiedRole]);

  const listQuery = useMemo<MessageListQuery>(
    () => ({
      scope: scope ?? { kind: "folder", folderId: "" },
      filter,
      conversation,
      cursor: null,
      limit: 100,
    }),
    [conversation, filter, scope],
  );

  const list = useInfiniteQuery({
    queryKey: queryKeys.messages(listQuery),
    queryFn: ({ pageParam }) =>
      messagesApi.list({ ...listQuery, cursor: (pageParam as string | null) ?? null }),
    initialPageParam: null as string | null,
    getNextPageParam: (lastPage) => lastPage.nextCursor ?? undefined,
    enabled: scope !== null,
    staleTime: 15_000,
  });

  const summaries = useMemo(
    () => (list.data?.pages ?? []).flatMap((page) => page.items),
    [list.data],
  );
  const rows = useMemo(
    () =>
      buildMessageRows(
        [...summaries].sort((a, b) => b.date.localeCompare(a.date)),
        now,
        1,
      ),
    [now, summaries],
  );
  const orderedIds = useMemo(() => messageIds(rows), [rows]);
  const accountById = useMemo(() => {
    const map = new Map<string, (typeof accounts)[number]>();
    for (const account of accounts) map.set(account.id, account);
    return map;
  }, [accounts]);
  const summaryById = useMemo(() => {
    const map = new Map<string, MessageSummary>();
    for (const summary of summaries) map.set(summary.id, summary);
    return map;
  }, [summaries]);

  const activeMessage = messageId ? summaryById.get(messageId) ?? null : null;
  const targets = useMemo(() => actionTargets(selected, messageId), [messageId, selected]);

  const setActive = useCallback(
    (id: string | null) => {
      setSelection("mail", { messageId: id });
    },
    [setSelection],
  );

  const invalidateOutbox = useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: ["messages"] });
    void queryClient.invalidateQueries({ queryKey: ["folders"] });
    void queryClient.invalidateQueries({ queryKey: ["unifiedCounts"] });
    void queryClient.invalidateQueries({ queryKey: ["message"] });
  }, [queryClient]);


  const scrollToMessage = useCallback(
    (id: string) => {
      const rowIndex = rows.findIndex(
        (row) => row.kind === "message" && row.summary.id === id,
      );
      if (rowIndex < 0) return;
      const container = listWrapRef.current?.firstElementChild as HTMLElement | null;
      const scroller =
        container && container.scrollHeight > container.clientHeight ? container : null;
      if (scroller) {
        const estimate = density === "compact" ? 56 : density === "spacious" ? 96 : 76;
        scroller.scrollTo({
          top: Math.max(0, rowIndex * estimate - scroller.clientHeight / 3),
          behavior: "smooth",
        });
      }
    },
    [density, rows],
  );

  const navigate = useCallback(
    (delta: 1 | -1) => {
      if (orderedIds.length === 0) return;
      const index = messageId ? orderedIds.indexOf(messageId) : -1;
      let nextIndex: number;
      if (index < 0) nextIndex = delta > 0 ? 0 : orderedIds.length - 1;
      else nextIndex = Math.min(orderedIds.length - 1, Math.max(0, index + delta));
      const nextId = orderedIds[nextIndex];
      if (!nextId) return;
      setActive(nextId);
      dispatchSelection({ type: "replace", ids: [nextId] });
      scrollToMessage(nextId);
    },
    [messageId, orderedIds, scrollToMessage, setActive],
  );

  const threadMetaFor = useCallback(
    (ids: string[]): { threadId: string; accountId: string; count: number } | null => {
      if (!conversation || ids.length !== 1) return null;
      const first = ids[0] ? summaryById.get(ids[0]) : undefined;
      if (!first || first.threadCount <= 1) return null;
      return {
        threadId: first.threadId,
        accountId: first.accountId,
        count: first.threadCount,
      };
    },
    [conversation, summaryById],
  );

  const runAction = useCallback(
    async (
      ids: string[],
      action: Parameters<typeof apply>[0]["action"],
      threadMeta?: { threadId: string; accountId: string; count: number } | null,
    ) => {
      if (ids.length === 0) return false;
      try {
        await apply({
          ids,
          action,
          ...(threadMeta
            ? { threadIds: [threadMeta.threadId], accountId: threadMeta.accountId }
            : {}),
        });
        messageActionToast(action, threadMeta?.count ?? ids.length);
        return true;
      } catch {
        return false;
      }
    },
    [apply],
  );

  const deleteMessages = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) return;
      // Queued messages are not real rows: deleting one discards it from the
      // outbox (and is never undoable).
      if (ids.some(isOutboxMessageId)) {
        const ok = await confirm({
          title: tt("mail.outbox.deleteTitle"),
          message:
            ids.length > 1
              ? tt("mail.outbox.deleteMessageMany", { count: ids.length })
              : tt("mail.outbox.deleteMessage"),
          confirmLabel: tt("mail.action.delete"),
          danger: true,
        });
        if (!ok) return;
        try {
          for (const id of ids) {
            await messagesApi.act([id], { kind: "delete", permanent: true });
          }
          dispatchSelection({ type: "remove", ids });
          if (messageId && ids.includes(messageId)) setActive(null);
          toast.success(tt("mail.outbox.deleted"));
          invalidateOutbox();
        } catch (error) {
          toast.error(tt("mail.outbox.deleteFailed"), errorText(error));
        }
        return;
      }
      const permanent = isPermanentDelete(folder?.role ?? unifiedRole);
      if (mailSettings.confirmDelete || permanent) {
        const ok = await confirm({
          title: permanent ? tt("mail.list.deletePermanentTitle") : tt("mail.list.deleteTitle"),
          message:
            ids.length > 1
              ? permanent
                ? tt("mail.list.deletePermanentMessageMany", { count: ids.length })
                : tt("mail.list.deleteMessageMany", { count: ids.length })
              : permanent
                ? tt("mail.list.deletePermanentMessage")
                : tt("mail.list.deleteMessage"),
          confirmLabel: tt("common.delete"),
          danger: true,
        });
        if (!ok) return;
      }
      const removed = new Set(ids);
      const next = nextAfterRemoval(orderedIds, removed, messageId, mailSettings.autoAdvance);
      const ok = await runAction(ids, { kind: "delete", permanent }, threadMetaFor(ids));
      if (!ok) return;
      dispatchSelection({ type: "remove", ids });
      setActive(next);
      if (next) scrollToMessage(next);
    },
    [
      folder?.role,
      invalidateOutbox,
      mailSettings.autoAdvance,
      mailSettings.confirmDelete,
      messageId,
      orderedIds,
      runAction,
      scrollToMessage,
      setActive,
      threadMetaFor,
      tt,
      unifiedRole,
    ],
  );

  const archiveMessages = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0 || ids.some(isOutboxMessageId)) return;
      const removed = new Set(ids);
      const next = nextAfterRemoval(orderedIds, removed, messageId, mailSettings.autoAdvance);
      const ok = await runAction(ids, { kind: "archive" }, threadMetaFor(ids));
      if (!ok) return;
      dispatchSelection({ type: "remove", ids });
      setActive(next);
    },
    [mailSettings.autoAdvance, messageId, orderedIds, runAction, setActive, threadMetaFor],
  );

  const setSeen = useCallback(
    (ids: string[], value: boolean) => {
      void runAction(ids, { kind: "setSeen", value }, threadMetaFor(ids));
    },
    [runAction, threadMetaFor],
  );

  const setFlagged = useCallback(
    (ids: string[], value: boolean) => {
      void runAction(ids, { kind: "setFlagged", value }, threadMetaFor(ids));
    },
    [runAction, threadMetaFor],
  );

  const moveMessages = useCallback(
    async (ids: string[], targetFolderId: string) => {
      if (ids.length === 0 || ids.some(isOutboxMessageId)) return;
      const removed = new Set(ids);
      const next = nextAfterRemoval(orderedIds, removed, messageId, mailSettings.autoAdvance);
      const ok = await runAction(
        ids,
        { kind: "move", folderId: targetFolderId },
        threadMetaFor(ids),
      );
      if (!ok) return;
      dispatchSelection({ type: "remove", ids });
      if (next) setActive(next);
    },
    [mailSettings.autoAdvance, messageId, orderedIds, runAction, setActive, threadMetaFor],
  );

  const markJunk = useCallback(
    async (ids: string[], value: boolean) => {
      if (ids.some(isOutboxMessageId)) return;
      const ok = await runAction(ids, { kind: "markJunk", value }, threadMetaFor(ids));
      if (ok) dispatchSelection({ type: "remove", ids });
    },
    [runAction, threadMetaFor],
  );

  const retryOutbox = useCallback(
    async (outboxMessageId: string) => {
      const itemId = outboxItemId(outboxMessageId);
      if (!itemId) return;
      try {
        await outboxApi.retry(itemId);
        toast.success(tt("mail.outbox.retried"));
        invalidateOutbox();
      } catch (error) {
        toast.error(tt("mail.outbox.retryFailed"), errorText(error));
      }
    },
    [invalidateOutbox, tt],
  );

  const retryAllOutbox = useCallback(async () => {
    try {
      await outboxApi.retry(null);
      toast.success(tt("mail.outbox.retryAllStarted"));
      invalidateOutbox();
    } catch (error) {
      toast.error(tt("mail.outbox.retryFailed"), errorText(error));
    }
  }, [invalidateOutbox, tt]);

  const cancelOutbox = useCallback(
    async (summary: MessageSummary) => {
      const itemId = outboxItemId(summary.id);
      if (!itemId) return;
      try {
        const draft = await outboxApi.cancel(itemId);
        compose.openDraftData(draft);
        if (messageId === summary.id) setActive(null);
        toast.success(tt("mail.outbox.cancelled"));
        invalidateOutbox();
      } catch (error) {
        toast.error(tt("mail.outbox.cancelFailed"), errorText(error));
      }
    },
    [compose, invalidateOutbox, messageId, setActive, tt],
  );

  const deleteOutbox = useCallback(
    async (summary: MessageSummary) => {
      const ok = await confirm({
        title: tt("mail.outbox.deleteTitle"),
        message: tt("mail.outbox.deleteMessage"),
        confirmLabel: tt("mail.action.delete"),
        danger: true,
      });
      if (!ok) return;
      try {
        await messagesApi.act([summary.id], { kind: "delete", permanent: true });
        dispatchSelection({ type: "remove", ids: [summary.id] });
        if (messageId === summary.id) setActive(null);
        toast.success(tt("mail.outbox.deleted"));
        invalidateOutbox();
      } catch (error) {
        toast.error(tt("mail.outbox.deleteFailed"), errorText(error));
      }
    },
    [invalidateOutbox, messageId, setActive, tt],
  );

  const swipeDescriptor = useCallback(
    (action: HoverAction, summary: MessageSummary): SwipeActionDescriptor | null => {
      switch (action) {
        case "archive":
          return {
            action,
            label: tt("mail.action.archive"),
            icon: <Archive16Regular />,
            tone: "accent",
          };
        case "delete":
          return {
            action,
            label: tt("mail.action.delete"),
            icon: <Delete16Regular />,
            tone: "danger",
          };
        case "flag":
          return {
            action,
            label: summary.flagged ? tt("mail.action.unflag") : tt("mail.action.flag"),
            icon: <Flag16Regular />,
            tone: "accent",
          };
        case "read":
          return {
            action,
            label: summary.seen ? tt("mail.action.markUnread") : tt("mail.action.markRead"),
            icon: summary.seen ? <MailUnread16Regular /> : <MailRead16Regular />,
            tone: "neutral",
          };
        default:
          return null;
      }
    },
    [tt],
  );

  const createTask = useCallback(
    async (id: string) => {
      try {
        await messagesApi.createTask(id);
        toast.success(tt("mail.toast.taskCreated"));
      } catch (error) {
        toast.error(tt("mail.toast.taskFailed"), errorText(error));
      }
    },
    [tt],
  );

  const saveAsEml = useCallback(
    async (summary: MessageSummary) => {
      try {
        const path = await saveDialog({
          defaultPath: `${safeFilename(summary.subject, tt("common.untitled"))}.eml`,
          filters: [{ name: "E-mail", extensions: ["eml"] }],
        });
        if (!path) return;
        await messagesApi.saveEml(summary.id, path);
        toast.success(tt("mail.toast.savedEml"));
      } catch (error) {
        toast.error(tt("mail.toast.saveFailed"), errorText(error));
      }
    },
    [tt],
  );

  const reply = useCallback(
    (summary: MessageSummary, mode: ReplyMode) => {
      if (summary.isDraft) {
        void compose.openDraft(summary.id);
        return;
      }
      void compose.replyInline(summary.id, mode);
    },
    [closeInlineCompose, compose],
  );

  const handleSelect = useCallback(
    (summary: MessageSummary, event: ReactMouseEvent) => {
      closeInlineCompose();
      if (selectionMode) {
        dispatchSelection({ type: "toggle", id: summary.id });
        return;
      }
      if (event.shiftKey) {
        dispatchSelection({ type: "range", id: summary.id, ordered: orderedIds });
      } else if (event.metaKey || event.ctrlKey) {
        dispatchSelection({ type: "toggle", id: summary.id });
      } else {
        dispatchSelection({ type: "replace", ids: [summary.id] });
      }
      setActive(summary.id);
    },
    [closeInlineCompose, orderedIds, selectionMode, setActive],
  );

  const openContextMenu = useCallback(
    (summary: MessageSummary, event: ReactMouseEvent) => {
      event.preventDefault();
      event.stopPropagation();
      if (!selected.ids.includes(summary.id)) {
        dispatchSelection({ type: "replace", ids: [summary.id] });
        setActive(summary.id);
      }
      setContextMenu({ summary, point: { x: event.clientX, y: event.clientY } });
    },
    [selected.ids, setActive],
  );

  useEffect(() => {
    return onMailCommand((command) => {
      switch (command.type) {
        case "navigate":
          navigate(command.delta);
          break;
        case "openWindow":
          if (activeMessage) {
            openMessageWindow(activeMessage.id, activeMessage.subject);
          }
          break;
        case "delete":
          void deleteMessages(targets);
          break;
        case "archive":
          void archiveMessages(targets);
          break;
        case "markRead":
          setSeen(targets, true);
          break;
        case "markUnread":
          setSeen(targets, false);
          break;
        case "toggleFlag": {
          const first = targets.map((id) => summaryById.get(id)).find(Boolean);
          setFlagged(targets, !(first?.flagged ?? false));
          break;
        }
        case "selectAll":
          dispatchSelection({ type: "replace", ids: orderedIds });
          break;
        case "undo":
          void undoLastMessageAction();
          break;
        case "focusSearch":
          searchRef.current?.focus();
          searchRef.current?.select();
          break;
        case "move":
          if (targets.length > 0 && activeMessage) {
            setHoverMenu({ summary: activeMessage, point: { x: 32, y: 64 } });
          }
          break;
        default:
          break;
      }
    });
  }, [
    activeMessage,
    archiveMessages,
    createTask,
    deleteMessages,
    navigate,
    orderedIds,
    saveAsEml,
    setFlagged,
    setSeen,
    summaryById,
    summaries,
    targets,
  ]);

  const syncNow = useCallback(async () => {
    try {
      await syncApi.now(folder?.accountId ?? null, "mail");
      toast.success(tt("mail.list.syncStarted"));
    } catch (error) {
      toast.error(tt("mail.toast.actionFailed"), errorText(error));
    }
  }, [folder?.accountId, tt]);

  const moveMenuEntries = useMemo<MenuEntry[]>(() => {
    const targetAccounts = new Set(
      targets
        .map((id) => summaryById.get(id)?.accountId)
        .filter((id): id is string => Boolean(id)),
    );
    if (targetAccounts.size !== 1) return [];
    const accountId = Array.from(targetAccounts)[0];
    const entries: MenuEntry[] = [];
    const seenAccounts = new Set<string>();
    for (const choice of folderChoices) {
      if (choice.account.id !== accountId) continue;
      if (!seenAccounts.has(choice.account.id)) {
        seenAccounts.add(choice.account.id);
        entries.push({ type: "header", id: `h-${choice.account.id}`, label: choice.account.displayName });
      }
      entries.push({
        id: `move-${choice.folder.id}`,
        label: `${" ".repeat(choice.depth * 2)}${choice.folder.name || tt(folderRoleKey(choice.folder.role))}`,
        icon: <Archive16Regular />,
        onSelect: () => void moveMessages(targets, choice.folder.id),
      });
    }
    return entries;
  }, [folderChoices, moveMessages, summaryById, targets, tt]);

  const contextTargets = useMemo(() => {
    if (!contextMenu) return [];
    return selected.ids.includes(contextMenu.summary.id) && selected.ids.length > 0
      ? selected.ids
      : [contextMenu.summary.id];
  }, [contextMenu, selected.ids]);

  const contextMenuItems = useMemo<MenuEntry[]>(() => {
    const summary = contextMenu?.summary;
    if (!summary) return [];
    const items: MenuEntry[] = [];
    if (isOutboxMessageId(summary.id)) {
      return [
        {
          id: "outbox-retry",
          label: tt("mail.outbox.retry"),
          icon: <ArrowSync16Regular />,
          onSelect: () => void retryOutbox(summary.id),
        },
        {
          id: "outbox-cancel",
          label: tt("mail.outbox.cancel"),
          icon: <DocumentEdit16Regular />,
          onSelect: () => void cancelOutbox(summary),
        },
        { type: "separator" },
        {
          id: "outbox-delete",
          label: tt("mail.action.delete"),
          icon: <Delete16Regular />,
          danger: true,
          onSelect: () => void deleteOutbox(summary),
        },
        { type: "separator" },
        {
          id: "open-window",
          label: tt("mail.action.openWindow"),
          icon: <Open16Regular />,
          onSelect: () => openMessageWindow(summary.id, summary.subject),
        },
        {
          id: "print",
          label: tt("mail.action.print"),
          icon: <Print16Regular />,
          onSelect: () => openMessageWindow(summary.id, summary.subject, true),
        },
      ];
    }
    if (summary.isDraft) {
      items.push({
        id: "open-draft",
        label: tt("mail.action.openDraft"),
        icon: <DocumentEdit16Regular />,
        onSelect: () => void compose.openDraft(summary.id),
      });
    } else {
      items.push(
        {
          id: "reply",
          label: tt("mail.action.reply"),
          icon: <ArrowReply16Regular />,
          shortcut: "Mod+R",
          onSelect: () => reply(summary, "reply"),
        },
        {
          id: "reply-all",
          label: tt("mail.action.replyAll"),
          icon: <ArrowReplyAll16Regular />,
          shortcut: "Mod+Shift+R",
          onSelect: () => reply(summary, "replyAll"),
        },
        {
          id: "forward",
          label: tt("mail.action.forward"),
          icon: <ArrowForward16Regular />,
          shortcut: "Mod+F",
          onSelect: () => reply(summary, "forward"),
        },
      );
    }
    items.push(
      { type: "separator" },
      {
        id: "mark-read",
        label: summary.seen ? tt("mail.action.markUnread") : tt("mail.action.markRead"),
        icon: summary.seen ? <MailUnread16Regular /> : <MailRead16Regular />,
        onSelect: () => setSeen(contextTargets, !summary.seen),
      },
      {
        id: "flag",
        label: summary.flagged ? tt("mail.action.unflag") : tt("mail.action.flag"),
        icon: <Flag16Regular />,
        checked: summary.flagged,
        onSelect: () => setFlagged(contextTargets, !summary.flagged),
      },
    );
    if (folderChoices.length > 0) {
      items.push({
        id: "move",
        label: tt("mail.action.move"),
        icon: <ArrowMove20Regular />,
        submenu: folderChoices.map((choice) => ({
          id: `ctx-move-${choice.folder.id}`,
          label: `${" ".repeat(choice.depth * 2)}${choice.folder.name || tt(folderRoleKey(choice.folder.role))}`,
          onSelect: () => void moveMessages(contextTargets, choice.folder.id),
        })),
      });
    }
    items.push(
      {
        id: "archive",
        label: tt("mail.action.archive"),
        icon: <Archive16Regular />,
        shortcut: "Backspace",
        onSelect: () => void archiveMessages(contextTargets),
      },
      {
        id: "delete",
        label: summary.isDraft ? tt("mail.action.deleteDraft") : tt("mail.action.delete"),
        icon: <Delete16Regular />,
        danger: true,
        shortcut: "Del",
        onSelect: () => void deleteMessages(contextTargets),
      },
      {
        id: "junk",
        label: tt("mail.action.markJunk"),
        icon: <Flag16Regular />,
        onSelect: () => void markJunk(contextTargets, true),
      },
      { type: "separator" },
      {
        id: "create-task",
        label: tt("mail.action.createTask"),
        icon: <TaskListSquareLtr16Regular />,
        onSelect: () => void createTask(summary.id),
      },
      {
        id: "open-window",
        label: tt("mail.action.openWindow"),
        icon: <Open16Regular />,
        onSelect: () => openMessageWindow(summary.id, summary.subject),
      },
      {
        id: "save-as",
        label: tt("mail.action.saveAs"),
        icon: <Save16Regular />,
        onSelect: () => void saveAsEml(summary),
      },
      {
        id: "print",
        label: tt("mail.action.print"),
        icon: <Print16Regular />,
        onSelect: () => openMessageWindow(summary.id, summary.subject, true),
      },
    );
    return items;
  }, [
    archiveMessages,
    cancelOutbox,
    compose,
    contextMenu,
    contextTargets,
    createTask,
    deleteMessages,
    deleteOutbox,
    folderChoices,
    markJunk,
    moveMessages,
    reply,
    retryOutbox,
    saveAsEml,
    setFlagged,
    setSeen,
    tt,
  ]);

  const hoverActionButtons = useCallback(
    (summary: MessageSummary) => {
      const actions: Array<{
        key: string;
        label: string;
        icon: JSX.Element;
        run: () => void;
      }> = [];
      if (isOutboxMessageId(summary.id)) {
        return [
          {
            key: "outbox-retry",
            label: tt("mail.outbox.retry"),
            icon: <ArrowSync16Regular />,
            run: () => void retryOutbox(summary.id),
          },
          {
            key: "outbox-cancel",
            label: tt("mail.outbox.cancel"),
            icon: <DocumentEdit16Regular />,
            run: () => void cancelOutbox(summary),
          },
          {
            key: "outbox-delete",
            label: tt("mail.action.delete"),
            icon: <Delete16Regular />,
            run: () => void deleteOutbox(summary),
          },
        ];
      }
      for (const action of mailSettings.hoverActions ?? []) {
        switch (action) {
          case "archive":
            actions.push({
              key: "archive",
              label: tt("mail.action.archive"),
              icon: <Archive16Regular />,
              run: () => void archiveMessages([summary.id]),
            });
            break;
          case "delete":
            actions.push({
              key: "delete",
              label: tt("mail.action.delete"),
              icon: <Delete16Regular />,
              run: () => void deleteMessages([summary.id]),
            });
            break;
          case "flag":
            actions.push({
              key: "flag",
              label: summary.flagged ? tt("mail.action.unflag") : tt("mail.action.flag"),
              icon: <Flag16Regular />,
              run: () => setFlagged([summary.id], !summary.flagged),
            });
            break;
          case "read":
            actions.push({
              key: "read",
              label: summary.seen ? tt("mail.action.markUnread") : tt("mail.action.markRead"),
              icon: summary.seen ? <MailUnread16Regular /> : <MailRead16Regular />,
              run: () => setSeen([summary.id], !summary.seen),
            });
            break;
          case "move":
            actions.push({
              key: "move",
              label: tt("mail.action.move"),
              icon: <ArrowMove20Regular />,
              run: () => {
                const element = document.activeElement as HTMLElement | null;
                const rect = element?.getBoundingClientRect();
                setHoverMenu({
                  summary,
                  point: rect
                    ? { x: rect.right, y: rect.bottom }
                    : { x: window.innerWidth / 2, y: 120 },
                });
              },
            });
            break;
          default:
            break;
        }
      }
      return actions;
    },
    [
      archiveMessages,
      cancelOutbox,
      deleteMessages,
      deleteOutbox,
      mailSettings.hoverActions,
      retryOutbox,
      setFlagged,
      setSeen,
      tt,
    ],
  );

  const hoverMenuEntries = useMemo<MenuEntry[]>(() => {
    if (!hoverMenu) return [];
    return folderChoices.map((choice) => ({
      id: `hover-move-${choice.folder.id}`,
      label: `${" ".repeat(choice.depth * 2)}${choice.folder.name || tt(folderRoleKey(choice.folder.role))}`,
      icon: <Archive16Regular />,
      onSelect: () => void moveMessages([hoverMenu.summary.id], choice.folder.id),
    }));
  }, [folderChoices, hoverMenu, moveMessages, tt]);

  const clearSearch = useCallback(() => {
    setSearchInput("");
    setSearchText("");
    setAdvanced({
      from: "",
      to: "",
      subject: "",
      hasAttachment: false,
      unread: false,
      flagged: false,
      dateFrom: null,
      dateTo: null,
      includeServer: false,
    });
    setFilter("all");
  }, []);

  // The bulk row is always on screen (except in the outbox view): hiding it
  // until something was selected made "select all" undiscoverable, and the
  // actions are cheap no-ops when the selection is empty.
  const hasSelection = selected.ids.length > 0;

  // Every row currently in the list is selected, which turns the toggle into
  // "deselect all". Selection can also hold ids that are no longer listed (a
  // filter or search change), hence the per-id check rather than a length
  // comparison — and a Set, because the list can hold thousands of rows.
  const selectedIdSet = useMemo(() => new Set(selected.ids), [selected.ids]);
  const allListedSelected =
    orderedIds.length > 0 && orderedIds.every((id) => selectedIdSet.has(id));

  const title = searchText
    ? tt("mail.list.searchTitle")
    : folderTitle(folder, unifiedRole, tt);
  const subtitle = searchText
    ? tt("mail.list.searchSubtitle", {
        count: list.data?.pages[0]?.total ?? summaries.length,
      })
    : unifiedRole
      ? tt("mail.list.unifiedSubtitle")
      : folder
        ? accountById.get(folder.accountId)?.displayName ?? ""
        : "";

  const emptyState = (() => {
    if (searchText) {
      return (
        <EmptyState
          icon={<Search16Regular />}
          title={tt("mail.list.emptySearchTitle")}
          description={tt("mail.list.emptySearchHint")}
          actions={
            <Button size="small" onClick={clearSearch}>
              {tt("mail.list.clearSearch")}
            </Button>
          }
        />
      );
    }
    const key =
      filter === "unread"
        ? "mail.list.emptyUnread"
        : filter === "flagged"
          ? "mail.list.emptyFlagged"
          : filter === "attachments"
            ? "mail.list.emptyAttachments"
            : filter === "mentionsMe"
              ? "mail.list.emptyMentions"
              : "mail.list.emptyFolder";
    return (
      <EmptyState
        icon={<Mail16Regular />}
        title={tt(key)}
        description={tt("mail.list.emptyHint")}
        actions={
          filter !== "all" ? (
            <Button size="small" onClick={() => setFilter("all")}>
              {tt("mail.filter.all")}
            </Button>
          ) : undefined
        }
      />
    );
  })();

  const filterItems: MenuEntry[] = useMemo(
    () =>
      (
        [
          ["all", "mail.filter.all"],
          ["unread", "mail.filter.unread"],
          ["flagged", "mail.filter.flagged"],
          ["attachments", "mail.filter.attachments"],
          ["mentionsMe", "mail.filter.mentionsMe"],
        ] as Array<[MailFilter, string]>
      ).map(([value, key]) => ({
        id: `filter-${value}`,
        label: tt(key),
        checked: filter === value,
        onSelect: () => setFilter(value),
      })),
    [filter, tt],
  );

  return (
    <section className={styles.listPane} aria-label={tt("mail.list.title")}>
      <header className={styles.listHeader}>
        <div className={styles.listTitleRow}>
          <div className={styles.listTitles}>
            <h2 className={styles.listTitle}>{title}</h2>
            {subtitle ? <span className={styles.listSubtitle}>{subtitle}</span> : null}
          </div>
          <div className={styles.listHeaderActions}>
            {outboxView ? (
              <Button
                size="small"
                appearance="subtle"
                icon={<ArrowSync16Regular />}
                onClick={() => void retryAllOutbox()}
                disabled={summaries.length === 0}
              >
                {tt("mail.outbox.retryAll")}
              </Button>
            ) : (
              <>
                {/* No Tooltip wrapper here: IconButton already renders one from
                    `label`, so wrapping it produced two bubbles for one button. */}
                <IconButton
                  icon={syncing ? <Spinner size="tiny" /> : <ArrowSync16Regular />}
                  label={tt("mail.list.sync")}
                  size="small"
                  onClick={() => void syncNow()}
                  disabled={syncing}
                />
                <Tooltip content={tt("mail.list.conversation")}>
                  <ToggleButton
                    pressed={conversation}
                    onPressedChange={(pressed) =>
                      updateSettings.mutate({ mail: { conversationView: pressed } })
                    }
                    icon={<ChatMultiple16Regular />}
                    size="small"
                    aria-label={tt("mail.list.conversation")}
                  />
                </Tooltip>
                <Tooltip content={tt("mail.list.selectionMode")}>
                  <ToggleButton
                    pressed={selectionMode}
                    onPressedChange={(pressed) => {
                      setSelectionMode(pressed);
                      dispatchSelection({ type: "clear" });
                    }}
                    icon={<CheckboxChecked16Regular />}
                    size="small"
                    aria-label={tt("mail.list.selectionMode")}
                  />
                </Tooltip>
                <MenuButton
                  label={tt("mail.list.filter")}
                  icon={<Options16Regular />}
                  items={filterItems}
                  appearance="subtle"
                  size="small"
                />
              </>
            )}
          </div>
        </div>

        {outboxView ? null : (
          <div className={styles.batchBar} role="toolbar" aria-label={tt("mail.list.batchBar")}>
            {/* Same control and the same pressed look as the header's selection
                toggle: one icon, state shown by the button being pressed. */}
            <Tooltip
              content={
                allListedSelected ? tt("mail.list.selectNone") : tt("mail.list.selectAll")
              }
            >
              <ToggleButton
                pressed={allListedSelected}
                onPressedChange={() =>
                  dispatchSelection(
                    allListedSelected
                      ? { type: "clear" }
                      : { type: "replace", ids: orderedIds },
                  )
                }
                icon={<SelectAllOn16Regular />}
                size="small"
                aria-label={tt("mail.list.selectAll")}
                disabled={orderedIds.length === 0}
              />
            </Tooltip>
            <IconButton
              icon={<Archive16Regular />}
              label={tt("mail.action.archive")}
              size="small"
              onClick={() => void archiveMessages(selected.ids)}
            />
            <IconButton
              icon={<Delete16Regular />}
              label={tt("mail.action.delete")}
              size="small"
              onClick={() => void deleteMessages(selected.ids)}
            />
            <IconButton
              icon={<MailRead16Regular />}
              label={tt("mail.action.markRead")}
              size="small"
              onClick={() => setSeen(selected.ids, true)}
            />
            <IconButton
              icon={<MailUnread16Regular />}
              label={tt("mail.action.markUnread")}
              size="small"
              onClick={() => setSeen(selected.ids, false)}
            />
            <IconButton
              icon={<Flag16Regular />}
              label={tt("mail.action.flag")}
              size="small"
              onClick={() => setFlagged(selected.ids, true)}
            />
            <MenuButton
              label={tt("mail.action.move")}
              icon={<ArrowMove20Regular />}
              items={moveMenuEntries}
              appearance="subtle"
              size="small"
              disabled={moveMenuEntries.length === 0}
            />
            {hasSelection ? (
              <span className={styles.batchCount}>{selected.ids.length}</span>
            ) : null}
          </div>
        )}
        <div className={styles.searchRow}>
          <SearchBox
            ref={searchRef}
            value={searchInput}
            onChange={setSearchInput}
            placeholder={tt("mail.list.searchPlaceholder")}
            shortcut={formatShortcut("Mod+E")}
          />
          <IconButton
            icon={<Options16Regular />}
            label={tt("mail.list.advanced")}
            size="small"
            selected={advancedOpen}
            onClick={() => setAdvancedOpen((open) => !open)}
          />
        </div>
        <div className={styles.scopeRow}>
            <span className={styles.scopeLabel}>{tt("mail.list.searchScope")}</span>
            <div className={styles.scopeChips} role="group" aria-label={tt("mail.list.searchScope")}>
              <ToggleButton
                pressed={effectiveSearchScope === "folder"}
                onPressedChange={() => setSearchScope("folder")}
                disabled={!canScopeFolder}
                size="small"
              >
                {tt("mail.list.scopeFolder")}
              </ToggleButton>
              <ToggleButton
                pressed={effectiveSearchScope === "all"}
                onPressedChange={() => setSearchScope("all")}
                size="small"
              >
                {tt("mail.list.scopeAll")}
              </ToggleButton>
            </div>
          </div>
        {advancedOpen ? (
          <div className={styles.advancedPanel}>
            <div className={styles.advancedGrid}>
              <Input
                size="small"
                label={tt("mail.list.from")}
                value={advanced.from}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, from: event.target.value }))
                }
              />
              <Input
                size="small"
                label={tt("mail.list.to")}
                value={advanced.to}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, to: event.target.value }))
                }
              />
              <Input
                size="small"
                label={tt("mail.list.subject")}
                value={advanced.subject}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, subject: event.target.value }))
                }
              />
              <DatePicker
                label={tt("mail.list.dateFrom")}
                value={advanced.dateFrom}
                onChange={(value) => setAdvanced((current) => ({ ...current, dateFrom: value }))}
              />
              <DatePicker
                label={tt("mail.list.dateTo")}
                value={advanced.dateTo}
                onChange={(value) => setAdvanced((current) => ({ ...current, dateTo: value }))}
              />
            </div>
            <div className={styles.advancedChecks}>
              <Checkbox
                label={tt("mail.list.hasAttachment")}
                checked={advanced.hasAttachment}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, hasAttachment: event.target.checked }))
                }
              />
              <Checkbox
                label={tt("mail.list.unreadOnly")}
                checked={advanced.unread}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, unread: event.target.checked }))
                }
              />
              <Checkbox
                label={tt("mail.list.flaggedOnly")}
                checked={advanced.flagged}
                onChange={(event) =>
                  setAdvanced((value) => ({ ...value, flagged: event.target.checked }))
                }
              />
              <Switch
                label={tt("mail.list.includeServer")}
                checked={advanced.includeServer}
                onChange={(checked) =>
                  setAdvanced((value) => ({ ...value, includeServer: checked }))
                }
              />
            </div>
            <div className={styles.advancedActions}>
              <Button size="small" appearance="subtle" onClick={clearSearch}>
                {tt("mail.list.clearFilters")}
              </Button>
              <Button size="small" appearance="primary" onClick={() => setAdvancedOpen(false)}>
                {tt("common.done")}
              </Button>
            </div>
          </div>
        ) : null}
      </header>

      <div className={styles.listBody} ref={listWrapRef}>
        {list.isLoading ? (
          <div className={styles.listLoading}>
            <Spinner size="medium" />
          </div>
        ) : list.isError ? (
          <EmptyState
            icon={<Mail16Regular />}
            title={tt("mail.list.errorTitle")}
            description={errorText(list.error)}
            actions={
              <Button size="small" onClick={() => void list.refetch()}>
                {tt("common.retry")}
              </Button>
            }
          />
        ) : (
          <VirtualList
            items={rows}
            estimateSize={density === "compact" ? 56 : density === "spacious" ? 96 : 76}
            getKey={(row) => row.key}
            onEndReached={() => {
              if (list.hasNextPage && !list.isFetchingNextPage) void list.fetchNextPage();
            }}
            emptyState={emptyState}
            ariaLabel={tt("mail.list.title")}
            role="listbox"
            renderItem={(row: MessageRow) => {
              if (row.kind === "header") {
                return (
                  <div className={styles.dateHeader}>
                    <span>{tt(groupLabelKey(row.group))}</span>
                    <span className={styles.dateHeaderCount}>{row.count}</span>
                  </div>
                );
              }
              const summary = row.summary;
              const isSelected = selected.ids.includes(summary.id);
              const isCurrent = summary.id === messageId;
              const account = accountById.get(summary.accountId);
              const showAccountStripe = unifiedRole !== null || (searchText && searchScope === "all");
              const expanded = expandedThreads.has(summary.threadId);
              const hoverActions = hoverActionButtons(summary);
              const outboxItem = isOutboxMessageId(summary.id);
              const outboxStatus = outboxItem ? parseOutboxStatus(summary.preview) : null;
              const swipeRightDescriptor = isSupportedSwipeAction(mailSettings.swipeRight)
                ? swipeDescriptor(mailSettings.swipeRight, summary)
                : null;
              const swipeLeftDescriptor = isSupportedSwipeAction(mailSettings.swipeLeft)
                ? swipeDescriptor(mailSettings.swipeLeft, summary)
                : null;
              const handleSwipe = (action: HoverAction) => {
                if (outboxItem) {
                  // Only discard makes sense for a queued message.
                  if (action === "delete") void deleteOutbox(summary);
                  return;
                }
                switch (action) {
                  case "archive":
                    void archiveMessages([summary.id]);
                    break;
                  case "delete":
                    void deleteMessages([summary.id]);
                    break;
                  case "flag":
                    setFlagged([summary.id], !summary.flagged);
                    break;
                  case "read":
                    setSeen([summary.id], !summary.seen);
                    break;
                  default:
                    break;
                }
              };
              return (
                <div className={styles.messageRowWrap}>
                  <SwipeableRow
                    swipeLeft={swipeLeftDescriptor}
                    swipeRight={swipeRightDescriptor}
                    onAction={handleSwipe}
                    disabled={selectionMode || (!swipeLeftDescriptor && !swipeRightDescriptor)}
                  >
                  <div
                    data-message-id={summary.id}
                    className={[
                      styles.messageRow,
                      isSelected ? styles.messageRowSelected : "",
                      isCurrent ? styles.messageRowCurrent : "",
                      !summary.seen ? styles.messageRowUnread : "",
                      outboxItem ? styles.messageRowOutbox : "",
                      outboxStatus?.state === "failed" ? styles.messageRowOutboxFailed : "",
                      hoverActions.length > 0 ? styles.messageRowHasActions : "",
                    ]
                      .filter(Boolean)
                      .join(" ")}
                    style={summaryStyle(density)}
                    role="option"
                    aria-selected={isSelected}
                    tabIndex={isSelected ? 0 : -1}
                    draggable={!outboxItem}
                    onDragStart={(event) => {
                      setMessageDragData(
                        event.dataTransfer,
                        isSelected && selected.ids.length > 0 ? selected.ids : [summary.id],
                      );
                    }}
                    onClick={(event) => handleSelect(summary, event)}
                    onKeyDown={(event) => {
                      if (
                        event.key === "ContextMenu" ||
                        (event.shiftKey && event.key === "F10")
                      ) {
                        event.preventDefault();
                        if (!selected.ids.includes(summary.id)) {
                          dispatchSelection({ type: "replace", ids: [summary.id] });
                          setActive(summary.id);
                        }
                        const rect = event.currentTarget.getBoundingClientRect();
                        setContextMenu({
                          summary,
                          point: { x: rect.left + 24, y: rect.bottom },
                        });
                      }
                    }}
                    onDoubleClick={() => {
                      if (summary.isDraft) void compose.openDraft(summary.id);
                      else openMessageWindow(summary.id, summary.subject);
                    }}
                    onContextMenu={(event) => openContextMenu(summary, event)}
                  >
                    {showAccountStripe && account ? (
                      <span
                        className={styles.accountStripe}
                        style={{ background: account.color }}
                        aria-hidden="true"
                      />
                    ) : null}
                    {selectionMode ? (
                      <span className={styles.rowCheckbox} onClick={(event) => event.stopPropagation()}>
                        <Checkbox
                          checked={isSelected}
                          onChange={() => dispatchSelection({ type: "toggle", id: summary.id })}
                          aria-label={tt("mail.list.selectMessage", { subject: summary.subject || tt("mail.list.noSubject") })}
                        />
                      </span>
                    ) : mailSettings.showAvatars ? (
                      <Avatar
                        className={styles.rowAvatar}
                        name={summary.from?.name}
                        email={summary.from?.email}
                        size={32}
                      />
                    ) : null}

                    <div className={styles.rowBody}>
                      <div className={styles.rowTop}>
                        <span className={styles.rowSender}>{senderName(summary)}</span>
                        {summary.importance === "high" ? (
                          <span className={styles.rowIconImportant} title={tt("mail.list.importanceHigh")}>
                            <Important16Regular />
                          </span>
                        ) : null}
                        {summary.hasAttachments ? (
                          <span className={styles.rowIcon} title={tt("mail.list.hasAttachments")}>
                            <Attach16Regular />
                          </span>
                        ) : null}
                        {summary.answered ? (
                          <span className={styles.rowIcon} title={tt("mail.list.answered")}>
                            <ArrowReply16Regular />
                          </span>
                        ) : null}
                        {summary.forwarded ? (
                          <span className={styles.rowIcon} title={tt("mail.list.forwarded")}>
                            <ArrowForward16Regular />
                          </span>
                        ) : null}
                        {summary.isDraft ? (
                          <span className={styles.rowIcon} title={tt("mail.list.draft")}>
                            <DocumentEdit16Regular />
                          </span>
                        ) : null}
                        {summary.flagged ? (
                          <span className={styles.rowIconFlag} title={tt("mail.action.flag")}>
                            <Flag16Regular />
                          </span>
                        ) : null}
                        {summary.threadCount > 1 ? (
                          <button
                            type="button"
                            className={[
                              styles.threadBadge,
                              expanded ? styles.threadBadgeOpen : "",
                              summary.threadUnread > 0 ? styles.threadBadgeUnread : "",
                            ]
                              .filter(Boolean)
                              .join(" ")}
                            title={
                              expanded
                                ? tt("mail.list.threadCollapse")
                                : tt("mail.list.threadExpand", { count: summary.threadCount })
                            }
                            aria-expanded={expanded}
                            onClick={(event) => {
                              event.stopPropagation();
                              setExpandedThreads((previous) => {
                                const next = new Set(previous);
                                if (next.has(summary.threadId)) next.delete(summary.threadId);
                                else next.add(summary.threadId);
                                return next;
                              });
                            }}
                          >
                            <ChatMultiple16Regular />
                            {summary.threadCount}
                          </button>
                        ) : null}
                        <span className={styles.rowSpacer} />
                        <span className={styles.rowDate}>{formatListDate(summary.date)}</span>
                      </div>
                      <div className={styles.rowSubject}>
                        {summary.subject || tt("mail.list.noSubject")}
                      </div>
                      {outboxItem ? (
                        <div
                          className={[
                            styles.rowOutboxStatus,
                            outboxStatus?.state === "failed" ? styles.rowOutboxStatusFailed : "",
                          ]
                            .filter(Boolean)
                            .join(" ")}
                        >
                          <span className={styles.rowOutboxIcon} aria-hidden="true">
                            {outboxStatus?.state === "failed" ? (
                              <Warning16Regular />
                            ) : (
                              <Send16Regular />
                            )}
                          </span>
                          <Tooltip
                            content={outboxStatus?.error ?? ""}
                            disabled={!outboxStatus?.error}
                            placement="bottom-start"
                            delay={300}
                          >
                            <span className={styles.rowOutboxLabel}>
                              {tt(
                                outboxStatusKey(outboxStatus?.state ?? "pending"),
                              )}
                            </span>
                          </Tooltip>
                          {outboxStatus?.error ? (
                            <span className={styles.rowOutboxError}>{outboxStatus.error}</span>
                          ) : null}
                        </div>
                      ) : mailSettings.previewLines > 0 && summary.preview ? (
                        <div
                          className={styles.rowPreview}
                          style={{ WebkitLineClamp: mailSettings.previewLines }}
                        >
                          {summary.preview}
                        </div>
                      ) : null}
                    </div>

                    {hoverActions.length > 0 ? (
                      <div
                        className={styles.rowHoverActions}
                        onClick={(event) => event.stopPropagation()}
                      >
                        {hoverActions.slice(0, 4).map((action) => (
                          <IconButton
                            key={action.key}
                            icon={action.icon}
                            label={action.label}
                            size="small"
                            appearance="subtle"
                            onClick={action.run}
                          />
                        ))}
                      </div>
                    ) : null}
                  </div>
                  </SwipeableRow>

                  {expanded ? (
                    <ThreadMembers
                      accountId={summary.accountId}
                      threadId={summary.threadId}
                      selectedId={messageId}
                      onSelect={(member) => {
                        dispatchSelection({ type: "replace", ids: [member.id] });
                        setActive(member.id);
                        closeInlineCompose();
                      }}
                      showAvatars={mailSettings.showAvatars}
                    />
                  ) : null}
                </div>
              );
            }}
          />
        )}
      </div>

      {list.isFetchingNextPage ? (
        <div className={styles.listFooter} role="status">
          <Spinner size="tiny" /> {tt("common.loading")}
        </div>
      ) : null}

      <Menu
        open={contextMenu !== null}
        onClose={() => setContextMenu(null)}
        items={contextMenuItems}
        point={contextMenu?.point ?? null}
        ariaLabel={tt("mail.list.messageMenu")}
      />

      <Menu
        open={hoverMenu !== null}
        onClose={() => setHoverMenu(null)}
        items={hoverMenuEntries}
        point={hoverMenu?.point ?? null}
        ariaLabel={tt("mail.action.move")}
      />
    </section>
  );
}
