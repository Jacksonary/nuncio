/**
 * Per-mailbox conversation view.
 *
 * The global setting (Settings → Reading) is only the default. A mailbox the
 * user toggled from the list header keeps its own choice, so flipping it in one
 * account does not regroup every other account. The unified views share one
 * entry because they have no single account.
 */
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";

export const UNIFIED_CONVERSATION_KEY = "unified";

/** Account id for a real folder, the shared key for the unified views. */
export function conversationScopeKey(accountId: string | null | undefined): string {
  return accountId ? accountId : UNIFIED_CONVERSATION_KEY;
}

/** The mailbox's own choice if it has one, else the global default. */
export function resolveConversation(
  overrides: Record<string, boolean>,
  scopeKey: string,
  fallback: boolean,
): boolean {
  return overrides[scopeKey] ?? fallback;
}

interface ConversationPrefsState {
  overrides: Record<string, boolean>;
  setConversation: (scopeKey: string, value: boolean) => void;
}

export const useConversationPrefs = create<ConversationPrefsState>()(
  persist(
    (set) => ({
      overrides: {},
      setConversation: (scopeKey, value) =>
        set((state) => ({ overrides: { ...state.overrides, [scopeKey]: value } })),
    }),
    {
      name: "nuncio.mail.conversation",
      storage: createJSONStorage(() => localStorage),
    },
  ),
);

/** Effective conversation view for one mailbox, plus its setter. */
export function useConversationView(
  accountId: string | null | undefined,
  fallback: boolean,
): [boolean, (value: boolean) => void] {
  const key = conversationScopeKey(accountId);
  const overrides = useConversationPrefs((state) => state.overrides);
  const setConversation = useConversationPrefs((state) => state.setConversation);
  return [resolveConversation(overrides, key, fallback), (value) => setConversation(key, value)];
}
