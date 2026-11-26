/**
 * Middle pane of the People module: search, virtualized alphabetical list
 * with A–Z/# section headers, Ctrl/⌘ and Shift multi-selection, and the bulk
 * action bar. The whole (filtered) contact set is loaded through
 * `contact_list` before sorting, because sections are built client-side from
 * the pinyin-aware sort key rather than the backend's `display_name` order.
 */
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  Add16Regular,
  ArrowSync16Regular,
  ArrowUpload16Regular,
  Delete16Regular,
  People20Regular,
  SelectAllOn16Regular,
  Star16Filled,
} from "@fluentui/react-icons";
import {
  Avatar,
  BulkBar,
  Button,
  Checkbox,
  DangerButton,
  EmptyState,
  IconButton,
  Menu,
  ModuleSyncStatus,
  SearchBox,
  Spinner,
  ToggleButton,
  Tooltip,
  VirtualList,
  confirm,
  toast,
  type MenuEntry,
} from "@/components";
import { sync as syncApi } from "@/ipc/api";
import type { ContactSummary } from "@/ipc/types";
import { formatShortcut } from "@/lib/hotkeys";
import { useT } from "@/lib/i18n";
import { useAppSettings } from "@/lib/useSettings";
import { useShellStore } from "@/lib/store";
import {
  CONTACTS_PAGE_SIZE,
  buildPeopleRows,
  formatSummaryName,
  isReadOnlyBook,
  partitionWritable,
  type ContactListOptions,
  type PeopleRow,
} from "./logic";
import { useAddressBooksQuery, useAllContactsQuery, useAllGroupsQuery } from "./queries";
import {
  addContactsToGroup,
  deleteContacts,
  exportContactIds,
  openNewContact,
  peopleErrorText,
} from "./actions";
import { usePeopleUiStore } from "./uiStore";
import styles from "./People.module.css";

const EMPTY_CONTACTS: ContactSummary[] = [];

function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setDebounced(value), delay);
    return () => window.clearTimeout(timer);
  }, [value, delay]);
  return debounced;
}

function scopeTitle(
  t: (key: string, vars?: Record<string, string | number>) => string,
  options: {
    favoritesOnly: boolean;
    addressBookId: string | null;
    groupId: string | null;
  },
  bookName: string | null,
  groupName: string | null,
): string {
  if (options.favoritesOnly) return t("people.sidebar.favorites");
  if (options.groupId) return groupName ?? t("people.sidebar.all");
  if (options.addressBookId) return bookName ?? t("people.sidebar.all");
  return t("people.sidebar.all");
}

export function ContactList() {
  const t = useT();
  const settings = useAppSettings();
  const selection = useShellStore((state) => state.selection.people);
  const setSelection = useShellStore((state) => state.setSelection);
  const favoritesOnly = usePeopleUiStore((state) => state.favoritesOnly);
  const setFavoritesOnly = usePeopleUiStore((state) => state.setFavoritesOnly);
  const search = usePeopleUiStore((state) => state.search);
  const setSearch = usePeopleUiStore((state) => state.setSearch);
  const selectedIds = usePeopleUiStore((state) => state.selectedIds);
  const setSelectedIds = usePeopleUiStore((state) => state.setSelectedIds);
  const setSelectionMode = usePeopleUiStore((state) => state.setSelectionMode);
  const selectionMode = usePeopleUiStore((state) => state.selectionMode);
  const toggleSelected = usePeopleUiStore((state) => state.toggleSelected);
  const setAnchor = usePeopleUiStore((state) => state.setAnchor);
  const clearSelection = usePeopleUiStore((state) => state.clearSelection);
  const openEdit = usePeopleUiStore((state) => state.openEdit);
  const focusSearchRequest = usePeopleUiStore((state) => state.focusSearchRequest);
  const selectAllRequest = usePeopleUiStore((state) => state.selectAllRequest);
  const deleteRequest = usePeopleUiStore((state) => state.deleteRequest);

  const booksQuery = useAddressBooksQuery();
  const groupsQuery = useAllGroupsQuery();
  const books = booksQuery.data ?? [];
  const groups = groupsQuery.data ?? [];

  const debouncedSearch = useDebouncedValue(search, 180);
  const trimmedSearch = debouncedSearch.trim();

  const effectiveLocale =
    settings.language === "system"
      ? (typeof navigator !== "undefined" && navigator.language) || "zh-CN"
      : settings.language;

  const listOptions: ContactListOptions = useMemo(
    () => ({ locale: effectiveLocale }),
    [effectiveLocale],
  );

  const listQuery = useMemo(
    () => ({
      addressBookId: favoritesOnly ? null : selection.addressBookId,
      groupId: favoritesOnly ? null : selection.groupId,
      query: trimmedSearch || null,
      favoritesOnly,
      cursor: null,
      limit: CONTACTS_PAGE_SIZE,
    }),
    [
      favoritesOnly,
      selection.addressBookId,
      selection.groupId,
      trimmedSearch,
    ],
  );

  const query = useAllContactsQuery(listQuery);
  const contacts: ContactSummary[] = query.data?.contacts ?? EMPTY_CONTACTS;

  const rows: PeopleRow[] = useMemo(
    () => buildPeopleRows(contacts, listOptions),
    [contacts, listOptions],
  );

  const total = query.data?.total ?? contacts.length;

  const searchRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    if (focusSearchRequest > 0) searchRef.current?.focus();
  }, [focusSearchRequest]);

  const visibleIds = useMemo(
    () =>
      rows
        .filter((row): row is Extract<PeopleRow, { kind: "contact" }> => row.kind === "contact")
        .map((row) => row.contact.id),
    [rows],
  );
  const allVisibleSelected =
    visibleIds.length > 0 && visibleIds.every((id) => selectedIds.includes(id));

  const selectAllVisible = useCallback(() => {
    if (visibleIds.length === 0) return;
    setSelectedIds(visibleIds);
    setSelectionMode("multi");
    setAnchor(visibleIds[0] ?? null);
  }, [setAnchor, setSelectedIds, setSelectionMode, visibleIds]);

  // `Ctrl/⌘+A` asks the store to select everything currently listed; the
  // request counter keeps it one-shot so a later refetch cannot replay it.
  const handledSelectAll = useRef(0);
  useEffect(() => {
    if (selectAllRequest === 0 || handledSelectAll.current === selectAllRequest) return;
    handledSelectAll.current = selectAllRequest;
    selectAllVisible();
  }, [selectAllRequest, selectAllVisible]);

  // Changing the scope or filter starts a fresh selection.
  useEffect(() => {
    clearSelection();
  }, [selection.addressBookId, selection.groupId, favoritesOnly, trimmedSearch, clearSelection]);

  const rangeSelect = useCallback(
    (targetId: string) => {
      const order = rows
        .filter((row): row is Extract<PeopleRow, { kind: "contact" }> => row.kind === "contact")
        .map((row) => row.contact.id);
      const anchorId = usePeopleUiStore.getState().anchorId;
      const anchorIndex = anchorId ? order.indexOf(anchorId) : -1;
      const targetIndex = order.indexOf(targetId);
      if (anchorIndex === -1 || targetIndex === -1) {
        setSelectedIds([targetId]);
        setAnchor(targetId);
        return;
      }
      const [from, to] =
        anchorIndex <= targetIndex ? [anchorIndex, targetIndex] : [targetIndex, anchorIndex];
      setSelectedIds(order.slice(from, to + 1));
    },
    [rows, setAnchor, setSelectedIds],
  );

  const handleRowClick = useCallback(
    (event: ReactMouseEvent, contact: ContactSummary) => {
      if (event.shiftKey) {
        rangeSelect(contact.id);
        setSelectionMode("multi");
        setSelection("people", { contactId: contact.id });
        return;
      }
      if (event.metaKey || event.ctrlKey) {
        toggleSelected(contact.id);
        setAnchor(contact.id);
        setSelection("people", { contactId: contact.id });
        return;
      }
      setSelectedIds([contact.id]);
      setSelectionMode("single");
      setAnchor(contact.id);
      setSelection("people", { contactId: contact.id });
    },
    [rangeSelect, setAnchor, setSelectedIds, setSelectionMode, setSelection, toggleSelected],
  );

  const handleCheckbox = useCallback(
    (contact: ContactSummary) => {
      toggleSelected(contact.id);
      setAnchor(contact.id);
      setSelection("people", { contactId: contact.id });
    },
    [setAnchor, setSelection, toggleSelected],
  );

  const [syncing, setSyncing] = useState(false);
  const syncNow = useCallback(async () => {
    setSyncing(true);
    try {
      // Contacts are shown across accounts, so the list header can only offer
      // the module-wide sync; a per-account scope would not match what is shown.
      await syncApi.now(null, "contacts");
      toast.success(t("people.list.synced"));
    } catch (error) {
      toast.error(t("people.list.sync"), peopleErrorText(error));
    } finally {
      setSyncing(false);
    }
  }, [t]);

  const confirmBulkDelete = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) return;
      // Contacts in read-only address books cannot be deleted; skip them and
      // tell the user instead of failing the whole operation.
      const { writable, readOnly } = partitionWritable(contacts, books, ids);
      if (writable.length === 0) {
        toast.info(t("people.bulk.readOnlySkipped", { count: readOnly.length }));
        return;
      }
      const approved = await confirm({
        title: t("people.bulk.deleteTitle"),
        message:
          writable.length === 1
            ? t("people.detail.deleteConfirmMessage", {
                name:
                  contacts.find((contact) => contact.id === writable[0])?.displayName ||
                  t("people.detail.noName"),
              })
            : t("people.bulk.deleteMessage", { count: writable.length }),
        confirmLabel: t("common.delete"),
        danger: true,
      });
      if (!approved) return;
      const deleted = await deleteContacts(writable);
      if (!deleted) return;
      if (readOnly.length > 0) {
        toast.info(t("people.bulk.readOnlySkipped", { count: readOnly.length }));
      }
      clearSelection();
    },
    [books, clearSelection, contacts, t],
  );

  useEffect(() => {
    if (!deleteRequest) return;
    // Consume before showing the dialog. The request is a command: once read
    // it is gone, so cancelling and remounting the list cannot replay it.
    const request = usePeopleUiStore.getState().consumeDeleteRequest();
    if (!request || request.ids.length === 0) return;
    void confirmBulkDelete(request.ids);
  }, [deleteRequest, confirmBulkDelete]);

  const clearFilters = useCallback(() => {
    setSearch("");
    setFavoritesOnly(false);
    if (selection.groupId) {
      setSelection("people", { groupId: null, contactId: null });
    }
  }, [selection.groupId, setFavoritesOnly, setSearch, setSelection]);

  const groupMenuItems = useMemo<MenuEntry[]>(() => {
    // Groups belong to one address book, so only offer the books the selected
    // contacts actually come from. Offering every writable book is what let a
    // cross-book membership slip through.
    const selected = new Set(selectedIds);
    const contactBook = new Map<string, string>();
    for (const contact of contacts) {
      if (selected.has(contact.id)) contactBook.set(contact.id, contact.addressBookId);
    }
    const selectedBookIds = new Set(contactBook.values());
    const writableBooks = books.filter(
      (book) => !book.isReadOnly && selectedBookIds.has(book.id),
    );
    const entries: MenuEntry[] = [];
    for (const book of writableBooks) {
      const bookGroups = groups.filter((group) => group.addressBookId === book.id);
      if (bookGroups.length === 0) continue;
      entries.push({ type: "header", id: `h:${book.id}`, label: book.name });
      for (const group of bookGroups) {
        entries.push({
          id: `g:${group.id}`,
          label: group.name,
          icon: <Add16Regular />,
          onSelect: () => {
            // Re-read the selection but keep the same book restriction: a
            // mixed selection only joins this book's contacts to this group.
            const ids = usePeopleUiStore
              .getState()
              .selectedIds.filter((id) => contactBook.get(id) === book.id);
            if (ids.length > 0) void addContactsToGroup(ids, group.id);
          },
        });
      }
    }
    if (entries.length === 0) {
      entries.push({
        id: "none",
        label: t("people.bulk.noGroups"),
        disabled: true,
      });
    }
    return entries;
  }, [books, contacts, groups, selectedIds, t]);

  const [groupMenu, setGroupMenu] = useState<{ open: boolean; anchor: HTMLElement | null }>({
    open: false,
    anchor: null,
  });

  const bookName =
    books.find((book) => book.id === selection.addressBookId)?.name ?? null;
  const groupName =
    groups.find((group) => group.id === selection.groupId)?.name ?? null;
  const title = scopeTitle(
    t,
    {
      favoritesOnly,
      addressBookId: selection.addressBookId,
      groupId: selection.groupId,
    },
    bookName,
    groupName,
  );

  // Search, favourites and a group are all filters over the address book; an
  // empty list under any of them means "no matches", not "no contacts yet".
  const filtered =
    Boolean(trimmedSearch) || favoritesOnly || Boolean(selection.groupId);
  const emptyState = (
    <div className={styles.listEmpty}>
      <EmptyState
        icon={<People20Regular />}
        title={
          filtered
            ? t("people.list.noMatchesTitle")
            : t("people.list.emptyTitle")
        }
        description={
          filtered
            ? trimmedSearch
              ? t("people.list.noMatchesDescription")
              : t("people.list.noMatchesFilteredDescription")
            : t("people.list.emptyDescription")
        }
        actions={
          filtered ? (
            <Button size="small" appearance="subtle" onClick={clearFilters}>
              {trimmedSearch ? t("common.clear") : t("people.list.clearFilters")}
            </Button>
          ) : (
            <Button size="small" appearance="primary" onClick={() => void openNewContact()}>
              {t("people.editor.titleCreate")}
            </Button>
          )
        }
      />
    </div>
  );

  const renderRow = (row: PeopleRow) => {
    if (row.kind === "header") {
      return (
        <div className={styles.sectionRow} role="presentation">
          <span className={styles.sectionLabel}>{row.label}</span>
          <span className={styles.sectionCount}>{row.count}</span>
        </div>
      );
    }
    const contact = row.contact;
    const selected = selectedIds.includes(contact.id);
    const isCurrent = selection.contactId === contact.id;
    const displayName =
      formatSummaryName(contact.displayName) || t("people.detail.noName");
    const meta = [contact.company, contact.primaryEmail].filter(Boolean).join(" · ");
    return (
      <div
        className={`${styles.contactRow} ${selected ? styles.contactRowSelected : ""} ${
          isCurrent ? styles.contactRowFocused : ""
        }`}
      >
        <span className={styles.rowCheckbox}>
          <Checkbox
            checked={selected}
            aria-label={t("people.list.toggleSelection", { name: displayName })}
            onChange={() => handleCheckbox(contact)}
          />
        </span>
        <button
          type="button"
          className={styles.rowButton}
          onClick={(event) => handleRowClick(event, contact)}
          onDoubleClick={() => {
            if (isReadOnlyBook(books, contact.addressBookId)) {
              toast.info(t("people.detail.readOnlyHint"));
              return;
            }
            openEdit(contact.id);
          }}
          aria-label={displayName}
          aria-pressed={selected}
        >
          <Avatar
            className={styles.rowAvatar}
            name={displayName}
            email={contact.primaryEmail}
            src={contact.photo}
            size={36}
          />
          <span className={styles.rowText}>
            <span className={styles.rowName}>{displayName}</span>
            <span className={styles.rowMeta}>
              {meta || t("people.list.noEmail")}
            </span>
          </span>
          {contact.isFavorite ? (
            <span className={styles.rowStar} aria-hidden="true">
              <Star16Filled />
            </span>
          ) : null}
        </button>
      </div>
    );
  };

  return (
    <div className={styles.listColumn}>
      <div className={styles.listHeader}>
        <div className={styles.listTitleRow}>
          <span className={styles.listTitle}>{title}</span>
          {/* Sync lives in the title row, above the bulk bar's own row, so an
              active selection never displaces or overlays it. */}
          <div className={styles.listTitleActions}>
            <span className={styles.listCount}>
              {query.isLoading || query.isError
                ? ""
                : t("people.list.total", { count: total })}
            </span>
            <ModuleSyncStatus module="contacts" />
            <IconButton
              icon={<ArrowSync16Regular />}
              label={t("people.list.sync")}
              size="small"
              onClick={() => void syncNow()}
              busy={syncing}
            />
          </div>
        </div>
        <SearchBox
          ref={searchRef}
          value={search}
          onChange={setSearch}
          placeholder={t("people.search.placeholder")}
          aria-label={t("people.search.placeholder")}
          shortcut={formatShortcut("Mod+E")}
        />
        {selectionMode === "multi" && selectedIds.length > 0 ? (
          <BulkBar
            ariaLabel={t("people.list.selectedCount", { count: selectedIds.length })}
            selectedCount={selectedIds.length}
            onClear={clearSelection}
            leading={
              <Tooltip
                content={
                  allVisibleSelected
                    ? t("people.list.selectNone")
                    : t("people.list.selectAll")
                }
              >
                <ToggleButton
                  pressed={allVisibleSelected}
                  onPressedChange={() =>
                    allVisibleSelected ? clearSelection() : selectAllVisible()
                  }
                  icon={<SelectAllOn16Regular />}
                  size="small"
                  aria-label={t("people.list.selectAll")}
                />
              </Tooltip>
            }
          >
            <IconButton
              icon={<Add16Regular />}
              label={t("people.bulk.addToGroup")}
              size="small"
              onClick={(event) => {
                setGroupMenu({ open: true, anchor: event.currentTarget });
              }}
            />
            <IconButton
              icon={<ArrowUpload16Regular />}
              label={t("people.bulk.export")}
              size="small"
              onClick={() => void exportContactIds(selectedIds, title || "contacts")}
            />
            <Tooltip content={t("people.bulk.delete")}>
              <DangerButton
                size="small"
                icon={<Delete16Regular />}
                aria-label={t("people.bulk.delete")}
                onClick={() => void confirmBulkDelete(selectedIds)}
              />
            </Tooltip>
          </BulkBar>
        ) : null}
      </div>

      <div className={styles.listBody}>
        {query.isLoading ? (
          <div className={styles.detailLoading}>
            <Spinner label={t("people.list.loading")} />
          </div>
        ) : query.isError ? (
          <div className={styles.listEmpty}>
            <EmptyState
              icon={<People20Regular />}
              title={t("people.detail.loadFailed")}
              description={peopleErrorText(query.error)}
              actions={
                <Button size="small" onClick={() => void query.refetch()}>
                  {t("common.retry")}
                </Button>
              }
            />
          </div>
        ) : rows.length === 0 ? (
          emptyState
        ) : (
          <VirtualList
            items={rows}
            className={styles.virtualList}
            estimateSize={58}
            getKey={(row) => row.key}
            renderItem={renderRow}
            ariaLabel={t("people.sidebar.all")}
          />
        )}
      </div>

      {/* The scope total is already shown next to the title; the footer keeps
          only the multi-select hint so the number is not repeated. */}
      <div className={styles.listFooter}>
        <span>{t("people.list.multiSelectHint")}</span>
      </div>

      <Menu
        open={groupMenu.open}
        onClose={() => setGroupMenu({ open: false, anchor: null })}
        anchor={groupMenu.anchor}
        items={groupMenuItems}
        ariaLabel={t("people.bulk.addToGroupMenu")}
      />
    </div>
  );
}

export default ContactList;
