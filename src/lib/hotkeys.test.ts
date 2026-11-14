import { describe, expect, it } from "vitest";
import {
  formatShortcut,
  isEditableTarget,
  matchesKeyboardEvent,
  parseShortcut,
} from "@/lib/hotkeys";

function event(overrides: Partial<KeyboardEvent>): Pick<
  KeyboardEvent,
  "key" | "ctrlKey" | "metaKey" | "altKey" | "shiftKey"
> {
  return {
    key: "",
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    shiftKey: false,
    ...overrides,
  };
}

describe("parseShortcut", () => {
  it("parses the Mod+Shift+R spec", () => {
    expect(parseShortcut("Mod+Shift+R")).toEqual({
      mod: true,
      ctrl: false,
      meta: false,
      alt: false,
      shift: true,
      key: "r",
    });
  });

  it("normalizes special keys", () => {
    expect(parseShortcut("Del").key).toBe("delete");
    expect(parseShortcut("F5").key).toBe("f5");
    expect(parseShortcut("Ctrl+,").key).toBe(",");
    expect(parseShortcut("Mod+ArrowDown").key).toBe("arrowdown");
    expect(parseShortcut("Ins").key).toBe("insert");
  });

  it("supports explicit Ctrl / Cmd", () => {
    const parsed = parseShortcut("Ctrl+Alt+K");
    expect(parsed.ctrl).toBe(true);
    expect(parsed.alt).toBe(true);
    expect(parsed.key).toBe("k");
    expect(parseShortcut("Cmd+P").meta).toBe(true);
  });

  it("returns an empty key for modifier-only specs", () => {
    expect(parseShortcut("Shift").key).toBe("");
  });
});

describe("formatShortcut", () => {
  it("formats for macOS with symbols", () => {
    expect(formatShortcut("Mod+Shift+R", true)).toBe("⇧⌘R");
    expect(formatShortcut("Del", true)).toBe("⌫");
  });

  it("formats for Windows/Linux", () => {
    expect(formatShortcut("Mod+Shift+R", false)).toBe("Ctrl+Shift+R");
    expect(formatShortcut("Del", false)).toBe("Delete");
    expect(formatShortcut("F5", false)).toBe("F5");
  });
});

describe("matchesKeyboardEvent", () => {
  const parsed = parseShortcut("Mod+Shift+R");

  it("matches Mod to Meta on macOS", () => {
    expect(
      matchesKeyboardEvent(event({ key: "R", metaKey: true, shiftKey: true }), parsed, true),
    ).toBe(true);
    expect(
      matchesKeyboardEvent(event({ key: "r", ctrlKey: true, shiftKey: true }), parsed, true),
    ).toBe(false);
  });

  it("matches Mod to Ctrl elsewhere", () => {
    expect(
      matchesKeyboardEvent(event({ key: "r", ctrlKey: true, shiftKey: true }), parsed, false),
    ).toBe(true);
    expect(
      matchesKeyboardEvent(event({ key: "r", ctrlKey: true }), parsed, false),
    ).toBe(false);
  });

  it("requires exact modifiers", () => {
    const simple = parseShortcut("Mod+R");
    expect(
      matchesKeyboardEvent(event({ key: "r", ctrlKey: true, shiftKey: true }), simple, false),
    ).toBe(false);
  });

  it("handles special keys", () => {
    expect(
      matchesKeyboardEvent(event({ key: "Delete" }), parseShortcut("Del"), false),
    ).toBe(true);
    expect(
      matchesKeyboardEvent(event({ key: "F5" }), parseShortcut("F5"), false),
    ).toBe(true);
  });
});

describe("isEditableTarget", () => {
  it("detects inputs, textareas, selects and contenteditable", () => {
    expect(isEditableTarget({ tagName: "INPUT" } as unknown as EventTarget)).toBe(true);
    expect(isEditableTarget({ tagName: "TEXTAREA" } as unknown as EventTarget)).toBe(true);
    expect(isEditableTarget({ tagName: "SELECT" } as unknown as EventTarget)).toBe(true);
    expect(
      isEditableTarget({ tagName: "DIV", isContentEditable: true } as unknown as EventTarget),
    ).toBe(true);
    expect(isEditableTarget({ tagName: "DIV" } as unknown as EventTarget)).toBe(false);
    expect(isEditableTarget(null)).toBe(false);
  });
});
