/**
 * IPC CONTRACT — every backend command and event.
 *
 * Rust side: each function below maps 1:1 to a `#[tauri::command]` with the
 * SAME snake_case name, registered in `src-tauri/src/commands/mod.rs`.
 * Argument objects use camelCase keys here; Tauri converts them to the
 * snake_case Rust parameter names (e.g. `{ accountId }` -> `account_id`).
 * Commands return `Result<T, AppError>`; AppError serializes to
 * `{ code: string, message: string }` (see IpcError).
 *
 * Do NOT add/rename commands without updating the Rust side and docs/ARCHITECTURE.md.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type * as T from "./types";

export interface IpcError {
  /** "notFound" | "auth" | "network" | "protocol" | "invalidInput" | "conflict" | "io" | "db" | "cancelled" | "unsupported" | "internal" */
  code: string;
  message: string;
}

export function isIpcError(e: unknown): e is IpcError {
  return typeof e === "object" && e !== null && "code" in e && "message" in e;
}

const call = <R>(cmd: string, args?: Record<string, unknown>) => invoke<R>(cmd, args);

/* ───────────────────────────── App ───────────────────────────── */
export const app = {
  info: () => call<T.AppInfo>("app_info"),
  storageUsage: () => call<T.StorageUsage>("app_storage_usage"),
  clearCache: () => call<void>("app_clear_cache"),
  /** Open a secondary window. route examples: "/message/<id>", "/compose/<draftId>", "/compose/new?accountId=..". */
  openWindow: (route: string, title: string) => call<void>("app_open_window", { route, title }),
  /** Path of a file the app was launched with / asked to open but not yet handled. */
  takePendingOpen: () => call<{ kind: "eml" | "ics" | "vcf" | "mailto" | "webcal"; value: string }[]>("app_take_pending_open"),
  backupExport: (path: string, password: string, includeCredentials: boolean) =>
    call<void>("app_backup_export", { path, password, includeCredentials }),
  backupImport: (path: string, password: string) => call<void>("app_backup_import", { path, password }),
  /** Reveal the log directory in the OS file manager. */
  openLogDir: () => call<void>("app_open_log_dir"),
  quit: () => call<void>("app_quit"),
};

/* ─────────────────────────── Settings ─────────────────────────── */
export const settings = {
  get: () => call<T.AppSettings>("settings_get"),
  update: (patch: T.SettingsPatch) => call<T.AppSettings>("settings_update", { patch }),
};

/* ──────────────────────── Accounts & auth ─────────────────────── */
export const accounts = {
  list: () => call<T.Account[]>("account_list"),
  discover: (email: string) => call<T.DiscoveryResult>("account_discover", { email }),
  test: (input: T.NewAccountInput) => call<T.ConnectionTestResult>("account_test", { input }),
  create: (input: T.NewAccountInput) => call<T.Account>("account_create", { input }),
  update: (id: T.Id, patch: T.AccountPatch) => call<T.Account>("account_update", { id, patch }),
  /** Replace stored password / app password and retry. */
  updatePassword: (id: T.Id, password: string) => call<void>("account_update_password", { id, password }),
  /** Re-run OAuth for an account in needsReauth. */
  reauth: (id: T.Id) => call<T.Account>("account_reauth", { id }),
  delete: (id: T.Id) => call<void>("account_delete", { id }),
  reorder: (ids: T.Id[]) => call<void>("account_reorder", { ids }),
  /** Starts system-browser OAuth (PKCE + loopback). Resolves when the user finishes; rejects with code "cancelled" on cancel/timeout (5 min). */
  oauthLogin: (provider: T.OAuthProvider, loginHint: string | null) =>
    call<T.OAuthResult>("auth_oauth_login", { provider, loginHint }),
  oauthCancel: () => call<void>("auth_oauth_cancel"),
};

export const identities = {
  list: (accountId: T.Id | null) => call<T.Identity[]>("identity_list", { accountId }),
  upsert: (identity: T.Identity) => call<T.Identity>("identity_upsert", { identity }),
  delete: (id: T.Id) => call<void>("identity_delete", { id }),
};

export const signatures = {
  list: () => call<T.Signature[]>("signature_list"),
  upsert: (signature: T.Signature) => call<T.Signature>("signature_upsert", { signature }),
  delete: (id: T.Id) => call<void>("signature_delete", { id }),
};

/* ────────────────────────────── Sync ───────────────────────────── */
export const sync = {
  /** accountId null = all accounts; module null = all modules. */
  now: (accountId: T.Id | null, module: T.SyncModule | null) => call<void>("sync_now", { accountId, module }),
  status: () => call<T.SyncStatus[]>("sync_status"),
};

/* ───────────────────────────── Mail ───────────────────────────── */
export const folders = {
  list: (accountId: T.Id) => call<T.Folder[]>("folder_list", { accountId }),
  unifiedCounts: () => call<T.UnifiedCounts>("folder_unified_counts"),
  create: (accountId: T.Id, parentId: T.Id | null, name: string) =>
    call<T.Folder>("folder_create", { accountId, parentId, name }),
  rename: (id: T.Id, name: string) => call<T.Folder>("folder_rename", { id, name }),
  move: (id: T.Id, newParentId: T.Id | null) => call<T.Folder>("folder_move", { id, newParentId }),
  delete: (id: T.Id) => call<void>("folder_delete", { id }),
  setFavorite: (id: T.Id, value: boolean) => call<void>("folder_set_favorite", { id, value }),
  setSyncEnabled: (id: T.Id, value: boolean) => call<void>("folder_set_sync_enabled", { id, value }),
  markAllRead: (id: T.Id) => call<void>("folder_mark_all_read", { id }),
  /** Permanently delete everything in a trash/junk folder. */
  empty: (id: T.Id) => call<void>("folder_empty", { id }),
  sync: (id: T.Id) => call<void>("folder_sync", { id }),
};

export const messages = {
  list: (query: T.MessageListQuery) => call<T.MessagePage>("message_list", { query }),
  /** Conversation members, oldest first. */
  thread: (accountId: T.Id, threadId: string) => call<T.MessageSummary[]>("message_thread", { accountId, threadId }),
  /** Downloads the body if not cached. allowRemote overrides the remote-content policy for this call. */
  get: (id: T.Id, allowRemote: boolean) => call<T.MessageDetail>("message_get", { id, allowRemote }),
  /** Optimistic: updates locally, queues server ops, emits mail://changed. */
  act: (ids: T.Id[], action: T.MessageAction) => call<void>("message_act", { ids, action }),
  /** Applies the action to every message of the given conversations. */
  actThreads: (accountId: T.Id, threadIds: string[], action: T.MessageAction) =>
    call<void>("message_act_threads", { accountId, threadIds, action }),
  undoLast: () => call<boolean>("message_undo_last"),
  saveEml: (id: T.Id, path: string) => call<void>("message_save_eml", { id, path }),
  openEml: (path: string) => call<T.MessageDetail>("message_open_eml", { path }),
  /** Remember sender (email) or domain ("@example.com") as trusted for remote content. */
  trustSender: (value: string) => call<void>("message_trust_sender", { value }),
  unsubscribe: (id: T.Id) => call<void>("message_unsubscribe", { id }),
  /** Create a To Do task linked to this message (flagged e-mail). */
  createTask: (id: T.Id) => call<T.Task>("message_create_task", { id }),
};

export const attachments = {
  /** Downloads if needed and opens with the OS default app. */
  open: (id: T.Id) => call<void>("attachment_open", { id }),
  save: (id: T.Id, path: string) => call<void>("attachment_save", { id, path }),
  saveAll: (messageId: T.Id, dir: string) => call<void>("attachment_save_all", { messageId, dir }),
  /** True for executable/script types that need an extra confirmation. */
  isDangerous: (filename: string) => call<boolean>("attachment_is_dangerous", { filename }),
};

export const compose = {
  /** New blank draft with default identity + signature. */
  create: (accountId: T.Id | null) => call<T.Draft>("compose_create", { accountId }),
  fromMailto: (url: string) => call<T.Draft>("compose_from_mailto", { url }),
  reply: (messageId: T.Id, mode: T.ReplyMode) => call<T.Draft>("compose_reply", { messageId, mode }),
  /** Open an existing draft message from the Drafts folder. */
  openDraft: (messageId: T.Id) => call<T.Draft>("compose_open_draft", { messageId }),
  getDraft: (draftId: T.Id) => call<T.Draft>("compose_get_draft", { draftId }),
  /** Saves locally (and queues a server Drafts APPEND). Returns the draft with id set. */
  save: (draft: T.Draft) => call<T.Draft>("compose_save", { draft }),
  discard: (draftId: T.Id) => call<void>("compose_discard", { draftId }),
  /** Validates, builds MIME, queues to outbox. */
  send: (draft: T.Draft) => call<void>("compose_send", { draft }),
  /** Copies files into the draft staging dir. */
  addAttachments: (paths: string[]) => call<T.DraftAttachment[]>("compose_add_attachments", { paths }),
  /** Stage raw bytes (pasted image) — returns attachment with contentId set. */
  addInlineImage: (filename: string, mimeType: string, base64Data: string) =>
    call<T.DraftAttachment>("compose_add_inline_image", { filename, mimeType, base64Data }),
  suggestRecipients: (query: string, limit: number) => call<T.Address[]>("compose_suggest_recipients", { query, limit }),
  /**
   * Acknowledge `app://flush-drafts`: the main window's unsaved drafts are on
   * disk and the webview may be destroyed (close-to-tray). `seq` is the id that
   * came with the event; an acknowledgement for any other handshake is ignored.
   */
  flushDone: (seq: number) => call<void>("compose_flush_done", { seq }),
};

export const rules = {
  list: () => call<T.MailRule[]>("rule_list"),
  upsert: (rule: T.MailRule) => call<T.MailRule>("rule_upsert", { rule }),
  delete: (id: T.Id) => call<void>("rule_delete", { id }),
  /** Run a rule now against a folder; returns number of affected messages. */
  runNow: (id: T.Id, folderId: T.Id) => call<number>("rule_run_now", { id, folderId }),
};

/** Outbox items appear in the "outbox" folder scope with MessageSummary.id = "outbox:<outboxId>". */
export const outbox = {
  /** Retry sending now. outboxId null = every failed/pending item. */
  retry: (outboxId: T.Id | null) => call<void>("outbox_retry", { outboxId }),
  /** Stop sending and turn the item back into an editable draft (returned). */
  cancel: (outboxId: T.Id) => call<T.Draft>("outbox_cancel", { outboxId }),
};

/* ──────────────────────────── Calendar ─────────────────────────── */
export const calendars = {
  list: () => call<T.Calendar[]>("calendar_list"),
  create: (accountId: T.Id, name: string, color: string) => call<T.Calendar>("calendar_create", { accountId, name, color }),
  update: (id: T.Id, name: string | null, color: string | null) => call<T.Calendar>("calendar_update", { id, name, color }),
  setVisible: (id: T.Id, visible: boolean) => call<void>("calendar_set_visible", { id, visible }),
  delete: (id: T.Id) => call<void>("calendar_delete", { id }),
  subscribe: (url: string, name: string, color: string) => call<T.Calendar>("calendar_subscribe", { url, name, color }),
  importIcs: (path: string, calendarId: T.Id) => call<number>("calendar_import_ics", { path, calendarId }),
  exportIcs: (calendarId: T.Id, path: string) => call<void>("calendar_export_ics", { calendarId, path }),
};

export const events = {
  /** Expanded occurrences overlapping [from, to) (ISO instants). calendarIds null = all visible. */
  range: (from: string, to: string, calendarIds: T.Id[] | null) =>
    call<T.EventInstance[]>("event_range", { from, to, calendarIds }),
  /** instanceStart selects a specific occurrence of a recurring series. */
  get: (eventId: T.Id, instanceStart: string | null) => call<T.CalendarEvent>("event_get", { eventId, instanceStart }),
  create: (input: T.EventInput) => call<T.CalendarEvent>("event_create", { input }),
  update: (eventId: T.Id, input: T.EventInput, scope: T.RecurrenceEditScope, instanceStart: string | null) =>
    call<T.CalendarEvent>("event_update", { eventId, input, scope, instanceStart }),
  delete: (eventId: T.Id, scope: T.RecurrenceEditScope, instanceStart: string | null, notifyAttendees: boolean) =>
    call<void>("event_delete", { eventId, scope, instanceStart, notifyAttendees }),
  respond: (eventId: T.Id, response: T.AttendeeStatus, comment: string | null, sendReply: boolean) =>
    call<void>("event_respond", { eventId, response, comment, sendReply }),
  search: (text: string, limit: number) => call<T.EventInstance[]>("event_search", { text, limit }),
  /** Respond to an invitation contained in an e-mail; adds/updates the event in the default calendar. */
  respondToInvite: (messageId: T.Id, response: T.AttendeeStatus, comment: string | null) =>
    call<void>("invite_respond", { messageId, response, comment }),
  dismissReminder: (eventId: T.Id, instanceStart: string) => call<void>("event_dismiss_reminder", { eventId, instanceStart }),
  snoozeReminder: (eventId: T.Id, instanceStart: string, minutes: number) =>
    call<void>("event_snooze_reminder", { eventId, instanceStart, minutes }),
};

/* ──────────────────────────── Contacts ─────────────────────────── */
export const contacts = {
  addressBooks: () => call<T.AddressBook[]>("addressbook_list"),
  list: (query: T.ContactListQuery) => call<T.ContactPage>("contact_list", { query }),
  get: (id: T.Id) => call<T.Contact>("contact_get", { id }),
  /** contact.id is ignored on create. */
  create: (contact: T.Contact) => call<T.Contact>("contact_create", { contact }),
  update: (contact: T.Contact) => call<T.Contact>("contact_update", { contact }),
  delete: (ids: T.Id[]) => call<void>("contact_delete", { ids }),
  setFavorite: (id: T.Id, value: boolean) => call<void>("contact_set_favorite", { id, value }),
  findByEmail: (email: string) => call<T.Contact | null>("contact_find_by_email", { email }),
  groups: (addressBookId: T.Id | null) => call<T.ContactGroup[]>("group_list", { addressBookId }),
  createGroup: (addressBookId: T.Id, name: string) => call<T.ContactGroup>("group_create", { addressBookId, name }),
  renameGroup: (id: T.Id, name: string) => call<T.ContactGroup>("group_rename", { id, name }),
  deleteGroup: (id: T.Id) => call<void>("group_delete", { id }),
  setGroupMembers: (groupId: T.Id, contactIds: T.Id[]) => call<void>("group_set_members", { groupId, contactIds }),
  importVcf: (path: string, addressBookId: T.Id) => call<number>("contact_import_vcf", { path, addressBookId }),
  exportVcf: (ids: T.Id[], path: string) => call<void>("contact_export_vcf", { ids, path }),
};

/* ────────────────────────────── To Do ──────────────────────────── */
export const tasks = {
  lists: () => call<T.TaskList[]>("tasklist_list"),
  createList: (accountId: T.Id, name: string, color: string) => call<T.TaskList>("tasklist_create", { accountId, name, color }),
  updateList: (id: T.Id, name: string | null, color: string | null) => call<T.TaskList>("tasklist_update", { id, name, color }),
  deleteList: (id: T.Id) => call<void>("tasklist_delete", { id }),
  smartCounts: (today: T.IsoDate) => call<T.SmartListCounts>("task_smart_counts", { today }),
  query: (query: T.TaskQuery) => call<T.Task[]>("task_query", { query }),
  get: (id: T.Id) => call<T.Task>("task_get", { id }),
  create: (listId: T.Id, input: T.TaskInput) => call<T.Task>("task_create", { listId, input }),
  update: (id: T.Id, input: T.TaskInput) => call<T.Task>("task_update", { id, input }),
  setCompleted: (id: T.Id, value: boolean) => call<T.Task>("task_set_completed", { id, value }),
  setImportance: (id: T.Id, importance: T.Importance) => call<T.Task>("task_set_importance", { id, importance }),
  /** date null removes from My Day. */
  setMyDay: (id: T.Id, date: T.IsoDate | null) => call<T.Task>("task_set_my_day", { id, date }),
  move: (id: T.Id, listId: T.Id) => call<T.Task>("task_move", { id, listId }),
  reorder: (listId: T.Id, ids: T.Id[]) => call<void>("task_reorder", { listId, ids }),
  delete: (id: T.Id) => call<void>("task_delete", { id }),
  search: (text: string) => call<T.Task[]>("task_search", { text }),
  dismissReminder: (id: T.Id) => call<void>("task_dismiss_reminder", { id }),
  snoozeReminder: (id: T.Id, minutes: number) => call<void>("task_snooze_reminder", { id, minutes }),
};

/* ───────────────────────────── Events ──────────────────────────── */
/** Backend -> frontend event names and payloads. */
export interface EventPayloads {
  "sync://status": T.SyncStatus;
  "accounts://changed": { accountIds: T.Id[] };
  "settings://changed": T.AppSettings;
  /** kind: new | updated | deleted | folders */
  "mail://changed": { accountId: T.Id; folderIds: T.Id[]; messageIds: T.Id[]; kind: "new" | "updated" | "deleted" | "folders" };
  "calendar://changed": { calendarIds: T.Id[] };
  "contacts://changed": { addressBookIds: T.Id[] };
  "tasks://changed": { listIds: T.Id[] };
  /** A queued server operation failed permanently; local state was rolled back. */
  "op://failed": { accountId: T.Id; description: string; error: string };
  /** Event/task reminder became due (an OS notification is also shown). */
  "reminder://due": { kind: "event" | "task"; id: T.Id; instanceStart: string | null; title: string; start: string | null; allDay: boolean; location: string | null };
  /** Navigate request (notification click, tray menu, deep link). */
  "app://navigate": { module: T.AppModule; accountId: T.Id | null; folderId: T.Id | null; messageId: T.Id | null; eventId: T.Id | null; taskId: T.Id | null };
  /** A file/URL was opened with the app (eml/ics/vcf/mailto/webcal). Frontend should call app.takePendingOpen(). */
  "app://open-requested": Record<string, never>;
  /** Tray "New message" etc. */
  "app://compose": { draft: T.Draft };
  /**
   * Main window is about to be destroyed (close-to-tray): save pending drafts,
   * then call `compose.flushDone(seq)` with this id. Echoing it back is what
   * lets the backend tell this close apart from an older, already cancelled one.
   */
  "app://flush-drafts": number;
  /** The close was abandoned: unfreeze the composer, the window stays open. */
  "app://close-cancelled": Record<string, never>;
}

export function on<K extends keyof EventPayloads>(
  name: K,
  handler: (payload: EventPayloads[K]) => void,
): Promise<UnlistenFn> {
  return listen<EventPayloads[K]>(name, (e) => handler(e.payload));
}
