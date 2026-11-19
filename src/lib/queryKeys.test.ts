import { describe, expect, it } from "vitest";
import {
  accountScopedPrefixes,
  invalidateAccountScopedData,
  type QueryKeyInvalidator,
} from "@/lib/queryKeys";
import { accountDataPrefixes } from "@/lib/eventBridge";
import { deleteAccountPrefixes } from "@/modules/settings/lib/hooks";

/** The caches an account owns. Kept literal so a dropped prefix is visible. */
const EXPECTED_ROOTS = [
  "accounts",
  "identities",
  "folders",
  "messages",
  "message",
  "thread",
  "unifiedCounts",
  "calendars",
  "events",
  "event",
  "addressBooks",
  "contacts",
  "contact",
  "groups",
  "taskLists",
  "tasks",
  "task",
  "smartCounts",
  "syncStatus",
  "rules",
];

function roots(prefixes: ReadonlyArray<readonly unknown[]>): string[] {
  return prefixes.map((prefix) => String(prefix[0])).sort();
}

describe("account-scoped invalidation", () => {
  it("covers every account-owned cache exactly once", () => {
    expect(roots(accountScopedPrefixes)).toEqual([...EXPECTED_ROOTS].sort());
  });

  it("invalidates each prefix once, in list order", () => {
    const calls: Array<readonly unknown[]> = [];
    const client: QueryKeyInvalidator = {
      invalidateQueries: ({ queryKey }) => {
        calls.push(queryKey);
      },
    };

    invalidateAccountScopedData(client);

    expect(calls).toEqual([...accountScopedPrefixes]);
  });

  it("keeps the delete mutation and the event bridge on the same set", () => {
    // Same array identity: replacing one call site with a hand-written list
    // breaks this instead of silently invalidating a different set.
    expect(deleteAccountPrefixes).toBe(accountScopedPrefixes);
    expect(accountDataPrefixes).toBe(accountScopedPrefixes);
  });
});
