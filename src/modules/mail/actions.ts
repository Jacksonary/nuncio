/**
 * Message mutations with optimistic cache updates and the undo toast.
 *
 * The backend applies actions optimistically too, but patching the TanStack
 * caches keeps the list and reader snappy while the command is in flight and
 * gives us a clean rollback point if it fails.
 */
import {
  useMutation,
  useQueryClient,
  type InfiniteData,
  type QueryKey,
} from "@tanstack/react-query";
import { isIpcError, messages as messagesApi } from "@/ipc/api";
import type {
  Folder,
  MessageAction,
  MessageDetail,
  MessagePage,
  MessageSummary,
} from "@/ipc/types";
import { toast } from "@/components/Toast";
import { t } from "@/lib/i18n";
import { queryClient } from "@/lib/queryClient";
import { queryKeys } from "@/lib/queryKeys";

/** A collapsed conversation row that must expand to its real messages. */
export interface MessageThreadRef {
  accountId: string;
  threadId: string;
}

export interface ApplyActionInput {
  ids: string[];
  action: MessageAction;
  /**
   * Conversations represented by the selected rows. Their members are fetched
   * first so single- and multi-select delete the same set of messages instead
   * of relying on a backend expansion that only ran for one row.
   */
  threads?: MessageThreadRef[];
  /** Legacy single-conversation hint still passed by the message list. */
  threadIds?: string[];
  accountId?: string;
}

export interface MessageActionOutcome {
  /** Undo-log entry id; empty when the action recorded nothing reversible. */
  undoId: string;
  /** Every message id the action targeted, threads expanded. */
  ids: string[];
}

type InfiniteMessages = InfiniteData<MessagePage, string | null>;

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function optimisticKind(action: MessageAction): "seen" | "flag" | "remove" {
  if (action.kind === "setSeen") return "seen";
  if (action.kind === "setFlagged") return "flag";
  return "remove";
}

function patchSummary(summary: MessageSummary, action: MessageAction): MessageSummary | null {
  const kind = optimisticKind(action);
  if (kind === "remove") return null;
  if (action.kind === "setSeen") {
    return { ...summary, seen: action.value };
  }
  if (action.kind === "setFlagged") {
    return { ...summary, flagged: action.value };
  }
  return summary;
}

function patchInfiniteMessageCaches(
  client: ReturnType<typeof useQueryClient>,
  ids: string[],
  action: MessageAction,
): Array<[QueryKey, InfiniteMessages | undefined]> {
  const idSet = new Set(ids);
  const snapshot = client.getQueriesData<InfiniteMessages>({ queryKey: ["messages"] });
  for (const [key, data] of snapshot) {
    if (!data || !Array.isArray(data.pages)) continue;
    let removed = 0;
    const pages = data.pages.map((page) => {
      const items: MessageSummary[] = [];
      for (const summary of page.items) {
        if (!idSet.has(summary.id)) {
          items.push(summary);
          continue;
        }
        const patched = patchSummary(summary, action);
        if (patched) items.push(patched);
        else removed += 1;
      }
      return {
        ...page,
        items,
        total: removed > 0 ? Math.max(0, page.total - removed) : page.total,
      };
    });
    client.setQueryData<InfiniteMessages>(key, { ...data, pages });
  }
  return snapshot;
}

function patchThreadCaches(
  client: ReturnType<typeof useQueryClient>,
  ids: string[],
  action: MessageAction,
): Array<[QueryKey, MessageSummary[] | undefined]> {
  const idSet = new Set(ids);
  const snapshot = client.getQueriesData<MessageSummary[]>({ queryKey: ["thread"] });
  for (const [key, data] of snapshot) {
    if (!Array.isArray(data)) continue;
    const next: MessageSummary[] = [];
    for (const summary of data) {
      if (!idSet.has(summary.id)) {
        next.push(summary);
        continue;
      }
      const patched = patchSummary(summary, action);
      if (patched) next.push(patched);
    }
    client.setQueryData<MessageSummary[]>(key, next);
  }
  return snapshot;
}

function patchDetailCache(
  client: ReturnType<typeof useQueryClient>,
  ids: string[],
  action: MessageAction,
): Array<[QueryKey, MessageDetail | undefined]> {
  const idSet = new Set(ids);
  const snapshot = client.getQueriesData<MessageDetail>({ queryKey: ["message"] });
  for (const [key, data] of snapshot) {
    if (!data || !data.summary) continue;
    if (!idSet.has(data.summary.id)) continue;
    if (optimisticKind(action) === "remove") continue;
    const patched = patchSummary(data.summary, action);
    if (patched) client.setQueryData<MessageDetail>(key, { ...data, summary: patched });
  }
  return snapshot;
}

function invalidateAll(): void {
  void queryClient.invalidateQueries({ queryKey: ["messages"] });
  void queryClient.invalidateQueries({ queryKey: ["thread"] });
  void queryClient.invalidateQueries({ queryKey: ["message"] });
  void queryClient.invalidateQueries({ queryKey: ["folders"] });
  void queryClient.invalidateQueries({ queryKey: ["unifiedCounts"] });
}

/** Union of the selected ids and every member of the selected conversations. */
export function mergeMessageIds(base: string[], memberLists: string[][]): string[] {
  const ids = new Set(base);
  for (const list of memberLists) {
    for (const id of list) ids.add(id);
  }
  return [...ids];
}

function dedupeThreadRefs(refs: MessageThreadRef[]): MessageThreadRef[] {
  const seen = new Set<string>();
  const result: MessageThreadRef[] = [];
  for (const ref of refs) {
    const key = `${ref.accountId}\u0000${ref.threadId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(ref);
  }
  return result;
}

/**
 * Conversation rows among the selected ids. A list row with `threadCount > 1`
 * stands for the whole conversation, whether one row or ten are selected.
 */
export function threadRefsFromSummaries(
  summaries: Iterable<MessageSummary>,
  ids: string[],
): MessageThreadRef[] {
  const idSet = new Set(ids);
  const refs: MessageThreadRef[] = [];
  for (const summary of summaries) {
    if (!idSet.has(summary.id) || summary.threadCount <= 1) continue;
    refs.push({ accountId: summary.accountId, threadId: summary.threadId });
  }
  return dedupeThreadRefs(refs);
}

/** Expand conversation rows into explicit message ids before the IPC call. */
export async function resolveActionIds(
  input: ApplyActionInput,
  summaries: Iterable<MessageSummary> = [],
): Promise<string[]> {
  // Only infer conversations for a multi-selection: a single row already
  // carries its conversation hint, and a single message opened from the
  // reader must stay a single message.
  const inferred = input.ids.length > 1 ? threadRefsFromSummaries(summaries, input.ids) : [];
  const refs = dedupeThreadRefs([
    ...(input.accountId
      ? (input.threadIds ?? []).map((threadId) => ({
          accountId: input.accountId as string,
          threadId,
        }))
      : []),
    ...(input.threads ?? []),
    ...inferred,
  ]);
  if (refs.length === 0) return input.ids;
  const memberLists = await Promise.all(
    refs.map(async (thread) => {
      const members = await messagesApi.thread(thread.accountId, thread.threadId);
      return members.map((member) => member.id);
    }),
  );
  return mergeMessageIds(input.ids, memberLists);
}

/**
 * Undo id of the action whose mutation just resolved.
 *
 * Call sites that can thread the id through the toast pass it explicitly; this
 * is the fallback for the ones that only have `(action, count)`.
 */
let lastUndoId: string | null = null;

/**
 * Undo one action by the id its mutation returned.
 *
 * A toast must revert the action it belongs to, not whatever ran last.
 */
export async function undoMessageAction(undoId?: string): Promise<void> {
  try {
    const undone = await messagesApi.undoLast(undoId && undoId.length > 0 ? undoId : null);
    if (undone) {
      invalidateAll();
      toast.success(t("mail.toast.undone"));
    } else {
      toast.info(t("mail.toast.nothingToUndo"));
    }
  } catch (error) {
    toast.error(t("mail.toast.undoFailed"), errorText(error));
  }
}

/** Global undo for the keyboard shortcut; reverts the most recent action. */
export async function undoLastMessageAction(): Promise<void> {
  await undoMessageAction();
}

export function messageActionToast(
  action: MessageAction,
  count: number,
  undoId?: string,
): void {
  let message: string | null = null;
  switch (action.kind) {
    case "delete":
      message = t("mail.toast.deleted", { count });
      break;
    case "move":
      message = t("mail.toast.moved", { count });
      break;
    case "archive":
      message = t("mail.toast.archived", { count });
      break;
    case "markJunk":
      message = action.value ? t("mail.toast.markedJunk", { count }) : null;
      break;
    default:
      message = null;
  }
  if (!message) return;
  // Capture the id now: by the time the toast is clicked another action may
  // have run, and the global "last entry" would then be the wrong one.
  const targetUndoId = undoId ?? lastUndoId ?? undefined;
  // No id means the backend recorded nothing reversible. Offering Undo anyway
  // would fall through to the global "undo last", reverting an unrelated
  // earlier action.
  const undoable = canOfferUndo(action, targetUndoId);
  toast.show({
    type: "success",
    title: message,
    duration: 6000,
    actions: undoable
      ? [
          {
            label: t("common.undo"),
            onClick: () => {
              void undoMessageAction(targetUndoId);
            },
          },
        ]
      : undefined,
  });
}

/**
 * Role-only permanent-delete check, mirroring the backend rule. Prefer
 * [`deletePlan`] when several messages are involved: each message's own folder
 * role decides.
 */
export function isPermanentDelete(role: string | null | undefined): boolean {
  return role === "trash" || role === "junk" || role === "drafts";
}

export interface DeletePlan {
  /** Messages that will be moved to Trash. */
  move: number;
  /** Messages that will be erased outright (drafts, Trash and Junk). */
  permanent: number;
}

/**
 * Mirror the backend rule per message so the confirmation describes what will
 * actually happen. The selected sidebar folder is not used: in a global search
 * it can differ from every result's real folder.
 */
export function deletePlan(
  targets: ReadonlyArray<Pick<MessageSummary, "isDraft" | "folderId">>,
  roleOf: (folderId: string) => string | null | undefined,
): DeletePlan {
  let move = 0;
  let permanent = 0;
  for (const target of targets) {
    if (target.isDraft || isPermanentDelete(roleOf(target.folderId))) permanent += 1;
    else move += 1;
  }
  return { move, permanent };
}

/**
 * Whether a delete must ask first. A permanent delete never goes silent, even
 * with "confirm before delete" off; a plain move to Trash is undoable and
 * follows the preference.
 */
export function needsDeleteConfirmation(confirmPreference: boolean, plan: DeletePlan): boolean {
  return confirmPreference || plan.permanent > 0;
}

/**
 * Undo is only offered when the action recorded something reversible. An empty
 * id would fall through to the global "undo last" and revert something else.
 */
export function canOfferUndo(action: MessageAction, undoId: string | null | undefined): boolean {
  return !(action.kind === "delete" && action.permanent) && Boolean(undoId);
}

/** All message summaries currently cached by the list queries. */
function cachedSummaries(client: ReturnType<typeof useQueryClient>): Map<string, MessageSummary> {
  const byId = new Map<string, MessageSummary>();
  for (const [, data] of client.getQueriesData<InfiniteMessages>({ queryKey: ["messages"] })) {
    if (!data) continue;
    for (const page of data.pages) {
      for (const summary of page.items) byId.set(summary.id, summary);
    }
  }
  return byId;
}

/** Folder id -> role for every account whose folders are cached. */
function cachedFolderRoles(client: ReturnType<typeof useQueryClient>): Map<string, string> {
  const roles = new Map<string, string>();
  for (const [, folders] of client.getQueriesData<Folder[]>({ queryKey: ["folders"] })) {
    for (const folder of folders ?? []) roles.set(folder.id, folder.role);
  }
  return roles;
}

/**
 * Never send `permanent: true` for messages that are not already past the
 * point of no return. The caller derives that flag from the selected sidebar
 * folder, which is the wrong object for global-search results; escalating
 * would delete an Inbox message instead of moving it.
 */
export function normalizeAction(
  action: MessageAction,
  ids: string[],
  summaries: Map<string, MessageSummary>,
  folderRoles: Map<string, string>,
): MessageAction {
  if (action.kind !== "delete" || !action.permanent) return action;
  const allPermanent = ids.every((id) => {
    const summary = summaries.get(id);
    if (!summary) return false;
    return summary.isDraft || isPermanentDelete(folderRoles.get(summary.folderId));
  });
  return allPermanent ? action : { kind: "delete", permanent: false };
}

interface ResolvedActionInput {
  ids: string[];
  action: MessageAction;
}

interface MutationContext {
  infinite: Array<[QueryKey, InfiniteMessages | undefined]>;
  threads: Array<[QueryKey, MessageSummary[] | undefined]>;
  details: Array<[QueryKey, MessageDetail | undefined]>;
}

export function useMessageActions() {
  const client = useQueryClient();
  const mutation = useMutation<MessageActionOutcome, unknown, ResolvedActionInput, MutationContext>({
    mutationFn: async (input) => ({
      undoId: await messagesApi.act(input.ids, input.action),
      ids: input.ids,
    }),
    onMutate: async (input): Promise<MutationContext> => {
      await client.cancelQueries({ queryKey: ["messages"] });
      const infinite = patchInfiniteMessageCaches(client, input.ids, input.action);
      const threads = patchThreadCaches(client, input.ids, input.action);
      const details = patchDetailCache(client, input.ids, input.action);
      return { infinite, threads, details };
    },
    onError: (error, _input, context) => {
      for (const [key, data] of context?.infinite ?? []) {
        client.setQueryData(key, data);
      }
      for (const [key, data] of context?.threads ?? []) {
        client.setQueryData(key, data);
      }
      for (const [key, data] of context?.details ?? []) {
        client.setQueryData(key, data);
      }
      toast.error(t("mail.toast.actionFailed"), errorText(error));
    },
    onSettled: () => {
      invalidateAll();
    },
  });

  const apply = async (input: ApplyActionInput): Promise<MessageActionOutcome> => {
    const summaries = cachedSummaries(client);
    const ids = await resolveActionIds(input, summaries.values());
    const action = normalizeAction(
      input.action,
      ids,
      summaries,
      cachedFolderRoles(client),
    );
    const outcome = await mutation.mutateAsync({ ids, action });
    lastUndoId = outcome.undoId || null;
    return outcome;
  };

  return {
    apply,
    isPending: mutation.isPending,
  };
}

/** Fetch a single message with the remote-content override. */
export async function fetchMessage(id: string, allowRemote: boolean): Promise<MessageDetail> {
  const detail = await messagesApi.get(id, allowRemote);
  queryClient.setQueryData(queryKeys.message(id), detail);
  return detail;
}
