/**
 * Mail folder pane: unified inbox, one expandable section per account. Each
 * account shows its inbox, drafts and favourites directly; everything else is
 * folded behind a single "More" row. Folder rows are drag & drop targets and
 * expose the full folder context menu.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Add16Regular,
  Archive24Regular,
  ChevronDown16Regular,
  ChevronRight16Regular,
  Delete16Regular,
  Delete24Regular,
  DocumentEdit24Regular,
  ErrorCircle16Regular,
  Folder16Regular,
  FolderAdd16Regular,
  FolderMail24Regular,
  MailInbox16Regular,
  Mail24Regular,
  MailMultiple24Regular,
  MailUnread16Regular,
  MoreHorizontal16Regular,
  ArrowSync16Regular,
  Rename16Regular,
  ArrowMove20Regular,
  Star16Regular,
  Warning16Regular,
  WifiOff20Regular,
  Send24Regular,
  Settings16Regular,
  EyeOff16Regular,
} from "@fluentui/react-icons";
import { useQueryClient } from "@tanstack/react-query";
import {
  Button,
  CountBadge,
  Dialog,
  DotBadge,
  EmptyState,
  Input,
  Menu,
  MenuButton,
  Select,
  Spinner,
  Tooltip,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import { folders as foldersApi, isIpcError, sync as syncApi } from "@/ipc/api";
import type { Account, Folder, FolderRole } from "@/ipc/types";
import { t, useT } from "@/lib/i18n";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useMessageActions, messageActionToast } from "./actions";
import { hasMessageDragData, readMessageDragData } from "./dnd";
import {
  aggregateUnread,
  flattenFolderTree,
  isSameOrDescendant,
  parseUnifiedFolderId,
  splitAccountFolders,
  unifiedFolderId,
  type AccountFolderSections,
  type FolderNode,
} from "./logic";
import {
  folderRoleKey,
  useAccountsQuery,
  useAllFolders,
  useMailAccounts,
  useUnifiedCounts,
  useUnifiedAccounts,
  useInvalidateMail,
} from "./queries";
import styles from "./mail.module.css";

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function folderIcon(folder: Folder, size: "small" | "large" = "small") {
  const small = size === "small";
  switch (folder.role) {
    case "inbox":
      return small ? <MailInbox16Regular /> : <Mail24Regular />;
    case "drafts":
      return <DocumentEdit24Regular style={{ fontSize: small ? 16 : 20 }} />;
    case "sent":
      return <Send24Regular style={{ fontSize: small ? 16 : 20 }} />;
    case "trash":
      return <Delete24Regular style={{ fontSize: small ? 16 : 20 }} />;
    case "junk":
      return <Warning16Regular style={{ fontSize: small ? 16 : 20 }} />;
    case "archive":
      return <Archive24Regular style={{ fontSize: small ? 16 : 20 }} />;
    case "outbox":
      return <ArrowSync16Regular style={{ fontSize: small ? 16 : 20 }} />;
    default:
      return small ? <Folder16Regular /> : <FolderMail24Regular />;
  }
}

function roleLabel(folder: Folder): string {
  if (folder.role === "other") return folder.name;
  const key = folderRoleKey(folder.role);
  const translated = t(key);
  return translated !== key ? translated : folder.name;
}

interface FolderDialogState {
  kind: "create" | "rename" | "move" | "delete" | "empty";
  accountId: string;
  folder?: Folder;
  parentId?: string | null;
}

interface FolderRowProps {
  node: FolderNode;
  depth: number;
  account: Account;
  selectedId: string | null;
  expanded: Set<string>;
  onToggle: (id: string) => void;
  onSelect: (folder: Folder) => void;
  onContextMenu: (folder: Folder, point: { x: number; y: number }) => void;
  onDropMessages: (folder: Folder, ids: string[]) => void;
  dragOverId: string | null;
  setDragOverId: (id: string | null) => void;
}

function FolderRow({
  node,
  depth,
  account,
  selectedId,
  expanded,
  onToggle,
  onSelect,
  onContextMenu,
  onDropMessages,
  dragOverId,
  setDragOverId,
}: FolderRowProps) {
  const { folder } = node;
  const open = expanded.has(folder.id);
  const hasChildren = node.children.length > 0;
  const selected = selectedId === folder.id;
  const dropActive = dragOverId === folder.id;
  // A `\Noselect` node is a grouping container (`[Gmail]`, Proton's `Folders`).
  // Selecting it would ask the server for a mailbox it refuses to open, so the
  // row only toggles its children.
  const selectable = folder.isSelectable;
  const activate = () => {
    if (selectable) onSelect(folder);
    else if (hasChildren) onToggle(folder.id);
  };
  return (
    <>
      <div
        className={[
          styles.folderRow,
          selected ? styles.folderRowSelected : "",
          dropActive ? styles.folderRowDrop : "",
          !folder.syncEnabled ? styles.folderRowMuted : "",
        ]
          .filter(Boolean)
          .join(" ")}
        style={{ paddingLeft: 6 + depth * 14 }}
        role="treeitem"
        aria-selected={selected}
        aria-expanded={hasChildren ? open : undefined}
        aria-disabled={selectable ? undefined : true}
        tabIndex={0}
        onClick={activate}
        onKeyDown={(event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            activate();
          } else if (event.key === "ArrowRight" && hasChildren && !open) {
            event.preventDefault();
            onToggle(folder.id);
          } else if (event.key === "ArrowLeft" && hasChildren && open) {
            event.preventDefault();
            onToggle(folder.id);
          } else if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
            event.preventDefault();
            const rect = event.currentTarget.getBoundingClientRect();
            onContextMenu(folder, { x: rect.left + 16, y: rect.bottom });
          }
        }}
        onContextMenu={(event) => {
          event.preventDefault();
          onContextMenu(folder, { x: event.clientX, y: event.clientY });
        }}
        onDragOver={(event) => {
          if (!folder.isSelectable || !hasMessageDragData(event.dataTransfer)) return;
          event.preventDefault();
          event.dataTransfer.dropEffect = "move";
          setDragOverId(folder.id);
        }}
        onDragLeave={() => {
          if (dragOverId === folder.id) setDragOverId(null);
        }}
        onDrop={(event) => {
          if (!folder.isSelectable || !hasMessageDragData(event.dataTransfer)) return;
          event.preventDefault();
          setDragOverId(null);
          const ids = readMessageDragData(event.dataTransfer);
          if (ids.length > 0) onDropMessages(folder, ids);
        }}
      >
        <button
          type="button"
          className={styles.folderChevron}
          aria-label={open ? t("common.collapse") : t("common.expand")}
          tabIndex={-1}
          onClick={(event) => {
            event.stopPropagation();
            if (hasChildren) onToggle(folder.id);
          }}
        >
          {hasChildren ? (
            open ? (
              <ChevronDown16Regular />
            ) : (
              <ChevronRight16Regular />
            )
          ) : (
            <span className={styles.folderChevronSpacer} />
          )}
        </button>
        <span className={styles.folderIcon} aria-hidden="true">
          {folderIcon(folder)}
        </span>
        <span className={styles.folderName} title={roleLabel(folder)}>
          {roleLabel(folder)}
        </span>
        {!folder.syncEnabled ? (
          <Tooltip content={t("mail.sidebar.notSynced")} placement="right">
            <span className={styles.folderMutedIcon} aria-label={t("mail.sidebar.notSynced")}>
              <EyeOff16Regular />
            </span>
          </Tooltip>
        ) : null}
        <CountBadge
          count={folder.unreadCount}
          appearance="accent"
          label={t("mail.sidebar.unreadCount", { count: folder.unreadCount })}
        />
      </div>
      {hasChildren && open
        ? node.children.map((child) => (
            <FolderRow
              key={child.folder.id}
              node={child}
              depth={depth + 1}
              account={account}
              selectedId={selectedId}
              expanded={expanded}
              onToggle={onToggle}
              onSelect={onSelect}
              onContextMenu={onContextMenu}
              onDropMessages={onDropMessages}
              dragOverId={dragOverId}
              setDragOverId={setDragOverId}
            />
          ))
        : null}
    </>
  );
}

export function MailSidebar() {
  const tt = useT();
  const queryClient = useQueryClient();
  const accountsQuery = useAccountsQuery();
  const mailAccounts = useMailAccounts();
  const unifiedAccounts = useUnifiedAccounts();
  const folders = useAllFolders(mailAccounts);
  const unifiedCounts = useUnifiedCounts();
  const { apply } = useMessageActions();
  const invalidateMail = useInvalidateMail();

  const selection = useShellStore((state) => state.selection.mail);
  const setSelection = useShellStore((state) => state.setSelection);
  const setShowSettings = useShellStore((state) => state.setShowSettings);
  const setShowAddAccount = useShellStore((state) => state.setShowAddAccount);
  const closeInlineCompose = useShellStore((state) => state.closeInlineCompose);

  const [expandedAccounts, setExpandedAccounts] = useState<Set<string>>(new Set());
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(new Set());
  const [moreAccounts, setMoreAccounts] = useState<Set<string>>(new Set());
  const [unifiedOpen, setUnifiedOpen] = useState(true);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const [menu, setMenu] = useState<{
    folder: Folder;
    point: { x: number; y: number };
  } | null>(null);
  const [folderDialog, setFolderDialog] = useState<FolderDialogState | null>(null);
  const [dialogValue, setDialogValue] = useState("");
  const [dialogParent, setDialogParent] = useState<string | null>(null);
  const [dialogBusy, setDialogBusy] = useState(false);

  // Expand every account and the roots that have children once data arrives.
  useEffect(() => {
    setExpandedAccounts((previous) => {
      let changed = false;
      const next = new Set(previous);
      for (const account of mailAccounts) {
        if (!next.has(account.id)) {
          next.add(account.id);
          changed = true;
        }
      }
      return changed ? next : previous;
    });
  }, [mailAccounts]);

  useEffect(() => {
    setExpandedFolders((previous) => {
      let changed = false;
      const next = new Set(previous);
      for (const account of mailAccounts) {
        const tree = folders.trees.get(account.id) ?? [];
        for (const root of tree) {
          if (root.children.length > 0 && !next.has(root.folder.id)) {
            next.add(root.folder.id);
            changed = true;
          }
        }
      }
      return changed ? next : previous;
    });
  }, [folders, mailAccounts]);

  /**
   * Each account's rows split into the top rows and the ones folded behind
   * "More". Shared by the render pass and by the auto-expand effect below.
   */
  const accountSections = useMemo(() => {
    const result = new Map<string, AccountFolderSections & { tree: FolderNode[] }>();
    for (const account of mailAccounts) {
      const tree = folders.trees.get(account.id) ?? [];
      // The outbox only appears once something is queued for sending.
      const visibleTree = tree.filter(
        (node) =>
          !(
            node.folder.role === "outbox" &&
            node.folder.totalCount === 0 &&
            node.children.length === 0
          ),
      );
      result.set(account.id, { tree, ...splitAccountFolders(visibleTree) });
    }
    return result;
  }, [folders, mailAccounts]);

  /**
   * Reveal "More" when the selected folder lives inside it, so deep links and
   * freshly created folders never end up selected-but-invisible. The effect
   * only fires for a selection it has not revealed yet: deriving the open
   * state from the selection instead would instantly force "More" back open
   * and make the row impossible to collapse while such a folder is selected.
   */
  const revealedFor = useRef<string | null>(null);
  useEffect(() => {
    const selectedId = selection.folderId;
    if (!selectedId || revealedFor.current === selectedId) return;
    const owner = mailAccounts.find((account) =>
      flattenFolderTree(accountSections.get(account.id)?.secondary ?? []).some(
        (entry) => entry.folder.id === selectedId,
      ),
    );
    if (!owner) {
      // The selection is on a top-level row. Forget it, so the same folder
      // moving into "More" later (e.g. its favourite flag was cleared) reveals
      // the section again instead of leaving the selected row hidden.
      revealedFor.current = null;
      return;
    }
    revealedFor.current = selectedId;
    setMoreAccounts((previous) => {
      if (previous.has(owner.id)) return previous;
      const next = new Set(previous);
      next.add(owner.id);
      return next;
    });
  }, [accountSections, mailAccounts, selection.folderId]);

  const unreadByAccount = useMemo(() => {
    const result = new Map<string, number>();
    for (const account of mailAccounts) {
      const list = folders.byAccount.get(account.id) ?? [];
      const inbox = list.find((folder) => folder.role === "inbox");
      result.set(account.id, inbox?.unreadCount ?? 0);
    }
    return result;
  }, [folders, mailAccounts]);

  const selectFolder = useCallback(
    (folder: Folder | null, unifiedRole: FolderRole | null) => {
      closeInlineCompose();
      if (folder) {
        setSelection("mail", { folderId: folder.id, messageId: null });
      } else if (unifiedRole) {
        setSelection("mail", { folderId: unifiedFolderId(unifiedRole), messageId: null });
      }
    },
    [closeInlineCompose, setSelection],
  );

  const toggleFolder = useCallback((id: string) => {
    setExpandedFolders((previous) => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const toggleAccount = useCallback((id: string) => {
    setExpandedAccounts((previous) => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const toggleMore = useCallback((id: string) => {
    setMoreAccounts((previous) => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const handleDropMessages = useCallback(
    async (folder: Folder, ids: string[]) => {
      try {
        await apply({ ids, action: { kind: "move", folderId: folder.id } });
        messageActionToast({ kind: "move", folderId: folder.id }, ids.length);
        void queryClient.invalidateQueries({ queryKey: queryKeys.folders(folder.accountId) });
      } catch {
        // The mutation already surfaced the error toast.
      }
    },
    [apply, queryClient],
  );

  const openCreateDialog = useCallback((accountId: string, parentId: string | null) => {
    setDialogValue("");
    setFolderDialog({ kind: "create", accountId, parentId });
  }, []);

  const openRenameDialog = useCallback((folder: Folder) => {
    setDialogValue(folder.name);
    setFolderDialog({ kind: "rename", accountId: folder.accountId, folder });
  }, []);

  const openMoveDialog = useCallback((folder: Folder) => {
    setDialogParent(folder.parentId);
    setFolderDialog({ kind: "move", accountId: folder.accountId, folder });
  }, []);

  const submitFolderDialog = useCallback(async () => {
    if (!folderDialog) return;
    setDialogBusy(true);
    try {
      switch (folderDialog.kind) {
        case "create": {
          const name = dialogValue.trim();
          if (!name) return;
          await foldersApi.create(folderDialog.accountId, folderDialog.parentId ?? null, name);
          toast.success(tt("mail.sidebar.folderCreated", { name }));
          break;
        }
        case "rename": {
          const name = dialogValue.trim();
          if (!name || !folderDialog.folder) return;
          await foldersApi.rename(folderDialog.folder.id, name);
          toast.success(tt("mail.sidebar.folderRenamed"));
          break;
        }
        case "move": {
          if (!folderDialog.folder) return;
          await foldersApi.move(folderDialog.folder.id, dialogParent);
          toast.success(tt("mail.sidebar.folderMoved"));
          break;
        }
        default:
          break;
      }
      invalidateMail();
      setFolderDialog(null);
    } catch (error) {
      toast.error(tt("mail.toast.actionFailed"), errorText(error));
    } finally {
      setDialogBusy(false);
    }
  }, [dialogParent, dialogValue, folderDialog, invalidateMail, tt]);

  const handleDeleteFolder = useCallback(
    async (folder: Folder) => {
      const ok = await confirm({
        title: tt("mail.sidebar.deleteFolderTitle"),
        message: tt("mail.sidebar.deleteFolderMessage", { name: roleLabel(folder) }),
        confirmLabel: tt("common.delete"),
        danger: true,
      });
      if (!ok) return;
      try {
        await foldersApi.delete(folder.id);
        if (selection.folderId === folder.id) {
          setSelection("mail", { folderId: null, messageId: null });
        }
        toast.success(tt("mail.sidebar.folderDeleted"));
        invalidateMail();
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [invalidateMail, selection.folderId, setSelection, tt],
  );

  const handleEmptyFolder = useCallback(
    async (folder: Folder) => {
      const ok = await confirm({
        title: tt("mail.sidebar.emptyFolderTitle"),
        message: tt("mail.sidebar.emptyFolderMessage", { name: roleLabel(folder) }),
        confirmLabel: tt("mail.sidebar.emptyFolderConfirm"),
        danger: true,
      });
      if (!ok) return;
      try {
        await foldersApi.empty(folder.id);
        toast.success(tt("mail.sidebar.folderEmptied"));
        invalidateMail();
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [invalidateMail, tt],
  );

  const handleMarkAllRead = useCallback(
    async (folder: Folder) => {
      try {
        await foldersApi.markAllRead(folder.id);
        toast.success(tt("mail.sidebar.markedAllRead", { name: roleLabel(folder) }));
        invalidateMail();
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [invalidateMail, tt],
  );

  const handleSyncFolder = useCallback(
    async (folder: Folder) => {
      try {
        await foldersApi.sync(folder.id);
        toast.success(tt("mail.sidebar.syncStarted", { name: roleLabel(folder) }));
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [tt],
  );

  const handleToggleFavorite = useCallback(
    async (folder: Folder) => {
      try {
        await foldersApi.setFavorite(folder.id, !folder.isFavorite);
        invalidateMail();
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [invalidateMail, tt],
  );

  const handleToggleSync = useCallback(
    async (folder: Folder) => {
      try {
        await foldersApi.setSyncEnabled(folder.id, !folder.syncEnabled);
        toast.success(
          folder.syncEnabled
            ? tt("mail.sidebar.syncDisabled", { name: roleLabel(folder) })
            : tt("mail.sidebar.syncEnabled", { name: roleLabel(folder) }),
        );
        invalidateMail();
      } catch (error) {
        toast.error(tt("mail.toast.actionFailed"), errorText(error));
      }
    },
    [invalidateMail, tt],
  );

  const folderMenuItems = useMemo<MenuEntry[]>(() => {
    const folder = menu?.folder;
    if (!folder) return [];
    const isSystemRole =
      folder.role !== "other" || ["inbox", "sent", "drafts", "trash", "junk", "archive", "outbox"].includes(folder.role);
    const items: MenuEntry[] = [
      {
        id: "new-subfolder",
        label: tt("mail.sidebar.newSubfolder"),
        icon: <FolderAdd16Regular />,
        onSelect: () => openCreateDialog(folder.accountId, folder.id),
      },
      {
        id: "rename",
        label: tt("mail.sidebar.rename"),
        icon: <Rename16Regular />,
        onSelect: () => openRenameDialog(folder),
        disabled: folder.role !== "other" && folder.role !== "archive",
      },
      {
        id: "move",
        label: tt("mail.sidebar.move"),
        icon: <ArrowMove20Regular />,
        onSelect: () => openMoveDialog(folder),
        disabled: folder.parentId === null && folder.role !== "other",
      },
      {
        id: "delete",
        label: tt("mail.sidebar.delete"),
        icon: <Delete16Regular />,
        danger: true,
        onSelect: () => void handleDeleteFolder(folder),
        disabled: isSystemRole,
      },
      { type: "separator" },
      {
        id: "favorite",
        label: folder.isFavorite ? tt("mail.sidebar.removeFavorite") : tt("mail.sidebar.addFavorite"),
        icon: <Star16Regular />,
        checked: folder.isFavorite,
        onSelect: () => void handleToggleFavorite(folder),
      },
      {
        id: "mark-read",
        label: tt("mail.sidebar.markAllReadMenu"),
        icon: <MailUnread16Regular />,
        onSelect: () => void handleMarkAllRead(folder),
      },
      {
        id: "empty",
        label: tt("mail.sidebar.emptyFolder"),
        icon: <Delete16Regular />,
        danger: true,
        onSelect: () => void handleEmptyFolder(folder),
        disabled: folder.role !== "trash" && folder.role !== "junk",
      },
      { type: "separator" },
      {
        id: "sync",
        label: tt("mail.sidebar.syncFolder"),
        icon: <ArrowSync16Regular />,
        onSelect: () => void handleSyncFolder(folder),
        disabled: !folder.syncEnabled,
      },
      {
        id: "toggle-sync",
        label: folder.syncEnabled ? tt("mail.sidebar.disableSync") : tt("mail.sidebar.enableSync"),
        icon: folder.syncEnabled ? <EyeOff16Regular /> : <MailUnread16Regular />,
        onSelect: () => void handleToggleSync(folder),
      },
    ];
    return items;
  }, [
    handleDeleteFolder,
    handleEmptyFolder,
    handleMarkAllRead,
    handleSyncFolder,
    handleToggleFavorite,
    handleToggleSync,
    menu,
    openCreateDialog,
    openMoveDialog,
    openRenameDialog,
    tt,
  ]);

  const accountMenu = useCallback(
    (account: Account): MenuEntry[] => [
      {
        id: "sync",
        label: tt("mail.sidebar.syncAccount"),
        icon: <ArrowSync16Regular />,
        onSelect: () => {
          syncApi.now(account.id, "mail").catch((error: unknown) => {
            toast.error(tt("mail.toast.actionFailed"), errorText(error));
          });
        },
      },
      {
        id: "new-folder",
        label: tt("mail.sidebar.newFolder"),
        icon: <FolderAdd16Regular />,
        onSelect: () => openCreateDialog(account.id, null),
      },
      {
        id: "settings",
        label: tt("mail.sidebar.accountSettings"),
        icon: <Settings16Regular />,
        onSelect: () => setShowSettings(true),
      },
      ...(account.state === "needsReauth"
        ? [
            {
              id: "repair",
              label: tt("mail.sidebar.repairAccount"),
              icon: <Warning16Regular />,
              onSelect: () => setShowSettings(true),
            },
          ]
        : []),
    ],
    [openCreateDialog, setShowSettings, tt],
  );

  if (accountsQuery.isLoading) {
    return (
      <div className={styles.sidebarLoading}>
        <Spinner size="small" />
      </div>
    );
  }

  if (mailAccounts.length === 0) {
    return (
      <div className={styles.sidebarEmpty}>
        <EmptyState
          compact
          icon={<Mail24Regular />}
          title={tt("mail.sidebar.noAccounts")}
          description={tt("mail.sidebar.noAccountsHint")}
          actions={
            <Button appearance="primary" size="small" onClick={() => setShowAddAccount(true)}>
              {tt("mail.sidebar.addAccount")}
            </Button>
          }
        />
      </div>
    );
  }

  const currentFolderId = selection.folderId;
  const currentUnifiedRole = parseUnifiedFolderId(currentFolderId);

  const unifiedEntries: Array<{ role: FolderRole; label: string; count: number }> = [
    { role: "inbox", label: tt("mail.folder.inbox"), count: unifiedCounts.data?.inbox ?? 0 },
    { role: "flagged", label: tt("mail.folder.flagged"), count: unifiedCounts.data?.flagged ?? 0 },
    { role: "drafts", label: tt("mail.folder.drafts"), count: unifiedCounts.data?.drafts ?? 0 },
  ];
  const outboxCount = unifiedCounts.data?.outbox ?? 0;
  if (outboxCount > 0) {
    unifiedEntries.push({ role: "outbox", label: tt("mail.folder.outbox"), count: outboxCount });
  }

  return (
    <div className={styles.sidebar}>
      {unifiedAccounts.length >= 2 ? (
        <div className={styles.sidebarSection}>
          <div
            className={[
              styles.unifiedRow,
              currentUnifiedRole === "inbox" ? styles.folderRowSelected : "",
            ]
              .filter(Boolean)
              .join(" ")}
            role="treeitem"
            aria-selected={currentUnifiedRole === "inbox"}
          >
            <button
              type="button"
              className={styles.folderChevron}
              aria-label={unifiedOpen ? tt("common.collapse") : tt("common.expand")}
              tabIndex={-1}
              onClick={() => setUnifiedOpen((open) => !open)}
            >
              {unifiedOpen ? <ChevronDown16Regular /> : <ChevronRight16Regular />}
            </button>
            <span className={styles.folderIcon} aria-hidden="true">
              <MailMultiple24Regular style={{ fontSize: 16 }} />
            </span>
            <button
              type="button"
              className={styles.unifiedName}
              onClick={() => selectFolder(null, "inbox")}
            >
              {tt("mail.sidebar.unified")}
            </button>
            <CountBadge
              count={unifiedCounts.data?.inbox ?? 0}
              label={tt("mail.sidebar.unreadCount", {
                count: unifiedCounts.data?.inbox ?? 0,
              })}
            />
          </div>
          {unifiedOpen
            ? unifiedEntries.map((entry) => {
                const id = unifiedFolderId(entry.role);
                const selected = currentFolderId === id;
                return (
                  <div
                    key={entry.role}
                    className={[
                      styles.folderRow,
                      selected ? styles.folderRowSelected : "",
                    ]
                      .filter(Boolean)
                      .join(" ")}
                    style={{ paddingLeft: 22 }}
                    role="treeitem"
                    aria-selected={selected}
                    tabIndex={0}
                    onClick={() => selectFolder(null, entry.role)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        selectFolder(null, entry.role);
                      }
                    }}
                  >
                    <span className={styles.folderChevronSpacer} />
                    <span className={styles.folderIcon} aria-hidden="true">
                      {entry.role === "inbox" ? (
                        <MailInbox16Regular />
                      ) : entry.role === "flagged" ? (
                        <Star16Regular />
                      ) : entry.role === "drafts" ? (
                        <DocumentEdit24Regular style={{ fontSize: 16 }} />
                      ) : (
                        <ArrowSync16Regular />
                      )}
                    </span>
                    <span className={styles.folderName}>{entry.label}</span>
                    <CountBadge
                      count={entry.count}
                      label={tt("mail.sidebar.unreadCount", { count: entry.count })}
                    />
                  </div>
                );
              })
            : null}
        </div>
      ) : null}

      {mailAccounts.map((account) => {
        const sections = accountSections.get(account.id);
        const tree = sections?.tree ?? [];
        const primary = sections?.primary ?? [];
        const secondary = sections?.secondary ?? [];
        const secondaryUnread = aggregateUnread(secondary);
        const accountOpen = expandedAccounts.has(account.id);
        const moreOpen = moreAccounts.has(account.id);
        const accountUnread = unreadByAccount.get(account.id) ?? 0;
        return (
          <div key={account.id} className={styles.sidebarSection}>
            <div className={styles.accountRow}>
              <button
                type="button"
                className={styles.folderChevron}
                aria-label={accountOpen ? tt("common.collapse") : tt("common.expand")}
                tabIndex={-1}
                onClick={() => toggleAccount(account.id)}
              >
                {accountOpen ? <ChevronDown16Regular /> : <ChevronRight16Regular />}
              </button>
              <DotBadge color={account.color} size={8} label={account.displayName} />
              <button
                type="button"
                className={styles.accountName}
                onClick={() => {
                  const inbox = (folders.byAccount.get(account.id) ?? []).find(
                    (folder) => folder.role === "inbox",
                  );
                  if (inbox) selectFolder(inbox, null);
                }}
                title={account.email}
              >
                {account.displayName}
              </button>
              {account.state === "syncing" ? <Spinner size="tiny" /> : null}
              {account.state === "needsReauth" ? (
                <Tooltip content={tt("mail.sidebar.reauthHint")} placement="top">
                  <span className={styles.accountWarning} aria-label={tt("mail.sidebar.reauthHint")}>
                    <Warning16Regular />
                  </span>
                </Tooltip>
              ) : null}
              {account.state === "error" ? (
                <Tooltip content={account.stateMessage ?? tt("mail.sidebar.stateError")} placement="top">
                  <span className={styles.accountError} aria-label={tt("mail.sidebar.stateError")}>
                    <ErrorCircle16Regular />
                  </span>
                </Tooltip>
              ) : null}
              {account.state === "offline" ? (
                <Tooltip content={tt("mail.sidebar.stateOffline")} placement="top">
                  <span className={styles.accountMuted} aria-label={tt("mail.sidebar.stateOffline")}>
                    <WifiOff20Regular />
                  </span>
                </Tooltip>
              ) : null}
              <CountBadge
                count={accountUnread}
                label={tt("mail.sidebar.unreadCount", { count: accountUnread })}
              />
              <MenuButton
                label={tt("mail.sidebar.accountMenu")}
                items={accountMenu(account)}
                appearance="subtle"
                size="small"
                tooltip={false}
                icon={<MoreHorizontal16Regular />}
              />
            </div>
            {account.state === "needsReauth" ? (
              <div className={styles.reauthBar}>
                <Warning16Regular aria-hidden="true" />
                <span>{tt("mail.sidebar.reauthMessage")}</span>
                <Button size="small" appearance="subtle" onClick={() => setShowSettings(true)}>
                  {tt("mail.sidebar.repairAccount")}
                </Button>
              </div>
            ) : null}
            {accountOpen ? (
              <div role="tree" aria-label={account.displayName}>
                {primary.map((node) => (
                  <FolderRow
                    key={node.folder.id}
                    node={node}
                    depth={0}
                    account={account}
                    selectedId={currentFolderId}
                    expanded={expandedFolders}
                    onToggle={toggleFolder}
                    onSelect={(folder) => selectFolder(folder, null)}
                    onContextMenu={(folder, point) => setMenu({ folder, point })}
                    onDropMessages={(folder, ids) => void handleDropMessages(folder, ids)}
                    dragOverId={dragOverId}
                    setDragOverId={setDragOverId}
                  />
                ))}
                {secondary.length > 0 ? (
                  <div
                    className={styles.folderRow}
                    style={{ paddingLeft: 6 }}
                    role="treeitem"
                    aria-expanded={moreOpen}
                    aria-label={
                      secondaryUnread > 0
                        ? `${tt("mail.sidebar.moreFolders")}, ${tt("mail.sidebar.unreadCount", { count: secondaryUnread })}`
                        : tt("mail.sidebar.moreFolders")
                    }
                    title={tt("mail.sidebar.moreFolders")}
                    tabIndex={0}
                    onClick={() => toggleMore(account.id)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        toggleMore(account.id);
                      }
                    }}
                  >
                    <span className={styles.folderChevronSpacer} />
                    <span className={styles.folderIcon} aria-hidden="true">
                      <MoreHorizontal16Regular />
                    </span>
                    <span className={styles.folderName}>
                      {tt("mail.sidebar.moreFolders")}
                    </span>
                    <CountBadge
                      count={secondaryUnread}
                      label={tt("mail.sidebar.unreadCount", { count: secondaryUnread })}
                    />
                  </div>
                ) : null}
                {moreOpen
                  ? secondary.map((node) => (
                      <FolderRow
                        key={node.folder.id}
                        node={node}
                        depth={0}
                        account={account}
                        selectedId={currentFolderId}
                        expanded={expandedFolders}
                        onToggle={toggleFolder}
                        onSelect={(folder) => selectFolder(folder, null)}
                        onContextMenu={(folder, point) => setMenu({ folder, point })}
                        onDropMessages={(folder, ids) => void handleDropMessages(folder, ids)}
                        dragOverId={dragOverId}
                        setDragOverId={setDragOverId}
                      />
                    ))
                  : null}
                {folders.isLoading && tree.length === 0 ? (
                  <div className={styles.folderLoading}>
                    <Spinner size="tiny" /> {tt("common.loading")}
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>
        );
      })}

      <div className={styles.sidebarFooter}>
        <Button
          appearance="subtle"
          size="small"
          fullWidth
          icon={<Add16Regular />}
          onClick={() => setShowAddAccount(true)}
        >
          {tt("mail.sidebar.addAccount")}
        </Button>
        <div className={styles.sidebarHint}>{tt("mail.sidebar.dragHint")}</div>
      </div>

      <Menu
        open={menu !== null}
        onClose={() => setMenu(null)}
        items={folderMenuItems}
        point={menu?.point ?? null}
        ariaLabel={tt("mail.sidebar.folderMenu")}
      />

      <Dialog
        open={folderDialog !== null}
        onClose={() => {
          if (!dialogBusy) setFolderDialog(null);
        }}
        title={
          folderDialog?.kind === "create"
            ? tt("mail.sidebar.createFolderTitle")
            : folderDialog?.kind === "rename"
              ? tt("mail.sidebar.renameFolderTitle")
              : tt("mail.sidebar.moveFolderTitle")
        }
        footer={
          <>
            <Button appearance="subtle" onClick={() => setFolderDialog(null)} disabled={dialogBusy}>
              {tt("common.cancel")}
            </Button>
            <Button appearance="primary" onClick={() => void submitFolderDialog()} disabled={dialogBusy}>
              {folderDialog?.kind === "rename"
                ? tt("common.save")
                : folderDialog?.kind === "move"
                  ? tt("mail.sidebar.moveFolderConfirm")
                  : tt("mail.sidebar.createFolderConfirm")}
            </Button>
          </>
        }
      >
        {folderDialog?.kind === "move" ? (
          <Select
            label={tt("mail.sidebar.moveFolderLabel")}
            value={dialogParent ?? ""}
            onChange={(value) => setDialogParent(value || null)}
            options={[
              { value: "", label: tt("mail.sidebar.moveFolderRoot") },
              ...(folderDialog.folder
                ? flattenFolderTree(folders.trees.get(folderDialog.accountId) ?? [])
                    .filter(
                      (entry) =>
                        folderDialog.folder !== undefined &&
                        !isSameOrDescendant(
                          folders.byAccount.get(folderDialog.accountId) ?? [],
                          folderDialog.folder.id,
                          entry.folder.id,
                        ),
                    )
                    .map((entry) => ({
                      value: entry.folder.id,
                      label: `${" ".repeat(entry.depth * 2)}${roleLabel(entry.folder)}`,
                    }))
                : []),
            ]}
          />
        ) : (
          <Input
            autoFocus
            label={tt("mail.sidebar.folderName")}
            value={dialogValue}
            onChange={(event) => setDialogValue(event.target.value)}
            maxLength={120}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void submitFolderDialog();
              }
            }}
          />
        )}
      </Dialog>
    </div>
  );
}
