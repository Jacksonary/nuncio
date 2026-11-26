import { describe, expect, it } from "vitest";
import type { SyncStatus } from "@/ipc/types";
import { mergeSyncStatusEvent } from "@/lib/eventBridge";

function status(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    accountId: "acct-1",
    module: "mail",
    state: "idle",
    progress: null,
    fullSync: false,
    seq: 1,
    message: null,
    lastSyncAt: null,
    ...overrides,
  };
}

describe("mergeSyncStatusEvent", () => {
  it("appends a status it has not seen yet", () => {
    const next = mergeSyncStatusEvent([], status({ seq: 4 }));
    expect(next).toHaveLength(1);
    expect(next[0].seq).toBe(4);
  });

  it("replaces the same account+module with a newer snapshot", () => {
    const current = [status({ state: "syncing", seq: 2 })];
    const next = mergeSyncStatusEvent(current, status({ state: "idle", seq: 3 }));
    expect(next).toHaveLength(1);
    expect(next[0].state).toBe("idle");
    expect(current[0].state).toBe("syncing");
  });

  it("drops a late cleanup that is older than the cached full-sync snapshot", () => {
    // The new pass published `fullSync=true` (seq 5); the old marker's cleanup
    // was committed earlier (seq 3) but delivered afterwards.
    const current = [status({ state: "syncing", fullSync: true, seq: 5 })];
    const staleCleanup = status({ state: "syncing", fullSync: false, seq: 3 });

    const next = mergeSyncStatusEvent(current, staleCleanup);

    expect(next).toBe(current);
    expect(next[0].fullSync).toBe(true);
  });

  it("leaves other modules untouched", () => {
    const calendar = status({ module: "calendar", seq: 1 });
    const next = mergeSyncStatusEvent(
      [calendar],
      status({ module: "mail", state: "syncing", seq: 2 }),
    );
    expect(next).toHaveLength(2);
    expect(next[0]).toBe(calendar);
    expect(next[1].module).toBe("mail");
  });
});
