/**
 * Query helpers for the composer. The shared `queryKeys` module has no entry
 * for a single draft (drafts are ephemeral UI state), so the composer uses its
 * own private key namespace.
 */
import { useQuery } from "@tanstack/react-query";
import { compose } from "@/ipc/api";
import type { Draft, Id } from "@/ipc/types";

export const composeQueryKeys = {
  draft: (draftId: Id) => ["composeDraft", draftId] as const,
};

/** Load a draft by id (used by the `#/compose/<id>` window route). */
export function useDraftQuery(draftId: Id | null) {
  return useQuery<Draft>({
    queryKey: composeQueryKeys.draft(draftId ?? "none"),
    queryFn: () => compose.getDraft(draftId as Id),
    enabled: Boolean(draftId),
    staleTime: Infinity,
    gcTime: 0,
    retry: 1,
  });
}

let pendingBlank: Promise<Draft> | null = null;

/**
 * Create a blank draft, de-duplicating concurrent calls. React 18 StrictMode
 * mounts effects twice; without this guard the backend would create two drafts.
 */
export function createBlankDraft(): Promise<Draft> {
  if (!pendingBlank) {
    pendingBlank = compose.create(null).finally(() => {
      pendingBlank = null;
    });
  }
  return pendingBlank;
}
