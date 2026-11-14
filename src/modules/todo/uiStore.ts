/**
 * Ephemeral To Do UI state that must be reachable from the module-level
 * contract (`onNew`, hotkeys) as well as from React components. Backend data
 * lives in TanStack Query; only session/local layout state lives here.
 */
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import type { TaskSortMode } from "./logic";

export interface DetailEditRequest {
  taskId: string;
  nonce: number;
}

export interface TodoUiState {
  /** Sort order chosen from the list header menu (persisted). */
  sortMode: TaskSortMode;
  setSortMode: (mode: TaskSortMode) => void;

  /** Completed section collapsed inside the current list (persisted). */
  completedCollapsed: boolean;
  setCompletedCollapsed: (collapsed: boolean) => void;

  /** Incremented to focus the "add task" input from the header button. */
  addRequest: number;
  requestAdd: () => void;

  /** Incremented to focus the detail title after Enter on a row. */
  editRequest: DetailEditRequest | null;
  requestDetailEdit: (taskId: string) => void;

  /** Incremented to open the "new list" dialog from the main pane. */
  createListRequest: number;
  requestCreateList: () => void;
}

export const useTodoUiStore = create<TodoUiState>()(
  persist(
    (set) => ({
      sortMode: "custom",
      setSortMode: (sortMode) => set({ sortMode }),

      completedCollapsed: false,
      setCompletedCollapsed: (completedCollapsed) => set({ completedCollapsed }),

      addRequest: 0,
      requestAdd: () => set((state) => ({ addRequest: state.addRequest + 1 })),

      editRequest: null,
      requestDetailEdit: (taskId) =>
        set((state) => ({
          editRequest: {
            taskId,
            nonce: (state.editRequest?.nonce ?? 0) + 1,
          },
        })),

      createListRequest: 0,
      requestCreateList: () =>
        set((state) => ({ createListRequest: state.createListRequest + 1 })),
    }),
    {
      name: "nuncio.todo.ui",
      storage: createJSONStorage(() => localStorage),
      partialize: (state) => ({
        sortMode: state.sortMode,
        completedCollapsed: state.completedCollapsed,
      }),
    },
  ),
);
