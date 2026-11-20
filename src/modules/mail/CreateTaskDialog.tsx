/**
 * "Create task from mail" dialog.
 *
 * The old flow used a fixed title/notes/list and silently flagged the mail.
 * This one lets the user pick the target list, due date, reminder and whether
 * the source message should be flagged.
 */
import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Button,
  Checkbox,
  DatePicker,
  Dialog,
  Input,
  Select,
  TextArea,
  TimePicker,
  toast,
} from "@/components";
import {
  accounts as accountsApi,
  isIpcError,
  messages as messagesApi,
  tasks as tasksApi,
} from "@/ipc/api";
import type { IsoDate, NewTaskFromMessage, TaskList } from "@/ipc/types";
import { queryKeys } from "@/lib/queryKeys";
import { useT } from "@/lib/i18n";
import { toIsoDate } from "@/lib/format";
import {
  combineReminder,
  defaultReminder,
  groupListsByAccount,
} from "@/modules/todo/logic";

export interface TaskSource {
  id: string;
  subject: string;
  accountId: string;
  from: string;
  date: string;
}

export interface CreateTaskDialogProps {
  open: boolean;
  source: TaskSource | null;
  onClose: () => void;
}

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function CreateTaskDialog({ open, source, onClose }: CreateTaskDialogProps) {
  const t = useT();
  const [title, setTitle] = useState("");
  const [notes, setNotes] = useState("");
  const [listId, setListId] = useState<string | null>(null);
  const [dueDate, setDueDate] = useState<IsoDate | null>(null);
  const [reminderOn, setReminderOn] = useState(false);
  const [reminderDate, setReminderDate] = useState<IsoDate>(() => defaultReminder(new Date()).date);
  const [reminderTime, setReminderTime] = useState<string>(
    () => defaultReminder(new Date()).time,
  );
  const [markFlagged, setMarkFlagged] = useState(false);
  const [busy, setBusy] = useState(false);

  const today = toIsoDate(new Date());

  const listsQuery = useQuery({
    queryKey: queryKeys.taskLists(),
    queryFn: tasksApi.lists,
    staleTime: 60_000,
    enabled: open,
  });
  const accountsQuery = useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
    staleTime: 60_000,
    enabled: open,
  });
  const lists: TaskList[] = listsQuery.data ?? [];
  const accounts = accountsQuery.data ?? [];

  const options = useMemo(() => {
    const groups = groupListsByAccount(lists, accounts);
    return groups.flatMap((group) =>
      group.lists
        .filter((list) => !list.isReadOnly)
        .map((list) => ({
          value: list.id,
          label: `${group.label} · ${list.name}`,
        })),
    );
  }, [lists, accounts]);

  // Reset the form each time the dialog opens for a message.
  useEffect(() => {
    if (!open || !source) return;
    setTitle(source.subject.trim() || t("mail.createTask.untitled"));
    setNotes(
      `${t("mail.createTask.sender")}：${source.from}\n${t("mail.createTask.date")}：${source.date}`,
    );
    setDueDate(today);
    setReminderOn(false);
    const defaults = defaultReminder(new Date());
    setReminderDate(defaults.date);
    setReminderTime(defaults.time);
    setMarkFlagged(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, source?.id]);

  // Default target: the mail account's default list, else any writable list.
  useEffect(() => {
    if (!open) return;
    const writable = lists.filter((list) => !list.isReadOnly);
    if (writable.length === 0) {
      setListId(null);
      return;
    }
    setListId((current) => {
      if (current && writable.some((list) => list.id === current)) return current;
      const preferred =
        writable.find((list) => list.accountId === source?.accountId && list.isDefault) ??
        writable.find((list) => list.accountId === source?.accountId) ??
        writable.find((list) => list.isDefault) ??
        writable[0];
      return preferred.id;
    });
  }, [open, lists, source?.accountId]);

  const submit = async () => {
    if (!source || !listId || busy) return;
    const input: NewTaskFromMessage = {
      title: title.trim(),
      notes,
      listId,
      dueDate,
      reminderAt: reminderOn
        ? combineReminder(reminderDate, reminderTime)
        : null,
      myDayDate: dueDate === today ? today : null,
      markFlagged,
    };
    setBusy(true);
    try {
      await messagesApi.createTask(source.id, input);
      toast.success(t("mail.toast.taskCreated"));
      onClose();
    } catch (error) {
      toast.error(t("mail.toast.taskFailed"), errorText(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("mail.createTask.title")}
      width={460}
      footer={
        <>
          <Button appearance="subtle" onClick={onClose} disabled={busy}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" onClick={() => void submit()} disabled={busy || !listId}>
            {t("mail.createTask.submit")}
          </Button>
        </>
      }
    >
      <div style={{ display: "flex", flexDirection: "column", gap: "var(--sp-3)" }}>
        <Input
          label={t("mail.createTask.titleLabel")}
          value={title}
          onChange={(event) => setTitle(event.target.value)}
        />
        <Select
          label={t("mail.createTask.list")}
          value={listId}
          options={options}
          placeholder={t("mail.createTask.listPlaceholder")}
          onChange={(value) => setListId(value)}
        />
        <DatePicker
          label={t("mail.createTask.due")}
          value={dueDate}
          clearable
          onChange={(value) => setDueDate(value)}
        />
        <Checkbox
          label={t("mail.createTask.reminder")}
          checked={reminderOn}
          onChange={(event) => setReminderOn(event.target.checked)}
        />
        {reminderOn ? (
          <div style={{ display: "flex", gap: "var(--sp-2)" }}>
            <DatePicker
              label={t("mail.createTask.reminderDate")}
              value={reminderDate}
              min={today}
              onChange={(value) => setReminderDate(value ?? today)}
            />
            <TimePicker
              label={t("mail.createTask.reminderTime")}
              value={reminderTime}
              onChange={(value) => setReminderTime(value ?? "09:00")}
            />
          </div>
        ) : null}
        <TextArea
          label={t("mail.createTask.notes")}
          rows={3}
          value={notes}
          onChange={(event) => setNotes(event.target.value)}
        />
        <Checkbox
          label={t("mail.createTask.flag")}
          description={t("mail.createTask.flagHint")}
          checked={markFlagged}
          onChange={(event) => setMarkFlagged(event.target.checked)}
        />
      </div>
    </Dialog>
  );
}
