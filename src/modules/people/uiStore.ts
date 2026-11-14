/**
 * Session-local People UI state: search text, multi-selection, the editor
 * request and one-shot commands triggered from module hotkeys. Backend data
 * stays in TanStack Query; the persisted scope lives in the shell store.
 */
import { create } from "zustand";
import type { Id } from "@/ipc/types";

export type ContactEditorRequest =
  | { mode: "create"; addressBookId: Id | null }
  | { mode: "edit"; contactId: Id };

interface PeopleUiState {
  favoritesOnly: boolean;
  search: string;
  selectedIds: Id[];
  /** "multi" reveals the bulk action bar (checkbox / Ctrl / Shift selection). */
  selectionMode: "single" | "multi";
  /** Anchor row for Shift-click range selection. */
  anchorId: Id | null;
  editor: ContactEditorRequest | null;
  importOpen: boolean;
  /** Incremented by the search hotkey; Main focuses the search box. */
  focusSearchRequest: number;
  /** Incremented by the Delete hotkey; Main shows the delete confirmation. */
  deleteRequest: number;

  setFavoritesOnly: (value: boolean) => void;
  setSearch: (value: string) => void;
  setSelectedIds: (ids: Id[]) => void;
  setSelectionMode: (mode: "single" | "multi") => void;
  toggleSelected: (id: Id) => void;
  setAnchor: (id: Id | null) => void;
  clearSelection: () => void;
  openCreate: (addressBookId: Id | null) => void;
  openEdit: (contactId: Id) => void;
  closeEditor: () => void;
  setImportOpen: (open: boolean) => void;
  requestSearchFocus: () => void;
  requestDelete: () => void;
}

export const usePeopleUiStore = create<PeopleUiState>()((set) => ({
  favoritesOnly: false,
  search: "",
  selectedIds: [],
  selectionMode: "single",
  anchorId: null,
  editor: null,
  importOpen: false,
  focusSearchRequest: 0,
  deleteRequest: 0,

  setFavoritesOnly: (value) => set({ favoritesOnly: value }),
  setSearch: (value) => set({ search: value }),
  setSelectedIds: (ids) => set({ selectedIds: ids }),
  setSelectionMode: (selectionMode) => set({ selectionMode }),
  toggleSelected: (id) =>
    set((state) => ({
      selectionMode: "multi",
      selectedIds: state.selectedIds.includes(id)
        ? state.selectedIds.filter((candidate) => candidate !== id)
        : [...state.selectedIds, id],
    })),
  setAnchor: (id) => set({ anchorId: id }),
  clearSelection: () => set({ selectedIds: [], anchorId: null, selectionMode: "single" }),
  openCreate: (addressBookId) => set({ editor: { mode: "create", addressBookId } }),
  openEdit: (contactId) => set({ editor: { mode: "edit", contactId } }),
  closeEditor: () => set({ editor: null }),
  setImportOpen: (open) => set({ importOpen: open }),
  requestSearchFocus: () =>
    set((state) => ({ focusSearchRequest: state.focusSearchRequest + 1 })),
  requestDelete: () => set((state) => ({ deleteRequest: state.deleteRequest + 1 })),
}));

/** Scope currently shown in the list. */
export interface PeopleScope {
  addressBookId: Id | null;
  groupId: Id | null;
  favoritesOnly: boolean;
}
