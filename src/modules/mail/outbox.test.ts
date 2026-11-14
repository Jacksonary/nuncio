import { describe, expect, it } from "vitest";
import {
  isOutboxMessageId,
  outboxItemId,
  outboxStatusKey,
  parseOutboxStatus,
} from "./outbox";
import { isSupportedSwipeAction } from "./logic";

describe("outbox id helpers", () => {
  it("recognises synthetic outbox message ids", () => {
    expect(isOutboxMessageId("outbox:abc-123")).toBe(true);
    expect(isOutboxMessageId("outbox:")).toBe(false);
    expect(isOutboxMessageId("message-id")).toBe(false);
    expect(isOutboxMessageId(null)).toBe(false);
  });

  it("strips the prefix for outbox commands", () => {
    expect(outboxItemId("outbox:abc-123")).toBe("abc-123");
    expect(outboxItemId("message-id")).toBeNull();
  });
});

describe("parseOutboxStatus", () => {
  it("reports failed items with their delivery error", () => {
    expect(parseOutboxStatus("Failed: smtp 535 authentication failed")).toEqual({
      state: "failed",
      error: "smtp 535 authentication failed",
    });
  });

  it("reports a failed item without a message", () => {
    expect(parseOutboxStatus("Failed:")).toEqual({ state: "failed", error: null });
  });

  it("reports sending and pending items", () => {
    expect(parseOutboxStatus("Sending to Test <test@example.com>")).toEqual({
      state: "sending",
      error: null,
    });
    expect(parseOutboxStatus("Queued · test@example.com")).toEqual({
      state: "pending",
      error: null,
    });
    expect(parseOutboxStatus("Waiting to retry (2 attempt(s)) · test@example.com")).toEqual({
      state: "pending",
      error: null,
    });
  });

  it("falls back to pending for unknown previews", () => {
    expect(parseOutboxStatus("")).toEqual({ state: "pending", error: null });
    expect(parseOutboxStatus(null)).toEqual({ state: "pending", error: null });
    expect(parseOutboxStatus("hello")).toEqual({ state: "pending", error: null });
  });

  it("maps states to i18n keys", () => {
    expect(outboxStatusKey("failed")).toBe("mail.outbox.state.failed");
    expect(outboxStatusKey("pending")).toBe("mail.outbox.state.pending");
  });
});

describe("isSupportedSwipeAction", () => {
  it("accepts actions that need no destination folder", () => {
    expect(isSupportedSwipeAction("archive")).toBe(true);
    expect(isSupportedSwipeAction("delete")).toBe(true);
    expect(isSupportedSwipeAction("flag")).toBe(true);
    expect(isSupportedSwipeAction("read")).toBe(true);
  });

  it("ignores move and none", () => {
    expect(isSupportedSwipeAction("move")).toBe(false);
    expect(isSupportedSwipeAction("none")).toBe(false);
    expect(isSupportedSwipeAction(null)).toBe(false);
  });
});
