/**
 * Mail command bus + shortcut resolution.
 *
 * The module-level shortcuts are registered by the mail main pane (mounted only
 * while the module is active) and translated into commands. The list and the
 * reading pane subscribe to the commands they own, which keeps the shortcut
 * handling independent of which child currently has focus.
 */
import type { ReplyMode } from "@/ipc/types";

export type MailCommand =
  | { type: "new"; accountId?: string | null }
  | { type: "reply"; mode: ReplyMode }
  | { type: "navigate"; delta: 1 | -1 }
  | { type: "openWindow" }
  | { type: "delete" }
  | { type: "archive" }
  | { type: "markRead" }
  | { type: "markUnread" }
  | { type: "toggleFlag" }
  | { type: "selectAll" }
  | { type: "undo" }
  | { type: "focusSearch" }
  | { type: "print" }
  | { type: "saveAs" }
  | { type: "createTask" }
  | { type: "move" };

export type MailCommandListener = (command: MailCommand) => void;

const listeners = new Set<MailCommandListener>();

export function onMailCommand(listener: MailCommandListener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function dispatchMailCommand(command: MailCommand): void {
  for (const listener of Array.from(listeners)) {
    try {
      listener(command);
    } catch (error) {
      console.error("[mail] command listener failed", error);
    }
  }
}

export interface ShortcutDescriptor {
  key: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

export interface ShortcutContext {
  /** Command on macOS, Ctrl elsewhere. */
  mac: boolean;
  /** True when focus is in a text field — shortcuts must not steal typing. */
  editable: boolean;
}

/**
 * Translate a keyboard event into a mail command. Returns null when the event
 * is not a mail shortcut. Pure so the mapping can be unit-tested.
 */
export function resolveMailShortcut(
  event: ShortcutDescriptor,
  context: ShortcutContext,
): MailCommand | null {
  if (context.editable) return null;
  const key = event.key;
  const lower = key.toLowerCase();
  const ctrl = Boolean(event.ctrlKey);
  const meta = Boolean(event.metaKey);
  const alt = Boolean(event.altKey);
  const shift = Boolean(event.shiftKey);
  const mod = context.mac ? meta && !ctrl : ctrl && !meta;

  if (mod && !alt) {
    if (lower === "r" && !shift) return { type: "reply", mode: "reply" };
    if (lower === "r" && shift) return { type: "reply", mode: "replyAll" };
    if (lower === "f" && !shift) return { type: "reply", mode: "forward" };
    if (lower === "a" && !shift) return { type: "selectAll" };
    if (lower === "z" && !shift) return { type: "undo" };
    if (lower === "e" && !shift) return { type: "focusSearch" };
    if (lower === "n" && !shift) return { type: "new" };
  }

  // Literal Ctrl on macOS (Mod is Cmd there); plain Ctrl on Windows/Linux too.
  if (ctrl && !meta && !alt && !shift) {
    if (lower === "q") return { type: "markRead" };
    if (lower === "u") return { type: "markUnread" };
  }

  if (!ctrl && !meta && !alt) {
    switch (key) {
      case "ArrowDown":
        return { type: "navigate", delta: 1 };
      case "ArrowUp":
        return { type: "navigate", delta: -1 };
      case "Enter":
        return { type: "openWindow" };
      case "Delete":
        return { type: "delete" };
      case "Backspace":
        return { type: "archive" };
      case "Insert":
        return { type: "toggleFlag" };
      default:
        break;
    }
  }

  return null;
}
