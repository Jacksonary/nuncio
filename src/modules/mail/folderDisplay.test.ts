/**
 * Per-provider simulations of the folder sidebar.
 *
 * Each test feeds a realistic `LIST` result (raw modified UTF-7 in
 * `remotePath`, decoded text in `name`, exactly like `sync::folders::sync`
 * stores it) through `buildFolderTree` → `displayFolderTree` →
 * `primaryNodes`/`secondaryNodes` and asserts what the user actually sees.
 *
 * The `role` field mirrors `role_for` in
 * `src-tauri/crates/mail/src/sync/folders.rs`; the matching Rust tests assert
 * the same names map to the same roles, so the two sides cannot drift.
 */
import { describe, expect, it } from "vitest";
import type { Folder, FolderRole } from "@/ipc/types";
import {
  buildFolderTree,
  displayFolderTree,
  primaryNodes,
  secondaryNodes,
  type FolderNode,
} from "./logic";
import { folderBadgeCount } from "./Sidebar";

interface SimFolder {
  /** Raw remote path as returned by `LIST` (may be modified UTF-7). */
  path: string;
  /** Decoded display name, i.e. the leaf of `path` after decoding. */
  name: string;
  role: FolderRole;
  selectable?: boolean;
}

function parentPath(path: string, delimiter: string): string | null {
  if (!delimiter) return null;
  const trimmed = path.replace(new RegExp(`${delimiter.replace(".", "\\.")}+$`), "");
  const index = trimmed.lastIndexOf(delimiter);
  if (index <= 0) return null;
  return trimmed.slice(0, index);
}

function toFolders(provider: string, fixtures: SimFolder[], delimiter = "/"): Folder[] {
  const idByPath = new Map(fixtures.map((fixture, index) => [fixture.path, `${provider}-${index}`]));
  return fixtures.map((fixture, index) => {
    const parent = parentPath(fixture.path, delimiter);
    return {
      id: idByPath.get(fixture.path) as string,
      accountId: provider,
      parentId: parent !== null ? (idByPath.get(parent) ?? null) : null,
      remotePath: fixture.path,
      name: fixture.name,
      role: fixture.role,
      unreadCount: 0,
      totalCount: 0,
      isFavorite: false,
      syncEnabled: true,
      isSelectable: fixture.selectable ?? true,
      sortOrder: index,
      lastSyncAt: null,
    } satisfies Folder;
  });
}

function names(nodes: FolderNode[]): string[] {
  return nodes.map((node) => node.folder.name);
}

function flatIds(nodes: FolderNode[]): string[] {
  const ids: string[] = [];
  for (const node of nodes) {
    ids.push(node.folder.id);
    ids.push(...flatIds(node.children));
  }
  return ids;
}

/** Every rendered row with its indentation, for "is it reachable?" checks. */
function flatNames(nodes: FolderNode[], depth = 0): string[] {
  const result: string[] = [];
  for (const node of nodes) {
    result.push(`${"  ".repeat(depth)}${node.folder.name}`);
    result.push(...flatNames(node.children, depth + 1));
  }
  return result;
}

/**
 * Render a provider's folder list and assert the invariants that hold for
 * every account:
 *
 * - no row is drawn in both sections,
 * - no user folder (`other`) is lost,
 * - every role keeps at least one visible folder,
 * - no `\Noselect`-free user folder is orphaned by hoisting/dedupe.
 */
function render(provider: string, fixtures: SimFolder[], delimiter = "/") {
  const folders = toFolders(provider, fixtures, delimiter);
  const tree = displayFolderTree(buildFolderTree(folders));
  const primary = primaryNodes(tree);
  const secondary = secondaryNodes(tree, primary);

  const primaryIds = flatIds(primary);
  const secondaryIds = flatIds(secondary);

  // No folder may appear in both sections.
  expect(primaryIds.filter((id) => secondaryIds.includes(id))).toEqual([]);
  const visible = new Set([...primaryIds, ...secondaryIds]);

  for (const folder of folders) {
    if (folder.role === "other") {
      // Only `[Gmail]`-style containers may disappear: they are grouping
      // nodes whose children are hoisted to the account root.
      const isContainer = /^\[(gmail|google mail)\]$/i.test(folder.name.trim());
      if (!isContainer) {
        expect(visible.has(folder.id), `${folder.name} should stay visible`).toBe(true);
      }
    }
  }

  const visibleRoles = new Set(
    [...primary, ...secondary]
      .flatMap((node) => [node.folder, ...node.children.map((child) => child.folder)])
      .map((folder) => folder.role),
  );
  for (const fixture of fixtures) {
    if (fixture.role === "other") continue;
    expect(
      visibleRoles.has(fixture.role),
      `role ${fixture.role} should keep a visible folder`,
    ).toBe(true);
  }

  return {
    tree,
    primary,
    secondary,
    primaryNames: names(primary),
    secondaryNames: names(secondary),
    /**
     * Every row in "More" with its full path, so a nested folder orphaned by
     * hoisting/dedupe shows up as its own top-level row rather than vanishing.
     */
    allNames: [...flatNames(primary), ...flatNames(secondary)],
    childNames: (name: string): string[] => {
      const find = (nodes: FolderNode[]): FolderNode | undefined => {
        for (const node of nodes) {
          if (node.folder.name === name) return node;
          const nested = find(node.children);
          if (nested) return nested;
        }
        return undefined;
      };
      const node = find(tree);
      return node ? node.children.map((child) => child.folder.name) : [];
    },
  };
}

/* ─────────────────────────── Microsoft 365 ─────────────────────────── */

describe("Outlook.com / Microsoft 365", () => {
  const account: SimFolder[] = [
    { path: "INBOX", name: "INBOX", role: "inbox" },
    { path: "Archive", name: "Archive", role: "archive" },
    { path: "Conversation History", name: "Conversation History", role: "other" },
    { path: "Deleted Items", name: "Deleted Items", role: "trash" },
    { path: "Drafts", name: "Drafts", role: "drafts" },
    { path: "Junk Email", name: "Junk Email", role: "junk" },
    { path: "Notes", name: "Notes", role: "other" },
    { path: "Outbox", name: "Outbox", role: "other" },
    { path: "Sent Items", name: "Sent Items", role: "sent" },
    { path: "Projects", name: "Projects", role: "other" },
    { path: "Projects/Alpha", name: "Alpha", role: "other" },
  ];

  it("keeps only Inbox and Drafts on top and folds the system folders into More", () => {
    const view = render("outlook", account);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual([
      "Sent Items",
      "Archive",
      "Junk Email",
      "Deleted Items",
      "Conversation History",
      "Notes",
      "Outbox",
      "Projects",
    ]);
    expect(view.childNames("Projects")).toEqual(["Alpha"]);
  });

  it("keeps subfolders of a folded folder reachable", () => {
    const view = render("outlook-nested", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Sent Items", name: "Sent Items", role: "sent" },
      { path: "Sent Items/2026", name: "2026", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX"]);
    expect(view.secondaryNames).toEqual(["Sent Items"]);
    expect(view.childNames("Sent Items")).toEqual(["2026"]);
  });
});

/* ─────────────────────────────── iCloud ────────────────────────────── */

describe("iCloud", () => {
  const account: SimFolder[] = [
    { path: "INBOX", name: "INBOX", role: "inbox" },
    { path: "Archive", name: "Archive", role: "archive" },
    { path: "Deleted Messages", name: "Deleted Messages", role: "trash" },
    { path: "Drafts", name: "Drafts", role: "drafts" },
    { path: "Junk", name: "Junk", role: "junk" },
    { path: "Notes", name: "Notes", role: "other" },
    { path: "Sent Messages", name: "Sent Messages", role: "sent" },
    { path: "Receipts", name: "Receipts", role: "other" },
  ];

  it("keeps Deleted Messages and Archive at the account root, folded into More", () => {
    const view = render("icloud", account);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual([
      "Sent Messages",
      "Archive",
      "Junk",
      "Deleted Messages",
      "Notes",
      "Receipts",
    ]);
    // Nothing was hoisted: both are roots of the displayed tree.
    expect(view.tree.map((node) => node.folder.name)).toEqual(
      expect.arrayContaining(["Archive", "Deleted Messages"]),
    );
  });

  it("keeps Archive subfolders visible", () => {
    const view = render("icloud-archive", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Archive", name: "Archive", role: "archive" },
      { path: "Archive/2025", name: "2025", role: "other" },
    ]);
    expect(view.childNames("Archive")).toEqual(["2025"]);
  });
});

/* ────────────────────────── Yahoo / AOL / GMX ──────────────────────── */

describe("Yahoo", () => {
  it("maps Draft / Bulk Mail and keeps user folders under More", () => {
    const view = render("yahoo", [
      { path: "INBOX", name: "Inbox", role: "inbox" },
      { path: "Archive", name: "Archive", role: "archive" },
      { path: "Bulk Mail", name: "Bulk Mail", role: "junk" },
      { path: "Draft", name: "Draft", role: "drafts" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Trash", name: "Trash", role: "trash" },
      { path: "Family", name: "Family", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["Inbox", "Draft"]);
    expect(view.secondaryNames).toEqual(["Sent", "Archive", "Bulk Mail", "Trash", "Family"]);
  });
});

describe("AOL", () => {
  it("keeps Saved / Recently Deleted under More", () => {
    const view = render("aol", [
      { path: "Inbox", name: "Inbox", role: "inbox" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Spam", name: "Spam", role: "junk" },
      { path: "Trash", name: "Trash", role: "trash" },
      { path: "Saved", name: "Saved", role: "other" },
      { path: "Recently Deleted", name: "Recently Deleted", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["Inbox", "Drafts"]);
    expect(view.secondaryNames).toEqual(["Sent", "Spam", "Trash", "Saved", "Recently Deleted"]);
  });
});

describe("GMX / mail.ru", () => {
  it("shows only Inbox and Drafts on top and keeps the system folders in More", () => {
    const view = render("gmx", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Spam", name: "Spam", role: "junk" },
      { path: "Trash", name: "Trash", role: "trash" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual(["Sent", "Spam", "Trash"]);
  });
});

/* ─────────────────────────── Chinese providers ─────────────────────── */

describe("QQ / Foxmail (modified UTF-7 on the wire)", () => {
  it("decodes the names and keeps user folders under the grouping container", () => {
    const view = render("qq", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "&XfJT0ZAB-", name: "已发送", role: "sent" },
      { path: "&g0l6P3ux-", name: "草稿箱", role: "drafts" },
      { path: "&XfJSIJZk-", name: "已删除", role: "trash" },
      { path: "&V4NXPpCuTvY-", name: "垃圾邮件", role: "junk" },
      {
        path: "&mHlM0Gs-",
        name: "其他文件夹",
        role: "other",
        selectable: false,
      },
      { path: "&mHlM0Gs-/&V4NXPpCuTvY2A-", name: "订阅邮件", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "草稿箱"]);
    expect(view.secondaryNames).toEqual(["已发送", "垃圾邮件", "已删除", "其他文件夹"]);
    expect(view.childNames("其他文件夹")).toEqual(["订阅邮件"]);
  });
});

describe("Tencent Exmail", () => {
  it("maps 已发送邮件", () => {
    const view = render("exmail", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "&XfJT0ZABkK5O9g-", name: "已发送邮件", role: "sent" },
      { path: "&g0l6P3ux-", name: "草稿箱", role: "drafts" },
      { path: "&XfJSIJZkkK5O9g-", name: "已删除邮件", role: "trash" },
      { path: "&V4NXPpCuTvY-", name: "垃圾邮件", role: "junk" },
      { path: "&ZLm8SA-", name: "日报", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "草稿箱"]);
    expect(view.secondaryNames).toEqual(["已发送邮件", "垃圾邮件", "已删除邮件", "日报"]);
  });
});

describe("Aliyun / Sina / Sohu", () => {
  it("maps 已发送 / 草稿箱 / 已删除 / 垃圾邮件", () => {
    const view = render("aliyun", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "&XfJT0ZAB-", name: "已发送", role: "sent" },
      { path: "&g0l6P3ux-", name: "草稿箱", role: "drafts" },
      { path: "&XfJSIJZk-", name: "已删除", role: "trash" },
      { path: "&V4NXPpCuTvY-", name: "垃圾邮件", role: "junk" },
      { path: "&V4NXPpCuTvY2A-", name: "订阅邮件", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "草稿箱"]);
    expect(view.secondaryNames).toEqual(["已发送", "垃圾邮件", "已删除", "订阅邮件"]);
  });

  it("maps 139's 发件箱 to Sent and 广告邮件 to Junk", () => {
    const view = render("139", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "&XfJT0ZCr-", name: "发件箱", role: "sent" },
      { path: "&g0l6P3ux-", name: "草稿箱", role: "drafts" },
      { path: "&XfJSIJZkkK5O9g-", name: "已删除邮件", role: "trash" },
      { path: "&V4NXPpCuTvY-", name: "垃圾邮件", role: "junk" },
      { path: "&mr3NqA-", name: "广告邮件", role: "junk" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "草稿箱"]);
    // The duplicate junk folder is folded away, but its children would not be.
    expect(view.secondaryNames).toEqual(["发件箱", "垃圾邮件", "已删除邮件"]);
  });
});

/* ──────────────────────── International providers ──────────────────── */

describe("Fastmail", () => {
  it("maps Junk Mail and nests user folders", () => {
    const view = render("fastmail", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Archive", name: "Archive", role: "archive" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Junk Mail", name: "Junk Mail", role: "junk" },
      { path: "Notes", name: "Notes", role: "other" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Templates", name: "Templates", role: "other" },
      { path: "Trash", name: "Trash", role: "trash" },
      { path: "Work", name: "Work", role: "other" },
      { path: "Work/2026", name: "2026", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual([
      "Sent",
      "Archive",
      "Junk Mail",
      "Trash",
      "Notes",
      "Templates",
      "Work",
    ]);
    expect(view.childNames("Work")).toEqual(["2026"]);
  });
});

describe("Zoho / Yandex", () => {
  it("maps Inbox / Spam / Templates", () => {
    const view = render("zoho", [
      { path: "Inbox", name: "Inbox", role: "inbox" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Spam", name: "Spam", role: "junk" },
      { path: "Templates", name: "Templates", role: "other" },
      { path: "Trash", name: "Trash", role: "trash" },
    ]);
    expect(view.primaryNames).toEqual(["Inbox", "Drafts"]);
    expect(view.secondaryNames).toEqual(["Sent", "Spam", "Trash", "Templates"]);
  });

  it("maps Yandex's Deleted Items", () => {
    const view = render("yandex", [
      { path: "INBOX", name: "Inbox", role: "inbox" },
      { path: "Deleted Items", name: "Deleted Items", role: "trash" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Spam", name: "Spam", role: "junk" },
      { path: "Templates", name: "Templates", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["Inbox", "Drafts"]);
    expect(view.secondaryNames).toEqual(["Sent", "Spam", "Deleted Items", "Templates"]);
  });
});

describe("Proton Bridge", () => {
  it("keeps Labels / Folders as containers and hides All Mail / Starred in More", () => {
    const view = render("proton", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "All Mail", name: "All Mail", role: "all" },
      { path: "Archive", name: "Archive", role: "archive" },
      { path: "Drafts", name: "Drafts", role: "drafts" },
      { path: "Folders", name: "Folders", role: "other", selectable: false },
      { path: "Folders/Receipts", name: "Receipts", role: "other" },
      { path: "Labels", name: "Labels", role: "other", selectable: false },
      { path: "Labels/Travel", name: "Travel", role: "other" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Spam", name: "Spam", role: "junk" },
      { path: "Starred", name: "Starred", role: "flagged" },
      { path: "Trash", name: "Trash", role: "trash" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual([
      "Sent",
      "Archive",
      "Spam",
      "Trash",
      "Starred",
      "All Mail",
      "Folders",
      "Labels",
    ]);
    expect(view.childNames("Folders")).toEqual(["Receipts"]);
    expect(view.childNames("Labels")).toEqual(["Travel"]);
  });
});

describe("GMX / mail.ru / Yandex (localised system folder names)", () => {
  it("maps GMX's German names", () => {
    const view = render("gmx-de", [
      { path: "Posteingang", name: "Posteingang", role: "inbox" },
      { path: "Gesendet", name: "Gesendet", role: "sent" },
      { path: "Entw&APw-rfe", name: "Entwürfe", role: "drafts" },
      { path: "Papierkorb", name: "Papierkorb", role: "trash" },
      { path: "Spamverdacht", name: "Spamverdacht", role: "junk" },
      { path: "Archiv", name: "Archiv", role: "archive" },
      { path: "Familie", name: "Familie", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["Posteingang", "Entwürfe"]);
    expect(view.secondaryNames).toEqual([
      "Gesendet",
      "Archiv",
      "Spamverdacht",
      "Papierkorb",
      "Familie",
    ]);
  });

  it("maps Yandex / mail.ru Cyrillic names", () => {
    const view = render("mailru", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "&BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-", name: "Отправленные", role: "sent" },
      { path: "&BCcENQRABD0EPgQyBDgEOgQ4-", name: "Черновики", role: "drafts" },
      { path: "&BBoEPgRABDcEOAQ9BDA-", name: "Корзина", role: "trash" },
      { path: "&BCEEPwQwBDw-", name: "Спам", role: "junk" },
      { path: "Рассылки", name: "Рассылки", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "Черновики"]);
    expect(view.secondaryNames).toEqual(["Отправленные", "Спам", "Корзина", "Рассылки"]);
  });
});

/* ────────────────────── Generic IMAP and edge cases ────────────────── */

describe("generic IMAP", () => {
  it("handles arbitrary nesting and role collisions", () => {
    const view = render("imap", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "INBOX/Newsletters", name: "Newsletters", role: "other" },
      { path: "Sent", name: "Sent", role: "sent" },
      { path: "Sent Items", name: "Sent Items", role: "sent" },
      { path: "Sent/2025", name: "2025", role: "other" },
      { path: "All", name: "All", role: "all" },
      { path: "All Mail", name: "All Mail", role: "all" },
      { path: "Projects/2026/Deep", name: "Deep", role: "other" },
      { path: "重要", name: "重要", role: "important" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX"]);
    // Exactly one folder per role survives; the collision does not duplicate.
    expect([...view.primaryNames, ...view.secondaryNames].filter((name) => name === "Sent")).toHaveLength(
      1,
    );
    // No folder is lost: the losing duplicate's children are re-parented.
    const secondaryRoles = view.secondary
      .flatMap((node) => [node.folder, ...node.children.map((child) => child.folder)])
      .map((folder) => folder.remotePath);
    expect(secondaryRoles).toContain("All Mail");
  });

  it("supports a non-slash delimiter", () => {
    const view = render(
      "imap-dot",
      [
        { path: "INBOX", name: "INBOX", role: "inbox" },
        { path: "INBOX.Sub", name: "Sub", role: "other" },
        { path: "Drafts", name: "Drafts", role: "drafts" },
        { path: "Sent", name: "Sent", role: "sent" },
        { path: "Trash", name: "Trash", role: "trash" },
        { path: "Projects.2026", name: "2026", role: "other" },
      ],
      ".",
    );
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual(["Sent", "Trash", "2026"]);
    expect(view.childNames("INBOX")).toEqual(["Sub"]);
  });

  it("hoists a Gmail-style container even when the server marks it selectable", () => {
    const view = render("gmail-selectable", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "[Gmail]", name: "[Gmail]", role: "other", selectable: true },
      { path: "[Gmail]/All Mail", name: "All Mail", role: "all" },
      { path: "[Gmail]/Drafts", name: "Drafts", role: "drafts" },
      { path: "[Gmail]/Important", name: "Important", role: "important" },
      { path: "[Gmail]/Sent Mail", name: "Sent Mail", role: "sent" },
      { path: "[Gmail]/Spam", name: "Spam", role: "junk" },
      { path: "[Gmail]/Starred", name: "Starred", role: "flagged" },
      { path: "[Gmail]/Trash", name: "Trash", role: "trash" },
      { path: "Receipts", name: "Receipts", role: "other" },
    ]);
    expect(view.primaryNames).toEqual(["INBOX", "Drafts"]);
    expect(view.secondaryNames).toEqual([
      "Sent Mail",
      "Spam",
      "Trash",
      "Starred",
      "Important",
      "All Mail",
      "Receipts",
    ]);
  });
});

describe("favourites and nested folders", () => {
  it("keeps the siblings of a favourited nested folder reachable", () => {
    const folders = toFolders("fav", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Projects", name: "Projects", role: "other" },
      { path: "Projects/Alpha", name: "Alpha", role: "other" },
      { path: "Projects/Beta", name: "Beta", role: "other" },
    ]);
    // Favourite the nested child only.
    folders[2].isFavorite = true;
    const tree = displayFolderTree(buildFolderTree(folders));
    const primary = primaryNodes(tree);
    const secondary = secondaryNodes(tree, primary);
    expect(flatNames(primary)).toEqual(["INBOX", "Projects", "  Alpha"]);
    // Beta is not favourited, so it lives under More — but it must not vanish.
    expect(flatNames(secondary)).toContain("Beta");
    const all = new Set([...flatNames(primary), ...flatNames(secondary)]);
    expect(all.has("Beta")).toBe(true);
  });
});

describe("POP3 and the local account", () => {
  it("renders a single primary inbox and no More section for POP3", () => {
    const view = render("pop3", [{ path: "INBOX", name: "Inbox", role: "inbox" }]);
    expect(view.primaryNames).toEqual(["Inbox"]);
    expect(view.secondaryNames).toEqual([]);
  });

  it("lists local-only folders under More instead of dropping them", () => {
    const view = render("local", [
      { path: "local:notes", name: "Notes", role: "other" },
      { path: "local:archive-2025", name: "Archive 2025", role: "other" },
    ]);
    expect(view.primaryNames).toEqual([]);
    expect(view.secondaryNames.sort()).toEqual(["Archive 2025", "Notes"].sort());
  });

  it("does not recurse forever on a parent/child cycle", () => {
    const folders = toFolders("cycle", [
      { path: "a", name: "A", role: "other" },
      { path: "b", name: "B", role: "other" },
    ]);
    // Pathological data: b is a's parent and a is b's parent.
    folders[0].parentId = folders[1].id;
    folders[1].parentId = folders[0].id;
    expect(() => displayFolderTree(buildFolderTree(folders))).not.toThrow();
  });
});

describe("structural edge cases", () => {
  it("turns a folder whose parent is missing into a root instead of hiding it", () => {
    const folders = toFolders("orphan", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Gone/Kept", name: "Kept", role: "other" },
    ]);
    // Simulate a stale parent_id: the parent row no longer exists.
    folders[1].parentId = "missing-parent";
    const tree = displayFolderTree(buildFolderTree(folders));
    expect(flatNames(tree)).toContain("Kept");
  });

  it("does not create two same-named siblings when hoisting", () => {
    const folders = toFolders("dupes", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "[Gmail]", name: "[Gmail]", role: "other", selectable: false },
      { path: "[Gmail]/Receipts", name: "Receipts", role: "other" },
      { path: "Receipts", name: "Receipts", role: "other" },
    ]);
    const tree = displayFolderTree(buildFolderTree(folders));
    const rendered = flatNames(tree);
    // One row at the root, one nested under the container: never two identical
    // sibling rows the user cannot tell apart.
    expect(rendered).toContain("Receipts");
    expect(rendered.filter((name) => name.trim() === "Receipts")).toHaveLength(2);
    expect(tree.filter((node) => node.folder.name === "Receipts")).toHaveLength(1);
  });

  it("keeps user folders nested under a \\Noselect container", () => {
    const view = render("noselect", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "Archive", name: "Archive", role: "archive", selectable: false },
      { path: "Archive/2024", name: "2024", role: "other" },
      { path: "Archive/2025", name: "2025", role: "other" },
    ]);
    // The container is folded into More and its children stay nested under it.
    expect(view.primaryNames).toEqual(["INBOX"]);
    expect(view.secondaryNames).toEqual(["Archive"]);
    expect(view.childNames("Archive").sort()).toEqual(["2024", "2025"]);
  });

  it("never shows a container row twice even if its name repeats", () => {
    const folders = toFolders("container-twice", [
      { path: "INBOX", name: "INBOX", role: "inbox" },
      { path: "[Gmail]", name: "[Gmail]", role: "other", selectable: false },
      { path: "[Gmail]/Sent Mail", name: "Sent Mail", role: "sent" },
    ]);
    const tree = displayFolderTree(buildFolderTree(folders));
    expect(flatNames(tree).filter((name) => name === "[Gmail]")).toHaveLength(0);
  });
});

/* ────────────────────────── inbox-only badges ─────────────────────── */

/**
 * Only the inbox advertises a number. Every other row (drafts, sent, flagged,
 * outbox, junk, trash) stayed blank on purpose: the user acts on unread mail
 * arriving in the inbox, and a number on every row made the rail unreadable.
 */
describe("folder badge scope", () => {
  const folder = (role: FolderRole, unreadCount: number) => ({ role, unreadCount });

  it("counts unread mail for the inbox", () => {
    expect(folderBadgeCount(folder("inbox", 4))).toBe(4);
    expect(folderBadgeCount(folder("inbox", 0))).toBe(0);
  });

  it("gives every other role no badge", () => {
    for (const role of [
      "drafts",
      "outbox",
      "flagged",
      "important",
      "sent",
      "junk",
      "trash",
      "archive",
      "other",
    ] as FolderRole[]) {
      expect(folderBadgeCount(folder(role, 7))).toBe(0);
    }
  });
});
