import { describe, expect, it } from "vitest";
import {
  UNIFIED_CONVERSATION_KEY,
  conversationScopeKey,
  resolveConversation,
} from "./conversationPrefs";

describe("per-mailbox conversation view", () => {
  it("falls back to the global default when a mailbox has no choice", () => {
    expect(resolveConversation({}, "acct-1", true)).toBe(true);
    expect(resolveConversation({}, "acct-1", false)).toBe(false);
  });

  it("keeps one mailbox's choice from touching the others", () => {
    const overrides = { "acct-1": false };
    expect(resolveConversation(overrides, "acct-1", true)).toBe(false);
    expect(resolveConversation(overrides, "acct-2", true)).toBe(true);
  });

  it("lets an explicit choice win over a changed default", () => {
    expect(resolveConversation({ "acct-1": true }, "acct-1", false)).toBe(true);
  });

  it("shares one key across the unified views", () => {
    expect(conversationScopeKey(null)).toBe(UNIFIED_CONVERSATION_KEY);
    expect(conversationScopeKey(undefined)).toBe(UNIFIED_CONVERSATION_KEY);
    expect(conversationScopeKey("acct-1")).toBe("acct-1");
  });
});
