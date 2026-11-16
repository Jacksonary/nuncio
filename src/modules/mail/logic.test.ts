import { describe, expect, it } from "vitest";
import type { MessageSummary } from "@/ipc/types";
import {
  EMPTY_SELECTION,
  aggregateUnread,
  buildFolderTree,
  buildMessageRows,
  dateGroupKey,
  displayFolderTree,
  primaryNodes,
  secondaryNodes,
  messageIds,
  nextAfterRemoval,
  parseUnifiedFolderId,
  selectionReducer,
  splitAccountFolders,
  unifiedFolderId,
  type FolderNode,
  type SelectionState,
} from "./logic";

function makeSummary(id: string, date: string): MessageSummary {
  return {
    id,
    accountId: "acct-1",
    folderId: "folder-1",
    threadId: `thread-${id}`,
    subject: `Subject ${id}`,
    from: { name: `Sender ${id}`, email: `${id}@example.com` },
    to: [],
    cc: [],
    date,
    preview: "preview",
    seen: false,
    flagged: false,
    answered: false,
    forwarded: false,
    isDraft: false,
    hasAttachments: false,
    importance: "normal",
    size: 100,
    threadCount: 1,
    threadUnread: 0,
  };
}

const NOW = new Date(2026, 10, 12, 12, 0, 0); // Thu 2026-11-12

describe("dateGroupKey", () => {
  it("buckets dates into Windows Mail style groups", () => {
    expect(dateGroupKey(new Date(2026, 10, 12, 8), NOW)).toBe("today");
    expect(dateGroupKey(new Date(2026, 10, 11, 23), NOW)).toBe("yesterday");
    expect(dateGroupKey(new Date(2026, 10, 10, 9), NOW)).toBe("thisWeek");
    expect(dateGroupKey(new Date(2026, 10, 4, 9), NOW)).toBe("lastWeek");
    expect(dateGroupKey(new Date(2026, 10, 1, 9), NOW)).toBe("thisMonth");
    expect(dateGroupKey(new Date(2026, 9, 20, 9), NOW)).toBe("earlier");
  });

  it("treats an invalid date as earlier", () => {
    expect(dateGroupKey("not-a-date", NOW)).toBe("earlier");
  });
});

describe("buildMessageRows", () => {
  it("emits a header per group and counts its messages", () => {
    const rows = buildMessageRows(
      [
        makeSummary("a", new Date(2026, 10, 12, 9).toISOString()),
        makeSummary("b", new Date(2026, 10, 12, 8).toISOString()),
        makeSummary("c", new Date(2026, 10, 11, 9).toISOString()),
        makeSummary("d", new Date(2026, 9, 20, 9).toISOString()),
      ],
      NOW,
    );
    expect(rows.map((row) => row.kind)).toEqual([
      "header",
      "message",
      "message",
      "header",
      "message",
      "header",
      "message",
    ]);
    const headers = rows.filter((row) => row.kind === "header");
    expect(headers.map((row) => (row.kind === "header" ? row.group : ""))).toEqual([
      "today",
      "yesterday",
      "earlier",
    ]);
    expect(headers.map((row) => (row.kind === "header" ? row.count : 0))).toEqual([2, 1, 1]);
    expect(messageIds(rows)).toEqual(["a", "b", "c", "d"]);
  });

  it("returns no rows for an empty list", () => {
    expect(buildMessageRows([], NOW)).toEqual([]);
  });
});

describe("selectionReducer", () => {
  const ordered = ["a", "b", "c", "d", "e"];

  it("replaces the selection and remembers the anchor", () => {
    const state = selectionReducer(EMPTY_SELECTION, { type: "replace", ids: ["b"] });
    expect(state).toEqual({ ids: ["b"], anchor: "b" });
  });

  it("toggles individual ids", () => {
    let state: SelectionState = { ids: ["a"], anchor: "a" };
    state = selectionReducer(state, { type: "toggle", id: "c" });
    expect(state).toEqual({ ids: ["a", "c"], anchor: "c" });
    state = selectionReducer(state, { type: "toggle", id: "a" });
    expect(state).toEqual({ ids: ["c"], anchor: "c" });
  });

  it("selects a contiguous range from the anchor", () => {
    let state: SelectionState = { ids: ["b"], anchor: "b" };
    state = selectionReducer(state, { type: "range", id: "d", ordered });
    expect(state.ids).toEqual(["b", "c", "d"]);
    expect(state.anchor).toBe("b");
    state = selectionReducer(state, { type: "range", id: "a", ordered });
    expect(state.ids).toEqual(["a", "b"]);
  });

  it("removes ids and clears the anchor when it disappears", () => {
    const state: SelectionState = { ids: ["a", "b", "c"], anchor: "b" };
    expect(selectionReducer(state, { type: "remove", ids: ["b"] })).toEqual({
      ids: ["a", "c"],
      anchor: null,
    });
    expect(selectionReducer(state, { type: "clear" })).toEqual(EMPTY_SELECTION);
  });
});

describe("nextAfterRemoval", () => {
  const ordered = ["a", "b", "c", "d"];

  it("advances to the next message", () => {
    expect(nextAfterRemoval(ordered, new Set(["b"]), "b", "next")).toBe("c");
  });

  it("falls back to the previous message at the end", () => {
    expect(nextAfterRemoval(ordered, new Set(["d"]), "d", "next")).toBe("c");
    expect(nextAfterRemoval(ordered, new Set(["c"]), "c", "previous")).toBe("b");
  });

  it("returns null in list mode or when everything was removed", () => {
    expect(nextAfterRemoval(ordered, new Set(["b"]), "b", "list")).toBeNull();
    expect(nextAfterRemoval(ordered, new Set(ordered), "b", "next")).toBeNull();
  });
});

describe("unified folder ids", () => {
  it("round-trips roles", () => {
    expect(parseUnifiedFolderId(unifiedFolderId("inbox"))).toBe("inbox");
    expect(parseUnifiedFolderId(unifiedFolderId("flagged"))).toBe("flagged");
    expect(parseUnifiedFolderId("folder-id")).toBeNull();
    expect(parseUnifiedFolderId("unified:bogus")).toBeNull();
  });
});

describe("folder tree display normalisation", () => {
  const folder = (over: Partial<import("@/ipc/types").Folder> & { id: string }): import("@/ipc/types").Folder => ({
    accountId: "acct-1",
    parentId: null,
    remotePath: over.remotePath ?? over.id,
    name: over.name ?? over.id,
    role: over.role ?? "other",
    unreadCount: 0,
    totalCount: 0,
    isFavorite: false,
    syncEnabled: true,
    isSelectable: true,
    sortOrder: 0,
    lastSyncAt: null,
    ...over,
  });

  it("hoists Gmail system folders out of the [Gmail] container", () => {
    const gmail = folder({ id: "g", name: "[Gmail]", remotePath: "[Gmail]", isSelectable: false });
    const tree = buildFolderTree([
      folder({ id: "inbox", role: "inbox" }),
      gmail,
      folder({ id: "drafts", role: "drafts", parentId: "g", name: "草稿" }),
      folder({ id: "sent", role: "sent", parentId: "g", name: "已发邮件" }),
    ]);
    const shown = displayFolderTree(tree);
    const ids = shown.map((node) => node.folder.id);
    expect(ids).not.toContain("g");
    expect(ids).toContain("drafts");
    expect(ids).toContain("sent");
  });

  it("keeps a single folder per role", () => {
    const tree = buildFolderTree([
      folder({ id: "sent-root", role: "sent", name: "Sent" }),
      folder({ id: "sent-gmail", role: "sent", name: "已发邮件" }),
      folder({ id: "junk", role: "junk", name: "垃圾邮件" }),
      folder({ id: "ads", role: "junk", name: "广告邮件" }),
    ]);
    const shown = displayFolderTree(tree);
    const roles = shown.map((node) => node.folder.role).filter((role) => role !== "other");
    expect(roles.filter((role) => role === "sent")).toHaveLength(1);
    expect(roles.filter((role) => role === "junk")).toHaveLength(1);
  });

  it("keeps only inbox and drafts on top and never repeats them under More", () => {
    const tree = buildFolderTree([
      folder({ id: "inbox", role: "inbox" }),
      folder({ id: "trash", role: "trash" }),
      folder({ id: "custom", role: "other", name: "订阅邮件" }),
      folder({ id: "g", name: "[Gmail]", remotePath: "[Gmail]", isSelectable: false }),
      folder({ id: "sent", role: "sent", parentId: "g" }),
    ]);
    const shown = displayFolderTree(tree);
    const primary = primaryNodes(shown);
    const secondary = secondaryNodes(shown, primary);
    const primaryIds = primary.map((node) => node.folder.id);
    const secondaryIds = secondary.map((node) => node.folder.id);
    expect(primaryIds).toEqual(["inbox"]);
    expect(secondaryIds).toEqual(expect.arrayContaining(["trash", "sent", "custom"]));
    for (const id of primaryIds) expect(secondaryIds).not.toContain(id);
  });
});

describe("aggregateUnread", () => {
  const node = (id: string, unreadCount: number, children: FolderNode[] = []): FolderNode => ({
    folder: {
      id,
      accountId: "acct-1",
      parentId: null,
      remotePath: id,
      name: id,
      role: "other",
      unreadCount,
      totalCount: 0,
      isFavorite: false,
      syncEnabled: true,
      isSelectable: true,
      sortOrder: 0,
      lastSyncAt: null,
    },
    children,
  });

  it("sums the nodes themselves and every descendant", () => {
    const tree = [
      node("a", 2, [node("a1", 3), node("a2", 0, [node("a2x", 4)])]),
      node("b", 1),
    ];
    expect(aggregateUnread(tree)).toBe(2 + 3 + 4 + 1);
  });

  it("returns 0 for an empty section", () => {
    expect(aggregateUnread([])).toBe(0);
  });

  it("adds each nested row's own count, not only the roots", () => {
    const tree = [node("parent", 5, [node("child", 5)])];
    expect(aggregateUnread(tree)).toBe(10);
  });
});

describe("splitAccountFolders", () => {
  const folder = (over: Partial<import("@/ipc/types").Folder> & { id: string }): import("@/ipc/types").Folder => ({
    accountId: "acct-1",
    parentId: null,
    remotePath: over.remotePath ?? over.id,
    name: over.name ?? over.id,
    role: over.role ?? "other",
    unreadCount: 0,
    totalCount: 0,
    isFavorite: false,
    syncEnabled: true,
    isSelectable: true,
    sortOrder: 0,
    lastSyncAt: null,
    ...over,
  });

  it("folds sent, trash and custom folders behind More", () => {
    const tree = displayFolderTree(
      buildFolderTree([
        folder({ id: "inbox", role: "inbox" }),
        folder({ id: "drafts", role: "drafts" }),
        folder({ id: "sent", role: "sent" }),
        folder({ id: "trash", role: "trash" }),
        folder({ id: "custom" }),
      ]),
    );
    const { primary, secondary } = splitAccountFolders(tree);
    expect(primary.map((entry) => entry.folder.id)).toEqual(["inbox", "drafts"]);
    expect(secondary.map((entry) => entry.folder.id)).toEqual(["sent", "trash", "custom"]);
  });

  it("promotes a non-empty outbox to the end of the primary section", () => {
    const tree = displayFolderTree(
      buildFolderTree([
        folder({ id: "inbox", role: "inbox" }),
        folder({ id: "sent", role: "sent" }),
        folder({ id: "outbox", role: "outbox", totalCount: 2 }),
      ]),
    );
    const { primary, secondary } = splitAccountFolders(tree);
    expect(primary.map((entry) => entry.folder.id)).toEqual(["inbox", "outbox"]);
    expect(secondary.map((entry) => entry.folder.id)).toEqual(["sent"]);
  });

  it("folds an empty outbox behind More", () => {
    const tree = displayFolderTree(
      buildFolderTree([
        folder({ id: "inbox", role: "inbox" }),
        folder({ id: "outbox", role: "outbox", totalCount: 0 }),
      ]),
    );
    const { primary, secondary } = splitAccountFolders(tree);
    expect(primary.map((entry) => entry.folder.id)).toEqual(["inbox"]);
    expect(secondary.map((entry) => entry.folder.id)).toEqual(["outbox"]);
  });

  it("keeps favourites on top next to inbox and drafts", () => {
    const tree = displayFolderTree(
      buildFolderTree([
        folder({ id: "inbox", role: "inbox" }),
        folder({ id: "sent", role: "sent" }),
        folder({ id: "starred", name: "Starred", isFavorite: true }),
      ]),
    );
    const { primary, secondary } = splitAccountFolders(tree);
    expect(primary.map((entry) => entry.folder.id)).toEqual(["inbox", "starred"]);
    expect(secondary.map((entry) => entry.folder.id)).toEqual(["sent"]);
  });

  it("leaves no More row for a POP3 account with a single inbox", () => {
    const tree = displayFolderTree(buildFolderTree([folder({ id: "inbox", role: "inbox" })]));
    const { primary, secondary } = splitAccountFolders(tree);
    expect(primary.map((entry) => entry.folder.id)).toEqual(["inbox"]);
    expect(secondary).toEqual([]);
    expect(primary).toHaveLength(1);
  });
});
