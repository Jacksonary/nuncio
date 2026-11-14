/**
 * Single integration point between the mail module and the composer module
 * (`src/modules/mail/compose`, owned by the compose agent).
 *
 * `useMailCompose()` exposes the inline compose flows used across the module;
 * `InlineComposePane` renders the composer inside the reading pane.
 */
import { useMemo } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import type { Draft, ReplyMode } from "@/ipc/types";
import { app } from "@/ipc/api";
import { t } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { closeCompose, ComposeView, useComposeActions, useComposeStore } from "./compose";

export { ComposeView } from "./compose";
import styles from "./mail.module.css";

/** Open a compose route in a secondary window (fallback: hash navigation). */
export function openComposeWindow(draftId: string | null, accountId: string | null): void {
  const query = !draftId && accountId ? `?accountId=${encodeURIComponent(accountId)}` : "";
  const route = draftId ? `/compose/${draftId}` : `/compose/new${query}`;
  if (!isTauri()) {
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
    return;
  }
  app.openWindow(route, t("shell.window.compose")).catch((error: unknown) => {
    console.warn("[mail] open compose window failed", error);
    if (typeof window !== "undefined") window.location.hash = `#${route}`;
  });
}

export function useMailCompose() {
  const actions = useComposeActions();
  const queryClient = useQueryClient();
  return useMemo(
    () => ({
      newInline: (accountId?: string | null) => actions.newMessage(accountId ?? null),
      replyInline: (messageId: string, mode: ReplyMode) => actions.reply(messageId, mode),
      openDraft: (messageId: string) => actions.openDraft(messageId),
      fromMailto: (url: string) => actions.fromMailto(url),
      /** Open an already-created draft (e.g. a cancelled outbox message). */
      openDraftData: (draft: Draft) => {
        useComposeStore.getState().open(draft);
        const shell = useShellStore.getState();
        shell.setActiveModule("mail");
        shell.setShowSettings(false);
        shell.openInlineCompose({ draftId: draft.id, accountId: draft.accountId });
      },
      invalidateMail: () => {
        void queryClient.invalidateQueries({ queryKey: ["messages"] });
        void queryClient.invalidateQueries({ queryKey: ["folders"] });
        void queryClient.invalidateQueries({ queryKey: ["unifiedCounts"] });
      },
    }),
    [actions, queryClient],
  );
}

/**
 * Inline composer shown in the reading pane (Win11 Mail style).
 *
 * The pop-out/close controls live inside `ComposeView` (they need access to
 * the draft to save before popping out), so this pane only provides the
 * section chrome and forwards `closeCompose` to the composer.
 */
export function InlineComposePane() {
  const inline = useShellStore((state) => state.inlineCompose);
  const compose = useMailCompose();

  if (!inline) return null;

  const handleClose = () => {
    // Close both the shell pane and the composer store so reopening starts
    // from a clean draft instead of a stale one.
    closeCompose();
  };

  return (
    <section className={styles.inlineCompose} aria-label={t("mail.compose.inlineTitle")}>
      <header className={styles.inlineComposeHeader}>
        <span className={styles.inlineComposeTitle}>{t("mail.compose.inlineTitle")}</span>
      </header>
      <div className={styles.inlineComposeBody}>
        <ComposeView
          draftId={inline.draftId}
          mode="inline"
          onClose={handleClose}
          onSent={() => {
            compose.invalidateMail();
            closeCompose();
          }}
        />
      </div>
    </section>
  );
}
