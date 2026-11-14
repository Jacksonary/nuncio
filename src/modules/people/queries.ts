/**
 * TanStack Query bindings for the People module. Every key comes from the
 * shared `queryKeys` helpers so the shell event bridge can invalidate them.
 */
import {
  useQueries,
  useQuery,
  type UseQueryResult,
} from "@tanstack/react-query";
import { accounts as accountsApi, contacts as contactsApi } from "@/ipc/api";
import type {
  Account,
  AddressBook,
  Contact,
  ContactGroup,
  ContactListQuery,
  ContactSummary,
  Id,
} from "@/ipc/types";
import { queryKeys } from "@/lib/queryKeys";
import { t } from "@/lib/i18n";
import { CONTACTS_PAGE_SIZE } from "./logic";

export function useAccountsQuery() {
  return useQuery<Account[]>({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });
}

export function useAddressBooksQuery() {
  return useQuery<AddressBook[]>({
    queryKey: queryKeys.addressBooks(),
    queryFn: contactsApi.addressBooks,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
}

/** All groups across address books, in one request (`addressBookId = null`). */
export function useAllGroupsQuery() {
  return useQuery<ContactGroup[]>({
    queryKey: queryKeys.groups(null),
    queryFn: () => contactsApi.groups(null),
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
}

export function useGroupsQuery(addressBookId: Id | null) {
  return useQuery<ContactGroup[]>({
    queryKey: queryKeys.groups(addressBookId),
    queryFn: () => contactsApi.groups(addressBookId),
    staleTime: 30_000,
    enabled: addressBookId !== null,
    refetchOnWindowFocus: false,
  });
}

/** Safety cap for the all-contacts walk: 500 pages × 200 = 100k contacts. */
const MAX_CONTACT_PAGES = 500;

export interface ContactCollection {
  contacts: ContactSummary[];
  total: number;
}

/**
 * Complete contact set for the list pane. The initial list is sorted and
 * sectioned on the client (last-name keys, pinyin initials), so every page has
 * to be present before the A–Z sections can be built — otherwise later pages
 * would be appended in `display_name` order and sections would jump. Contacts
 * are in the hundreds, so walking the cursor once is cheap; the result is
 * cached and shared through the usual `["contacts", query]` prefix so the
 * shell event bridge invalidates it like any other contact query.
 */
export function useAllContactsQuery(
  query: ContactListQuery,
  enabled = true,
): UseQueryResult<ContactCollection, unknown> {
  return useQuery({
    queryKey: [...queryKeys.contacts(query), "all"] as const,
    queryFn: async ({ signal }) => {
      const seen = new Map<string, ContactSummary>();
      const usedCursors = new Set<string>();
      let cursor: string | null = null;
      let total = 0;
      let pageCount = 0;
      for (;;) {
        signal.throwIfAborted();
        pageCount += 1;
        if (pageCount > MAX_CONTACT_PAGES) {
          throw new Error(t("people.list.paginationError"));
        }
        const page = await contactsApi.list({ ...query, cursor });
        if (pageCount === 1) total = page.total;
        for (const item of page.items) {
          if (!seen.has(item.id)) seen.set(item.id, item);
        }
        const next = page.nextCursor;
        if (!next) break;
        // A repeated cursor would loop forever; treat it as a broken backend.
        if (next === cursor || usedCursors.has(next)) {
          throw new Error(t("people.list.paginationError"));
        }
        usedCursors.add(next);
        cursor = next;
      }
      return { contacts: [...seen.values()], total: Math.max(total, seen.size) };
    },
    staleTime: 15_000,
    enabled,
    refetchOnWindowFocus: false,
  });
}

export function useContactQuery(id: Id | null) {
  return useQuery<Contact>({
    queryKey: queryKeys.contact(id ?? ""),
    queryFn: () => contactsApi.get(id as Id),
    enabled: id !== null,
    staleTime: 15_000,
    refetchOnWindowFocus: false,
  });
}

function countQuery(patch: Partial<ContactListQuery>): ContactListQuery {
  return {
    addressBookId: null,
    groupId: null,
    query: null,
    favoritesOnly: false,
    cursor: null,
    limit: 1,
    ...patch,
  };
}

export interface PeopleCounts {
  all: number | null;
  favorites: number | null;
  byBook: Record<Id, number | null>;
}

/**
 * Sidebar counts. Each scope fetches a single row and reads `total`, which keeps
 * the payload tiny while using ordinary contact-list query keys.
 */
export function usePeopleCounts(books: AddressBook[]): PeopleCounts {
  const allQuery = countQuery({});
  const favoritesQuery = countQuery({ favoritesOnly: true });
  const bookQueries = books.map((book) => countQuery({ addressBookId: book.id }));

  const queries = useQueries({
    queries: [
      {
        queryKey: queryKeys.contacts(allQuery),
        queryFn: () => contactsApi.list(allQuery),
        staleTime: 30_000,
        refetchOnWindowFocus: false,
      },
      {
        queryKey: queryKeys.contacts(favoritesQuery),
        queryFn: () => contactsApi.list(favoritesQuery),
        staleTime: 30_000,
        refetchOnWindowFocus: false,
      },
      ...bookQueries.map((query) => ({
        queryKey: queryKeys.contacts(query),
        queryFn: () => contactsApi.list(query),
        staleTime: 30_000,
        refetchOnWindowFocus: false,
      })),
    ],
  });

  const byBook: Record<Id, number | null> = {};
  books.forEach((book, index) => {
    byBook[book.id] = queries[index + 2]?.data?.total ?? null;
  });
  return {
    all: queries[0]?.data?.total ?? null,
    favorites: queries[1]?.data?.total ?? null,
    byBook,
  };
}

/** Page size used by the list; exported for tests and callers. */
export { CONTACTS_PAGE_SIZE };
