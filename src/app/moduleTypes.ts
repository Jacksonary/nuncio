/**
 * Module contract. Every `src/modules/<module>/index.tsx` default-exports a
 * `ModuleDefinition`; the shell renders its Sidebar and Main and wires the
 * primary "new" action and module-level hotkeys.
 *
 * Secondary windows and the settings flyout have their own prop contracts,
 * declared here so module agents and the shell agree on the signatures.
 */
import type { ComponentType } from "react";
import type { Account, AppModule } from "@/ipc/types";
import type { HotkeyDefinition } from "@/lib/hotkeys";

export interface ModuleIconProps {
  className?: string;
}

export type ModuleIcon = ComponentType<ModuleIconProps>;

export interface ModuleDefinition {
  id: AppModule;
  /** i18n key for the module name (e.g. "shell.mail"). */
  labelKey: string;
  Icon: ModuleIcon;
  /** Left navigation content for the module (folders, mini calendar, …). */
  Sidebar: ComponentType;
  /** Main content pane. */
  Main: ComponentType;
  /** Primary action in the nav header ("New mail", "New event", …). */
  onNew?: () => void;
  /** Module-scoped global shortcuts, registered while the module is active. */
  hotkeys?: HotkeyDefinition[];
}

export interface MessageWindowProps {
  /** Message id for `#/message/<id>`; null for an external .eml file. */
  messageId: string | null;
  /** Path for `#/message/eml?path=...`; null for a stored message. */
  emlPath: string | null;
}

export interface ComposeWindowProps {
  /** Draft id for `#/compose/<draftId>`; null for a new message. */
  draftId: string | null;
  /** Preferred account for `#/compose/new?accountId=...`. */
  accountId: string | null;
}

export interface SettingsPanelProps {
  onClose: () => void;
}

export interface AddAccountDialogProps {
  open: boolean;
  onClose: () => void;
  onCreated?: (account: Account) => void;
}

export interface AccountsSectionProps {
  /** Called when the user asks to add another account. */
  onAddAccount?: () => void;
}
