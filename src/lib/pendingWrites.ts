/**
 * The newest draft content that has not reached storage yet.
 *
 * The composer's save logic lives in a React component, and that component is
 * unmounted when the user switches modules — while a send started a moment
 * earlier is still running. If the close-to-tray handshake trusted "no composer
 * mounted" as "nothing to save", it would acknowledge and let the backend destroy
 * the webview with the only copy of the text still held in a promise chain.
 *
 * This module keeps that one fact outside React, so the close path can ask it
 * directly.
 *
 * Deliberately ONE slot holding one snapshot, rather than a registry of writes,
 * versions and per-draft identities. A registry re-implements the composer's own
 * dirty/revision/in-flight bookkeeping a second time, and the two copies then
 * disagree — the earlier attempt at that produced bugs where a stale success
 * cleared a newer failure, and where a draft that gained an id late became two
 * separate records. What the close path actually needs is narrower: "is there
 * text on screen that is not on disk, and can I still get it there?"
 */

import type { Draft } from "@/ipc/types";

/** Text that a live composer holds but storage does not. */
let unsaved: Draft | null = null;

/**
 * Record the snapshot a composer is about to write.
 *
 * Called synchronously at the moment content becomes dirty, so the snapshot is
 * the newest text *before* any await. It is cleared only by a successful write
 * of that same snapshot, or when the composer is discarded/sent.
 */
export function markUnsaved(draft: Draft): void {
  unsaved = draft;
}

/**
 * The write reached storage.
 *
 * `written` is the snapshot that was written, and it is required: the call is a
 * no-op when a *newer* snapshot has been recorded meanwhile, which is the
 * difference between "this text is saved" and "some text is saved". An optional
 * argument would let a caller clear the slot unconditionally by omission, which
 * is exactly the bug this comparison exists to prevent.
 */
export function markSaved(written: Draft): void {
  if (unsaved === written) unsaved = null;
}

/**
 * Forget the snapshot because its draft is gone on purpose (sent, discarded).
 *
 * Takes the snapshot that is being abandoned so one composer cannot clear the
 * protection of another draft: the slot is module-wide, and a bare "clear
 * everything" would silently drop the only copy of text that an unmounted
 * composer left behind.
 */
export function forgetUnsaved(snapshot?: Draft): void {
  if (!snapshot || unsaved === snapshot) unsaved = null;
}

export function hasUnsavedDraft(): boolean {
  return unsaved !== null;
}

/**
 * Try to put the outstanding snapshot on disk. Returns whether it is safe to
 * destroy the webview afterwards.
 *
 * No retry loop and no version guessing: either the stored snapshot is written,
 * or the caller must not acknowledge. A refused acknowledgement costs the user a
 * second close attempt; a wrong acknowledgement costs them the message.
 */
export async function flushUnsavedDraft(): Promise<boolean> {
  const draft = unsaved;
  if (!draft) return true;
  try {
    await composeSave(draft);
    // Only clear if the snapshot did not change while the write ran; a newer one
    // arriving means there is still text that has not reached storage.
    if (unsaved === draft) unsaved = null;
    return !hasUnsavedDraft();
  } catch (error) {
    console.warn("[compose] the unsaved draft could not be written", error);
    return false;
  }
}

/**
 * Injected by the event bridge so this module stays free of transport details
 * and can be tested without Tauri.
 */
let composeSave: (draft: Draft) => Promise<Draft> = async () => {
  throw new Error("compose.save has not been registered");
};

export function registerComposeSave(save: (draft: Draft) => Promise<Draft>): void {
  composeSave = save;
}

/** Test-only. */
export function resetUnsavedDraft(): void {
  unsaved = null;
}
