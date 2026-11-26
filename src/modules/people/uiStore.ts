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

/** A delete command raised by the Delete hotkey, valid for one list read. */
export interface ContactDeleteRequest {
  ids: Id[];
}

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
  /** Incremented by the select-all hotkey; the list selects every visible row. */
  selectAllRequest: number;
  /**
   * Raised by the Delete hotkey. The ids are captured at that moment; the list
   * consumes the request when it reads it, so cancelling the confirmation
   * cannot replay it after a remount.
   */
  deleteRequest: ContactDeleteRequest | null;

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
  requestSelectAll: () => void;
  requestDelete: (fallbackId?: Id | null) => void;
  /** Read and clear the pending delete command (one-shot semantics). */
  consumeDeleteRequest: () => ContactDeleteRequest | null;
}

export const usePeopleUiStore = create<PeopleUiState>()((set, get) => ({
  favoritesOnly: false,
  search: "",
  selectedIds: [],
  selectionMode: "single",
  anchorId: null,
  editor: null,
  importOpen: false,
  focusSearchRequest: 0,
  selectAllRequest: 0,
  deleteRequest: null,

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
  requestSelectAll: () =>
    set((state) => ({ selectAllRequest: state.selectAllRequest + 1 })),
  requestDelete: (fallbackId) =>
    set((state) => {
      const ids =
        state.selectedIds.length > 0
          ? state.selectedIds
          : fallbackId
            ? [fallbackId]
            : [];
      // Nothing selected: leave any state untouched rather than queueing an
      // empty confirmation.
      if (ids.length === 0) return {};
      return { deleteRequest: { ids } };
    }),
  consumeDeleteRequest: () => {
    const request = get().deleteRequest;
    if (request) set({ deleteRequest: null });
    return request;
  },
}));

/** Scope currently shown in the list. */
export interface PeopleScope {
  addressBookId: Id | null;
  groupId: Id | null;
  favoritesOnly: boolean;
}
