/**
 * Imperative People actions shared by the toolbar, list, detail pane and the
 * module-level "new contact" entry point. All backend access goes through
 * `src/ipc/api.ts`; nothing here renders.
 */
import { isTauri } from "@tauri-apps/api/core";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { confirm, toast } from "@/components";
import { accounts as accountsApi, calendars as calendarsApi, compose, contacts, events, isIpcError } from "@/ipc/api";
import type {
  Account,
  AddressBook,
  AppSettings,
  Calendar,
  Contact,
  ContactListQuery,
  Draft,
  EventInput,
} from "@/ipc/types";
import { queryClient } from "@/lib/queryClient";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { getLocale, t } from "@/lib/i18n";
import { toIsoDate } from "@/lib/format";
import { useShellStore } from "@/lib/store";
import { DEFAULT_SETTINGS } from "@/lib/useSettings";
import { useComposeStore } from "@/modules/mail/compose";
import { formatContactName, primaryEmail } from "./logic";
import { usePeopleUiStore } from "./uiStore";

export function peopleErrorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function invalidateContacts(): void {
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.contacts });
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.contact });
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.groups });
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.addressBooks });
}

export function writableAddressBooks(books: AddressBook[]): AddressBook[] {
  return books.filter((book) => !book.isReadOnly);
}

/** Address book for a brand-new contact: current scope, default, then first. */
export function defaultWritableBook(books: AddressBook[]): AddressBook | null {
  const writable = writableAddressBooks(books);
  if (writable.length === 0) return null;
  const selected = useShellStore.getState().selection.people.addressBookId;
  const selectedBook = selected ? writable.find((book) => book.id === selected) : undefined;
  return (
    selectedBook ??
    writable.find((book) => book.isDefault) ??
    writable[0] ??
    null
  );
}

/** Entry point for the shell's primary "New contact" action. */
export async function openNewContact(): Promise<void> {
  try {
    const books = await queryClient.ensureQueryData<AddressBook[]>({
      queryKey: queryKeys.addressBooks(),
      queryFn: contacts.addressBooks,
    });
    const book = defaultWritableBook(books);
    if (!book) {
      toast.error(t("people.editor.noAddressBooks"));
      return;
    }
    usePeopleUiStore.getState().openCreate(book.id);
  } catch (error) {
    toast.error(t("people.error.action"), peopleErrorText(error));
  }
}

/* ─────────────────────────── Compose e-mail ────────────────────────── */

async function mailAccountIdFor(contactAccountId: string): Promise<string | null> {
  try {
    const list = await queryClient.ensureQueryData<Account[]>({
      queryKey: queryKeys.accounts(),
      queryFn: accountsApi.list,
    });
    const preferred = list.find(
      (account) => account.id === contactAccountId && account.mailEnabled,
    );
    if (preferred) return preferred.id;
    return list.find((account) => account.mailEnabled)?.id ?? null;
  } catch (error) {
    console.error("[people] could not load accounts", error);
    return null;
  }
}

function mergeRecipient(draft: Draft, email: string, name: string | null): Draft {
  if (draft.to.some((address) => address.email.toLowerCase() === email.toLowerCase())) {
    return draft;
  }
  return { ...draft, to: [...draft.to, { name: name || null, email }] };
}

/**
 * Open the mail composer addressed to the contact. `compose_create` has no
 * recipient parameter, so the draft is created, the recipient is injected and
 * the draft is saved before opening it in the shared inline composer.
 */
export async function composeToEmail(
  email: string,
  name: string | null,
  contactAccountId: string | null,
): Promise<void> {
  try {
    const accountId = await mailAccountIdFor(contactAccountId ?? "");
    const draft = await compose.create(accountId);
    const withRecipient = mergeRecipient(draft, email, name);
    let persisted = withRecipient;
    try {
      persisted = await compose.save(withRecipient);
    } catch (saveError) {
      // A failed local save should not block the user from writing the mail.
      console.error("[people] draft save failed", saveError);
    }
    useComposeStore.getState().open(persisted);
    const shell = useShellStore.getState();
    shell.setActiveModule("mail");
    shell.setShowSettings(false);
    shell.openInlineCompose({ draftId: persisted.id, accountId: persisted.accountId });
  } catch (error) {
    toast.error(t("people.error.compose"), peopleErrorText(error));
  }
}

export async function composeToContact(
  contact: Pick<Contact, "accountId" | "emails" | "displayName" | "givenName" | "familyName">,
): Promise<void> {
  const email = primaryEmail(contact);
  if (!email) {
    toast.info(t("people.detail.missingEmail"));
    return;
  }
  const name = formatContactName(contact) || null;
  await composeToEmail(email, name, contact.accountId);
}

/* ────────────────────────── Schedule meeting ───────────────────────── */

function nextHalfHour(from = new Date()): Date {
  const date = new Date(from.getTime());
  date.setSeconds(0, 0);
  const minutes = date.getMinutes();
  if (minutes < 30) date.setMinutes(30);
  else {
    date.setMinutes(0);
    date.setHours(date.getHours() + 1);
  }
  return date;
}

async function pickWritableCalendar(preferredAccountId: string): Promise<Calendar | null> {
  try {
    const list = await queryClient.ensureQueryData<Calendar[]>({
      queryKey: queryKeys.calendars(),
      queryFn: calendarsApi.list,
    });
    const writable = list.filter((calendar) => !calendar.isReadOnly);
    const sameAccount = writable.filter(
      (calendar) => calendar.accountId === preferredAccountId,
    );
    return (
      sameAccount.find((calendar) => calendar.isVisible) ??
      sameAccount[0] ??
      writable.find((calendar) => calendar.isVisible) ??
      writable[0] ??
      null
    );
  } catch (error) {
    console.error("[people] could not load calendars", error);
    return null;
  }
}

function cachedSettings(): AppSettings {
  return queryClient.getQueryData<AppSettings>(queryKeys.settings()) ?? DEFAULT_SETTINGS;
}

/**
 * Create a calendar event at the next half hour and invite the contact.
 * The calendar module's create seed has no attendee field, so the event is
 * created through `event_create` and then opened in the calendar module.
 */
export async function scheduleMeetingWithContact(
  contact: Pick<Contact, "accountId" | "emails" | "displayName" | "givenName" | "familyName">,
): Promise<void> {
  const email = primaryEmail(contact);
  if (!email) {
    toast.info(t("people.detail.missingEmail"));
    return;
  }
  const calendar = await pickWritableCalendar(contact.accountId);
  if (!calendar) {
    toast.error(t("people.detail.noWritableCalendar"));
    return;
  }
  const name = formatContactName(contact) || email;
  const settings = cachedSettings();
  const start = nextHalfHour();
  const duration = Math.max(15, settings.calendar.defaultEventDurationMinutes);
  const end = new Date(start.getTime() + duration * 60_000);
  const approved = await confirm({
    title: t("people.detail.meetingConfirmTitle"),
    message: t("people.detail.meetingConfirmMessage", {
      calendar: calendar.name,
      name,
      time: start.toLocaleString(getLocale()),
    }),
    confirmLabel: t("people.detail.scheduleMeeting"),
  });
  if (!approved) return;
  const input: EventInput = {
    calendarId: calendar.id,
    title: t("people.detail.meetingTitle", { name }),
    location: "",
    description: "",
    start: start.toISOString(),
    end: end.toISOString(),
    allDay: false,
    timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC",
    recurrence: null,
    showAs: "busy",
    isPrivate: false,
    attendees: [
      { email, name: name || null, role: "required", status: "needsAction" },
    ],
    reminders: [Math.max(0, settings.calendar.defaultReminderMinutes)],
    onlineMeetingUrl: null,
    color: null,
    notifyAttendees: true,
  };
  try {
    const created = await events.create(input);
    void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.events });
    const shell = useShellStore.getState();
    shell.setSelection("calendar", {
      date: toIsoDate(start),
      view: "day",
      eventId: created.id,
      instanceStart: created.start,
    });
    shell.setActiveModule("calendar");
    toast.success(t("people.detail.meetingCreated"), name);
  } catch (error) {
    toast.error(t("people.error.action"), peopleErrorText(error));
  }
}

/* ──────────────────────────── Mutations ────────────────────────────── */

export async function setContactFavorite(contact: Contact, value: boolean): Promise<void> {
  // Optimistic detail update; list rows refresh through invalidation.
  const key = queryKeys.contact(contact.id);
  const previous = queryClient.getQueryData<Contact>(key);
  if (previous) queryClient.setQueryData<Contact>(key, { ...previous, isFavorite: value });
  try {
    await contacts.setFavorite(contact.id, value);
    invalidateContacts();
  } catch (error) {
    if (previous) queryClient.setQueryData<Contact>(key, previous);
    toast.error(t("people.error.action"), peopleErrorText(error));
  }
}

export async function deleteContacts(ids: string[]): Promise<boolean> {
  if (ids.length === 0) return true;
  try {
    await contacts.delete(ids);
    invalidateContacts();
    const state = usePeopleUiStore.getState();
    state.clearSelection();
    const selected = useShellStore.getState().selection.people.contactId;
    if (selected && ids.includes(selected)) {
      useShellStore.getState().setSelection("people", { contactId: null });
    }
    toast.success(t("people.detail.deleted", { count: ids.length }));
    return true;
  } catch (error) {
    toast.error(t("people.error.delete"), peopleErrorText(error));
    return false;
  }
}

export async function deleteContactWithConfirm(contact: Contact): Promise<void> {
  const approved = await confirm({
    title: t("people.detail.deleteConfirmTitle"),
    message: t("people.detail.deleteConfirmMessage", {
      name: formatContactName(contact) || contact.displayName,
    }),
    confirmLabel: t("common.delete"),
    danger: true,
  });
  if (approved) await deleteContacts([contact.id]);
}

/** Add contacts to an existing group, preserving their other groups. */
export async function addContactsToGroup(
  ids: string[],
  groupId: string,
): Promise<void> {
  if (ids.length === 0) return;
  let added = 0;
  let failed = 0;
  for (const id of ids) {
    try {
      const contact = await contacts.get(id);
      if (contact.groupIds.includes(groupId)) {
        added += 1;
        continue;
      }
      await contacts.update({ ...contact, groupIds: [...contact.groupIds, groupId] });
      added += 1;
    } catch (error) {
      failed += 1;
      console.error("[people] add to group failed", id, error);
    }
  }
  invalidateContacts();
  if (failed > 0) {
    toast.error(t("people.error.action"), t("people.bulk.addFailed", { count: failed }));
  } else {
    toast.success(t("people.bulk.addedToGroup", { count: added }));
  }
}

/* ────────────────────────── Import / export ─────────────────────────── */

function exportQueryForScope(
  addressBookId: string | null,
  groupId: string | null,
  favoritesOnly: boolean,
): ContactListQuery {
  return {
    addressBookId,
    groupId,
    query: null,
    favoritesOnly,
    cursor: null,
    limit: 500,
  };
}

/** Collect every id matching the scope, following the cursor. */
export async function collectContactIds(query: ContactListQuery): Promise<string[]> {
  const ids: string[] = [];
  let cursor: string | null = null;
  let guard = 0;
  do {
    const page = await contacts.list({ ...query, cursor, limit: 500 });
    for (const item of page.items) ids.push(item.id);
    cursor = page.nextCursor;
    guard += 1;
    if (guard > 400) break;
  } while (cursor);
  return ids;
}

export async function exportContactsByScope(options: {
  addressBookId: string | null;
  groupId: string | null;
  favoritesOnly: boolean;
  suggestedName: string;
}): Promise<void> {
  if (!isTauri()) {
    toast.error(t("people.export.title"), t("people.export.notAvailable"));
    return;
  }
  try {
    const ids = await collectContactIds(
      exportQueryForScope(options.addressBookId, options.groupId, options.favoritesOnly),
    );
    if (ids.length === 0) {
      toast.info(t("people.export.empty"));
      return;
    }
    const path = await saveFileDialog({
      defaultPath: `${options.suggestedName || "contacts"}.vcf`,
      filters: [{ name: "vCard", extensions: ["vcf"] }],
    });
    if (!path) return;
    await contacts.exportVcf(ids, path);
    toast.success(t("people.export.done", { count: ids.length }));
  } catch (error) {
    toast.error(t("people.export.failed"), peopleErrorText(error));
  }
}

export async function exportContactIds(
  ids: string[],
  suggestedName: string,
): Promise<void> {
  if (!isTauri()) {
    toast.error(t("people.export.title"), t("people.export.notAvailable"));
    return;
  }
  if (ids.length === 0) {
    toast.info(t("people.export.empty"));
    return;
  }
  try {
    const path = await saveFileDialog({
      defaultPath: `${suggestedName || "contacts"}.vcf`,
      filters: [{ name: "vCard", extensions: ["vcf"] }],
    });
    if (!path) return;
    await contacts.exportVcf(ids, path);
    toast.success(t("people.export.done", { count: ids.length }));
  } catch (error) {
    toast.error(t("people.export.failed"), peopleErrorText(error));
  }
}

export async function exportAllContacts(): Promise<void> {
  await exportContactsByScope({
    addressBookId: null,
    groupId: null,
    favoritesOnly: false,
    suggestedName: "contacts",
  });
}

/* ──────────────────────── Contact persistence ──────────────────────── */

export async function saveContact(contact: Contact, isNew: boolean): Promise<Contact> {
  const saved = isNew
    ? await contacts.create(contact)
    : await contacts.update(contact);
  invalidateContacts();
  const shell = useShellStore.getState();
  const scope = shell.selection.people;
  const favoritesOnly = usePeopleUiStore.getState().favoritesOnly;
  if (isNew && (favoritesOnly || (scope.addressBookId && scope.addressBookId !== saved.addressBookId))) {
    // Keep the new contact visible after the editor closes.
    usePeopleUiStore.getState().setFavoritesOnly(false);
    shell.setSelection("people", {
      addressBookId: saved.addressBookId,
      groupId: null,
      contactId: saved.id,
    });
  } else if (isNew && scope.groupId && !saved.groupIds.includes(scope.groupId)) {
    shell.setSelection("people", { groupId: null, contactId: saved.id });
  } else {
    shell.setSelection("people", { contactId: saved.id });
  }
  usePeopleUiStore.getState().setSelectedIds([saved.id]);
  usePeopleUiStore.getState().setAnchor(saved.id);
  return saved;
}
