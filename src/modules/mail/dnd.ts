/** Drag & drop payload shared by the message list and the folder pane. */
export const MESSAGE_DRAG_MIME = "application/x-nuncio-message-ids";

export function setMessageDragData(dataTransfer: DataTransfer, ids: string[]): void {
  try {
    dataTransfer.setData(MESSAGE_DRAG_MIME, JSON.stringify(ids));
    dataTransfer.setData("text/plain", ids.join(","));
    dataTransfer.effectAllowed = "move";
  } catch {
    // Some browsers restrict custom MIME types; text/plain still works.
  }
}

export function hasMessageDragData(dataTransfer: DataTransfer | null): boolean {
  if (!dataTransfer) return false;
  return Array.from(dataTransfer.types ?? []).includes(MESSAGE_DRAG_MIME);
}

export function readMessageDragData(dataTransfer: DataTransfer | null): string[] {
  if (!dataTransfer) return [];
  try {
    const raw = dataTransfer.getData(MESSAGE_DRAG_MIME);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        return parsed.filter((value): value is string => typeof value === "string");
      }
    }
  } catch {
    // Fall through to the plain text variant.
  }
  const plain = dataTransfer.getData("text/plain");
  return plain ? plain.split(",").map((value) => value.trim()).filter(Boolean) : [];
}
