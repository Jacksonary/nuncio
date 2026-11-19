/**
 * Ephemeral calendar UI state that must be reachable from the module-level
 * shell contracts (`onNew`, hotkeys) as well as from React components.
 * Only session-local UI state lives here; calendar data stays in TanStack Query.
 */
import { create } from "zustand";
import type { RecurrenceEditScope } from "@/ipc/types";

export interface NewEventSeed {
  start: Date;
  end: Date;
  allDay: boolean;
  calendarId: string | null;
  /**
   * Draft title typed into QuickCreate. "More options" hands the same draft to
   * the full editor instead of starting from an empty form.
   */
  title?: string;
}

/** Keep the QuickCreate draft when continuing into the full editor. */
export function mergeSeedDraft(
  seed: NewEventSeed,
  draft: { title: string; calendarId: string | null },
): NewEventSeed {
  return {
    ...seed,
    title: draft.title,
    calendarId: draft.calendarId ?? seed.calendarId,
  };
}

export type EditorRequest =
  | { mode: "create"; seed: NewEventSeed }
  | { mode: "edit"; eventId: string; instanceStart: string | null };

interface CalendarUiState {
  editor: EditorRequest | null;
  /** Incremented by the Del shortcut; Main handles the interactive prompt. */
  deleteRequest: number;
  openCreate: (seed: NewEventSeed) => void;
  openEdit: (eventId: string, instanceStart: string | null) => void;
  closeEditor: () => void;
  requestDeleteSelected: () => void;
}

export const useCalendarUiStore = create<CalendarUiState>()((set) => ({
  editor: null,
  deleteRequest: 0,
  openCreate: (seed) => set({ editor: { mode: "create", seed } }),
  openEdit: (eventId, instanceStart) =>
    set({ editor: { mode: "edit", eventId, instanceStart } }),
  closeEditor: () => set({ editor: null }),
  requestDeleteSelected: () => set((state) => ({ deleteRequest: state.deleteRequest + 1 })),
}));

export type { RecurrenceEditScope };
