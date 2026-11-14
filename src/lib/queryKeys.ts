/**
 * Query keys — the exact shapes defined in docs/ARCHITECTURE.md §8.
 * Module agents MUST use these helpers instead of writing key arrays inline,
 * and the event bridge invalidates by prefix (see `queryKeyPrefixes`).
 */
import type {
  ContactListQuery,
  Id,
  IsoDate,
  MessageListQuery,
  TaskQuery,
} from "@/ipc/types";

export const queryKeyPrefixes = {
  accounts: ["accounts"] as const,
  folders: ["folders"] as const,
  unifiedCounts: ["unifiedCounts"] as const,
  messages: ["messages"] as const,
  message: ["message"] as const,
  thread: ["thread"] as const,
  calendars: ["calendars"] as const,
  events: ["events"] as const,
  event: ["event"] as const,
  addressBooks: ["addressBooks"] as const,
  contacts: ["contacts"] as const,
  contact: ["contact"] as const,
  groups: ["groups"] as const,
  taskLists: ["taskLists"] as const,
  tasks: ["tasks"] as const,
  task: ["task"] as const,
  smartCounts: ["smartCounts"] as const,
  settings: ["settings"] as const,
  syncStatus: ["syncStatus"] as const,
  identities: ["identities"] as const,
  signatures: ["signatures"] as const,
  rules: ["rules"] as const,
};

export const queryKeys = {
  accounts: () => [...queryKeyPrefixes.accounts] as const,
  folders: (accountId: Id) => ["folders", accountId] as const,
  unifiedCounts: () => [...queryKeyPrefixes.unifiedCounts] as const,
  messages: (query: MessageListQuery) => ["messages", query] as const,
  message: (id: Id) => ["message", id] as const,
  thread: (accountId: Id, threadId: string) => ["thread", accountId, threadId] as const,
  calendars: () => [...queryKeyPrefixes.calendars] as const,
  events: (from: string, to: string, calendarIds: Id[] | null) =>
    ["events", from, to, calendarIds] as const,
  event: (id: Id, instanceStart: string | null) => ["event", id, instanceStart] as const,
  addressBooks: () => [...queryKeyPrefixes.addressBooks] as const,
  contacts: (query: ContactListQuery) => ["contacts", query] as const,
  contact: (id: Id) => ["contact", id] as const,
  groups: (addressBookId: Id | null) => ["groups", addressBookId] as const,
  taskLists: () => [...queryKeyPrefixes.taskLists] as const,
  tasks: (query: TaskQuery) => ["tasks", query] as const,
  task: (id: Id) => ["task", id] as const,
  smartCounts: (today: IsoDate) => ["smartCounts", today] as const,
  settings: () => [...queryKeyPrefixes.settings] as const,
  syncStatus: () => [...queryKeyPrefixes.syncStatus] as const,
  identities: (accountId: Id | null) => ["identities", accountId] as const,
  signatures: () => [...queryKeyPrefixes.signatures] as const,
  rules: () => [...queryKeyPrefixes.rules] as const,
};

export type QueryKeyHelpers = typeof queryKeys;
