/**
 * Modal dialogs for the To Do module: create/rename/recolor a list, pick a due
 * date, and the custom recurrence editor. All of them are controlled by their
 * parent and own only their form state.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowRepeatAll20Regular,
  CalendarLtr20Regular,
  Color20Regular,
  Delete20Regular,
} from "@fluentui/react-icons";
import type { Account, IsoDate, RecurrenceRule, TaskList, Weekday } from "@/ipc/types";
import {
  Button,
  ColorPicker,
  DatePicker,
  Dialog,
  Input,
  RadioGroup,
  Select,
  ToggleButton,
  toast,
} from "@/components";
import { useLanguage, useT } from "@/lib/i18n";
import {
  WEEKDAY_ORDER,
  clampRecurrenceInterval,
  type RecurrenceFrequency,
} from "./logic";
import { createTaskList, updateTaskList } from "./actions";
import styles from "./Dialogs.module.css";

const DEFAULT_LIST_COLOR = "#0f6cbd";

/* ─────────────────────────── Create list ─────────────────────────────── */

export interface CreateListDialogProps {
  open: boolean;
  onClose: () => void;
  accounts: Account[];
  defaultAccountId?: string | null;
  onCreated?: (list: TaskList) => void;
}

export function CreateListDialog({
  open,
  onClose,
  accounts,
  defaultAccountId,
  onCreated,
}: CreateListDialogProps) {
  const t = useT();
  const [name, setName] = useState("");
  const [accountId, setAccountId] = useState<string | null>(null);
  const [color, setColor] = useState<string>(DEFAULT_LIST_COLOR);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const accountOptions = useMemo(() => {
    const capable = accounts.filter(
      (account) => account.tasksEnabled || account.provider === "local",
    );
    return (capable.length > 0 ? capable : accounts).map((account) => ({
      value: account.id,
      label: account.displayName || account.email,
    }));
  }, [accounts]);

  useEffect(() => {
    if (!open) return;
    setName("");
    setColor(DEFAULT_LIST_COLOR);
    setError(null);
    setBusy(false);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const preferred =
      (defaultAccountId
        ? accounts.find((account) => account.id === defaultAccountId)
        : undefined) ??
      accounts.find((account) => account.id === "local") ??
      accounts.find((account) => account.tasksEnabled) ??
      accounts[0];
    setAccountId((current) =>
      current && accounts.some((account) => account.id === current)
        ? current
        : preferred?.id ?? null,
    );
  }, [open, accounts, defaultAccountId]);

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError(t("todo.newList.nameRequired"));
      return;
    }
    if (!accountId) {
      setError(t("todo.newList.accountRequired"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const list = await createTaskList(accountId, trimmed, color);
      if (!list) {
        setBusy(false);
        return;
      }
      toast.success(t("todo.toast.listCreated"), trimmed);
      onCreated?.(list);
      onClose();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("todo.newList.title")}
      width={440}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} onClick={() => void submit()}>
            {t("todo.newList.create")}
          </Button>
        </>
      }
    >
      <div className={styles.form}>
        <Input
          autoFocus
          label={t("todo.newList.name")}
          placeholder={t("todo.newList.namePlaceholder")}
          value={name}
          disabled={busy}
          onChange={(event) => setName(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              void submit();
            }
          }}
          error={error ?? undefined}
        />
        <Select
          label={t("todo.newList.account")}
          value={accountId}
          onChange={setAccountId}
          options={accountOptions}
          disabled={busy || accountOptions.length === 0}
        />
        <div className={styles.colorSection}>
          <span className={styles.sectionLabel}>{t("todo.changeColor.color")}</span>
          <ColorPicker
            value={color}
            onChange={(value) => setColor(value ?? DEFAULT_LIST_COLOR)}
            disabled={busy}
            ariaLabel={t("todo.changeColor.color")}
          />
        </div>
      </div>
    </Dialog>
  );
}

/* ─────────────────────────── Rename list ─────────────────────────────── */

export interface RenameListDialogProps {
  open: boolean;
  onClose: () => void;
  list: TaskList | null;
}

export function RenameListDialog({ open, onClose, list }: RenameListDialogProps) {
  const t = useT();
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(list?.name ?? "");
    setError(null);
    setBusy(false);
    // Reset only when the dialog opens or targets another list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, list?.id]);

  const submit = async () => {
    if (!list) return;
    const trimmed = name.trim();
    if (!trimmed) {
      setError(t("todo.newList.nameRequired"));
      return;
    }
    setBusy(true);
    try {
      const updated = await updateTaskList(list.id, trimmed, null);
      if (updated) {
        toast.success(t("todo.toast.listUpdated"), trimmed);
        onClose();
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("todo.renameList.title")}
      width={420}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} onClick={() => void submit()}>
            {t("common.save")}
          </Button>
        </>
      }
    >
      <Input
        autoFocus
        label={t("todo.newList.name")}
        value={name}
        disabled={busy}
        onChange={(event) => setName(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            void submit();
          }
        }}
        error={error ?? undefined}
      />
    </Dialog>
  );
}

/* ─────────────────────────── List color ──────────────────────────────── */

export interface ColorListDialogProps {
  open: boolean;
  onClose: () => void;
  list: TaskList | null;
}

export function ColorListDialog({ open, onClose, list }: ColorListDialogProps) {
  const t = useT();
  const [color, setColor] = useState<string>(DEFAULT_LIST_COLOR);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setColor(list?.color ?? DEFAULT_LIST_COLOR);
    setBusy(false);
    // Reset only when the dialog opens or targets another list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, list?.id]);

  const submit = async () => {
    if (!list) return;
    setBusy(true);
    try {
      const updated = await updateTaskList(list.id, null, color);
      if (updated) onClose();
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("todo.changeColor.title")}
      width={400}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      icon={<Color20Regular />}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} onClick={() => void submit()}>
            {t("common.save")}
          </Button>
        </>
      }
    >
      <ColorPicker
        value={color}
        onChange={(value) => setColor(value ?? DEFAULT_LIST_COLOR)}
        disabled={busy}
        ariaLabel={t("todo.changeColor.color")}
      />
    </Dialog>
  );
}

/* ──────────────────────────── Due date ───────────────────────────────── */

export interface DueDateDialogProps {
  open: boolean;
  onClose: () => void;
  value: IsoDate | null;
  onPick: (date: IsoDate | null) => void;
}

export function DueDateDialog({ open, onClose, value, onPick }: DueDateDialogProps) {
  const t = useT();
  const [date, setDate] = useState<IsoDate | null>(value);

  useEffect(() => {
    if (open) setDate(value);
  }, [open, value]);

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("todo.dueDialog.title")}
      width={380}
      icon={<CalendarLtr20Regular />}
      footerLeading={
        value ? (
          <Button
            appearance="subtle"
            icon={<Delete20Regular />}
            onClick={() => {
              onPick(null);
              onClose();
            }}
          >
            {t("todo.menu.dueClear")}
          </Button>
        ) : null
      }
      footer={
        <>
          <Button appearance="subtle" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            disabled={!date}
            onClick={() => {
              onPick(date);
              onClose();
            }}
          >
            {t("common.ok")}
          </Button>
        </>
      }
    >
      {/* Clearing lives in the footer so it persists immediately; a picker-local
          clear would only disable Confirm and then be discarded on cancel. */}
      <DatePicker value={date} onChange={setDate} label={t("todo.detail.dueDate")} />
    </Dialog>
  );
}

/* ─────────────────────────── Recurrence ──────────────────────────────── */

export interface RecurrenceDialogProps {
  open: boolean;
  onClose: () => void;
  initial: RecurrenceRule | null;
  onSave: (rule: RecurrenceRule) => void;
}

type EndMode = "never" | "on" | "after";
type MonthlyMode = "day" | "nth";

export function RecurrenceDialog({ open, onClose, initial, onSave }: RecurrenceDialogProps) {
  const t = useT();
  const locale = useLanguage();
  const [freq, setFreq] = useState<RecurrenceFrequency>("weekly");
  const [interval, setInterval] = useState("1");
  const [weekdays, setWeekdays] = useState<Weekday[]>(["MO"]);
  const [monthlyMode, setMonthlyMode] = useState<MonthlyMode>("day");
  const [monthDay, setMonthDay] = useState("1");
  const [nth, setNth] = useState("1");
  const [nthWeekday, setNthWeekday] = useState<Weekday>("MO");
  const [month, setMonth] = useState("1");
  const [endMode, setEndMode] = useState<EndMode>("never");
  const [until, setUntil] = useState<IsoDate | null>(null);
  const [count, setCount] = useState("10");
  const [error, setError] = useState<string | null>(null);
  const wasOpen = useRef(false);

  useEffect(() => {
    if (!open) {
      wasOpen.current = false;
      return;
    }
    if (wasOpen.current) return;
    wasOpen.current = true;
    const today = new Date();
    const rule = initial;
    setFreq(rule?.freq ?? "weekly");
    setInterval(String(rule?.interval && rule.interval > 0 ? rule.interval : 1));
    setWeekdays(rule && rule.byWeekday?.length > 0 ? rule.byWeekday : ["MO"]);
    setMonthlyMode(rule && rule.bySetPos?.length > 0 ? "nth" : "day");
    setMonthDay(String(rule?.byMonthDay?.[0] ?? today.getDate()));
    setNth(String(rule?.bySetPos?.[0] ?? 1));
    setNthWeekday(rule?.byWeekday?.[0] ?? "MO");
    setMonth(String(rule?.byMonth?.[0] ?? today.getMonth() + 1));
    setEndMode(rule?.until ? "on" : rule?.count ? "after" : "never");
    setUntil(rule?.until ?? null);
    setCount(String(rule?.count ?? 10));
    setError(null);
  }, [open, initial]);

  const intervalValue = clampRecurrenceInterval(Number(interval));
  const dayValue = clampDay(Number(monthDay));
  const countValue = Math.max(1, Math.round(Number(count) || 1));
  const monthValue = Math.min(12, Math.max(1, Math.round(Number(month) || 1)));
  const nthValue = Number(nth) === -1 ? -1 : Math.min(4, Math.max(1, Math.round(Number(nth) || 1)));

  const unitLabel =
    freq === "daily"
      ? t("todo.repeat.custom.unitDay")
      : freq === "weekly"
        ? t("todo.repeat.custom.unitWeek")
        : freq === "monthly"
          ? t("todo.repeat.custom.unitMonth")
          : t("todo.repeat.custom.unitYear");

  const freqOptions = useMemo(
    () => [
      { value: "daily", label: t("todo.repeat.custom.daily") },
      { value: "weekly", label: t("todo.repeat.custom.weekly") },
      { value: "monthly", label: t("todo.repeat.custom.monthly") },
      { value: "yearly", label: t("todo.repeat.custom.yearly") },
    ],
    [t],
  );

  const weekdayOptions = useMemo(
    () =>
      WEEKDAY_ORDER.map((day) => ({
        value: String(day),
        label: t(`todo.weekday.${weekdayKey(day)}`),
      })),
    [t],
  );

  const nthOptions = useMemo(
    () => [
      { value: "1", label: t("todo.repeat.nth.1") },
      { value: "2", label: t("todo.repeat.nth.2") },
      { value: "3", label: t("todo.repeat.nth.3") },
      { value: "4", label: t("todo.repeat.nth.4") },
      { value: "-1", label: t("todo.repeat.nth.last") },
    ],
    [t],
  );

  const monthOptions = useMemo(
    () =>
      Array.from({ length: 12 }, (_, index) => {
        const value = index + 1;
        const label = new Intl.DateTimeFormat(locale, { month: "long" }).format(
          new Date(2024, index, 15),
        );
        return { value: String(value), label };
      }),
    [locale],
  );

  const submit = () => {
    if (intervalValue < 1) {
      setError(t("todo.repeat.custom.invalidInterval"));
      return;
    }
    if (freq === "weekly" && weekdays.length === 0) {
      setError(t("todo.repeat.custom.selectWeekday"));
      return;
    }
    if (endMode === "on" && !until) {
      setError(t("todo.repeat.custom.selectEndDate"));
      return;
    }
    if (endMode === "after" && countValue < 1) {
      setError(t("todo.repeat.custom.invalidCount"));
      return;
    }
    const rule: RecurrenceRule = {
      freq,
      interval: intervalValue,
      byWeekday:
        freq === "weekly"
          ? [...weekdays]
          : freq === "monthly" && monthlyMode === "nth"
            ? [nthWeekday]
            : [],
      bySetPos: freq === "monthly" && monthlyMode === "nth" ? [nthValue] : [],
      byMonthDay:
        freq === "monthly" && monthlyMode === "day"
          ? [dayValue]
          : freq === "yearly"
            ? [dayValue]
            : [],
      byMonth: freq === "yearly" ? [monthValue] : [],
      count: endMode === "after" ? countValue : null,
      until: endMode === "on" ? until : null,
    };
    onSave(rule);
    onClose();
  };

  const toggleWeekday = (day: Weekday) => {
    setWeekdays((current) =>
      current.includes(day) ? current.filter((value) => value !== day) : [...current, day],
    );
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("todo.repeat.custom.title")}
      width={500}
      icon={<ArrowRepeatAll20Regular />}
      footer={
        <>
          <Button appearance="subtle" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" onClick={submit}>
            {t("todo.repeat.custom.save")}
          </Button>
        </>
      }
    >
      <div className={styles.form}>
        <Select
          label={t("todo.repeat.custom.frequency")}
          value={freq}
          onChange={(value) => setFreq(value as RecurrenceFrequency)}
          options={freqOptions}
        />

        <Input
          type="number"
          min={1}
          max={99}
          label={t("todo.repeat.custom.interval")}
          value={interval}
          onChange={(event) => setInterval(event.target.value)}
          suffix={<span className={styles.unit}>{unitLabel}</span>}
        />

        {freq === "weekly" ? (
          <div className={styles.weekdaySection}>
            <span className={styles.sectionLabel}>{t("todo.repeat.custom.onWeekdays")}</span>
            <div className={styles.weekdays} role="group" aria-label={t("todo.repeat.custom.onWeekdays")}>
              {WEEKDAY_ORDER.map((day) => {
                const key = weekdayKey(day);
                return (
                  <ToggleButton
                    key={key}
                    pressed={weekdays.includes(day)}
                    onPressedChange={() => toggleWeekday(day)}
                    size="small"
                    aria-label={t(`todo.weekdayFull.${key}`)}
                  >
                    {t(`todo.weekday.${key}`)}
                  </ToggleButton>
                );
              })}
            </div>
          </div>
        ) : null}

        {freq === "monthly" ? (
          <div className={styles.monthlySection}>
            <RadioGroup
              label={t("todo.repeat.custom.monthlyMode")}
              value={monthlyMode}
              onChange={(value) => setMonthlyMode(value as MonthlyMode)}
              orientation="horizontal"
              options={[
                { value: "day", label: t("todo.repeat.custom.byMonthDay") },
                { value: "nth", label: t("todo.repeat.custom.byWeekday") },
              ]}
            />
            {monthlyMode === "day" ? (
              <Input
                type="number"
                min={1}
                max={31}
                label={t("todo.repeat.custom.dayOfMonth")}
                value={monthDay}
                onChange={(event) => setMonthDay(event.target.value)}
              />
            ) : (
              <div className={styles.twoColumns}>
                <Select
                  label={t("todo.repeat.custom.nth")}
                  value={nth}
                  onChange={setNth}
                  options={nthOptions}
                />
                <Select
                  label={t("todo.repeat.custom.weekday")}
                  value={String(nthWeekday)}
                  onChange={(value) => setNthWeekday(value as Weekday)}
                  options={weekdayOptions}
                />
              </div>
            )}
          </div>
        ) : null}

        {freq === "yearly" ? (
          <div className={styles.twoColumns}>
            <Select
              label={t("todo.repeat.custom.month")}
              value={month}
              onChange={setMonth}
              options={monthOptions}
            />
            <Input
              type="number"
              min={1}
              max={31}
              label={t("todo.repeat.custom.dayOfMonth")}
              value={monthDay}
              onChange={(event) => setMonthDay(event.target.value)}
            />
          </div>
        ) : null}

        <div className={styles.endSection}>
          <RadioGroup
            label={t("todo.repeat.custom.end")}
            value={endMode}
            onChange={(value) => setEndMode(value as EndMode)}
            options={[
              { value: "never", label: t("todo.repeat.custom.never") },
              { value: "on", label: t("todo.repeat.custom.onDate") },
              { value: "after", label: t("todo.repeat.custom.afterCount") },
            ]}
          />
          {endMode === "on" ? (
            <DatePicker
              value={until}
              onChange={setUntil}
              label={t("todo.repeat.custom.endDate")}
            />
          ) : null}
          {endMode === "after" ? (
            <Input
              type="number"
              min={1}
              max={999}
              label={t("todo.repeat.custom.count")}
              value={count}
              onChange={(event) => setCount(event.target.value)}
            />
          ) : null}
        </div>

        {error ? (
          <div className={styles.error} role="alert">
            {error}
          </div>
        ) : null}
      </div>
    </Dialog>
  );
}

function clampDay(value: number): number {
  if (!Number.isFinite(value)) return 1;
  return Math.min(31, Math.max(1, Math.round(value)));
}

function weekdayKey(day: Weekday): string {
  return day;
}
