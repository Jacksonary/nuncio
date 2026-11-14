/**
 * UI state store (zustand). Backend data lives in TanStack Query; this store
 * only holds ephemeral UI state and a few persisted layout preferences.
 */
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import type { AppModule, CalendarView, Id, SmartList } from "@/ipc/types";

export interface MailSelection {
  folderId: Id | null;
  messageId: Id | null;
}

export interface CalendarSelection {
  /** "YYYY-MM-DD" anchor date for the current view. */
  date: string | null;
  view: CalendarView | null;
  eventId: Id | null;
  instanceStart: string | null;
}

export interface PeopleSelection {
  contactId: Id | null;
  addressBookId: Id | null;
  groupId: Id | null;
}

export interface TodoSelection {
  listId: Id | null;
  taskId: Id | null;
  smart: SmartList | null;
}

export interface SelectionState {
  mail: MailSelection;
  calendar: CalendarSelection;
  people: PeopleSelection;
  todo: TodoSelection;
}

export interface InlineComposeState {
  draftId: Id | null;
  accountId: Id | null;
}

export interface ShellStore {
  activeModule: AppModule;
  railCollapsed: boolean;
  showSettings: boolean;
  selection: SelectionState;
  /** Persisted splitter sizes, keyed by `storageKey`. */
  panelSizes: Record<string, number>;
  /** Add-account wizard opened from the mail sidebar (not the settings panel). */
  showAddAccount: boolean;
  inlineCompose: InlineComposeState | null;
  pendingIcsPath: string | null;
  pendingVcfPath: string | null;
  firstRunSkipped: boolean;

  setActiveModule: (module: AppModule) => void;
  setRailCollapsed: (collapsed: boolean) => void;
  toggleRail: () => void;
  setShowSettings: (open: boolean) => void;
  setShowAddAccount: (open: boolean) => void;
  setSelection: <K extends keyof SelectionState>(
    module: K,
    patch: Partial<SelectionState[K]>,
  ) => void;
  setPanelSize: (key: string, size: number) => void;
  openInlineCompose: (state: InlineComposeState) => void;
  closeInlineCompose: () => void;
  setPendingIcsPath: (path: string | null) => void;
  setPendingVcfPath: (path: string | null) => void;
  setFirstRunSkipped: (skipped: boolean) => void;
}

const initialSelection: SelectionState = {
  mail: { folderId: null, messageId: null },
  calendar: { date: null, view: null, eventId: null, instanceStart: null },
  people: { contactId: null, addressBookId: null, groupId: null },
  todo: { listId: null, taskId: null, smart: null },
};

export const useShellStore = create<ShellStore>()(
  persist(
    (set) => ({
      activeModule: "mail",
      railCollapsed: false,
      showSettings: false,
      showAddAccount: false,
      selection: initialSelection,
      panelSizes: {},
      inlineCompose: null,
      pendingIcsPath: null,
      pendingVcfPath: null,
      firstRunSkipped: false,

      setActiveModule: (module) => set({ activeModule: module }),
      setRailCollapsed: (collapsed) => set({ railCollapsed: collapsed }),
      toggleRail: () => set((state) => ({ railCollapsed: !state.railCollapsed })),
      setShowSettings: (open) => set({ showSettings: open }),
      setShowAddAccount: (open) => set({ showAddAccount: open }),
      setSelection: (module, patch) =>
        set((state) => ({
          selection: {
            ...state.selection,
            [module]: { ...state.selection[module], ...patch },
          },
        })),
      setPanelSize: (key, size) =>
        set((state) => ({ panelSizes: { ...state.panelSizes, [key]: size } })),
      openInlineCompose: (inlineCompose) => set({ inlineCompose }),
      closeInlineCompose: () => set({ inlineCompose: null }),
      setPendingIcsPath: (path) => set({ pendingIcsPath: path }),
      setPendingVcfPath: (path) => set({ pendingVcfPath: path }),
      setFirstRunSkipped: (skipped) => set({ firstRunSkipped: skipped }),
    }),
    {
      name: "nuncio.shell",
      storage: createJSONStorage(() => localStorage),
      partialize: (state) => ({
        activeModule: state.activeModule,
        railCollapsed: state.railCollapsed,
        panelSizes: state.panelSizes,
        firstRunSkipped: state.firstRunSkipped,
      }),
    },
  ),
);

/** Convenience selector: currently selected ids for the active module. */
export function useActiveSelection() {
  const activeModule = useShellStore((state) => state.activeModule);
  const selection = useShellStore((state) => state.selection);
  return { module: activeModule, selection: selection[activeModule] };
}
