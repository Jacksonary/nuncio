/**
 * Compose store — the single "currently open inline composer" for the mail
 * module. The reading pane renders `ComposeView` when `active` is non-null.
 *
 * `revision` increments only when a *new* draft is opened (not when autosave
 * syncs the same draft back), so the mounted editor is not remounted on every
 * keystroke.
 */
import { create } from "zustand";
import type { Draft } from "@/ipc/types";

export interface ComposeStoreState {
  /** Draft shown in the inline composer; null = no composer. */
  active: Draft | null;
  /** Bumped whenever `open` is called, used as a React key. */
  revision: number;
  open: (draft: Draft) => void;
  /**
   * Replace the active draft after a save. Pass `expected` (the value the
   * caller last saw in the store) so a late save from a closed/replaced
   * composer cannot clobber a newer draft.
   */
  update: (draft: Draft, expected?: Draft | null) => void;
  close: () => void;
  /**
   * Registers the mounted composer's "save now" function. The shell calls it
   * before the webview is destroyed (close-to-tray) so pending edits are not
   * lost with the window.
   */
  flushHandler: (() => Promise<boolean>) | null;
  setFlushHandler: (handler: (() => Promise<boolean>) | null) => void;
  /**
   * Called when the backend decides not to destroy the window after all, so the
   * composer can unfreeze. Only an explicit signal lifts the freeze; a timer
   * could expire while the close is still pending.
   */
  cancelCloseHandler: (() => void) | null;
  setCancelCloseHandler: (handler: (() => void) | null) => void;
}

export const useComposeStore = create<ComposeStoreState>((set) => ({
  active: null,
  revision: 0,
  flushHandler: null,
  setFlushHandler: (flushHandler) => set({ flushHandler }),
  cancelCloseHandler: null,
  setCancelCloseHandler: (cancelCloseHandler) => set({ cancelCloseHandler }),
  open: (draft) =>
    set((state) => ({
      active: draft,
      revision: state.revision + 1,
    })),
  update: (draft, expected) =>
    set((state) => {
      if (!state.active) return state;
      if (expected != null) {
        if (state.active !== expected) return state;
      } else if (state.active.id !== draft.id) {
        return state;
      }
      return { active: draft };
    }),
  close: () => set({ active: null }),
}));
