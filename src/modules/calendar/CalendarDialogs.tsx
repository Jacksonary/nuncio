import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { isTauri } from "@tauri-apps/api/core";
import type { Account, Calendar } from "@/ipc/types";
import {
  Button,
  COLOR_PRESETS,
  ColorPicker,
  Dialog,
  EmptyState,
  Input,
  Select,
  toast,
} from "@/components";
import { calendars as calendarApi, isIpcError } from "@/ipc/api";
import { queryKeyPrefixes } from "@/lib/queryKeys";
import { useT } from "@/lib/i18n";
import styles from "./CalendarDialogs.module.css";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

export interface CreateCalendarDialogProps {
  open: boolean;
  onClose: () => void;
  accounts: Account[];
}

export function CreateCalendarDialog({ open, onClose, accounts }: CreateCalendarDialogProps) {
  const t = useT();
  const client = useQueryClient();
  const [name, setName] = useState("");
  const [accountId, setAccountId] = useState<string | null>(null);
  const [color, setColor] = useState<string | null>(COLOR_PRESETS[0]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setName("");
    setColor(COLOR_PRESETS[0]);
    setError(null);
    const preferred =
      accounts.find((account) => account.id === "local") ??
      accounts.find((account) => account.calendarEnabled) ??
      accounts[0];
    setAccountId(preferred?.id ?? null);
    // Only reset the form when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open || accountId !== null) return;
    const preferred =
      accounts.find((account) => account.id === "local") ??
      accounts.find((account) => account.calendarEnabled) ??
      accounts[0];
    if (preferred) setAccountId(preferred.id);
  }, [open, accountId, accounts]);

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError(t("calendar.dialog.create.nameRequired"));
      return;
    }
    if (!accountId) return;
    setBusy(true);
    setError(null);
    try {
      await calendarApi.create(accountId, trimmed, color ?? COLOR_PRESETS[0]);
      toast.success(t("calendar.toast.calendarCreated"), trimmed);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      onClose();
    } catch (submitError) {
      toast.error(t("calendar.error.action"), errorMessage(submitError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("calendar.dialog.create.title")}
      width={460}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            loading={busy}
            disabled={!accountId}
            onClick={() => void submit()}
          >
            {t("calendar.dialog.create.submit")}
          </Button>
        </>
      }
    >
      <div className={styles.form}>
        <Input
          label={t("calendar.dialog.create.name")}
          value={name}
          onChange={(event) => setName(event.target.value)}
          placeholder={t("calendar.dialog.create.namePlaceholder")}
          error={error}
          autoFocus
          onKeyDown={(event) => {
            if (event.key === "Enter") void submit();
          }}
        />
        <Select
          label={t("calendar.dialog.create.account")}
          value={accountId}
          onChange={setAccountId}
          options={accounts.map((account) => ({
            value: account.id,
            label: account.displayName || account.email,
            icon: (
              <span className={styles.accountDot} style={{ background: account.color }} aria-hidden="true" />
            ),
          }))}
        />
        <ColorPicker
          label={t("calendar.dialog.create.color")}
          value={color}
          onChange={setColor}
          allowNull={false}
        />
      </div>
    </Dialog>
  );
}

export interface SubscribeCalendarDialogProps {
  open: boolean;
  onClose: () => void;
}

export function SubscribeCalendarDialog({ open, onClose }: SubscribeCalendarDialogProps) {
  const t = useT();
  const client = useQueryClient();
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  const [color, setColor] = useState<string | null>(COLOR_PRESETS[1]);
  const [busy, setBusy] = useState(false);
  const [errors, setErrors] = useState<{ url?: string; name?: string }>({});

  useEffect(() => {
    if (!open) return;
    setUrl("");
    setName("");
    setColor(COLOR_PRESETS[1]);
    setErrors({});
  }, [open]);

  const submit = async () => {
    const trimmedUrl = url.trim();
    const trimmedName = name.trim();
    const nextErrors: { url?: string; name?: string } = {};
    if (!/^(webcal|https?):\/\/.+/i.test(trimmedUrl)) {
      nextErrors.url = t("calendar.dialog.subscribe.invalidUrl");
    }
    if (!trimmedName) nextErrors.name = t("calendar.dialog.subscribe.nameRequired");
    setErrors(nextErrors);
    if (nextErrors.url || nextErrors.name) return;
    setBusy(true);
    try {
      await calendarApi.subscribe(trimmedUrl, trimmedName, color ?? COLOR_PRESETS[1]);
      toast.success(t("calendar.toast.subscribed"), trimmedName);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      onClose();
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("calendar.dialog.subscribe.title")}
      width={480}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} onClick={() => void submit()}>
            {t("calendar.dialog.subscribe.submit")}
          </Button>
        </>
      }
    >
      <div className={styles.form}>
        <Input
          label={t("calendar.dialog.subscribe.url")}
          value={url}
          onChange={(event) => setUrl(event.target.value)}
          placeholder="webcal://example.com/holidays.ics"
          hint={t("calendar.dialog.subscribe.urlHint")}
          error={errors.url}
          autoFocus
        />
        <Input
          label={t("calendar.dialog.subscribe.name")}
          value={name}
          onChange={(event) => setName(event.target.value)}
          placeholder={t("calendar.dialog.subscribe.namePlaceholder")}
          error={errors.name}
        />
        <ColorPicker
          label={t("calendar.dialog.subscribe.color")}
          value={color}
          onChange={setColor}
          allowNull={false}
        />
      </div>
    </Dialog>
  );
}

export interface ImportCalendarDialogProps {
  open: boolean;
  onClose: () => void;
  calendars: Calendar[];
}

export function ImportCalendarDialog({ open, onClose, calendars }: ImportCalendarDialogProps) {
  const t = useT();
  const client = useQueryClient();
  const writable = useMemo(
    () => calendars.filter((calendar) => !calendar.isReadOnly),
    [calendars],
  );
  const [path, setPath] = useState<string | null>(null);
  const [calendarId, setCalendarId] = useState<string | null>(writable[0]?.id ?? null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setPath(null);
    setError(null);
    setCalendarId(writable[0]?.id ?? null);
    // Only reset the form when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open || calendarId !== null) return;
    if (writable[0]) setCalendarId(writable[0].id);
  }, [open, calendarId, writable]);

  const chooseFile = async () => {
    setError(null);
    if (!isTauri()) {
      setError(t("calendar.toast.notAvailable"));
      return;
    }
    try {
      const selected = await openFileDialog({
        multiple: false,
        directory: false,
        filters: [{ name: "iCalendar", extensions: ["ics", "ical", "ifb"] }],
      });
      if (typeof selected === "string") setPath(selected);
    } catch (chooseError) {
      toast.error(t("calendar.error.action"), errorMessage(chooseError));
    }
  };

  const submit = async () => {
    if (!path) {
      setError(t("calendar.dialog.import.noFile"));
      return;
    }
    if (!calendarId) return;
    setBusy(true);
    try {
      const count = await calendarApi.importIcs(path, calendarId);
      toast.success(t("calendar.dialog.import.done", { count }));
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.events });
      onClose();
    } catch (importError) {
      toast.error(t("calendar.error.action"), errorMessage(importError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("calendar.dialog.import.title")}
      width={480}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            loading={busy}
            disabled={!path || !calendarId}
            onClick={() => void submit()}
          >
            {t("calendar.dialog.import.submit")}
          </Button>
        </>
      }
    >
      {writable.length === 0 ? (
        <EmptyState compact title={t("calendar.dialog.import.noCalendars")} />
      ) : (
        <div className={styles.form}>
          <div className={styles.fileRow}>
            <Button size="small" onClick={() => void chooseFile()}>
              {t("calendar.dialog.import.chooseFile")}
            </Button>
            <span className={styles.fileName}>
              {path ? basename(path) : t("calendar.dialog.import.noFile")}
            </span>
          </div>
          {error ? <p className={styles.error}>{error}</p> : null}
          <Select
            label={t("calendar.dialog.import.chooseCalendar")}
            value={calendarId}
            onChange={setCalendarId}
            options={writable.map((calendar) => ({
              value: calendar.id,
              label: calendar.name,
              icon: (
                <span
                  className={styles.accountDot}
                  style={{ background: calendar.color }}
                  aria-hidden="true"
                />
              ),
            }))}
          />
        </div>
      )}
    </Dialog>
  );
}

export interface RenameCalendarDialogProps {
  open: boolean;
  onClose: () => void;
  calendar: Calendar | null;
}

export function RenameCalendarDialog({ open, onClose, calendar }: RenameCalendarDialogProps) {
  const t = useT();
  const client = useQueryClient();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open || !calendar) return;
    setName(calendar.name);
    setError(null);
    // Only reset when a different calendar is opened.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, calendar?.id]);

  const submit = async () => {
    if (!calendar) return;
    const trimmed = name.trim();
    if (!trimmed) {
      setError(t("calendar.dialog.create.nameRequired"));
      return;
    }
    setBusy(true);
    try {
      await calendarApi.update(calendar.id, trimmed, null);
      toast.success(t("calendar.toast.calendarUpdated"), trimmed);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      onClose();
    } catch (renameError) {
      toast.error(t("calendar.error.action"), errorMessage(renameError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("calendar.dialog.rename.title")}
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
        label={t("calendar.dialog.rename.name")}
        value={name}
        onChange={(event) => setName(event.target.value)}
        error={error}
        autoFocus
        onKeyDown={(event) => {
          if (event.key === "Enter") void submit();
        }}
      />
    </Dialog>
  );
}

export interface ColorCalendarDialogProps {
  open: boolean;
  onClose: () => void;
  calendar: Calendar | null;
}

export function ColorCalendarDialog({ open, onClose, calendar }: ColorCalendarDialogProps) {
  const t = useT();
  const client = useQueryClient();
  const [color, setColor] = useState<string | null>(COLOR_PRESETS[0]);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !calendar) return;
    setColor(calendar.color);
    // Only reset when a different calendar is opened.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, calendar?.id]);

  const submit = async () => {
    if (!calendar || !color) return;
    setBusy(true);
    try {
      await calendarApi.update(calendar.id, null, color);
      toast.success(t("calendar.toast.calendarUpdated"), calendar.name);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.events });
      onClose();
    } catch (colorError) {
      toast.error(t("calendar.error.action"), errorMessage(colorError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("calendar.dialog.color.title")}
      width={420}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} disabled={!color} onClick={() => void submit()}>
            {t("common.save")}
          </Button>
        </>
      }
    >
      <ColorPicker
        label={t("calendar.dialog.create.color")}
        value={color}
        onChange={setColor}
        allowNull={false}
      />
    </Dialog>
  );
}
