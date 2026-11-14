/**
 * Imperative compose entry points used by the mail module (new/reply/draft/
 * mailto). Each creates a Draft through the IPC layer and opens it in the
 * shared inline composer store.
 */
import { useMemo } from "react";
import { compose, isIpcError } from "@/ipc/api";
import type { Draft, ReplyMode } from "@/ipc/types";
import { toast } from "@/components";
import { t } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { useComposeStore } from "./store";
import "./i18n";

export interface ComposeActions {
  /** Create a blank draft, optionally for a specific account. */
  newMessage: (accountId?: string | null) => Promise<void>;
  reply: (messageId: string, mode: ReplyMode) => Promise<void>;
  /** Open a draft message that lives in the Drafts folder. */
  openDraft: (messageId: string) => Promise<void>;
  /** Create a draft from a mailto: URL. */
  fromMailto: (url: string) => Promise<void>;
}

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function presentDraft(draft: Draft): void {
  useComposeStore.getState().open(draft);
  const shell = useShellStore.getState();
  shell.setActiveModule("mail");
  shell.setShowSettings(false);
  shell.openInlineCompose({ draftId: draft.id, accountId: draft.accountId });
}

async function run(
  task: () => Promise<Draft>,
  failureKey: string,
): Promise<void> {
  try {
    const draft = await task();
    presentDraft(draft);
  } catch (error) {
    console.error("[compose] failed to open draft", error);
    toast.error(t(failureKey), errorMessage(error));
  }
}

const actions: ComposeActions = {
  newMessage: (accountId = null) => run(() => compose.create(accountId), "compose.error.createFailed"),
  reply: (messageId, mode) => run(() => compose.reply(messageId, mode), "compose.error.openFailed"),
  openDraft: (messageId) => run(() => compose.openDraft(messageId), "compose.error.openFailed"),
  fromMailto: (url) => run(() => compose.fromMailto(url), "compose.error.openFailed"),
};

/** Stable compose actions; safe to call from event handlers and hotkeys. */
export function useComposeActions(): ComposeActions {
  return useMemo(() => actions, []);
}

/** Non-React variant for hotkey definitions and event bridges. */
export const composeActions: ComposeActions = actions;

/** Close the inline composer everywhere (store + shell selection). */
export function closeCompose(): void {
  useComposeStore.getState().close();
  useShellStore.getState().closeInlineCompose();
}
