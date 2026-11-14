/**
 * Mail module entry: split list/reading pane, module-level shortcuts, inline
 * composer host and the "new message" action used by the shell.
 */
import { useCallback, useEffect, useMemo, useRef } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { on } from "@/ipc/api";
import { Mail24Regular } from "@fluentui/react-icons";
import type { ModuleDefinition } from "@/app/moduleTypes";
import { Splitter } from "@/components";
import { isEditableTarget } from "@/lib/hotkeys";
import { isMac } from "@/lib/platform";
import { useShellStore } from "@/lib/store";
import { dispatchMailCommand, onMailCommand, resolveMailShortcut } from "./commands";
import { InlineComposePane, useMailCompose } from "./composeIntegration";
import { parseUnifiedFolderId, unifiedFolderId } from "./logic";
import { MessageList } from "./MessageList";
import { ReadingPane } from "./ReadingPane";
import { useAllFolders, useUnifiedAccounts, useMailAccounts } from "./queries";
import { MailSidebar } from "./Sidebar";
import styles from "./mail.module.css";

/** Elements whose native Enter/Space activation must not be hijacked. */
const INTERACTIVE_SELECTOR =
  "button, a, [role='button'], [role='menuitem'], [role='menuitemcheckbox'], [role='tab']";

function MailMain() {
  const mailAccounts = useMailAccounts();
  const unifiedAccounts = useUnifiedAccounts();
  const folders = useAllFolders(mailAccounts);
  const compose = useMailCompose();
  const inlineCompose = useShellStore((state) => state.inlineCompose);
  const selection = useShellStore((state) => state.selection.mail);
  const setSelection = useShellStore((state) => state.setSelection);

  const folder = selection.folderId ? folders.byId.get(selection.folderId) ?? null : null;
  const preferredAccountId = useMemo(() => {
    if (folder) return folder.accountId;
    if (unifiedAccounts.length > 0) return unifiedAccounts[0]?.id ?? null;
    return mailAccounts[0]?.id ?? null;
  }, [folder, mailAccounts, unifiedAccounts]);

  const preferredAccountRef = useRef<string | null>(preferredAccountId);
  useEffect(() => {
    preferredAccountRef.current = preferredAccountId;
  }, [preferredAccountId]);

  // Pick a sensible default folder once folders have loaded.
  useEffect(() => {
    if (selection.folderId) {
      const unified = parseUnifiedFolderId(selection.folderId);
      if (unified) return;
      if (!folders.isLoading && folders.folders.length > 0 && !folders.byId.has(selection.folderId)) {
        setSelection("mail", { folderId: null, messageId: null });
      }
      return;
    }
    if (folders.isLoading) return;
    if (mailAccounts.length === 0) return;
    if (unifiedAccounts.length >= 2) {
      setSelection("mail", { folderId: unifiedFolderId("inbox"), messageId: null });
      return;
    }
    const firstAccount = mailAccounts[0];
    const inbox = firstAccount
      ? (folders.byAccount.get(firstAccount.id) ?? []).find((entry) => entry.role === "inbox")
      : undefined;
    if (inbox) setSelection("mail", { folderId: inbox.id, messageId: null });
  }, [
    folders,
    mailAccounts,
    selection.folderId,
    setSelection,
    unifiedAccounts.length,
  ]);

  // The shell's "new mail" button and hotkey both flow through the command bus.
  useEffect(() => {
    return onMailCommand((command) => {
      if (command.type === "new") {
        void compose.newInline(command.accountId ?? preferredAccountRef.current);
      }
    });
  }, [compose]);

  // Notifications may carry only an account id. The shell bridge covers
  // folder/message selection; fall back to the account's inbox here.
  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void on("app://navigate", (payload) => {
      if (payload.module !== "mail") return;
      if (payload.folderId || payload.messageId || !payload.accountId) return;
      const inbox = (folders.byAccount.get(payload.accountId) ?? []).find(
        (entry) => entry.role === "inbox",
      );
      if (inbox) {
        setSelection("mail", { folderId: inbox.id, messageId: null });
      }
    })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch((error: unknown) => {
        console.warn("[mail] failed to listen for navigation events", error);
      });
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [folders.byAccount, setSelection]);

  // Module-scoped keyboard shortcuts (mounted with the module).
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.defaultPrevented) return;
      const target = event.target;
      const editable = isEditableTarget(target);
      const command = resolveMailShortcut(
        {
          key: event.key,
          ctrlKey: event.ctrlKey,
          metaKey: event.metaKey,
          altKey: event.altKey,
          shiftKey: event.shiftKey,
        },
        { mac: isMac(), editable },
      );
      if (!command) return;
      if (
        target instanceof Element &&
        target.closest(INTERACTIVE_SELECTOR) &&
        (event.key === "Enter" || event.key === " ")
      ) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      dispatchMailCommand(command);
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  return (
    <div className={styles.mailMain}>
      <Splitter
        storageKey="mail.list"
        primary={<MessageList />}
        secondary={
          inlineCompose ? <InlineComposePane /> : <ReadingPane messageId={selection.messageId} />
        }
        defaultSize={380}
        min={300}
        max={720}
        primaryClassName={styles.listColumn}
        secondaryClassName={styles.readerColumn}
        handleLabel=""
        resetOnDoubleClick
      />
    </div>
  );
}

export const mailModule: ModuleDefinition = {
  id: "mail",
  labelKey: "shell.mail",
  Icon: Mail24Regular,
  Sidebar: MailSidebar,
  Main: MailMain,
  onNew: () => dispatchMailCommand({ type: "new" }),
};

export default mailModule;
