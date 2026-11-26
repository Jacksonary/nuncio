/**
 * The WebView ships its own context menu (Back / Reload / Inspect Element).
 * It has no place in a desktop app: our menus are React components that call
 * `preventDefault()` on their own targets, so every area without one — empty
 * sidebar space, the calendar grid background, a blank reading pane — used to
 * open that browser menu instead.
 *
 * The platform menu is still the right one where it carries real value:
 * editing a field (cut/copy/paste, spellcheck), right-clicking a link
 * ("Copy Link"), or copying a text selection.
 */
const NATIVE_MENU_TARGETS = [
  "input",
  "textarea",
  "a[href]",
  "[contenteditable]:not([contenteditable='false'])",
].join(", ");

export function prefersNativeContextMenu(
  target: EventTarget | null,
  selection: Selection | null,
): boolean {
  const element =
    typeof Element !== "undefined" && target instanceof Element ? target : null;
  if (element?.closest(NATIVE_MENU_TARGETS)) return true;
  return Boolean(selection && !selection.isCollapsed && selection.toString().trim());
}

/** Suppress the WebView menu everywhere except the cases above. */
export function installContextMenuGuard(): void {
  if (typeof document === "undefined") return;
  document.addEventListener("contextmenu", (event) => {
    if (prefersNativeContextMenu(event.target, window.getSelection())) return;
    event.preventDefault();
  });
}
