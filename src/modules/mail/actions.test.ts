import { describe, expect, it, vi } from "vitest";
import type { MessageSummary } from "@/ipc/types";

// Thread members the mocked IPC returns, keyed by "account:thread".
const { threadMembers } = vi.hoisted(() => ({
  threadMembers: new Map<string, string[]>([
    ["acc:t1", ["a", "b", "c"]],
    ["acc:t2", ["x", "y"]],
  ]),
}));

vi.mock("@/ipc/api", () => ({
  isIpcError: () => false,
  messages: {
    thread: async (accountId: string, threadId: string) =>
      (threadMembers.get(`${accountId}:${threadId}`) ?? []).map((id) => ({ id })),
  },
}));

import {
  canOfferUndo,
  deletePlan,
  mergeMessageIds,
  needsDeleteConfirmation,
  normalizeAction,
  resolveActionIds,
  threadRefsFromSummaries,
} from "./actions";

function summary(
  partial: Partial<MessageSummary> & Pick<MessageSummary, "id" | "folderId">,
): MessageSummary {
  return {
    accountId: "acc",
    threadId: partial.id,
    subject: "",
    from: null,
    to: [],
    cc: [],
    date: "2026-11-18T08:00:00Z",
    preview: "",
    seen: true,
    flagged: false,
    answered: false,
    forwarded: false,
    isDraft: false,
    hasAttachments: false,
    importance: "normal",
    size: 0,
    threadCount: 1,
    threadUnread: 0,
    ...partial,
  };
}

const roles = new Map([
  ["inbox", "inbox"],
  ["drafts", "drafts"],
  ["trash", "trash"],
  ["junk", "junk"],
]);

describe("deletePlan", () => {
  const roleOf = (folderId: string) => roles.get(folderId);

  it("moves a message from a normal folder to Trash", () => {
    expect(
      deletePlan([summary({ id: "a", folderId: "inbox" })], roleOf),
    ).toEqual({ move: 1, permanent: 0 });
  });

  it("deletes Trash and Junk messages outright", () => {
    expect(deletePlan([summary({ id: "a", folderId: "trash" })], roleOf)).toEqual({
      move: 0,
      permanent: 1,
    });
    expect(deletePlan([summary({ id: "b", folderId: "junk" })], roleOf)).toEqual({
      move: 0,
      permanent: 1,
    });
  });

  it("treats a discarded draft as permanent even in the Drafts folder", () => {
    expect(
      deletePlan([summary({ id: "d", folderId: "drafts", isDraft: true })], roleOf),
    ).toEqual({ move: 0, permanent: 1 });
    // The folder role alone is enough: a stale is_draft flag must not turn
    // "discard" into a Trash move that keeps the row alive.
    expect(deletePlan([summary({ id: "e", folderId: "drafts" })], roleOf)).toEqual({
      move: 0,
      permanent: 1,
    });
  });

  it("reports both counts for a mixed selection", () => {
    expect(
      deletePlan(
        [
          summary({ id: "a", folderId: "inbox" }),
          summary({ id: "b", folderId: "inbox" }),
          summary({ id: "c", folderId: "trash" }),
        ],
        roleOf,
      ),
    ).toEqual({ move: 2, permanent: 1 });
  });
});

describe("normalizeAction", () => {
  const inbox = summary({ id: "a", folderId: "inbox" });
  const trash = summary({ id: "b", folderId: "trash" });
  const summaries = new Map([
    [inbox.id, inbox],
    [trash.id, trash],
  ]);

  it("refuses to escalate a mixed selection to a permanent delete", () => {
    // The caller derived `permanent` from the selected sidebar folder (Trash),
    // but one of the messages really lives in the Inbox.
    expect(
      normalizeAction({ kind: "delete", permanent: true }, ["a", "b"], summaries, roles),
    ).toEqual({ kind: "delete", permanent: false });
  });

  it("keeps an explicit permanent delete when every message is already there", () => {
    expect(
      normalizeAction({ kind: "delete", permanent: true }, ["b"], summaries, roles),
    ).toEqual({ kind: "delete", permanent: true });
  });

  it("leaves non-delete actions alone", () => {
    const action = { kind: "archive" as const };
    expect(normalizeAction(action, ["a"], summaries, roles)).toBe(action);
  });
});

describe("thread expansion", () => {
  it("takes one ref per selected conversation and ignores plain rows", () => {
    const summaries = [
      summary({ id: "a", folderId: "inbox", threadId: "t1", threadCount: 3 }),
      summary({ id: "x", folderId: "inbox", threadId: "t2" }),
      summary({ id: "y", folderId: "inbox", threadId: "t1", threadCount: 3 }),
    ];
    expect(threadRefsFromSummaries(summaries, ["a", "x"])).toEqual([
      { accountId: "acc", threadId: "t1" },
    ]);
    // Two selected rows of the same conversation must not fetch it twice.
    expect(threadRefsFromSummaries(summaries, ["a", "y"])).toEqual([
      { accountId: "acc", threadId: "t1" },
    ]);
  });

  it("merges base ids with thread members without duplicates", () => {
    expect(mergeMessageIds(["a"], [["a", "b", "c"]])).toEqual(["a", "b", "c"]);
  });

  it("single-select and multi-select of the same conversations resolve the same set", async () => {
    const action = { kind: "delete", permanent: false } as const;
    // One row selected: its conversation expands.
    const single = await resolveActionIds({
      ids: ["a"],
      action,
      threads: [{ accountId: "acc", threadId: "t1" }],
    });
    expect(single).toEqual(["a", "b", "c"]);

    // Two conversation rows selected: both expand, in one call.
    const multi = await resolveActionIds({
      ids: ["a", "x"],
      action,
      threads: [
        { accountId: "acc", threadId: "t1" },
        { accountId: "acc", threadId: "t2" },
      ],
    });
    expect(new Set(multi)).toEqual(new Set(["a", "b", "c", "x", "y"]));

    // The multi-select result is exactly the union of the two singles, so a
    // row deletes the same messages whether or not another row is selected.
    const secondSingle = await resolveActionIds({
      ids: ["x"],
      action,
      threads: [{ accountId: "acc", threadId: "t2" }],
    });
    expect(new Set(multi)).toEqual(new Set([...single, ...secondSingle]));
  });

  it("infers conversations for a multi-selection from the list summaries", async () => {
    const summaries = [
      summary({ id: "a", folderId: "inbox", threadId: "t1", threadCount: 3 }),
      summary({ id: "x", folderId: "inbox", threadId: "t2", threadCount: 2 }),
    ];
    await expect(
      resolveActionIds({ ids: ["a", "x"], action: { kind: "archive" } }, summaries),
    ).resolves.toEqual(["a", "x", "b", "c", "y"]);
  });

  it("returns the plain ids when no conversations are hinted", async () => {
    await expect(
      resolveActionIds({ ids: ["a"], action: { kind: "archive" } }),
    ).resolves.toEqual(["a"]);
  });

  it("does not infer a conversation for a single message opened in the reader", async () => {
    const summaries = [summary({ id: "a", folderId: "inbox", threadId: "t1", threadCount: 3 })];
    await expect(
      resolveActionIds({ ids: ["a"], action: { kind: "delete", permanent: false } }, summaries),
    ).resolves.toEqual(["a"]);
  });
});

describe("needsDeleteConfirmation", () => {
  it("honours the preference for a plain move to Trash", () => {
    expect(needsDeleteConfirmation(false, { move: 3, permanent: 0 })).toBe(false);
    expect(needsDeleteConfirmation(true, { move: 3, permanent: 0 })).toBe(true);
  });

  it("never goes silent when anything is deleted permanently", () => {
    expect(needsDeleteConfirmation(false, { move: 0, permanent: 2 })).toBe(true);
    expect(needsDeleteConfirmation(false, { move: 2, permanent: 1 })).toBe(true);
  });
});

describe("canOfferUndo", () => {
  it("offers undo only when the backend recorded an entry", () => {
    expect(canOfferUndo({ kind: "archive" }, "undo-1")).toBe(true);
    expect(canOfferUndo({ kind: "archive" }, "")).toBe(false);
    expect(canOfferUndo({ kind: "archive" }, null)).toBe(false);
    expect(canOfferUndo({ kind: "archive" }, undefined)).toBe(false);
  });

  it("never offers undo for a permanent delete", () => {
    expect(canOfferUndo({ kind: "delete", permanent: true }, "undo-1")).toBe(false);
    expect(canOfferUndo({ kind: "delete", permanent: false }, "undo-1")).toBe(true);
  });
});
