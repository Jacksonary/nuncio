import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  flushUnsavedDraft,
  forgetUnsaved,
  hasUnsavedDraft,
  markSaved,
  markUnsaved,
  registerComposeSave,
  resetUnsavedDraft,
} from "./pendingWrites";
import type { Draft } from "@/ipc/types";

function draft(id: string | null, subject: string): Draft {
  return {
    id,
    accountId: "acc",
    identityId: null,
    from: { name: null, email: "me@example.com" },
    to: [],
    cc: [],
    bcc: [],
    subject,
    html: "",
    attachments: [],
    references: [],
    inReplyTo: null,
    replyMode: null,
    isForwarded: false,
    remoteDraftMessageId: null,
    updatedAt: "2026-01-01T00:00:00Z",
  } as unknown as Draft;
}

describe("pendingWrites", () => {
  beforeEach(() => {
    resetUnsavedDraft();
  });

  afterEach(() => {
    resetUnsavedDraft();
  });

  it("reports nothing to flush when no composer recorded a snapshot", async () => {
    const save = vi.fn();
    registerComposeSave(save);
    expect(hasUnsavedDraft()).toBe(false);
    await expect(flushUnsavedDraft()).resolves.toBe(true);
    expect(save).not.toHaveBeenCalled();
  });

  it("writes the recorded snapshot and clears it", async () => {
    const save = vi.fn(async (value: Draft) => ({ ...value, id: "d1" }));
    registerComposeSave(save);
    const snapshot = draft(null, "hello");
    markUnsaved(snapshot);

    await expect(flushUnsavedDraft()).resolves.toBe(true);
    expect(save).toHaveBeenCalledWith(snapshot);
    expect(hasUnsavedDraft()).toBe(false);
  });

  it("keeps the snapshot when the write fails, so the close is refused", async () => {
    registerComposeSave(async () => {
      throw new Error("disk full");
    });
    markUnsaved(draft(null, "hello"));

    await expect(flushUnsavedDraft()).resolves.toBe(false);
    // Still recorded: a later attempt can try again instead of losing the text.
    expect(hasUnsavedDraft()).toBe(true);
  });

  it("does not let an older successful write clear a newer snapshot", () => {
    const older = draft("d1", "first");
    markUnsaved(older);
    markSaved(older);
    expect(hasUnsavedDraft()).toBe(false);

    // The user kept typing: a newer snapshot replaced the older one, and the
    // older write finishing late must not declare it saved.
    const newer = draft("d1", "second");
    markUnsaved(newer);
    markSaved(older);
    expect(hasUnsavedDraft()).toBe(true);
  });

  it("re-checks the slot after writing, so content typed during the write still blocks the close", async () => {
    let release: (() => void) | undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    registerComposeSave(async (value: Draft) => {
      await gate;
      return { ...value, id: "d1" };
    });

    const first = draft(null, "before");
    markUnsaved(first);
    const flushing = flushUnsavedDraft();
    // Typed while the write is in flight.
    markUnsaved(draft("d1", "after"));
    release?.();

    await expect(flushing).resolves.toBe(false);
    expect(hasUnsavedDraft()).toBe(true);
  });

  it("forgets the snapshot when the draft is sent or discarded", () => {
    markUnsaved(draft(null, "bye"));
    expect(hasUnsavedDraft()).toBe(true);
    forgetUnsaved();
    expect(hasUnsavedDraft()).toBe(false);
  });
});

describe("pendingWrites ownership", () => {
  beforeEach(() => {
    resetUnsavedDraft();
  });

  afterEach(() => {
    resetUnsavedDraft();
  });

  it("does not let one composer forget another draft's protection", () => {
    const mine = draft("a", "mine");
    const theirs = draft("b", "theirs");
    markUnsaved(theirs);
    // The slot holds `theirs`; forgetting `mine` must leave it alone.
    forgetUnsaved(mine);
    expect(hasUnsavedDraft()).toBe(true);
    forgetUnsaved(theirs);
    expect(hasUnsavedDraft()).toBe(false);
  });

  it("does not let a stale save clear a snapshot it did not write", () => {
    const older = draft("a", "older");
    const newer = draft("a", "newer");
    markUnsaved(newer);
    markSaved(older);
    expect(hasUnsavedDraft()).toBe(true);
  });
});
