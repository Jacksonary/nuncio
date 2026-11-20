/**
 * Reading pane: single message reader and the stacked conversation view.
 * The sanitized HTML document is rendered in a locked-down sandboxed iframe
 * (no allow-scripts, no allow-same-origin) and fills the remaining height with
 * its own internal scrolling.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  Archive24Regular,
  ArrowForward24Regular,
  ArrowMove24Regular,
  ArrowReply24Regular,
  ArrowReplyAll24Regular,
  Attach16Regular,
  BookOpen24Regular,
  DarkTheme24Regular,
  Code24Regular,
  Delete24Regular,
  DocumentEdit16Regular,
  Flag24Regular,
  Mail16Regular,
  MailUnread16Regular,
  MoreHorizontal20Regular,
  Open16Regular,
  Print24Regular,
  Save24Regular,
  TaskListSquareLtr16Regular,
  Warning16Regular,
} from "@fluentui/react-icons";
import {
  Avatar,
  Badge,
  Button,
  EmptyState,
  IconButton,
  MenuButton,
  Spinner,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import {
  app,
  folders as foldersApi,
  isIpcError,
  messages as messagesApi,
} from "@/ipc/api";
import type {
  Account,
  Address,
  Folder,
  FolderRole,
  MessageDetail,
  MessageSummary,
  ReplyMode,
} from "@/ipc/types";
import {
  formatAddressFull,
  formatAddressList,
  formatDateTime,
  formatListDate,
} from "@/lib/format";
import { t, useT } from "@/lib/i18n";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useIsDarkTheme } from "@/lib/useSettings";
import {
  isPermanentDelete,
  messageActionToast,
  useMessageActions,
} from "./actions";
import { onMailCommand } from "./commands";
import { useMailCompose } from "./composeIntegration";
import { safeFilename, parseUnifiedFolderId } from "./logic";
import { useConversationView } from "./conversationPrefs";
import { isOutboxMessageId } from "./outbox";
import {
  buildArticle,
  readReadingView,
  writeReadingView,
  type ArticleBlock,
} from "./readingView";
import {
  AttachmentStrip,
  InviteCard,
  OutboxNotice,
  RawHeadersDialog,
  RemoteContentBar,
  SenderContact,
} from "./MessageParts";
import {
  useAccountsQuery,
  useAllFolders,
  useMailSettings,
  type FolderChoice,
} from "./queries";
import styles from "./mail.module.css";

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/**
 * The backend cannot complete some unsubscribes itself (plain HTTP, or HTTPS
 * without List-Unsubscribe-Post: One-Click) and returns an `unsupported` error
 * whose message is `open <url> to unsubscribe`. Return that URL so the reader
 * can hand it to the OS browser.
 */
function unsubscribeUrlFrom(error: unknown): string | null {
  if (!isIpcError(error) || error.code !== "unsupported") return null;
  const match = /open (\S+) to unsubscribe/.exec(error.message);
  if (!match) return null;
  return /^(https?|mailto):/i.test(match[1]) ? match[1] : null;
}

function accountFor(accounts: Account[], accountId: string): Account | null {
  return accounts.find((account) => account.id === accountId) ?? null;
}

function openMessageWindow(id: string, subject: string, print = false): void {
  const route = print ? `/message/${id}?print=1` : `/message/${id}`;
  if (!isTauri()) {
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
    return;
  }
  app.openWindow(route, subject || t("shell.window.message")).catch((error: unknown) => {
    console.warn("[mail] open message window failed", error);
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
  });
}

/* ───────────────────────────── Commander ───────────────────────────── */

export interface ReadingPaneProps {
  messageId: string | null;
  emlPath?: string | null;
  standalone?: boolean;
  print?: boolean;
  /** Called after the open message was archived/deleted (secondary window). */
  onRemoved?: () => void;
}

export function ReadingPane({
  messageId,
  emlPath = null,
  standalone = false,
  print = false,
  onRemoved,
}: ReadingPaneProps) {
  if (emlPath) return <ExternalReader path={emlPath} print={print} />;
  if (!messageId) return <ReaderEmpty />;
  return (
    <MessageReader
      key={messageId}
      messageId={messageId}
      standalone={standalone}
      print={print}
      onRemoved={onRemoved}
    />
  );
}

function ReaderEmpty() {
  const tt = useT();
  return (
    <div className={styles.readerPane}>
      <EmptyState
        icon={<Mail16Regular />}
        title={tt("mail.reader.emptyTitle")}
        description={tt("mail.reader.emptyHint")}
      />
    </div>
  );
}

function ExternalReader({ path, print }: { path: string; print: boolean }) {
  const tt = useT();
  const query = useQuery({
    queryKey: ["message", "eml", path],
    queryFn: () => messagesApi.openEml(path),
    staleTime: Infinity,
    retry: 1,
  });
  if (query.isLoading) {
    return (
      <div className={styles.readerPane}>
        <div className={styles.readerLoading}>
          <Spinner size="medium" />
        </div>
      </div>
    );
  }
  if (query.isError || !query.data) {
    return (
      <div className={styles.readerPane}>
        <EmptyState
          icon={<Warning16Regular />}
          title={tt("mail.reader.openFailed")}
          description={errorText(query.error)}
          actions={
            <Button size="small" onClick={() => void query.refetch()}>
              {tt("common.retry")}
            </Button>
          }
        />
      </div>
    );
  }
  return (
    <div className={styles.readerPane}>
      <MessageCard key={query.data.summary.id} detail={query.data} external print={print} />
    </div>
  );
}

/* ─────────────────────────── Single reader ─────────────────────────── */

function MessageReader({
  messageId,
  standalone,
  print,
  onRemoved,
}: {
  messageId: string;
  standalone: boolean;
  print: boolean;
  onRemoved?: () => void;
}) {
  const tt = useT();
  const mailSettings = useMailSettings();
  const queryClient = useQueryClient();
  const accountsQuery = useAccountsQuery();
  const accounts = accountsQuery.data ?? [];
  const folders = useAllFolders(accounts);
  const allowRemoteRef = useRef(false);

  useEffect(() => {
    allowRemoteRef.current = false;
  }, [messageId]);

  const detailQuery = useQuery({
    queryKey: queryKeys.message(messageId),
    // Read the override through a ref so refetches triggered in the same tick
    // as the "show images" click already request the remote content.
    queryFn: () => messagesApi.get(messageId, allowRemoteRef.current),
    staleTime: 30_000,
  });

  const detail = detailQuery.data;

  // Same mailbox key the list uses, so the reader groups exactly when the list
  // does: the unified views share one choice, a real folder uses its account's,
  // and a standalone window (no selection) falls back to the message's account.
  const listFolderId = useShellStore((state) => state.selection.mail.folderId);
  const listFolder = listFolderId ? folders.byId.get(listFolderId) : undefined;
  const conversationAccountId = parseUnifiedFolderId(listFolderId)
    ? null
    : (listFolder?.accountId ?? detail?.summary.accountId ?? null);
  const [conversationView] = useConversationView(
    conversationAccountId,
    mailSettings.conversationView,
  );

  if (detailQuery.isLoading) {
    return (
      <div className={styles.readerPane}>
        <div className={styles.readerLoading}>
          <Spinner size="medium" />
        </div>
      </div>
    );
  }
  if (detailQuery.isError || !detail) {
    return (
      <div className={styles.readerPane}>
        <EmptyState
          icon={<Warning16Regular />}
          title={tt("mail.reader.loadFailed")}
          description={errorText(detailQuery.error)}
          actions={
            <Button size="small" onClick={() => void detailQuery.refetch()}>
              {tt("common.retry")}
            </Button>
          }
        />
      </div>
    );
  }

  const folder = folders.byId.get(detail.summary.folderId);
  const conversation = conversationView && detail.summary.threadCount > 1;

  if (conversation) {
    return (
      <ConversationReader
        messageId={messageId}
        accountId={detail.summary.accountId}
        threadId={detail.summary.threadId}
        folders={folders.byId}
        standalone={standalone}
        onRemoved={onRemoved}
      />
    );
  }

  return (
    <div className={styles.readerPane}>
      <MessageCard
        detail={detail}
        folderRole={folder?.role ?? null}
        active
        print={print}
        standalone={standalone}
        onRemoved={onRemoved}
        onAllowRemote={() => {
          allowRemoteRef.current = true;
          void messagesApi
            .get(messageId, true)
            .then((next) => {
              queryClient.setQueryData(queryKeys.message(messageId), next);
            })
            .catch((error: unknown) => {
              toast.error(tt("mail.remote.loadFailed"), errorText(error));
            });
        }}
      />
    </div>
  );
}

/* ─────────────────────── Conversation reader ─────────────────────── */

function ConversationReader({
  messageId,
  accountId,
  threadId,
  folders,
  standalone,
  onRemoved,
}: {
  messageId: string;
  accountId: string;
  threadId: string;
  folders: Map<string, Folder>;
  standalone: boolean;
  onRemoved?: () => void;
}) {
  const tt = useT();
  const threadQuery = useQuery({
    queryKey: queryKeys.thread(accountId, threadId),
    queryFn: () => messagesApi.thread(accountId, threadId),
    staleTime: 15_000,
  });
  const members = useMemo(() => threadQuery.data ?? [], [threadQuery.data]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set([messageId]));

  useEffect(() => {
    const next = new Set<string>([messageId]);
    for (const member of members) if (!member.seen) next.add(member.id);
    setExpanded(next);
  }, [messageId, members]);

  if (threadQuery.isLoading) {
    return (
      <div className={styles.readerPane}>
        <div className={styles.readerLoading}>
          <Spinner size="medium" />
        </div>
      </div>
    );
  }
  if (threadQuery.isError) {
    return (
      <div className={styles.readerPane}>
        <EmptyState
          icon={<Warning16Regular />}
          title={tt("mail.reader.loadFailed")}
          description={errorText(threadQuery.error)}
          actions={
            <Button size="small" onClick={() => void threadQuery.refetch()}>
              {tt("common.retry")}
            </Button>
          }
        />
      </div>
    );
  }

  return (
    <div className={styles.readerPane}>
      <div className={styles.conversationHeader}>
        <span className={styles.conversationTitle}>{tt("mail.reader.conversation")}</span>
        <span className={styles.conversationCount}>
          {tt("mail.reader.conversationCount", { count: members.length })}
        </span>
      </div>
      <div className={styles.conversationScroll}>
        {members.map((member, index) => {
          const isOpen = expanded.has(member.id);
          const role = folders.get(member.folderId)?.role ?? null;
          return (
            <ThreadMember
              key={member.id}
              summary={member}
              index={index}
              total={members.length}
              expanded={isOpen}
              selected={member.id === messageId}
              folderRole={role}
              standalone={standalone}
              onRemoved={onRemoved}
              onExpand={() => {
                setExpanded((previous) => new Set(previous).add(member.id));
                useShellStore.getState().setSelection("mail", { messageId: member.id });
              }}
            />
          );
        })}
      </div>
    </div>
  );
}

function ThreadMember({
  summary,
  index,
  total,
  expanded,
  selected,
  folderRole,
  standalone,
  onRemoved,
  onExpand,
}: {
  summary: MessageSummary;
  index: number;
  total: number;
  expanded: boolean;
  selected: boolean;
  folderRole: FolderRole | null;
  standalone: boolean;
  onRemoved?: () => void;
  onExpand: () => void;
}) {
  const tt = useT();
  const query = useQuery({
    queryKey: queryKeys.message(summary.id),
    queryFn: () => messagesApi.get(summary.id, false),
    enabled: expanded,
    staleTime: 30_000,
  });

  if (!expanded) {
    return (
      <button
        type="button"
        className={styles.collapsedMessage}
        onClick={onExpand}
        aria-expanded={false}
      >
        <Avatar name={summary.from?.name} email={summary.from?.email} size={26} />
        <span className={styles.collapsedSender}>{summary.from?.name || summary.from?.email || tt("mail.list.unknownSender")}</span>
        <span className={styles.collapsedSubject}>{summary.subject || tt("mail.list.noSubject")}</span>
        {summary.hasAttachments ? <Attach16Regular aria-hidden="true" /> : null}
        <span className={styles.collapsedDate}>{formatListDate(summary.date)}</span>
      </button>
    );
  }

  if (query.isLoading) {
    return (
      <div className={styles.threadCardLoading}>
        <Spinner size="small" />
      </div>
    );
  }
  if (query.isError || !query.data) {
    return (
      <div className={styles.threadCardLoading}>
        <span>{tt("mail.reader.loadFailed")}</span>
        <Button size="small" onClick={() => void query.refetch()}>
          {tt("common.retry")}
        </Button>
      </div>
    );
  }

  return (
    <div className={styles.threadCard}>
      <div className={styles.threadCardLabel}>
        {tt("mail.reader.threadPosition", { index: index + 1, total })}
      </div>
      <MessageCard
        detail={query.data}
        folderRole={folderRole}
        active={selected}
        standalone={standalone}
        onRemoved={onRemoved}
        thread
      />
    </div>
  );
}

/* ──────────────────────────── Message card ──────────────────────────── */

interface MessageCardProps {
  detail: MessageDetail;
  folderRole?: FolderRole | null;
  external?: boolean;
  /** Whether this card reacts to global mail commands (only one at a time). */
  active?: boolean;
  thread?: boolean;
  standalone?: boolean;
  print?: boolean;
  onRemoved?: () => void;
  onAllowRemote?: () => void;
}

function MessageCard({
  detail,
  folderRole = null,
  external = false,
  active = false,
  thread = false,
  standalone = false,
  print = false,
  onRemoved,
  onAllowRemote,
}: MessageCardProps) {
  const tt = useT();
  const queryClient = useQueryClient();
  const compose = useMailCompose();
  const { apply } = useMessageActions();
  const accountsQuery = useAccountsQuery();
  const accounts = accountsQuery.data ?? [];
  const [recipientsOpen, setRecipientsOpen] = useState(false);
  const [rawOpen, setRawOpen] = useState(false);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  // Reading view ("readability") preference, remembered per message.
  const [readingView, setReadingView] = useState(() => readReadingView(detail.summary.id));
  // "View on white": a one-off choice for the message on screen. It lives only
  // in this card's memory (never stored), so showing another message, or
  // coming back to this one later, starts from the app theme again.
  const isDark = useIsDarkTheme();
  const [whiteHtml, setWhiteHtml] = useState<string | null>(null);
  const [whiteBusy, setWhiteBusy] = useState(false);
  const remoteAllowedRef = useRef(false);
  // The message on screen right now, so a slow fetch that finishes after the
  // user moved on cannot paint its result onto a different message.
  const currentIdRef = useRef(detail.summary.id);
  currentIdRef.current = detail.summary.id;
  // Once a message has ever been seen while open, never auto-mark it again in
  // this view (otherwise "mark as unread" would immediately flip back).
  const seenTrueRef = useRef(false);

  const summary = detail.summary;
  // Synthetic outbox rows are read-only projections of the send queue.
  const isOutbox = isOutboxMessageId(summary.id);
  const articleBlocks = useMemo(
    () => buildArticle(detail.html, detail.text),
    [detail.html, detail.text],
  );
  const canUseReadingView = detail.html.trim().length > 0 && articleBlocks.length > 0;
  const showReadingView = readingView && canUseReadingView;
  const account = accountFor(accounts, summary.accountId);
  const sender = summary.from ?? detail.replyTo[0] ?? null;
  const to = detail.to.length > 0 ? detail.to : summary.to;
  const showRemoteBar =
    detail.hasRemoteContent && !detail.remoteContentAllowed && !external;
  const printedRef = useRef(false);
  const folderChoicesQuery = useQuery({
    queryKey: queryKeys.folders(summary.accountId || "external"),
    queryFn: () => foldersApi.list(summary.accountId),
    enabled: !external && Boolean(summary.accountId),
    staleTime: 30_000,
  });

  useEffect(() => {
    seenTrueRef.current = false;
  }, [summary.id]);

  // A card can be reused when the selected message changes (e.g. external
  // .eml files in the same window); restore that message's saved preference.
  useEffect(() => {
    setReadingView(readReadingView(summary.id));
  }, [summary.id]);

  useEffect(() => {
    if (summary.seen) seenTrueRef.current = true;
  }, [summary.id, summary.seen]);

  // Mark as read when opened (covers stacked conversations where several
  // messages are visible at once).
  useEffect(() => {
    if (summary.seen || external || seenTrueRef.current) return;
    seenTrueRef.current = true;
    void apply({ ids: [summary.id], action: { kind: "setSeen", value: true } }).catch(() => {
      // The mutation surfaces its own error; a failed read receipt must not
      // prevent the user from reading the message.
    });
  }, [apply, external, summary.id, summary.seen]);

  const clearSelection = useCallback(() => {
    useShellStore.getState().setSelection("mail", { messageId: null });
  }, []);

  const reply = useCallback(
    (mode: ReplyMode) => {
      if (isOutbox) return;
      if (summary.isDraft) {
        void compose.openDraft(summary.id);
        return;
      }
      void compose.replyInline(summary.id, mode);
    },
    [compose, isOutbox, summary.id, summary.isDraft],
  );

  const archive = useCallback(async () => {
    if (isOutbox) return;
    setBusyAction("archive");
    try {
      const outcome = await apply({ ids: [summary.id], action: { kind: "archive" } });
      messageActionToast({ kind: "archive" }, 1, outcome.undoId);
      if (standalone) onRemoved?.();
      else clearSelection();
    } catch {
      // toast shown by the mutation
    } finally {
      setBusyAction(null);
    }
  }, [apply, clearSelection, isOutbox, onRemoved, standalone, summary.id]);

  const remove = useCallback(async () => {
    // A draft is always gone for good: moving it to Trash would leave
    // is_draft set and keep inflating the Drafts count.
    const permanent = summary.isDraft || isPermanentDelete(folderRole);
    const ok = await confirm({
      title: permanent ? tt("mail.reader.deletePermanentTitle") : tt("mail.reader.deleteTitle"),
      message: permanent
        ? tt("mail.reader.deletePermanentMessage")
        : tt("mail.reader.deleteMessage"),
      confirmLabel: tt("common.delete"),
      danger: true,
    });
    if (!ok) return;
    setBusyAction("delete");
    try {
      const outcome = await apply({ ids: [summary.id], action: { kind: "delete", permanent } });
      messageActionToast({ kind: "delete", permanent }, 1, outcome.undoId);
      if (standalone) onRemoved?.();
      else clearSelection();
    } catch {
      // toast shown by the mutation
    } finally {
      setBusyAction(null);
    }
  }, [
    apply,
    clearSelection,
    folderRole,
    standalone,
    summary.id,
    summary.isDraft,
    tt,
  ]);

  const discardOutbox = useCallback(async () => {
    const ok = await confirm({
      title: tt("mail.outbox.discardTitle"),
      message: tt("mail.outbox.discardMessage"),
      confirmLabel: tt("mail.outbox.discard"),
      danger: true,
    });
    if (!ok) return;
    setBusyAction("delete");
    try {
      await messagesApi.act([summary.id], { kind: "delete", permanent: true });
      toast.success(tt("mail.outbox.discarded"));
      void queryClient.invalidateQueries({ queryKey: ["messages"] });
      void queryClient.invalidateQueries({ queryKey: ["folders"] });
      void queryClient.invalidateQueries({ queryKey: ["unifiedCounts"] });
      void queryClient.invalidateQueries({ queryKey: ["message"] });
      if (standalone) onRemoved?.();
      else clearSelection();
    } catch (error) {
      toast.error(tt("mail.toast.actionFailed"), errorText(error));
    } finally {
      setBusyAction(null);
    }
  }, [clearSelection, onRemoved, queryClient, standalone, summary.id, tt]);

  const toggleFlag = useCallback(async () => {
    if (isOutbox) return;
    try {
      await apply({
        ids: [summary.id],
        action: { kind: "setFlagged", value: !summary.flagged },
      });
    } catch {
      // toast shown by the mutation
    }
  }, [apply, isOutbox, summary.flagged, summary.id]);

  const toggleReadingView = useCallback(() => {
    setReadingView((previous) => {
      const next = !previous;
      writeReadingView(detail.summary.id, next);
      return next;
    });
  }, [detail.summary.id]);

  const markUnread = useCallback(async () => {
    if (isOutbox) return;
    try {
      await apply({ ids: [summary.id], action: { kind: "setSeen", value: false } });
    } catch {
      // toast shown by the mutation
    }
  }, [apply, isOutbox, summary.id]);

  const createTask = useCallback(async () => {
    try {
      await messagesApi.createTask(summary.id);
      toast.success(tt("mail.toast.taskCreated"));
    } catch (error) {
      toast.error(tt("mail.toast.taskFailed"), errorText(error));
    }
  }, [summary.id, tt]);

  const saveAs = useCallback(async () => {
    if (external) return;
    try {
      const path = await saveDialog({
        defaultPath: `${safeFilename(summary.subject, tt("common.untitled"))}.eml`,
        filters: [{ name: tt("mail.action.saveAsFilter"), extensions: ["eml"] }],
      });
      if (!path) return;
      await messagesApi.saveEml(summary.id, path);
      toast.success(tt("mail.toast.savedEml"));
    } catch (error) {
      toast.error(tt("mail.toast.saveFailed"), errorText(error));
    }
  }, [external, summary.id, summary.subject, tt]);

  const unsubscribe = useCallback(async () => {
    const ok = await confirm({
      title: tt("mail.reader.unsubscribeTitle"),
      message: tt("mail.reader.unsubscribeMessage"),
      confirmLabel: tt("mail.reader.unsubscribe"),
      danger: true,
    });
    if (!ok) return;
    try {
      await messagesApi.unsubscribe(summary.id);
      toast.success(tt("mail.reader.unsubscribed"));
    } catch (error) {
      const url = unsubscribeUrlFrom(error);
      if (url) {
        // The provider requires the user to finish on its own page.
        if (isTauri()) {
          openUrl(url).catch((openError: unknown) => {
            console.warn("[mail] could not open the unsubscribe URL", openError);
          });
        } else {
          try {
            window.open(url, "_blank", "noopener,noreferrer");
          } catch {
            // Popup blocked in the browser preview.
          }
        }
        return;
      }
      toast.error(tt("mail.toast.actionFailed"), errorText(error));
    }
  }, [summary.id, tt]);

  const printMessage = useCallback(() => {
    if (external) {
      window.print();
      return;
    }
    openMessageWindow(summary.id, summary.subject, true);
  }, [external, summary.id, summary.subject]);

  useEffect(() => {
    // New message, or the body changed under us (for example remote images were
    // just allowed): drop the white copy rather than show a stale one.
    setWhiteHtml(null);
  }, [summary.id, detail.html]);
  useEffect(() => {
    remoteAllowedRef.current = false;
  }, [summary.id]);

  const toggleWhiteBackground = useCallback(async () => {
    if (whiteHtml !== null) {
      setWhiteHtml(null);
      return;
    }
    const requestedFor = summary.id;
    setWhiteBusy(true);
    try {
      const next = await messagesApi.get(requestedFor, remoteAllowedRef.current, true);
      if (currentIdRef.current === requestedFor) setWhiteHtml(next.html);
    } catch (error) {
      toast.error(tt("mail.reader.whiteBackgroundFailed"), errorText(error));
    } finally {
      setWhiteBusy(false);
    }
  }, [summary.id, tt, whiteHtml]);

  const allowRemote = useCallback(async () => {
    remoteAllowedRef.current = true;
    if (onAllowRemote) {
      onAllowRemote();
      return;
    }
    try {
      const next = await messagesApi.get(summary.id, true);
      queryClient.setQueryData(queryKeys.message(summary.id), next);
    } catch (error) {
      toast.error(tt("mail.remote.loadFailed"), errorText(error));
    }
  }, [onAllowRemote, queryClient, summary.id, tt]);

  const printOnce = useCallback(() => {
    if (!print || printedRef.current) return;
    printedRef.current = true;
    try {
      window.print();
    } catch (error) {
      console.warn("[mail] print failed", error);
    }
  }, [print]);

  // Fallback when the body never fires onLoad (empty/broken message).
  useEffect(() => {
    if (!print) return;
    const timer = window.setTimeout(printOnce, 1500);
    return () => window.clearTimeout(timer);
  }, [print, printOnce]);

  // The reading view has no iframe to wait for, so print it as soon as the
  // article is on screen.
  useEffect(() => {
    if (!print || !showReadingView) return;
    printOnce();
  }, [print, printOnce, showReadingView]);

  // Global mail shortcuts that belong to the open message.
  useEffect(() => {
    if (!active) return;
    return onMailCommand((command) => {
      switch (command.type) {
        case "reply":
          reply(command.mode);
          break;
        case "print":
          printMessage();
          break;
        case "saveAs":
          void saveAs();
          break;
        case "createTask":
          void createTask();
          break;
        default:
          break;
      }
    });
  }, [active, createTask, printMessage, reply, saveAs]);

  const folderChoices = useMemo<FolderChoice[]>(() => {
    const list = folderChoicesQuery.data ?? [];
    const result: FolderChoice[] = [];
    for (const folder of list) {
      if (!folder.isSelectable) continue;
      const folderAccount = accountFor(accounts, folder.accountId);
      if (!folderAccount) continue;
      result.push({ folder, account: folderAccount, depth: folder.parentId ? 1 : 0 });
    }
    return result;
  }, [accounts, folderChoicesQuery.data]);

  const moveEntries = useMemo<MenuEntry[]>(() => {
    const entries: MenuEntry[] = [];
    for (const choice of folderChoices) {
      if (choice.folder.id === summary.folderId) continue;
      entries.push({
        id: `move-${choice.folder.id}`,
        label: `${" ".repeat(choice.depth * 2)}${choice.folder.name}`,
        onSelect: () => {
          void (async () => {
            try {
              const outcome = await apply({
                ids: [summary.id],
                action: { kind: "move", folderId: choice.folder.id },
              });
              messageActionToast({ kind: "move", folderId: choice.folder.id }, 1, outcome.undoId);
              if (standalone) onRemoved?.();
              else clearSelection();
            } catch {
              // toast shown by the mutation
            }
          })();
        },
      });
    }
    return entries;
  }, [apply, clearSelection, folderChoices, onRemoved, standalone, summary.folderId, summary.id]);

  const moreEntries = useMemo<MenuEntry[]>(() => {
    const entries: MenuEntry[] = [];
    if (!external) {
      entries.push({
        id: "mark-unread",
        label: tt("mail.action.markUnread"),
        icon: <MailUnread16Regular />,
        onSelect: () => void markUnread(),
      });
    }
    entries.push({
      id: "print",
      label: tt("mail.action.print"),
      icon: <Print24Regular style={{ fontSize: 16 }} />,
      onSelect: printMessage,
    });
    if (!external) {
      entries.push(
        {
          id: "save-as",
          label: tt("mail.action.saveAs"),
          icon: <Save24Regular style={{ fontSize: 16 }} />,
          onSelect: () => void saveAs(),
        },
        {
          id: "raw",
          label: tt("mail.action.viewSource"),
          icon: <Code24Regular style={{ fontSize: 16 }} />,
          onSelect: () => setRawOpen(true),
        },
        {
          id: "open-window",
          label: tt("mail.action.openWindow"),
          icon: <Open16Regular />,
          onSelect: () => openMessageWindow(summary.id, summary.subject),
        },
      );
      if (detail.listUnsubscribe) {
        entries.push({
          id: "unsubscribe",
          label: tt("mail.reader.unsubscribe"),
          onSelect: () => void unsubscribe(),
        });
      }
      entries.push({
        id: "create-task",
        label: tt("mail.action.createTask"),
        icon: <TaskListSquareLtr16Regular />,
        onSelect: () => void createTask(),
      });
    }
    return entries;
  }, [createTask, detail.listUnsubscribe, external, markUnread, printMessage, saveAs, summary.id, summary.subject, tt, unsubscribe]);

  const outboxMoreEntries = useMemo<MenuEntry[]>(
    () => [
      {
        id: "print",
        label: tt("mail.action.print"),
        icon: <Print24Regular style={{ fontSize: 16 }} />,
        onSelect: printMessage,
      },
      {
        id: "raw",
        label: tt("mail.action.viewSource"),
        icon: <Code24Regular style={{ fontSize: 16 }} />,
        onSelect: () => setRawOpen(true),
      },
      {
        id: "open-window",
        label: tt("mail.action.openWindow"),
        icon: <Open16Regular />,
        onSelect: () => openMessageWindow(summary.id, summary.subject),
      },
    ],
    [printMessage, summary.id, summary.subject, tt],
  );

  const recipientsSummary = formatAddressList(to) || tt("mail.reader.noRecipients");

  // Only where it can make a difference: a dark theme, an HTML body that is not
  // swapped for the reading view, and a stored message (outbox and external
  // .eml renders do not take this choice).
  const whiteBackgroundButton =
    isDark && !showReadingView && !isOutbox && !external && detail.html.trim() ? (
      <IconButton
        icon={<DarkTheme24Regular />}
        label={
          whiteHtml !== null
            ? tt("mail.reader.whiteBackgroundOff")
            : tt("mail.reader.whiteBackground")
        }
        selected={whiteHtml !== null}
        onClick={() => void toggleWhiteBackground()}
        disabled={whiteBusy}
      />
    ) : null;

  const readingViewButton = detail.html.trim() ? (
    <IconButton
      icon={<BookOpen24Regular />}
      label={showReadingView ? tt("mail.reader.readingViewOff") : tt("mail.reader.readingView")}
      tooltip={canUseReadingView ? undefined : tt("mail.reader.readingViewUnavailable")}
      selected={showReadingView}
      onClick={toggleReadingView}
      disabled={!canUseReadingView}
    />
  ) : null;

  return (
    <article className={[styles.messageCard, thread ? styles.threadMessageCard : ""].filter(Boolean).join(" ")}>
      <header className={styles.messageHeader}>
        <div className={styles.subjectRow}>
          <h1 className={styles.messageSubject}>{summary.subject || tt("mail.list.noSubject")}</h1>
          {summary.importance === "high" ? (
            <Badge appearance="warning">{tt("mail.reader.importanceHigh")}</Badge>
          ) : null}
          {summary.flagged ? <Flag24Regular className={styles.headerFlag} aria-label={tt("mail.action.flag")} /> : null}
        </div>

        <div className={styles.messageMeta}>
          <Avatar
            name={sender?.name}
            email={sender?.email}
            size={36}
            className={styles.headerAvatar}
          />
          <div className={styles.messageMetaText}>
            <div className={styles.senderLine}>
              {sender ? (
                <SenderContact address={sender} />
              ) : (
                <span>{tt("mail.list.unknownSender")}</span>
              )}
              {sender ? <span className={styles.senderAddress}>&lt;{sender.email}&gt;</span> : null}
            </div>
            <div className={styles.recipientLine}>
              <button
                type="button"
                className={styles.recipientToggle}
                onClick={() => setRecipientsOpen((open) => !open)}
                aria-expanded={recipientsOpen}
              >
                {tt("mail.reader.to")} {recipientsSummary}
              </button>
              {detail.cc.length > 0 ? (
                <span className={styles.recipientChip}>
                  {tt("mail.reader.cc")} {formatAddressList(detail.cc)}
                </span>
              ) : null}
              {detail.bcc.length > 0 ? (
                <span className={styles.recipientChip}>
                  {tt("mail.reader.bcc")} {formatAddressList(detail.bcc)}
                </span>
              ) : null}
              <span className={styles.messageDate}>{formatDateTime(summary.date)}</span>
            </div>
            {recipientsOpen ? (
              <div className={styles.recipientsDetail}>
                <RecipientRow label={tt("mail.reader.to")} addresses={to} />
                {detail.cc.length > 0 ? (
                  <RecipientRow label={tt("mail.reader.cc")} addresses={detail.cc} />
                ) : null}
                {detail.bcc.length > 0 ? (
                  <RecipientRow label={tt("mail.reader.bcc")} addresses={detail.bcc} />
                ) : null}
                {detail.replyTo.length > 0 ? (
                  <RecipientRow label={tt("mail.reader.replyTo")} addresses={detail.replyTo} />
                ) : null}
              </div>
            ) : null}
          </div>
          {account ? (
            <span className={styles.accountChip}>
              <span className={styles.accountDot} style={{ background: account.color }} aria-hidden="true" />
              {account.displayName}
            </span>
          ) : null}
        </div>

        {external ? (
          <div className={styles.externalNotice}>{tt("mail.reader.externalNotice")}</div>
        ) : null}
      </header>

      {isOutbox ? (
        <div className={styles.messageToolbar} role="toolbar" aria-label={tt("mail.reader.toolbar")}>
          <IconButton
            icon={busyAction === "delete" ? <Spinner size="tiny" /> : <Delete24Regular />}
            label={tt("mail.outbox.discard")}
            onClick={() => void discardOutbox()}
            disabled={busyAction !== null}
          />
          <MenuButton
            label={tt("mail.action.more")}
            icon={<MoreHorizontal20Regular />}
            items={outboxMoreEntries}
            appearance="subtle"
          />
          <span className={styles.toolbarSpacer} />
          {readingViewButton}
        </div>
      ) : !external ? (
        <div className={styles.messageToolbar} role="toolbar" aria-label={tt("mail.reader.toolbar")}>
          <IconButton
            icon={<ArrowReply24Regular />}
            label={tt("mail.action.reply")}
            onClick={() => reply("reply")}
          />
          <IconButton
            icon={<ArrowReplyAll24Regular />}
            label={tt("mail.action.replyAll")}
            onClick={() => reply("replyAll")}
          />
          <IconButton
            icon={<ArrowForward24Regular />}
            label={tt("mail.action.forward")}
            onClick={() => reply("forward")}
          />
          <span className={styles.toolbarSeparator} aria-hidden="true" />
          <IconButton
            icon={busyAction === "archive" ? <Spinner size="tiny" /> : <Archive24Regular />}
            label={tt("mail.action.archive")}
            onClick={() => void archive()}
            disabled={busyAction !== null}
          />
          <IconButton
            icon={busyAction === "delete" ? <Spinner size="tiny" /> : <Delete24Regular />}
            label={tt("mail.action.delete")}
            onClick={() => void remove()}
            disabled={busyAction !== null}
          />
          <IconButton
            icon={<Flag24Regular />}
            label={summary.flagged ? tt("mail.action.unflag") : tt("mail.action.flag")}
            selected={summary.flagged}
            onClick={() => void toggleFlag()}
          />
          <MenuButton
            label={tt("mail.action.move")}
            icon={<ArrowMove24Regular />}
            items={moveEntries}
            appearance="subtle"
            disabled={moveEntries.length === 0}
          />
          <MenuButton
            label={tt("mail.action.more")}
            icon={<MoreHorizontal20Regular />}
            items={moreEntries}
            appearance="subtle"
          />
          <span className={styles.toolbarSpacer} />
          {whiteBackgroundButton}
          {readingViewButton}
          {summary.isDraft ? (
            <Button
              size="small"
              appearance="subtle"
              icon={<DocumentEdit16Regular />}
              onClick={() => void compose.openDraft(summary.id)}
            >
              {tt("mail.action.openDraft")}
            </Button>
          ) : null}
        </div>
      ) : (
        <div className={styles.messageToolbar}>
          {readingViewButton}
          <Button size="small" appearance="subtle" icon={<Print24Regular />} onClick={printMessage}>
            {tt("mail.action.print")}
          </Button>
        </div>
      )}

      {isOutbox ? <OutboxNotice preview={summary.preview} /> : null}
      {showRemoteBar && !showReadingView ? (
        <RemoteContentBar detail={detail} onAllow={() => void allowRemote()} />
      ) : null}
      {detail.invite && !isOutbox ? (
        <InviteCard messageId={summary.id} invite={detail.invite} />
      ) : null}

      <div className={styles.bodyWrap}>
        {showReadingView ? (
          <ReadingArticle blocks={articleBlocks} />
        ) : detail.html ? (
          <iframe
            className={styles.messageFrame}
            title={summary.subject || tt("mail.reader.messageBody")}
            sandbox="allow-popups allow-popups-to-escape-sandbox"
            srcDoc={whiteHtml ?? detail.html}
            onLoad={printOnce}
          />
        ) : detail.text ? (
          <pre className={styles.plainBody}>{detail.text}</pre>
        ) : (
          <div className={styles.emptyBody}>{tt("mail.reader.emptyBody")}</div>
        )}
      </div>

      {detail.attachments.length > 0 ? (
        <AttachmentStrip messageId={summary.id} attachments={detail.attachments} />
      ) : null}

      <RawHeadersDialog open={rawOpen} onClose={() => setRawOpen(false)} headers={detail.rawHeaders} />
    </article>
  );
}

/**
 * Typographic rendering of the extracted article. Only text nodes are
 * rendered, so no markup from the sender can reach the app document.
 */
function ReadingArticle({ blocks }: { blocks: ArticleBlock[] }) {
  const markers = useMemo(() => {
    let ordered = 0;
    return blocks.map((block) => {
      if (block.type !== "listItem") {
        ordered = 0;
        return "";
      }
      if (block.ordered) {
        ordered += 1;
        return `${ordered}.`;
      }
      ordered = 0;
      return "•";
    });
  }, [blocks]);

  return (
    <div className={styles.readingView}>
      <article className={styles.readingArticle}>
        {blocks.map((block, index) => {
          const key = `${index}-${block.type}`;
          switch (block.type) {
            case "heading": {
              const level = Math.min(6, Math.max(1, block.level ?? 2));
              const Tag = `h${level}` as "h1" | "h2" | "h3" | "h4" | "h5" | "h6";
              return (
                <Tag
                  key={key}
                  className={[styles.readingHeading, styles[`readingHeading${level}`]]
                    .filter(Boolean)
                    .join(" ")}
                >
                  {block.text}
                </Tag>
              );
            }
            case "listItem":
              return (
                <div key={key} className={styles.readingListItem}>
                  <span className={styles.readingListMarker} aria-hidden="true">
                    {markers[index]}
                  </span>
                  <span className={styles.readingListText}>{block.text}</span>
                </div>
              );
            case "quote":
              return (
                <blockquote key={key} className={styles.readingQuote}>
                  {block.text}
                </blockquote>
              );
            case "pre":
              return (
                <pre key={key} className={styles.readingPre}>
                  {block.text}
                </pre>
              );
            default:
              return (
                <p key={key} className={styles.readingParagraph}>
                  {block.text}
                </p>
              );
          }
        })}
      </article>
    </div>
  );
}

function RecipientRow({ label, addresses }: { label: string; addresses: Address[] }) {
  return (
    <div className={styles.recipientDetailRow}>
      <span className={styles.recipientDetailLabel}>{label}</span>
      <span className={styles.recipientDetailValue}>
        {addresses.map((address) => (
          <span key={`${address.email}-${address.name ?? ""}`} title={formatAddressFull(address)}>
            {formatAddressFull(address)}
          </span>
        ))}
      </span>
    </div>
  );
}

export default ReadingPane;
