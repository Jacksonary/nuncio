/**
 * Pure helpers for the mail module: date grouping, multi-selection and
 * folder-tree shape. Kept free of React and IPC so they are unit-testable.
 */
import type { Folder, FolderRole, HoverAction, MessageSummary } from "@/ipc/types";
import { addDays, isSameDay, isThisWeek, startOfWeek } from "@/lib/format";

/* ───────────────────────────── Date groups ───────────────────────────── */

export type DateGroupKey =
  | "today"
  | "yesterday"
  | "thisWeek"
  | "lastWeek"
  | "thisMonth"
  | "earlier";

export const DATE_GROUP_ORDER: DateGroupKey[] = [
  "today",
  "yesterday",
  "thisWeek",
  "lastWeek",
  "thisMonth",
  "earlier",
];

/** Bucket a message date into the Windows Mail style date groups. */
export function dateGroupKey(
  value: string | Date,
  now: Date,
  weekStartsOn = 1,
): DateGroupKey {
  const date = value instanceof Date ? value : new Date(value);
  if (Number.isNaN(date.getTime())) return "earlier";
  if (isSameDay(date, now)) return "today";
  if (isSameDay(date, addDays(now, -1))) return "yesterday";
  if (isThisWeek(date, now, weekStartsOn)) return "thisWeek";
  const lastWeekStart = addDays(startOfWeek(now, weekStartsOn), -7);
  const lastWeekEnd = addDays(lastWeekStart, 7);
  if (date >= lastWeekStart && date < lastWeekEnd) return "lastWeek";
  if (
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth()
  ) {
    return "thisMonth";
  }
  return "earlier";
}

export type MessageRow =
  | {
      kind: "header";
      key: string;
      group: DateGroupKey;
      count: number;
    }
  | { kind: "message"; key: string; summary: MessageSummary };

/**
 * Flatten a newest-first message array into header + message rows for the
 * virtualized list. A group header is emitted whenever the bucket changes.
 */
export function buildMessageRows(
  items: MessageSummary[],
  now: Date,
  weekStartsOn = 1,
): MessageRow[] {
  const rows: MessageRow[] = [];
  let current: DateGroupKey | null = null;
  let headerIndex = -1;
  for (const summary of items) {
    const group = dateGroupKey(summary.date, now, weekStartsOn);
    if (group !== current) {
      current = group;
      rows.push({ kind: "header", key: `header-${group}`, group, count: 0 });
      headerIndex = rows.length - 1;
    }
    const header = rows[headerIndex];
    if (header.kind === "header") header.count += 1;
    rows.push({ kind: "message", key: `message-${summary.id}`, summary });
  }
  return rows;
}

/** Ordered top-level message ids (headers removed), in list order. */
export function messageIds(rows: MessageRow[]): string[] {
  return rows
    .filter((row): row is Extract<MessageRow, { kind: "message" }> => row.kind === "message")
    .map((row) => row.summary.id);
}

/* ─────────────────────────── Multi-selection ─────────────────────────── */

export interface SelectionState {
  ids: string[];
  anchor: string | null;
}

export const EMPTY_SELECTION: SelectionState = { ids: [], anchor: null };

export type SelectionAction =
  | { type: "replace"; ids: string[] }
  | { type: "toggle"; id: string }
  | { type: "range"; id: string; ordered: string[] }
  | { type: "remove"; ids: string[] }
  | { type: "clear" };

function unique(ids: string[]): string[] {
  return Array.from(new Set(ids));
}

/** Reducer backing Ctrl/Shift click and the selection-mode checkboxes. */
export function selectionReducer(
  state: SelectionState,
  action: SelectionAction,
): SelectionState {
  switch (action.type) {
    case "replace": {
      const ids = unique(action.ids);
      return { ids, anchor: ids.length > 0 ? ids[ids.length - 1] : null };
    }
    case "toggle": {
      if (state.ids.includes(action.id)) {
        return {
          ids: state.ids.filter((id) => id !== action.id),
          anchor: state.anchor === action.id ? null : state.anchor,
        };
      }
      return { ids: [...state.ids, action.id], anchor: action.id };
    }
    case "range": {
      const anchorIndex = state.anchor ? action.ordered.indexOf(state.anchor) : -1;
      const targetIndex = action.ordered.indexOf(action.id);
      if (targetIndex < 0) return state;
      if (anchorIndex < 0) return { ids: [action.id], anchor: action.id };
      const start = Math.min(anchorIndex, targetIndex);
      const end = Math.max(anchorIndex, targetIndex);
      return { ids: action.ordered.slice(start, end + 1), anchor: state.anchor };
    }
    case "remove": {
      const removed = new Set(action.ids);
      return {
        ids: state.ids.filter((id) => !removed.has(id)),
        anchor: state.anchor && removed.has(state.anchor) ? null : state.anchor,
      };
    }
    case "clear":
      return EMPTY_SELECTION;
    default:
      return state;
  }
}

/** Ids to act on: the explicit multi-selection, else the open message. */
export function actionTargets(
  selection: SelectionState,
  activeId: string | null,
): string[] {
  if (selection.ids.length > 0) return selection.ids;
  return activeId ? [activeId] : [];
}

/**
 * Pick the message that should be opened after deleting `removed` from the
 * ordered id list, honouring the auto-advance preference.
 */
export function nextAfterRemoval(
  ordered: string[],
  removed: Set<string>,
  current: string | null,
  mode: "next" | "previous" | "list",
): string | null {
  if (mode === "list") return null;
  const currentIndex = current ? ordered.indexOf(current) : -1;
  const remaining = ordered.filter((id) => !removed.has(id) && id !== current);
  if (remaining.length === 0) return null;
  if (currentIndex < 0) return remaining[0];
  if (mode === "previous") {
    for (let index = currentIndex - 1; index >= 0; index -= 1) {
      const id = ordered[index];
      if (id && !removed.has(id)) return id;
    }
    return remaining[remaining.length - 1] ?? null;
  }
  for (let index = currentIndex + 1; index < ordered.length; index += 1) {
    const id = ordered[index];
    if (id && !removed.has(id)) return id;
  }
  for (let index = currentIndex - 1; index >= 0; index -= 1) {
    const id = ordered[index];
    if (id && !removed.has(id)) return id;
  }
  return remaining[0] ?? null;
}

/* ───────────────────────────── Folder tree ───────────────────────────── */

export const ROLE_ORDER: Record<FolderRole, number> = {
  inbox: 0,
  drafts: 1,
  sent: 2,
  archive: 3,
  junk: 4,
  trash: 5,
  outbox: 6,
  flagged: 7,
  important: 8,
  all: 9,
  other: 10,
};

export interface FolderNode {
  folder: Folder;
  children: FolderNode[];
}

function compareFolders(a: Folder, b: Folder): number {
  const favoriteDelta = Number(b.isFavorite) - Number(a.isFavorite);
  if (favoriteDelta !== 0 && a.role === "other" && b.role === "other") {
    return favoriteDelta;
  }
  const roleDelta = (ROLE_ORDER[a.role] ?? 99) - (ROLE_ORDER[b.role] ?? 99);
  if (roleDelta !== 0) return roleDelta;
  const favorite = Number(b.isFavorite) - Number(a.isFavorite);
  if (favorite !== 0) return favorite;
  if (a.sortOrder !== b.sortOrder) return a.sortOrder - b.sortOrder;
  return a.name.localeCompare(b.name);
}

export function buildFolderTree(folders: Folder[]): FolderNode[] {
  const nodes = new Map<string, FolderNode>();
  for (const folder of folders) {
    nodes.set(folder.id, { folder, children: [] });
  }
  const roots: FolderNode[] = [];
  for (const node of nodes.values()) {
    const parent = node.folder.parentId ? nodes.get(node.folder.parentId) : undefined;
    if (parent) parent.children.push(node);
    else roots.push(node);
  }
  const sortLevel = (level: FolderNode[]) => {
    level.sort((a, b) => compareFolders(a.folder, b.folder));
    for (const node of level) sortLevel(node.children);
  };
  sortLevel(roots);
  return roots;
}

/** True when `candidateId` is `folderId` or one of its descendants. */
export function isSameOrDescendant(
  folders: Folder[],
  folderId: string,
  candidateId: string,
): boolean {
  if (folderId === candidateId) return true;
  let cursor = folders.find((folder) => folder.id === candidateId);
  const seen = new Set<string>();
  while (cursor && cursor.parentId && !seen.has(cursor.id)) {
    seen.add(cursor.id);
    if (cursor.parentId === folderId) return true;
    cursor = folders.find((folder) => folder.id === cursor?.parentId);
  }
  return false;
}

/** Folders shown before "More" is expanded: the system folders + favourites. */
export function isPrimaryFolder(folder: Folder): boolean {
  if (folder.isFavorite) return true;
  return (
    folder.role === "inbox" ||
    folder.role === "drafts" ||
    folder.role === "sent" ||
    folder.role === "trash" ||
    folder.role === "junk" ||
    folder.role === "archive" ||
    folder.role === "outbox"
  );
}

/** Names Gmail gives the container holding its system folders. */
const SYSTEM_CONTAINERS = ["[gmail]", "[google mail]"];

/**
 * A `[Gmail]`-style grouping node rather than a mailbox the user opens.
 *
 * Non-selectable containers are always grouping nodes. Some servers report the
 * container as selectable even though it only holds the system folders, so a
 * container whose children carry a real role is hoisted too. A plain user
 * folder that happens to be called `[Gmail]` and holds only user folders is
 * left alone.
 */
function isSystemContainer(node: FolderNode): boolean {
  const name = node.folder.name.trim().toLowerCase();
  const path = node.folder.remotePath.trim().toLowerCase();
  const known = SYSTEM_CONTAINERS.includes(name) || SYSTEM_CONTAINERS.includes(path);
  if (!known) return false;
  if (!node.folder.isSelectable) return true;
  return node.children.some((child) => child.folder.role !== "other");
}

/**
 * Gmail nests its system folders (Drafts, Sent, Trash, …) under a selectable
 * `[Gmail]` container. Hoisting them to the account root keeps every account's
 * sidebar looking the same and stops the container from being rendered twice.
 *
 * Other providers' containers (Proton Bridge's `Folders`/`Labels`) are left in
 * place: they group user folders rather than system folders, and flattening
 * them would merge two namespaces that can legitimately hold the same names.
 *
 * A child whose name would collide with a folder already at the root stays
 * inside the container, so the sidebar never shows two identical sibling rows
 * the user cannot tell apart.
 */
export function hoistSystemFolders(tree: FolderNode[]): FolderNode[] {
  const result: FolderNode[] = [];
  const taken = new Set<string>();
  const key = (node: FolderNode) => node.folder.name.trim().toLowerCase();

  const plain: FolderNode[] = [];
  const containers: FolderNode[] = [];
  for (const node of tree) {
    const rebuilt = { folder: node.folder, children: hoistSystemFolders(node.children) };
    if (isSystemContainer(node)) containers.push(rebuilt);
    else plain.push(rebuilt);
  }

  for (const node of plain) {
    taken.add(key(node));
    result.push(node);
  }

  for (const container of containers) {
    const kept: FolderNode[] = [];
    for (const child of container.children) {
      if (taken.has(key(child))) {
        kept.push(child);
      } else {
        taken.add(key(child));
        result.push(child);
      }
    }
    // Only the names that collided stay behind, and they keep their parent so
    // the row reads as `[Gmail] › Receipts` rather than a bare duplicate.
    if (kept.length > 0 && !taken.has(key(container))) {
      taken.add(key(container));
      result.push({ folder: container.folder, children: kept });
    }
  }

  // Hoisted nodes arrive after their former siblings, so re-sort the level to
  // keep Inbox → system folders → user folders order.
  result.sort((a, b) => compareFolders(a.folder, b.folder));
  return result;
}

/**
 * Keep only the first folder per role. Gmail exposes both a root `Sent` and
 * `[Gmail]/Sent Mail`, and Chinese providers ship extra spam-ish folders that
 * map to the same role; showing both produces duplicated rows.
 *
 * A dropped duplicate is not allowed to take its children with it: the
 * children are spliced in at the duplicate's position so user folders nested
 * under a colliding system folder stay reachable.
 *
 * `all`, `flagged` and `important` are label-like roles that users also give
 * their own folders ("All", "Starred", "Important"). Dropping such a folder
 * would hide mail, so the loser is demoted to a plain folder instead and shown
 * under "More"; only the core mail roles are collapsed to a single row.
 */
const COLLAPSIBLE_ROLES = new Set<string>(["inbox", "sent", "drafts", "trash", "junk", "archive"]);

export function dedupeRoleFolders(tree: FolderNode[]): FolderNode[] {
  const seen = new Set<string>();
  const walk = (nodes: FolderNode[]): FolderNode[] => {
    const kept: FolderNode[] = [];
    for (const node of nodes) {
      const role = node.folder.role;
      if (role === "other") {
        kept.push({ folder: node.folder, children: walk(node.children) });
        continue;
      }
      if (seen.has(role)) {
        if (COLLAPSIBLE_ROLES.has(role)) {
          kept.push(...walk(node.children));
        } else {
          kept.push({
            folder: { ...node.folder, role: "other" },
            children: walk(node.children),
          });
        }
        continue;
      }
      seen.add(role);
      kept.push({ folder: node.folder, children: walk(node.children) });
    }
    return kept;
  };
  return walk(tree);
}

/** Apply the display normalisation used by the sidebar. */
export function displayFolderTree(tree: FolderNode[]): FolderNode[] {
  return dedupeRoleFolders(hoistSystemFolders(tree));
}

/**
 * Tree of the folders that belong to the always-visible section. Children of
 * a primary folder are included so nested custom folders remain reachable.
 */
export function primaryNodes(tree: FolderNode[]): FolderNode[] {
  const keep = (node: FolderNode): FolderNode | null => {
    const primary = isPrimaryFolder(node.folder);
    // Inside a primary folder every child is reachable (e.g. `Sent Items/2026`),
    // not just children that happen to carry a role of their own.
    const children = node.children
      .map((child) => (primary ? child : keep(child)))
      .filter((child): child is FolderNode => child !== null);
    if (primary || children.length > 0) {
      return { folder: node.folder, children };
    }
    return null;
  };
  return tree.map(keep).filter((node): node is FolderNode => node !== null);
}

/**
 * Remaining folders after the primary section has been taken out.
 *
 * A container can be primary while only some of its children are (favouriting
 * `Projects/Alpha` renders `Projects` in the primary tree with `Alpha` alone).
 * The siblings that were left out must still appear under "More", attached to
 * the same parent, or they would be unreachable.
 */
export function secondaryNodes(tree: FolderNode[], primary: FolderNode[] = []): FolderNode[] {
  // Anything already rendered in the primary section (including the container
  // that holds it) must not appear again under "More".
  const shown = new Set<string>();
  const collect = (nodes: FolderNode[]) => {
    for (const node of nodes) {
      shown.add(node.folder.id);
      collect(node.children);
    }
  };
  collect(primary);

  const prune = (nodes: FolderNode[]): FolderNode[] => {
    const result: FolderNode[] = [];
    for (const node of nodes) {
      const children = prune(node.children);
      if (shown.has(node.folder.id)) {
        result.push(...children);
      } else {
        result.push({ folder: node.folder, children });
      }
    }
    return result;
  };
  return prune(tree);
}

/** Flatten a tree depth-first, used for move-to-folder menus. */
export function flattenFolderTree(
  tree: FolderNode[],
  depth = 0,
): Array<{ folder: Folder; depth: number }> {
  const result: Array<{ folder: Folder; depth: number }> = [];
  for (const node of tree) {
    result.push({ folder: node.folder, depth });
    result.push(...flattenFolderTree(node.children, depth + 1));
  }
  return result;
}

/* ──────────────────────────── Unified scopes ──────────────────────────── */

export const UNIFIED_PREFIX = "unified:";

export function unifiedFolderId(role: FolderRole): string {
  return `${UNIFIED_PREFIX}${role}`;
}

export function parseUnifiedFolderId(value: string | null): FolderRole | null {
  if (!value || !value.startsWith(UNIFIED_PREFIX)) return null;
  const role = value.slice(UNIFIED_PREFIX.length) as FolderRole;
  return Object.prototype.hasOwnProperty.call(ROLE_ORDER, role) ? role : null;
}

/* ───────────────────────────── Swipe actions ──────────────────────────── */

export type SupportedSwipeAction = "archive" | "delete" | "flag" | "read";

/**
 * Swipe gestures only support actions that make sense without a target
 * folder; "move" has no destination and "none" disables the gesture.
 */
export function isSupportedSwipeAction(
  action: HoverAction | null | undefined,
): action is SupportedSwipeAction {
  return (
    action === "archive" ||
    action === "delete" ||
    action === "flag" ||
    action === "read"
  );
}

/* ───────────────────────────── Misc helpers ───────────────────────────── */

export function safeFilename(subject: string, fallback: string): string {
  const cleaned = (subject || fallback)
    .replace(/[\\/:*?"<>|\u0000-\u001f]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 120);
  return cleaned || fallback;
}

export function isDraftSummary(summary: Pick<MessageSummary, "folderId" | "isDraft">): boolean {
  return summary.isDraft;
}
