/**
 * Opens the "create task from mail" dialog from anywhere in the mail module.
 *
 * The dialog is rendered once by the module shell; the list and the reading
 * pane only publish which message it should be about.
 */
import { create } from "zustand";
import type { Address, MessageSummary } from "@/ipc/types";
import { formatDateTime } from "@/lib/format";
import type { TaskSource } from "./CreateTaskDialog";

interface MailTaskDialogState {
  source: TaskSource | null;
  open: (source: TaskSource) => void;
  close: () => void;
}

export const useMailTaskDialog = create<MailTaskDialogState>((set) => ({
  source: null,
  open: (source) => set({ source }),
  close: () => set({ source: null }),
}));

function formatSender(from: Address | null): string {
  if (!from) return "";
  return from.name ? `${from.name} <${from.email}>` : from.email;
}

export function openTaskFromMessage(summary: MessageSummary): void {
  useMailTaskDialog.getState().open({
    id: summary.id,
    subject: summary.subject,
    accountId: summary.accountId,
    from: formatSender(summary.from),
    date: formatDateTime(summary.date),
  });
}
