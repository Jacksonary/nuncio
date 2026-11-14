/**
 * People module entry point: Windows 11 Mail & Calendar contacts styling,
 * alphabetical virtualized list, address books/groups, .vcf import/export,
 * contact detail and a full editor.
 */
import { People24Regular } from "@fluentui/react-icons";
import type { ModuleDefinition } from "@/app/moduleTypes";
import type { HotkeyDefinition } from "@/lib/hotkeys";
import { openNewContact } from "./actions";
import { usePeopleUiStore } from "./uiStore";
import { PeopleSidebar } from "./Sidebar";
import { PeopleMain } from "./Main";

/** Dialog safety: module shortcuts must not act underneath a modal. */
function noDialogOpen(): boolean {
  if (usePeopleUiStore.getState().editor) return false;
  return document.querySelector('[role="dialog"], [role="alertdialog"]') === null;
}

const hotkeys: HotkeyDefinition[] = [
  {
    id: "people.newContact",
    keys: "Mod+N",
    handler: () => {
      void openNewContact();
    },
    enabled: noDialogOpen,
    preventDefault: true,
  },
  {
    id: "people.search",
    keys: "Mod+E",
    handler: () => usePeopleUiStore.getState().requestSearchFocus(),
    enabled: noDialogOpen,
    preventDefault: true,
  },
  {
    id: "people.delete",
    keys: "Delete",
    handler: () => usePeopleUiStore.getState().requestDelete(),
    enabled: noDialogOpen,
    preventDefault: true,
  },
];

export const peopleModule: ModuleDefinition = {
  id: "people",
  labelKey: "shell.people",
  Icon: People24Regular,
  Sidebar: PeopleSidebar,
  Main: PeopleMain,
  onNew: () => {
    void openNewContact();
  },
  hotkeys,
};

export default peopleModule;
