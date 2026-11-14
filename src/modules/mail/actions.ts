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
import type { MessageAction, MessageDetail, MessagePage, MessageSummary } from "@/ipc/types";
import { toast } from "@/components/Toast";
import { t } from "@/lib/i18n";
import { queryClient } from "@/lib/queryClient";
import { queryKeys } from "@/lib/queryKeys";

export interface ApplyActionInput {
  ids: string[];
  action: MessageAction;
  /** Apply the action to whole conversations (optimistic list removal only). */
  threadIds?: string[];
  accountId?: string;
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

function patchSummary(summary: MessageSummary, input: ApplyActionInput): MessageSummary | null {
  const kind = optimisticKind(input.action);
  if (kind === "remove") return null;
  if (input.action.kind === "setSeen") {
    return { ...summary, seen: input.action.value };
  }
  if (input.action.kind === "setFlagged") {
    return { ...summary, flagged: input.action.value };
  }
  return summary;
}

function patchInfiniteMessageCaches(
  client: ReturnType<typeof useQueryClient>,
  input: ApplyActionInput,
): Array<[QueryKey, InfiniteMessages | undefined]> {
  const idSet = new Set(input.ids);
  const threadSet = input.threadIds ? new Set(input.threadIds) : null;
  const snapshot = client.getQueriesData<InfiniteMessages>({ queryKey: ["messages"] });
  for (const [key, data] of snapshot) {
    if (!data || !Array.isArray(data.pages)) continue;
    let removed = 0;
    const pages = data.pages.map((page) => {
      const items: MessageSummary[] = [];
      for (const summary of page.items) {
        const matches =
          idSet.has(summary.id) ||
          (threadSet !== null &&
            threadSet.has(summary.threadId) &&
            (input.accountId === undefined || summary.accountId === input.accountId));
        if (!matches) {
          items.push(summary);
          continue;
        }
        const patched = patchSummary(summary, input);
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
  input: ApplyActionInput,
): Array<[QueryKey, MessageSummary[] | undefined]> {
  const idSet = new Set(input.ids);
  const threadSet = input.threadIds ? new Set(input.threadIds) : null;
  const snapshot = client.getQueriesData<MessageSummary[]>({ queryKey: ["thread"] });
  for (const [key, data] of snapshot) {
    if (!Array.isArray(data)) continue;
    const next: MessageSummary[] = [];
    for (const summary of data) {
      const matches =
        idSet.has(summary.id) ||
        (threadSet !== null &&
          threadSet.has(summary.threadId) &&
          (input.accountId === undefined || summary.accountId === input.accountId));
      if (!matches) {
        next.push(summary);
        continue;
      }
      const patched = patchSummary(summary, input);
      if (patched) next.push(patched);
    }
    client.setQueryData<MessageSummary[]>(key, next);
  }
  return snapshot;
}

function patchDetailCache(
  client: ReturnType<typeof useQueryClient>,
  input: ApplyActionInput,
): Array<[QueryKey, MessageDetail | undefined]> {
  const idSet = new Set(input.ids);
  const snapshot = client.getQueriesData<MessageDetail>({ queryKey: ["message"] });
  for (const [key, data] of snapshot) {
    if (!data || !data.summary) continue;
    if (!idSet.has(data.summary.id)) continue;
    if (optimisticKind(input.action) === "remove") continue;
    const patched = patchSummary(data.summary, input);
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

export async function undoLastMessageAction(): Promise<void> {
  try {
    const undone = await messagesApi.undoLast();
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

export function messageActionToast(action: MessageAction, count: number): void {
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
  const undoable = !(action.kind === "delete" && action.permanent);
  toast.show({
    type: "success",
    title: message,
    duration: 6000,
    actions: undoable
      ? [
          {
            label: t("common.undo"),
            onClick: () => {
              void undoLastMessageAction();
            },
          },
        ]
      : undefined,
  });
}

/**
 * Delete from a folder that already is Trash/Junk becomes permanent; every
 * other folder moves the message into Trash.
 */
export function isPermanentDelete(role: string | null | undefined): boolean {
  return role === "trash" || role === "junk";
}

interface MutationContext {
  infinite: Array<[QueryKey, InfiniteMessages | undefined]>;
  threads: Array<[QueryKey, MessageSummary[] | undefined]>;
  details: Array<[QueryKey, MessageDetail | undefined]>;
}

export function useMessageActions() {
  const client = useQueryClient();
  const mutation = useMutation<void, unknown, ApplyActionInput, MutationContext>({
    mutationFn: async (input) => {
      if (input.threadIds && input.threadIds.length > 0 && input.accountId) {
        await messagesApi.actThreads(input.accountId, input.threadIds, input.action);
      } else {
        await messagesApi.act(input.ids, input.action);
      }
    },
    onMutate: async (input): Promise<MutationContext> => {
      await client.cancelQueries({ queryKey: ["messages"] });
      const infinite = patchInfiniteMessageCaches(client, input);
      const threads = patchThreadCaches(client, input);
      const details = patchDetailCache(client, input);
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

  return {
    apply: (input: ApplyActionInput) => mutation.mutateAsync(input),
    isPending: mutation.isPending,
  };
}

/** Fetch a single message with the remote-content override. */
export async function fetchMessage(id: string, allowRemote: boolean): Promise<MessageDetail> {
  const detail = await messagesApi.get(id, allowRemote);
  queryClient.setQueryData(queryKeys.message(id), detail);
  return detail;
}
