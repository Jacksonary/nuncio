import { describe, expect, it } from "vitest";
import type { SyncModule, SyncState, SyncStatus } from "@/ipc/types";
import { presentationFor } from "./ModuleSyncStatus";

function status(overrides: Partial<SyncStatus> = {}): SyncStatus {
  return {
    accountId: "acct-1",
    module: "mail",
    state: "idle",
    progress: null,
    message: null,
    lastSyncAt: null,
    ...overrides,
  };
}

function syncing(
  module: SyncModule,
  progress: { done: number; total: number } | null,
  accountId = "acct-1",
): SyncStatus {
  return status({ accountId, module, state: "syncing", progress });
}

describe("presentationFor", () => {
  it("is idle and silent when there are no entries", () => {
    expect(presentationFor([], "mail")).toEqual({
      kind: "idle",
      labelKey: null,
      detail: null,
    });
  });

  it("is idle when only other modules have statuses", () => {
    const result = presentationFor([syncing("calendar", { done: 1, total: 2 })], "mail");
    expect(result.kind).toBe("idle");
    expect(result.labelKey).toBeNull();
  });

  it("reports syncing without a label of its own", () => {
    // The refresh button spins during a sync; a second spinner plus a caption
    // beside it says the same thing twice, so this state carries no text.
    const result = presentationFor([syncing("mail", { done: 120, total: 800 })], "mail");
    expect(result.kind).toBe("syncing");
    expect(result.labelKey).toBeNull();
    expect(result.detail).toBeNull();
  });

  it("reports syncing for any account of the module", () => {
    const result = presentationFor(
      [
        syncing("mail", { done: 10, total: 100 }, "acct-1"),
        syncing("mail", { done: 20, total: 50 }, "acct-2"),
      ],
      "mail",
    );
    expect(result.kind).toBe("syncing");
  });

  it("reports syncing even when no entry carries progress", () => {
    const result = presentationFor(
      [syncing("mail", null, "acct-1"), syncing("mail", null, "acct-2")],
      "mail",
    );
    expect(result.kind).toBe("syncing");
    expect(result.labelKey).toBeNull();
  });

  it("does not let another module's error leak in (the original bug)", () => {
    const statuses = [
      status({ module: "mail", state: "syncing", progress: { done: 3, total: 9 } }),
      status({
        module: "tasks",
        accountId: "acct-2",
        state: "error",
        message: "429 Too Many Requests",
      }),
    ];
    expect(presentationFor(statuses, "mail").kind).toBe("syncing");
    expect(presentationFor(statuses, "tasks")).toMatchObject({
      kind: "error",
      detail: "429 Too Many Requests",
    });
  });

  it("orders needsReauth above error above retrying above syncing", () => {
    const all = [
      syncing("mail", { done: 1, total: 4 }),
      status({ module: "mail", accountId: "acct-2", state: "retrying", message: "retry" }),
      status({ module: "mail", accountId: "acct-3", state: "error", message: "boom" }),
      status({ module: "mail", accountId: "acct-4", state: "needsReauth", message: "auth" }),
    ];
    expect(presentationFor(all, "mail").kind).toBe("needsReauth");
    expect(presentationFor(all.slice(0, 3), "mail").kind).toBe("error");
    expect(presentationFor(all.slice(0, 2), "mail").kind).toBe("retrying");
    expect(presentationFor(all.slice(0, 1), "mail").kind).toBe("syncing");
  });

  it("carries the first non-empty raw message as detail", () => {    const result = presentationFor(
      [
        status({ module: "mail", accountId: "acct-1", state: "error", message: "  " }),
        status({ module: "mail", accountId: "acct-2", state: "error", message: "503 Unavailable" }),
        status({ module: "mail", accountId: "acct-3", state: "error", message: "second" }),
      ],
      "mail",
    );
    expect(result.detail).toBe("503 Unavailable");
  });

  it("uses namespaced label keys and never mentions offline", () => {
    const states: SyncState[] = ["idle", "syncing", "retrying", "error", "needsReauth"];
    for (const state of states) {
      const result = presentationFor([status({ module: "mail", state })], "mail");
      expect(result.kind).not.toBe("offline");
      if (result.labelKey !== null) {
        expect(result.labelKey.startsWith("sync.")).toBe(true);
        expect(result.labelKey.toLowerCase()).not.toContain("offline");
      }
    }
  });

  it("treats a legacy offline status as idle instead of surfacing it", () => {
    // The backend no longer emits "offline"; a stale status must not resurrect it.
    const legacy = status({ module: "mail", state: "offline" as SyncState });
    const result = presentationFor([legacy], "mail");
    expect(result.kind).toBe("idle");
    expect(result.labelKey).toBeNull();
  });

  describe("account scope", () => {
    it("ignores failures on accounts outside the current view", () => {
      // The bug this covers: looking at one account's inbox while another
      // account failed made the header claim the visible account was broken.
      const statuses = [
        status({ module: "mail", accountId: "acct-2", state: "error", message: "auth" }),
      ];
      expect(presentationFor(statuses, "mail", ["acct-1"]).kind).toBe("idle");
      expect(presentationFor(statuses, "mail", ["acct-1", "acct-2"]).kind).toBe("error");
    });

    it("ignores a sync running on accounts outside the current view", () => {
      const statuses = [syncing("mail", null, "acct-2")];
      expect(presentationFor(statuses, "mail", ["acct-1"]).kind).toBe("idle");
      expect(presentationFor(statuses, "mail", ["acct-2"]).kind).toBe("syncing");
    });

    it("keeps the highest-priority state within the scope only", () => {
      const statuses = [
        status({ module: "mail", accountId: "acct-1", state: "retrying", message: "retry" }),
        status({ module: "mail", accountId: "acct-2", state: "needsReauth", message: "auth" }),
      ];
      expect(presentationFor(statuses, "mail", ["acct-1"]).kind).toBe("retrying");
      expect(presentationFor(statuses, "mail", ["acct-2"]).kind).toBe("needsReauth");
    });

    it("treats an empty scope as nothing to report", () => {
      const statuses = [
        status({ module: "mail", accountId: "acct-1", state: "error", message: "boom" }),
      ];
      expect(presentationFor(statuses, "mail", []).kind).toBe("idle");
    });

    it("stays module-wide when no scope is given", () => {
      const statuses = [
        status({ module: "mail", accountId: "acct-2", state: "error", message: "boom" }),
      ];
      expect(presentationFor(statuses, "mail").kind).toBe("error");
    });
  });
});
