/**
 * Global keyboard shortcut registry.
 *
 * Shortcut syntax: "Mod+Shift+R", "Ctrl+1", "F5", "Del", "Ctrl+,".
 * "Mod" is Command on macOS and Ctrl elsewhere. Matching is exact: extra
 * modifiers held by the user make the shortcut not match. Shortcuts are
 * ignored while typing in inputs/textareas/contenteditable regions unless
 * `allowInInputs` is set.
 */
import { isMac } from "@/lib/platform";

export interface ParsedShortcut {
  mod: boolean;
  ctrl: boolean;
  meta: boolean;
  alt: boolean;
  shift: boolean;
  /** Normalized key: lowercase letters, "delete", "f5", ",", " " … */
  key: string;
}

export interface HotkeyDefinition {
  id: string;
  keys: string;
  handler: (event: KeyboardEvent) => void;
  /** Optional predicate; when false the shortcut is skipped. */
  enabled?: () => boolean;
  allowInInputs?: boolean;
  preventDefault?: boolean;
  stopPropagation?: boolean;
}

const KEY_ALIASES: Record<string, string> = {
  del: "delete",
  delete: "delete",
  backspace: "backspace",
  ins: "insert",
  insert: "insert",
  esc: "escape",
  escape: "escape",
  space: " ",
  spacebar: " ",
  plus: "+",
  arrowup: "arrowup",
  up: "arrowup",
  arrowdown: "arrowdown",
  down: "arrowdown",
  arrowleft: "arrowleft",
  left: "arrowleft",
  arrowright: "arrowright",
  right: "arrowright",
  pageup: "pageup",
  pagedown: "pagedown",
  home: "home",
  end: "end",
  enter: "enter",
  tab: "tab",
  comma: ",",
};

function normalizeKeyToken(token: string): string {
  const lower = token.toLowerCase();
  if (Object.prototype.hasOwnProperty.call(KEY_ALIASES, lower)) {
    return KEY_ALIASES[lower];
  }
  if (token.length === 1) return lower;
  return lower;
}

/** Parse a shortcut spec such as "Mod+Shift+R". Never throws. */
export function parseShortcut(spec: string): ParsedShortcut {
  const result: ParsedShortcut = {
    mod: false,
    ctrl: false,
    meta: false,
    alt: false,
    shift: false,
    key: "",
  };
  const parts = spec
    .split("+")
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
  if (parts.length === 0) return result;

  const keyParts: string[] = [];
  for (const part of parts) {
    const lower = part.toLowerCase();
    switch (lower) {
      case "mod":
        result.mod = true;
        break;
      case "ctrl":
      case "control":
        result.ctrl = true;
        break;
      case "cmd":
      case "command":
      case "meta":
      case "super":
      case "win":
        result.meta = true;
        break;
      case "alt":
      case "option":
      case "opt":
        result.alt = true;
        break;
      case "shift":
        result.shift = true;
        break;
      default:
        keyParts.push(part);
    }
  }
  // The key is the last non-modifier token ("Ctrl++" is not supported).
  const keyToken = keyParts[keyParts.length - 1];
  result.key = keyToken ? normalizeKeyToken(keyToken) : "";
  return result;
}

function keyLabel(key: string, mac: boolean): string {
  switch (key) {
    case "delete":
      return mac ? "⌫" : "Delete";
    case "backspace":
      return mac ? "⌫" : "Backspace";
    case "insert":
      return mac ? "Ins" : "Insert";
    case "escape":
      return mac ? "⎋" : "Esc";
    case " ":
      return "Space";
    case "enter":
      return mac ? "↩" : "Enter";
    case "tab":
      return mac ? "⇥" : "Tab";
    case "arrowup":
      return "↑";
    case "arrowdown":
      return "↓";
    case "arrowleft":
      return "←";
    case "arrowright":
      return "→";
    case "pageup":
      return mac ? "⇞" : "Page Up";
    case "pagedown":
      return mac ? "⇟" : "Page Down";
    case "home":
      return mac ? "↖" : "Home";
    case "end":
      return mac ? "↘" : "End";
    default:
      return key.length === 1 ? key.toUpperCase() : key.toUpperCase();
  }
}

/**
 * Human readable shortcut: "⇧⌘R" on macOS, "Ctrl+Shift+R" elsewhere.
 * Used by menus and tooltips.
 */
export function formatShortcut(spec: string, mac = isMac()): string {
  const parsed = parseShortcut(spec);
  const key = keyLabel(parsed.key, mac);
  if (mac) {
    let out = "";
    if (parsed.ctrl) out += "⌃";
    if (parsed.alt) out += "⌥";
    if (parsed.shift) out += "⇧";
    if (parsed.meta || parsed.mod) out += "⌘";
    return `${out}${key}`;
  }
  const parts: string[] = [];
  if (parsed.ctrl || parsed.mod) parts.push("Ctrl");
  if (parsed.alt) parts.push("Alt");
  if (parsed.shift) parts.push("Shift");
  if (parsed.meta) parts.push("Win");
  parts.push(key);
  return parts.join("+");
}

function eventKey(event: Pick<KeyboardEvent, "key">): string {
  const key = event.key;
  if (!key) return "";
  if (key === " ") return " ";
  return normalizeKeyToken(key);
}

/** Exact-match test used by the registry and by module-local handlers. */
export function matchesKeyboardEvent(
  event: Pick<KeyboardEvent, "key" | "ctrlKey" | "metaKey" | "altKey" | "shiftKey">,
  parsed: ParsedShortcut,
  mac = isMac(),
): boolean {
  if (!parsed.key) return false;
  if (eventKey(event) !== parsed.key) return false;
  const wantCtrl = parsed.ctrl || (parsed.mod && !mac);
  const wantMeta = parsed.meta || (parsed.mod && mac);
  return (
    event.ctrlKey === wantCtrl &&
    event.metaKey === wantMeta &&
    event.altKey === parsed.alt &&
    event.shiftKey === parsed.shift
  );
}

/** True when the event target is a text input where typing should win. */
export function isEditableTarget(target: EventTarget | null): boolean {
  if (!target || typeof target !== "object") return false;
  const element = target as HTMLElement;
  const tag = element.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
  if (element.isContentEditable) return true;
  const role = element.getAttribute?.("role");
  return role === "textbox" || role === "combobox" || role === "searchbox";
}

const registered: Array<HotkeyDefinition & { parsed: ParsedShortcut }> = [];
let cleanup: (() => void) | null = null;

/** Register a shortcut. Returns an unsubscribe function. */
export function registerHotkey(definition: HotkeyDefinition): () => void {
  const entry = { ...definition, parsed: parseShortcut(definition.keys) };
  registered.push(entry);
  return () => {
    const index = registered.indexOf(entry);
    if (index >= 0) registered.splice(index, 1);
  };
}

/** Register many shortcuts at once. */
export function registerHotkeys(definitions: HotkeyDefinition[]): () => void {
  const unsubscribers = definitions.map(registerHotkey);
  return () => {
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}

function handleKeyDown(event: KeyboardEvent): void {
  if (event.defaultPrevented && !event.isComposing) {
    // A child handler already consumed the event.
    return;
  }
  const mac = isMac();
  const editable = isEditableTarget(event.target);
  for (let index = registered.length - 1; index >= 0; index -= 1) {
    const definition = registered[index];
    if (definition.enabled && !definition.enabled()) continue;
    if (editable && !definition.allowInInputs) continue;
    if (!matchesKeyboardEvent(event, definition.parsed, mac)) continue;
    if (definition.preventDefault !== false) event.preventDefault();
    definition.handler(event);
    if (definition.stopPropagation !== false) event.stopPropagation();
    return;
  }
}

/**
 * Attach the single window-level listener. Safe to call multiple times: only
 * the first call attaches anything. Returns a cleanup function.
 */
export function installHotkeys(): () => void {
  if (cleanup) return cleanup;
  window.addEventListener("keydown", handleKeyDown, true);
  cleanup = () => {
    window.removeEventListener("keydown", handleKeyDown, true);
    cleanup = null;
  };
  return cleanup;
}

/** Test-only introspection. */
export function registeredHotkeys(): ReadonlyArray<HotkeyDefinition> {
  return registered;
}
