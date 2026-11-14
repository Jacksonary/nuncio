/** Drag-and-drop payload shared by task rows and sidebar list targets. */
export const TASK_DRAG_TYPE = "application/x-nuncio-task";

export function hasTaskDrag(dataTransfer: DataTransfer): boolean {
  return Array.from(dataTransfer.types).includes(TASK_DRAG_TYPE);
}

export function readDraggedTaskId(dataTransfer: DataTransfer): string {
  return dataTransfer.getData(TASK_DRAG_TYPE) || dataTransfer.getData("text/plain");
}

export function writeDraggedTaskId(dataTransfer: DataTransfer, taskId: string): void {
  dataTransfer.setData(TASK_DRAG_TYPE, taskId);
  // Fallback for environments that only expose text/plain.
  dataTransfer.setData("text/plain", taskId);
  dataTransfer.effectAllowed = "move";
}
