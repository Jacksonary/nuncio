import { useEffect, useMemo, useState, type KeyboardEvent } from "react";
import type { Calendar, EventInput } from "@/ipc/types";
import { Button, Input, Popover, Select, toast } from "@/components";
import { events, isIpcError } from "@/ipc/api";
import { useLanguage, useT } from "@/lib/i18n";
import { formatTime } from "@/lib/format";
import { invalidateCalendarData } from "./actions";
import { localTimeZone } from "./dates";
import { rangeToInput as rangeToInputModel } from "./model";
import type { NewEventSeed } from "./uiStore";
import styles from "./QuickCreate.module.css";

export interface QuickCreateProps {
  seed: NewEventSeed;
  point: { x: number; y: number } | null;
  calendars: Calendar[];
  defaultReminderMinutes: number;
  onClose: () => void;
  onOpenEditor: (seed: NewEventSeed) => void;
}

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/** Minimal "title + time + calendar" creation surface anchored to a click/drag. */
export function QuickCreate({
  seed,
  point,
  calendars,
  defaultReminderMinutes,
  onClose,
  onOpenEditor,
}: QuickCreateProps) {
  const t = useT();
  const locale = useLanguage();
  const writable = useMemo(() => calendars.filter((calendar) => !calendar.isReadOnly), [calendars]);
  const [title, setTitle] = useState("");
  const [calendarId, setCalendarId] = useState<string | null>(
    seed.calendarId ?? writable[0]?.id ?? null,
  );
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!calendarId && writable[0]?.id) setCalendarId(writable[0].id);
  }, [calendarId, writable]);

  const timeText = useMemo(() => {
    const formatter = new Intl.DateTimeFormat(locale, {
      weekday: "short",
      month: "short",
      day: "numeric",
    });
    if (seed.allDay) {
      // Never render clock times for all-day seeds: the date is a calendar
      // day, not an instant.
      return `${formatter.format(seed.start)} · ${t("calendar.allDay")}`;
    }
    return `${formatter.format(seed.start)} ${formatTime(seed.start, { locale })} – ${formatTime(
      seed.end,
      { locale },
    )}`;
  }, [seed, locale, t]);

  const create = async () => {
    const trimmed = title.trim();
    if (!trimmed || !calendarId || busy) return;
    setBusy(true);
    try {
      const range = rangeToInputModel(seed.start, seed.end, seed.allDay);
      const input: EventInput = {
        calendarId,
        title: trimmed,
        location: "",
        description: "",
        start: range.start,
        end: range.end,
        allDay: seed.allDay,
        timeZone: localTimeZone(),
        recurrence: null,
        showAs: "busy",
        isPrivate: false,
        attendees: [],
        reminders: defaultReminderMinutes >= 0 ? [defaultReminderMinutes] : [],
        onlineMeetingUrl: null,
        color: null,
        notifyAttendees: false,
      };
      await events.create(input);
      invalidateCalendarData();
      toast.success(t("calendar.toast.created"), trimmed);
      onClose();
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void create();
    }
  };

  return (
    <Popover
      open
      onClose={onClose}
      point={point}
      placement="bottom-start"
      role="dialog"
      ariaLabel={t("calendar.quick.title")}
      minWidth={300}
      className={styles.popover}
    >
      <div className={styles.header}>
        <span className={styles.accent} aria-hidden="true" />
        <span className={styles.heading}>{t("calendar.quick.title")}</span>
      </div>
      <Input
        autoFocus
        value={title}
        onChange={(event) => setTitle(event.target.value)}
        onKeyDown={handleKeyDown}
        placeholder={t("calendar.quick.titlePlaceholder")}
        aria-label={t("calendar.quick.titlePlaceholder")}
      />
      <div className={styles.time} aria-label={t("calendar.quick.time")}>
        {timeText}
      </div>
      <Select
        value={calendarId}
        onChange={setCalendarId}
        options={writable.map((calendar) => ({
          value: calendar.id,
          label: calendar.name,
          icon: (
            <span
              className={styles.swatch}
              style={{ background: calendar.color }}
              aria-hidden="true"
            />
          ),
        }))}
        ariaLabel={t("calendar.quick.calendar")}
        size="small"
        disabled={writable.length === 0}
      />
      {writable.length === 0 ? (
        <p className={styles.hint}>{t("calendar.editor.noCalendars")}</p>
      ) : null}
      <div className={styles.footer}>
        <Button appearance="subtle" size="small" onClick={() => onOpenEditor(seed)}>
          {t("calendar.quick.more")}
        </Button>
        <span className={styles.spacer} />
        <Button appearance="subtle" size="small" onClick={onClose}>
          {t("common.cancel")}
        </Button>
        <Button
          appearance="primary"
          size="small"
          loading={busy}
          disabled={!title.trim() || !calendarId}
          onClick={() => void create()}
        >
          {t("calendar.quick.create")}
        </Button>
      </div>
    </Popover>
  );
}
