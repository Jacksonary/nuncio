import { describe, expect, it } from "vitest";
import { resolveMailShortcut, type ShortcutDescriptor } from "./commands";

function event(partial: ShortcutDescriptor): ShortcutDescriptor {
  return {
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    shiftKey: false,
    ...partial,
  };
}

describe("resolveMailShortcut", () => {
  it("maps reply shortcuts with Mod on Windows/Linux and macOS", () => {
    expect(resolveMailShortcut(event({ key: "r", ctrlKey: true }), { mac: false, editable: false })).toEqual({
      type: "reply",
      mode: "reply",
    });
    expect(resolveMailShortcut(event({ key: "R", ctrlKey: true, shiftKey: true }), { mac: false, editable: false })).toEqual({
      type: "reply",
      mode: "replyAll",
    });
    expect(resolveMailShortcut(event({ key: "r", metaKey: true }), { mac: true, editable: false })).toEqual({
      type: "reply",
      mode: "reply",
    });
    expect(resolveMailShortcut(event({ key: "r", ctrlKey: true }), { mac: true, editable: false })).toBeNull();
  });

  it("maps forward, select all, undo, search and new", () => {
    const mod = { mac: false, editable: false };
    expect(resolveMailShortcut(event({ key: "f", ctrlKey: true }), mod)).toEqual({
      type: "reply",
      mode: "forward",
    });
    expect(resolveMailShortcut(event({ key: "a", ctrlKey: true }), mod)).toEqual({ type: "selectAll" });
    expect(resolveMailShortcut(event({ key: "z", ctrlKey: true }), mod)).toEqual({ type: "undo" });
    expect(resolveMailShortcut(event({ key: "e", ctrlKey: true }), mod)).toEqual({ type: "focusSearch" });
    expect(resolveMailShortcut(event({ key: "n", ctrlKey: true }), mod)).toEqual({ type: "new" });
  });

  it("uses literal Ctrl for read/unread on both platforms", () => {
    expect(resolveMailShortcut(event({ key: "q", ctrlKey: true }), { mac: true, editable: false })).toEqual({
      type: "markRead",
    });
    expect(resolveMailShortcut(event({ key: "u", ctrlKey: true }), { mac: false, editable: false })).toEqual({
      type: "markUnread",
    });
  });

  it("maps navigation and mutation keys", () => {
    const ctx = { mac: false, editable: false };
    expect(resolveMailShortcut(event({ key: "ArrowDown" }), ctx)).toEqual({ type: "navigate", delta: 1 });
    expect(resolveMailShortcut(event({ key: "ArrowUp" }), ctx)).toEqual({ type: "navigate", delta: -1 });
    expect(resolveMailShortcut(event({ key: "Enter" }), ctx)).toEqual({ type: "openWindow" });
    expect(resolveMailShortcut(event({ key: "Delete" }), ctx)).toEqual({ type: "delete" });
    expect(resolveMailShortcut(event({ key: "Backspace" }), ctx)).toEqual({ type: "archive" });
    expect(resolveMailShortcut(event({ key: "Insert" }), ctx)).toEqual({ type: "toggleFlag" });
  });

  it("ignores shortcuts while typing", () => {
    expect(
      resolveMailShortcut(event({ key: "r", ctrlKey: true }), { mac: false, editable: true }),
    ).toBeNull();
    expect(resolveMailShortcut(event({ key: "Delete" }), { mac: false, editable: true })).toBeNull();
  });

  it("returns null for unrelated keys", () => {
    const ctx = { mac: false, editable: false };
    expect(resolveMailShortcut(event({ key: "x" }), ctx)).toBeNull();
    expect(resolveMailShortcut(event({ key: "r" }), ctx)).toBeNull();
    expect(resolveMailShortcut(event({ key: "r", ctrlKey: true, altKey: true }), ctx)).toBeNull();
  });
});
