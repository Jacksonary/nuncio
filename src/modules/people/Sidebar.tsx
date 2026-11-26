/**
 * People navigation: all contacts / favorites, one address-book tree per
 * account with nested groups, import/export and group management.
 */
import { useCallback, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  Add16Regular,
  BookContacts20Regular,
  ContactCard20Regular,
  Edit16Regular,
  Delete16Regular,
  People20Regular,
  PeopleTeam16Regular,
  Star20Filled,
  Star20Regular,
} from "@fluentui/react-icons";
import {
  Button,
  ContextMenu,
  EmptyState,
  Spinner,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import { contacts } from "@/ipc/api";
import type { Account, AddressBook, ContactGroup } from "@/ipc/types";
import { useT } from "@/lib/i18n";
import { queryKeyPrefixes } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import {
  defaultWritableBook,
  exportContactsByScope,
  peopleErrorText,
  writableAddressBooks,
} from "./actions";
import { GroupNameDialog, VcfImportDialog } from "./dialogs";
import { useAccountsQuery, useAddressBooksQuery, useAllGroupsQuery, usePeopleCounts } from "./queries";
import { usePeopleUiStore } from "./uiStore";
import styles from "./People.module.css";

type GroupDialogState =
  | { mode: "create"; addressBookId: string }
  | { mode: "rename"; group: ContactGroup }
  | null;

interface AccountSection {
  accountId: string;
  name: string;
  color: string | null;
  books: AddressBook[];
}

function accountSections(accounts: Account[], books: AddressBook[]): AccountSection[] {
  const sections: AccountSection[] = [];
  const seen = new Set<string>();
  for (const account of accounts) {
    const owned = books.filter((book) => book.accountId === account.id);
    if (owned.length === 0) continue;
    seen.add(account.id);
    sections.push({
      accountId: account.id,
      name: account.displayName || account.email,
      color: account.color,
      books: owned,
    });
  }
  const orphanBooks = books.filter((book) => !seen.has(book.accountId));
  if (orphanBooks.length > 0) {
    sections.push({
      accountId: "__other__",
      name: "",
      color: null,
      books: orphanBooks,
    });
  }
  return sections;
}

export function PeopleSidebar() {
  const t = useT();
  const client = useQueryClient();
  const booksQuery = useAddressBooksQuery();
  const accountsQuery = useAccountsQuery();
  const groupsQuery = useAllGroupsQuery();
  const selection = useShellStore((state) => state.selection.people);
  const setSelection = useShellStore((state) => state.setSelection);
  const favoritesOnly = usePeopleUiStore((state) => state.favoritesOnly);
  const setFavoritesOnly = usePeopleUiStore((state) => state.setFavoritesOnly);
  const clearSelection = usePeopleUiStore((state) => state.clearSelection);

  const books = useMemo(() => booksQuery.data ?? [], [booksQuery.data]);
  const accounts = accountsQuery.data ?? [];
  const groups = useMemo(() => groupsQuery.data ?? [], [groupsQuery.data]);
  const counts = usePeopleCounts(books);
  const sections = useMemo(() => accountSections(accounts, books), [accounts, books]);

  const [groupDialog, setGroupDialog] = useState<GroupDialogState>(null);
  const [importState, setImportState] = useState<{
    open: boolean;
    addressBookId: string | null;
  }>({ open: false, addressBookId: null });

  const allSelected = !favoritesOnly && !selection.addressBookId && !selection.groupId;
  const favoritesSelected = favoritesOnly;

  const selectScope = useCallback(
    (addressBookId: string | null, groupId: string | null, favorites: boolean) => {
      setFavoritesOnly(favorites);
      clearSelection();
      setSelection("people", {
        addressBookId,
        groupId,
        contactId: null,
      });
    },
    [clearSelection, setFavoritesOnly, setSelection],
  );

  const currentScopeName = useMemo(() => {
    if (favoritesOnly) return t("people.sidebar.favorites");
    if (selection.groupId) {
      const group = groups.find((candidate) => candidate.id === selection.groupId);
      if (group) return group.name;
    }
    if (selection.addressBookId) {
      const book = books.find((candidate) => candidate.id === selection.addressBookId);
      if (book) return book.name;
    }
    return "contacts";
  }, [books, favoritesOnly, groups, selection.addressBookId, selection.groupId, t]);

  const exportCurrent = useCallback(() => {
    void exportContactsByScope({
      addressBookId: favoritesOnly ? null : selection.addressBookId,
      groupId: favoritesOnly ? null : selection.groupId,
      favoritesOnly,
      suggestedName: currentScopeName,
    });
  }, [currentScopeName, favoritesOnly, selection.addressBookId, selection.groupId]);

  const openImport = useCallback(
    (addressBookId?: string | null) => {
      const writable = writableAddressBooks(books);
      const fallback = defaultWritableBook(books);
      const preferred =
        (addressBookId
          ? writable.find((book) => book.id === addressBookId)
          : undefined) ?? fallback;
      if (!preferred) {
        toast.error(t("people.import.noWritableBooks"));
        return;
      }
      setImportState({ open: true, addressBookId: preferred.id });
    },
    [books, t],
  );

  const deleteGroup = useCallback(
    async (group: ContactGroup) => {
      const approved = await confirm({
        title: t("people.group.deleteTitle"),
        message: t("people.group.deleteMessage", { name: group.name }),
        confirmLabel: t("common.delete"),
        danger: true,
      });
      if (!approved) return;
      try {
        await contacts.deleteGroup(group.id);
        void client.invalidateQueries({ queryKey: queryKeyPrefixes.groups });
        void client.invalidateQueries({ queryKey: queryKeyPrefixes.contacts });
        if (selection.groupId === group.id) {
          setSelection("people", { groupId: null, contactId: null });
        }
        toast.success(t("people.sidebar.groupDeleted", { name: group.name }));
      } catch (error) {
        toast.error(t("people.error.action"), peopleErrorText(error));
      }
    },
    [client, selection.groupId, setSelection, t],
  );

  const groupMenu = useCallback(
    (group: ContactGroup): MenuEntry[] => {
      const book = books.find((candidate) => candidate.id === group.addressBookId);
      const readOnly = book?.isReadOnly ?? false;
      return [
        {
          id: "rename",
          label: t("people.sidebar.renameGroup"),
          icon: <Edit16Regular />,
          disabled: readOnly,
          onSelect: () => setGroupDialog({ mode: "rename", group }),
        },
        { type: "separator" },
        {
          id: "delete",
          label: t("people.sidebar.deleteGroup"),
          icon: <Delete16Regular />,
          danger: true,
          disabled: readOnly,
          onSelect: () => void deleteGroup(group),
        },
      ];
    },
    [books, deleteGroup, t],
  );

  const bookMenu = useCallback(
    (book: AddressBook): MenuEntry[] => [
      {
        id: "new-group",
        label: t("people.sidebar.newGroup"),
        icon: <Add16Regular />,
        disabled: book.isReadOnly,
        onSelect: () => setGroupDialog({ mode: "create", addressBookId: book.id }),
      },
      { type: "separator" },
      {
        id: "import",
        label: t("people.sidebar.importIntoBook"),
        disabled: book.isReadOnly,
        onSelect: () => openImport(book.id),
      },
      {
        id: "export",
        label: t("people.sidebar.exportBook"),
        onSelect: () =>
          void exportContactsByScope({
            addressBookId: book.id,
            groupId: null,
            favoritesOnly: false,
            suggestedName: book.name,
          }),
      },
    ],
    [openImport, t],
  );

  const renderGroupRow = (group: ContactGroup) => {
    const selected = !favoritesOnly && selection.groupId === group.id;
    const readOnly =
      books.find((book) => book.id === group.addressBookId)?.isReadOnly ?? false;
    return (
      <ContextMenu key={group.id} items={groupMenu(group)}>
        <button
          type="button"
          className={`${styles.navRow} ${styles.groupRow} ${selected ? styles.navRowSelected : ""}`}
          aria-current={selected ? "true" : undefined}
          onClick={() => selectScope(group.addressBookId, group.id, false)}
          onDoubleClick={() => {
            if (!readOnly) setGroupDialog({ mode: "rename", group });
          }}
        >
          <span className={styles.navIcon} aria-hidden="true">
            <PeopleTeam16Regular />
          </span>
          <span className={styles.navLabel}>{group.name}</span>
          <span className={styles.navCount}>{group.memberCount}</span>
        </button>
      </ContextMenu>
    );
  };

  const renderBook = (book: AddressBook) => {
    const selected =
      !favoritesOnly && selection.addressBookId === book.id && !selection.groupId;
    const bookGroups = groups.filter((group) => group.addressBookId === book.id);
    const count = counts.byBook[book.id];
    return (
      <div key={book.id} className={styles.bookGroup}>
        <ContextMenu items={bookMenu(book)}>
          <button
            type="button"
            className={`${styles.navRow} ${styles.bookRow} ${selected ? styles.navRowSelected : ""}`}
            aria-current={selected ? "true" : undefined}
            title={book.isReadOnly ? t("people.sidebar.readOnlyBook") : undefined}
            onClick={() => selectScope(book.id, null, false)}
          >
            <span className={styles.navIcon} aria-hidden="true">
              <BookContacts20Regular />
            </span>
            <span className={styles.navLabel}>{book.name}</span>
            {count !== null ? <span className={styles.navCount}>{count}</span> : null}
          </button>
        </ContextMenu>
        {bookGroups.length === 0 ? (
          <div className={styles.groupEmpty}>{t("people.sidebar.noGroups")}</div>
        ) : (
          bookGroups.map(renderGroupRow)
        )}
        {!book.isReadOnly ? (
          <button
            type="button"
            className={`${styles.navRow} ${styles.newGroupRow}`}
            onClick={() => setGroupDialog({ mode: "create", addressBookId: book.id })}
          >
            <span className={styles.navIcon} aria-hidden="true">
              <Add16Regular />
            </span>
            <span className={styles.navLabel}>{t("people.sidebar.newGroup")}</span>
          </button>
        ) : null}
      </div>
    );
  };

  return (
    <div className={styles.sidebar}>
      <div className={styles.sidebarScroll}>
        <button
          type="button"
          className={`${styles.navRow} ${allSelected ? styles.navRowSelected : ""}`}
          aria-current={allSelected ? "true" : undefined}
          onClick={() => selectScope(null, null, false)}
        >
          <span className={styles.navIcon} aria-hidden="true">
            <People20Regular />
          </span>
          <span className={styles.navLabel}>{t("people.sidebar.all")}</span>
          {counts.all !== null ? <span className={styles.navCount}>{counts.all}</span> : null}
        </button>

        <button
          type="button"
          className={`${styles.navRow} ${favoritesSelected ? styles.navRowSelected : ""}`}
          aria-current={favoritesSelected ? "true" : undefined}
          onClick={() => selectScope(null, null, true)}
        >
          <span className={styles.navIcon} aria-hidden="true">
            {favoritesSelected ? <Star20Filled /> : <Star20Regular />}
          </span>
          <span className={styles.navLabel}>{t("people.sidebar.favorites")}</span>
          {counts.favorites !== null ? (
            <span className={styles.navCount}>{counts.favorites}</span>
          ) : null}
        </button>

        {booksQuery.isLoading ? (
          <div className={styles.sidebarLoading}>
            <Spinner size="small" label={t("people.sidebar.loadingBooks")} />
          </div>
        ) : books.length === 0 ? (
          <div className={styles.sidebarEmpty}>
            <EmptyState
              compact
              icon={<ContactCard20Regular />}
              title={t("people.sidebar.noAddressBooks")}
            />
          </div>
        ) : (
          sections.map((section) => (
            <div key={section.accountId}>
              <div className={styles.sectionHeader}>
                {section.color ? (
                  <span
                    className={styles.accountDot}
                    style={{ background: section.color }}
                    aria-hidden="true"
                  />
                ) : null}
                <span>
                  {section.accountId === "__other__"
                    ? t("people.sidebar.unknownAccount")
                    : section.name}
                </span>
              </div>
              {section.books.map(renderBook)}
            </div>
          ))
        )}
      </div>

      <div className={styles.sidebarFooter}>
        <Button
          size="small"
          appearance="subtle"
          onClick={() => openImport(selection.addressBookId)}
          disabled={writableAddressBooks(books).length === 0}
          title={t("people.sidebar.import")}
        >
          {t("people.sidebar.importAction")}
        </Button>
        <Button
          size="small"
          appearance="subtle"
          onClick={exportCurrent}
          title={t("people.sidebar.exportCurrent")}
          disabled={books.length === 0}
        >
          {t("people.sidebar.exportAction")}
        </Button>
      </div>

      <GroupNameDialog
        open={groupDialog !== null}
        mode={groupDialog?.mode ?? "create"}
        addressBookId={
          groupDialog?.mode === "create" ? groupDialog.addressBookId : null
        }
        group={groupDialog?.mode === "rename" ? groupDialog.group : null}
        onClose={() => setGroupDialog(null)}
      />

      <VcfImportDialog
        open={importState.open}
        initialAddressBookId={importState.addressBookId}
        onClose={() => setImportState({ open: false, addressBookId: null })}
      />
    </div>
  );
}

export default PeopleSidebar;
