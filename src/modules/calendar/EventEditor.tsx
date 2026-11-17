import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Add16Regular,
  CalendarLtr20Regular,
  Clock16Regular,
  Delete16Regular,
  Delete20Regular,
  Link20Regular,
  People20Regular,
  Video20Regular,
} from "@fluentui/react-icons";
import type {
  Address,
  Attendee,
  Calendar,
  CalendarEvent,
  EventInput,
  RecurrenceEditScope,
  ShowAs,
} from "@/ipc/types";
import {
  Button,
  Checkbox,
  ColorPicker,
  DangerButton,
  DatePicker,
  Dialog,
  EmptyState,
  IconButton,
  Input,
  Select,
  Switch,
  TextArea,
  TimePicker,
  TokenInput,
  toast,
  confirm,
} from "@/components";
import { compose, events, isIpcError } from "@/ipc/api";
import { queryKeys } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { formatTimeOfDay, parseTimeOfDay } from "@/lib/format";
import {
  addDaysLocal,
  combineDateAndMinutes,
  formatDateOnly,
  formatZoneOffset,
  isValidTimeZone,
  listTimeZones,
  localTimeZone,
  parseDateOnly,
  wallClockDate,
  zonedDateTimeToDate,
  type CalendarSettings,
} from "./dates";
import {
  eventEndDateExclusive,
  eventStartDate,
  minutesOfDay,
  splitAttendees,
  rangeToInput,
  rangeToInputInZone,
} from "./model";
import {
  defaultRepeatState,
  repeatFromRule,
  ruleFromRepeat,
  weekdayLabel,
  weekdayOf,
  type RepeatState,
} from "./recurrence";
import { RecurrenceEditor } from "./RecurrenceEditor";
import { ScopeDialog } from "./ScopeDialog";
import {
  deleteEventSeries,
  invalidateCalendarData,
  isRecurringEvent,
  updateEventSeries,
} from "./actions";
import { openExternal } from "./openExternal";
import type { EditorRequest, NewEventSeed } from "./uiStore";
import styles from "./EventEditor.module.css";

export interface EventEditorProps {
  request: EditorRequest;
  calendars: Calendar[];
  settings: CalendarSettings;
  instanceStart: string | null;
  onClose: () => void;
  onSaved: (event: CalendarEvent) => void;
  onDeleted: (eventId: string) => void;
}

interface FormState {
  title: string;
  location: string;
  description: string;
  allDay: boolean;
  startDate: string;
  startTime: string;
  endDate: string;
  endTime: string;
  calendarId: string;
  color: string | null;
  showAs: ShowAs;
  isPrivate: boolean;
  timeZone: string;
  repeat: RepeatState;
  /** Minutes before start; an event may carry several reminders. */
  reminders: number[];
  onlineMeetingUrl: string;
  required: Address[];
  optional: Address[];
  notify: boolean;
}

const REMINDER_VALUES = ["0", "5", "15", "30", "60", "120", "1440"];
const CUSTOM_REMINDER_FALLBACK = 45;
const MAX_REMINDER_MINUTES = 40_320;

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function toAddress(attendee: Attendee): Address {
  return { name: attendee.name, email: attendee.email };
}

/** Keep the user's reminders, dedupe and clamp them to a sane range. */
function normalizeReminders(values: number[]): number[] {
  const result: number[] = [];
  for (const value of values) {
    if (!Number.isFinite(value)) continue;
    const minutes = Math.max(0, Math.min(MAX_REMINDER_MINUTES, Math.round(value)));
    if (!result.includes(minutes)) result.push(minutes);
  }
  return result;
}

function eventTimeZone(event: CalendarEvent | null): string {
  if (event?.timeZone && isValidTimeZone(event.timeZone)) return event.timeZone;
  return localTimeZone();
}

function initialForm(
  event: CalendarEvent | null,
  seed: NewEventSeed | null,
  calendars: Calendar[],
  settings: CalendarSettings,
): FormState {
  const writable = calendars.filter((calendar) => !calendar.isReadOnly);
  const fallbackCalendarId = seed?.calendarId ?? writable[0]?.id ?? calendars[0]?.id ?? "";
  if (event) {
    const zone = eventTimeZone(event);
    const startDate = eventStartDate(event);
    const endDate = eventEndDateExclusive(event);
    // Timed events are shown as the wall clock of their own zone; all-day
    // events only carry calendar dates.
    const startWall = event.allDay ? startDate : wallClockDate(startDate, zone);
    const endWall = event.allDay ? endDate : wallClockDate(endDate, zone);
    const allDayEnd = event.allDay
      ? (() => {
          const exclusive = parseDateOnly(event.end) ?? startDate;
          const inclusive = addDaysLocal(exclusive, -1);
          return inclusive < startDate ? startDate : inclusive;
        })()
      : endWall;
    const buckets = splitAttendees(event.attendees);
    return {
      title: event.title,
      location: event.location,
      description: event.description,
      allDay: event.allDay,
      startDate: formatDateOnly(startWall),
      startTime: event.allDay ? "00:00" : formatTimeOfDay(minutesOfDay(startWall)),
      endDate: formatDateOnly(allDayEnd),
      endTime: event.allDay ? "00:00" : formatTimeOfDay(minutesOfDay(endWall)),
      calendarId: event.calendarId || fallbackCalendarId,
      color: event.color,
      showAs: event.showAs,
      isPrivate: event.isPrivate,
      timeZone: zone,
      repeat: repeatFromRule(event.recurrence, startWall),
      reminders: normalizeReminders(event.reminders),
      onlineMeetingUrl: event.onlineMeetingUrl ?? "",
      required: buckets.required.map(toAddress),
      optional: buckets.optional.map(toAddress),
      notify: event.attendees.length > 0,
    };
  }

  const start = seed?.start ?? new Date();
  const end = seed?.end ?? new Date(start.getTime() + 30 * 60_000);
  return {
    title: "",
    location: "",
    description: "",
    allDay: seed?.allDay ?? false,
    startDate: formatDateOnly(start),
    startTime: formatTimeOfDay(minutesOfDay(start)),
    endDate: formatDateOnly(end),
    endTime: formatTimeOfDay(minutesOfDay(end)),
    calendarId: fallbackCalendarId,
    color: null,
    showAs: "busy",
    isPrivate: false,
    timeZone: localTimeZone(),
    repeat: defaultRepeatState(),
    reminders:
      settings.defaultReminderMinutes >= 0 ? [settings.defaultReminderMinutes] : [],
    onlineMeetingUrl: "",
    required: [],
    optional: [],
    notify: true,
  };
}

export function EventEditor({
  request,
  calendars,
  settings,
  instanceStart,
  onClose,
  onSaved,
  onDeleted,
}: EventEditorProps) {
  const t = useT();
  const locale = useLanguage();
  const editing = request.mode === "edit";
  const query = useQuery({
    queryKey: queryKeys.event(
      request.mode === "edit" ? request.eventId : "",
      request.mode === "edit" ? request.instanceStart : null,
    ),
    queryFn: () =>
      events.get(
        request.mode === "edit" ? request.eventId : "",
        request.mode === "edit" ? request.instanceStart : null,
      ),
    enabled: editing,
    staleTime: 0,
    gcTime: 30_000,
  });

  if (editing && query.isLoading) {
    return (
      <Dialog
        open
        onClose={onClose}
        title={t("calendar.editor.editTitle")}
        width={640}
        footer={
          <Button appearance="subtle" onClick={onClose}>
            {t("common.cancel")}
          </Button>
        }
      >
        <div className={styles.loading}>{t("common.loading")}</div>
      </Dialog>
    );
  }

  if (editing && (query.isError || !query.data)) {
    return (
      <Dialog
        open
        onClose={onClose}
        title={t("calendar.editor.editTitle")}
        width={640}
        footer={
          <Button appearance="subtle" onClick={onClose}>
            {t("common.close")}
          </Button>
        }
      >
        <EmptyState
          compact
          title={t("calendar.error.load")}
          description={query.error ? errorMessage(query.error) : undefined}
          actions={
            <Button size="small" onClick={() => void query.refetch()}>
              {t("calendar.error.retry")}
            </Button>
          }
        />
      </Dialog>
    );
  }

  const event = editing ? query.data ?? null : null;
  return (
    <EventForm
      key={event ? `${event.id}:${instanceStart ?? ""}` : "new"}
      event={event}
      seed={request.mode === "create" ? request.seed : null}
      calendars={calendars}
      settings={settings}
      instanceStart={instanceStart}
      locale={locale}
      onClose={onClose}
      onSaved={onSaved}
      onDeleted={onDeleted}
    />
  );
}

interface EventFormProps {
  event: CalendarEvent | null;
  seed: NewEventSeed | null;
  calendars: Calendar[];
  settings: CalendarSettings;
  instanceStart: string | null;
  locale: string;
  onClose: () => void;
  onSaved: (event: CalendarEvent) => void;
  onDeleted: (eventId: string) => void;
}

function EventForm({
  event,
  seed,
  calendars,
  settings,
  instanceStart,
  locale,
  onClose,
  onSaved,
  onDeleted,
}: EventFormProps) {
  const t = useT();
  const [form, setForm] = useState<FormState>(() =>
    initialForm(event, seed, calendars, settings),
  );
  const [errors, setErrors] = useState<{ title?: string; time?: string }>({});
  const [busy, setBusy] = useState(false);
  const [pendingSave, setPendingSave] = useState<EventInput | null>(null);
  const [pendingDelete, setPendingDelete] = useState(false);

  const writable = useMemo(
    () => calendars.filter((calendar) => !calendar.isReadOnly),
    [calendars],
  );
  const currentCalendar = calendars.find((calendar) => calendar.id === form.calendarId);
  const readOnlyCalendar = Boolean(currentCalendar?.isReadOnly);
  const canSave = writable.length > 0 && !readOnlyCalendar;

  const patch = (next: Partial<FormState>) => setForm((current) => ({ ...current, ...next }));

  const startInstant = useMemo(() => {
    const day = parseDateOnly(form.startDate) ?? new Date();
    return combineDateAndMinutes(day, form.allDay ? 0 : (parseTimeOfDay(form.startTime) ?? 0));
  }, [form.startDate, form.startTime, form.allDay]);

  const reminderOptions = useMemo(
    () => [
      { value: "0", label: t("calendar.reminder.atStart") },
      { value: "5", label: t("calendar.reminder.minutes", { count: 5 }) },
      { value: "15", label: t("calendar.reminder.minutes", { count: 15 }) },
      { value: "30", label: t("calendar.reminder.minutes", { count: 30 }) },
      { value: "60", label: t("calendar.reminder.hours", { count: 1 }) },
      { value: "120", label: t("calendar.reminder.hours", { count: 2 }) },
      { value: "1440", label: t("calendar.reminder.days", { count: 1 }) },
      { value: "custom", label: t("calendar.reminder.custom") },
    ],
    [t],
  );

  const showAsOptions = useMemo(
    () => [
      { value: "busy", label: t("calendar.editor.showAs.busy") },
      { value: "free", label: t("calendar.editor.showAs.free") },
      { value: "tentative", label: t("calendar.editor.showAs.tentative") },
      { value: "oof", label: t("calendar.editor.showAs.oof") },
      { value: "workingElsewhere", label: t("calendar.editor.showAs.workingElsewhere") },
    ],
    [t],
  );

  const calendarOptions = useMemo(
    () =>
      calendars.map((calendar) => ({
        value: calendar.id,
        label: calendar.name,
        disabled: calendar.isReadOnly,
        icon: (
          <span className={styles.swatch} style={{ background: calendar.color }} aria-hidden="true" />
        ),
      })),
    [calendars],
  );

  const timeZoneOptions = useMemo(() => {
    const day = parseDateOnly(form.startDate);
    const minutes = form.allDay ? 0 : (parseTimeOfDay(form.startTime) ?? 0);
    return listTimeZones([form.timeZone]).map((zone) => {
      // Show the offset that applies to the currently entered wall clock.
      const at = day
        ? zonedDateTimeToDate(
            zone,
            day.getFullYear(),
            day.getMonth() + 1,
            day.getDate(),
            Math.floor(minutes / 60),
            minutes % 60,
          )
        : startInstant;
      return { value: zone, label: `${zone} (${formatZoneOffset(zone, at)})` };
    });
  }, [form.timeZone, form.startDate, form.startTime, form.allDay, startInstant]);

  const setReminderAt = (index: number, minutes: number) => {
    patch({
      reminders: form.reminders.map((value, position) =>
        position === index ? minutes : value,
      ),
    });
  };

  const removeReminder = (index: number) => {
    patch({ reminders: form.reminders.filter((_, position) => position !== index) });
  };

  const addReminder = () => {
    const used = new Set(form.reminders);
    const candidate = [15, 60, 1440, 5, 30, 120, 0].find((value) => !used.has(value));
    const next =
      candidate ??
      Math.min(
        MAX_REMINDER_MINUTES,
        (form.reminders[form.reminders.length - 1] ?? 0) + 60,
      );
    if (used.has(next)) return;
    patch({ reminders: [...form.reminders, next] });
  };

  const suggestionSource = useMemo(
    () => (query: string) =>
      compose.suggestRecipients(query, 8).catch(() => [] as Address[]),
    [],
  );

  const buildInput = (): EventInput | null => {
    const title = form.title.trim();
    const startDay = parseDateOnly(form.startDate);
    const endDay = parseDateOnly(form.endDate);
    const nextErrors: { title?: string; time?: string } = {};
    if (!title) nextErrors.title = t("calendar.editor.titleRequired");
    if (!startDay || !endDay) nextErrors.time = t("calendar.editor.invalidDate");

    let start = "";
    let end = "";
    if (!nextErrors.time && startDay && endDay) {
      if (form.allDay) {
        if (endDay < startDay) {
          nextErrors.time = t("calendar.editor.endBeforeStart");
        } else {
          const range = rangeToInput(startDay, addDaysLocal(endDay, 1), true);
          start = range.start;
          end = range.end;
        }
      } else {
        const startMinutes = parseTimeOfDay(form.startTime);
        const endMinutes = parseTimeOfDay(form.endTime);
        if (startMinutes === null || endMinutes === null) {
          nextErrors.time = t("calendar.editor.invalidDate");
        } else {
          // Interpret the typed wall clock in the event's own zone, then store
          // ISO strings carrying that zone's offset.
          const zone = isValidTimeZone(form.timeZone) ? form.timeZone : localTimeZone();
          const startDate = zonedDateTimeToDate(
            zone,
            startDay.getFullYear(),
            startDay.getMonth() + 1,
            startDay.getDate(),
            Math.floor(startMinutes / 60),
            startMinutes % 60,
          );
          const endDate = zonedDateTimeToDate(
            zone,
            endDay.getFullYear(),
            endDay.getMonth() + 1,
            endDay.getDate(),
            Math.floor(endMinutes / 60),
            endMinutes % 60,
          );
          if (endDate <= startDate) {
            nextErrors.time = t("calendar.editor.endBeforeStart");
          } else {
            const range = rangeToInputInZone(startDate, endDate, false, zone);
            start = range.start;
            end = range.end;
          }
        }
      }
    }

    setErrors(nextErrors);
    if (nextErrors.title || nextErrors.time || !form.calendarId) return null;

    const recurrence = ruleFromRepeat(form.repeat, startInstant);
    const reminders = normalizeReminders(form.reminders);

    const original = new Map(
      (event?.attendees ?? []).map((attendee) => [attendee.email.toLowerCase(), attendee]),
    );
    const attendees: Attendee[] = [
      ...form.required.map((address) => ({
        email: address.email,
        name: address.name,
        role: "required" as const,
        status: original.get(address.email.toLowerCase())?.status ?? ("needsAction" as const),
      })),
      ...form.optional.map((address) => ({
        email: address.email,
        name: address.name,
        role: "optional" as const,
        status: original.get(address.email.toLowerCase())?.status ?? ("needsAction" as const),
      })),
    ];

    return {
      calendarId: form.calendarId,
      title,
      location: form.location.trim(),
      description: form.description,
      start,
      end,
      allDay: form.allDay,
      timeZone: isValidTimeZone(form.timeZone) ? form.timeZone : localTimeZone(),
      recurrence,
      showAs: form.showAs,
      isPrivate: form.isPrivate,
      attendees,
      reminders,
      onlineMeetingUrl: form.onlineMeetingUrl.trim() || null,
      color: form.color,
      notifyAttendees: form.notify && attendees.length > 0,
    };
  };

  const finalize = (saved: CalendarEvent, message: string) => {
    toast.success(message, saved.title || t("calendar.untitled"));
    invalidateCalendarData();
    onSaved(saved);
    onClose();
  };

  const save = async () => {
    if (busy || !canSave) return;
    const input = buildInput();
    if (!input) return;
    setBusy(true);
    try {
      if (!event) {
        const created = await events.create(input);
        finalize(created, t("calendar.toast.created"));
        return;
      }
      if (isRecurringEvent(event)) {
        setPendingSave(input);
        return;
      }
      const updated = await updateEventSeries(event, input, "all", instanceStart);
      finalize(updated, t("calendar.toast.updated"));
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  const chooseScope = async (scope: RecurrenceEditScope) => {
    if (pendingSave && event) {
      setBusy(true);
      try {
        const updated = await updateEventSeries(event, pendingSave, scope, instanceStart);
        setPendingSave(null);
        finalize(updated, t("calendar.toast.updated"));
      } catch (error) {
        toast.error(t("calendar.error.action"), errorMessage(error));
      } finally {
        setBusy(false);
      }
      return;
    }
    if (pendingDelete && event) {
      setBusy(true);
      try {
        await deleteEventSeries(event, scope, instanceStart, event.attendees.length > 0);
        setPendingDelete(false);
        toast.success(t("calendar.toast.deleted"));
        invalidateCalendarData();
        onDeleted(event.id);
        onClose();
      } catch (error) {
        toast.error(t("calendar.error.action"), errorMessage(error));
      } finally {
        setBusy(false);
      }
    }
  };

  const requestDelete = async () => {
    if (!event) return;
    if (isRecurringEvent(event)) {
      setPendingDelete(true);
      return;
    }
    const ok = await confirm({
      title: t("calendar.confirm.deleteTitle"),
      message: t("calendar.confirm.deleteMessage", {
        title: event.title || t("calendar.untitled"),
      }),
      confirmLabel: t("calendar.confirm.delete"),
      cancelLabel: t("common.cancel"),
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    try {
      await deleteEventSeries(event, "all", instanceStart, event.attendees.length > 0);
      toast.success(t("calendar.toast.deleted"));
      invalidateCalendarData();
      onDeleted(event.id);
      onClose();
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  const weekdayName = weekdayLabel(weekdayOf(startInstant), locale);

  return (
    <>
      <Dialog
        open
        onClose={onClose}
        title={event ? t("calendar.editor.editTitle") : t("calendar.editor.createTitle")}
        icon={<CalendarLtr20Regular />}
        width={660}
        dismissible={!busy}
        closeOnBackdrop={!busy}
        className={styles.dialog}
        bodyClassName={styles.body}
        footerLeading={
          event ? (
            <DangerButton
              size="small"
              icon={<Delete20Regular />}
              disabled={busy || readOnlyCalendar}
              onClick={() => void requestDelete()}
            >
              {t("calendar.editor.deleteEvent")}
            </DangerButton>
          ) : null
        }
        footer={
          <>
            <Button appearance="subtle" disabled={busy} onClick={onClose}>
              {t("common.cancel")}
            </Button>
            <Button
              appearance="primary"
              loading={busy}
              disabled={!canSave}
              onClick={() => void save()}
            >
              {busy ? t("calendar.editor.saving") : t("calendar.editor.save")}
            </Button>
          </>
        }
      >
        {writable.length === 0 ? (
          <p className={styles.warning}>{t("calendar.editor.noCalendars")}</p>
        ) : null}
        {readOnlyCalendar ? (
          <p className={styles.warning}>{t("calendar.editor.readOnlyCalendar")}</p>
        ) : null}

        <Input
          label={t("calendar.editor.title")}
          value={form.title}
          onChange={(inputEvent) => {
            patch({ title: inputEvent.target.value });
            if (errors.title) setErrors((current) => ({ ...current, title: undefined }));
          }}
          placeholder={t("calendar.editor.titlePlaceholder")}
          error={errors.title}
          autoFocus
          required
        />

        <div className={styles.inlineRow}>
          <Input
            label={t("calendar.editor.location")}
            value={form.location}
            onChange={(inputEvent) => patch({ location: inputEvent.target.value })}
            placeholder={t("calendar.editor.locationPlaceholder")}
            containerClassName={styles.grow}
          />
          <Switch
            checked={form.allDay}
            onChange={(checked) => {
              if (!checked && form.startTime === "00:00" && form.endTime === "00:00") {
                const startMinutes = parseTimeOfDay(settings.workHoursStart) ?? 9 * 60;
                const endMinutes = Math.min(
                  1439,
                  startMinutes + Math.max(15, settings.defaultEventDurationMinutes),
                );
                patch({
                  allDay: false,
                  startTime: formatTimeOfDay(startMinutes),
                  endTime: formatTimeOfDay(endMinutes),
                  endDate: form.endDate < form.startDate ? form.startDate : form.endDate,
                });
                return;
              }
              patch({ allDay: checked });
            }}
            label={t("calendar.editor.allDay")}
            className={styles.allDaySwitch}
          />
        </div>

        <div className={styles.dateTimeGrid}>
          <DatePicker
            label={t("calendar.editor.startDate")}
            value={form.startDate}
            onChange={(value) => {
              if (!value) return;
              patch({ startDate: value, ...(form.allDay ? { endDate: value } : {}) });
            }}
            weekStartsOn={settings.firstDayOfWeek}
          />
          {!form.allDay ? (
            <TimePicker
              label={t("calendar.editor.startTime")}
              value={form.startTime}
              onChange={(value) => {
                if (value) patch({ startTime: value });
              }}
            />
          ) : null}
          <DatePicker
            label={t("calendar.editor.endDate")}
            value={form.endDate}
            onChange={(value) => {
              if (value) patch({ endDate: value });
            }}
            weekStartsOn={settings.firstDayOfWeek}
            min={form.allDay ? form.startDate : undefined}
          />
          {!form.allDay ? (
            <TimePicker
              label={t("calendar.editor.endTime")}
              value={form.endTime}
              onChange={(value) => {
                if (value) patch({ endTime: value });
              }}
            />
          ) : null}
        </div>
        {errors.time ? <p className={styles.fieldError}>{errors.time}</p> : null}
        <Select
          label={t("calendar.editor.timeZone")}
          value={form.timeZone}
          onChange={(value) => patch({ timeZone: value })}
          options={timeZoneOptions}
          hint={t("calendar.editor.timeZoneHint")}
        />

        <div className={styles.twoColumns}>
          <Select
            label={t("calendar.editor.calendar")}
            value={form.calendarId}
            onChange={(value) => patch({ calendarId: value })}
            options={calendarOptions}
          />
          <ColorPicker
            label={t("calendar.editor.color")}
            value={form.color}
            onChange={(value) => patch({ color: value })}
            allowNull
          />
        </div>

        <section className={styles.section}>
          <RecurrenceEditor
            value={form.repeat}
            onChange={(value) => patch({ repeat: value })}
            start={startInstant}
          />
        </section>

        <section className={styles.section}>
          <div className={styles.sectionTitle}>
            <Clock16Regular aria-hidden="true" />
            {t("calendar.editor.reminders")}
          </div>
          {form.reminders.length === 0 ? (
            <p className={styles.hint}>{t("calendar.editor.noReminders")}</p>
          ) : (
            form.reminders.map((minutes, index) => {
              const preset = REMINDER_VALUES.includes(String(minutes));
              return (
                <div className={styles.reminderRow} key={index}>
                  <Select
                    value={preset ? String(minutes) : "custom"}
                    onChange={(value) => {
                      if (value === "custom") {
                        setReminderAt(
                          index,
                          preset ? CUSTOM_REMINDER_FALLBACK : minutes,
                        );
                      } else {
                        setReminderAt(index, Number.parseInt(value, 10) || 0);
                      }
                    }}
                    options={reminderOptions}
                    ariaLabel={t("calendar.editor.reminder")}
                    size="small"
                    className={styles.reminderSelect}
                  />
                  {!preset ? (
                    <Input
                      type="number"
                      min={0}
                      max={MAX_REMINDER_MINUTES}
                      value={String(minutes)}
                      onChange={(inputEvent) => {
                        const parsed = Number.parseInt(inputEvent.target.value, 10);
                        setReminderAt(index, Number.isFinite(parsed) ? parsed : 0);
                      }}
                      aria-label={t("calendar.reminder.customLabel")}
                      size="small"
                      containerClassName={styles.reminderInput}
                    />
                  ) : null}
                  <IconButton
                    size="small"
                    icon={<Delete16Regular />}
                    label={t("calendar.editor.removeReminder")}
                    onClick={() => removeReminder(index)}
                    className={styles.reminderRemove}
                  />
                </div>
              );
            })
          )}
          <div>
            <Button
              size="small"
              appearance="subtle"
              icon={<Add16Regular />}
              onClick={addReminder}
            >
              {t("calendar.editor.addReminder")}
            </Button>
          </div>
        </section>

        <Select
          label={t("calendar.editor.showAs")}
          value={form.showAs}
          onChange={(value) => patch({ showAs: value as ShowAs })}
          options={showAsOptions}
          className={styles.narrow}
        />

        <div className={styles.inlineRow}>
          <Switch
            checked={form.isPrivate}
            onChange={(checked) => patch({ isPrivate: checked })}
            label={t("calendar.editor.private")}
          />
          <span className={styles.hint}>{t("calendar.editor.privateHint")}</span>
        </div>

        <section className={styles.section}>
          <div className={styles.sectionTitle}>
            <People20Regular aria-hidden="true" />
            {t("calendar.editor.attendees")}
          </div>
          <TokenInput
            label={t("calendar.editor.required")}
            value={form.required}
            onChange={(value) => patch({ required: value })}
            suggestions={suggestionSource}
            placeholder={t("calendar.editor.addPeople")}
          />
          <TokenInput
            label={t("calendar.editor.optional")}
            value={form.optional}
            onChange={(value) => patch({ optional: value })}
            suggestions={suggestionSource}
            placeholder={t("calendar.editor.addPeople")}
          />
          {form.required.length + form.optional.length > 0 ? (
            <Checkbox
              checked={form.notify}
              onChange={(inputEvent) => patch({ notify: inputEvent.target.checked })}
              label={t("calendar.editor.notify")}
            />
          ) : null}
        </section>

        <div className={styles.meetingRow}>
          <Input
            label={t("calendar.editor.onlineMeeting")}
            value={form.onlineMeetingUrl}
            onChange={(inputEvent) => patch({ onlineMeetingUrl: inputEvent.target.value })}
            placeholder={t("calendar.editor.onlineMeetingPlaceholder")}
            icon={<Video20Regular />}
            containerClassName={styles.grow}
          />
          {form.onlineMeetingUrl.trim() ? (
            <Button
              size="small"
              icon={<Link20Regular />}
              onClick={() => openExternal(form.onlineMeetingUrl)}
              className={styles.joinButton}
            >
              {t("calendar.editor.openMeeting")}
            </Button>
          ) : null}
        </div>

        <TextArea
          label={t("calendar.editor.description")}
          value={form.description}
          onChange={(inputEvent) => patch({ description: inputEvent.target.value })}
          placeholder={t("calendar.editor.descriptionPlaceholder")}
          rows={4}
        />

        {form.repeat.mode === "weekly" && form.repeat.byWeekday.length === 0 ? (
          <p className={styles.hint}>
            {t("calendar.repeat.summary.weekly", {
              days: weekdayName,
            })}
          </p>
        ) : null}
      </Dialog>
      <ScopeDialog
        open={pendingSave !== null || pendingDelete}
        mode={pendingDelete ? "delete" : "edit"}
        onCancel={() => {
          setPendingSave(null);
          setPendingDelete(false);
        }}
        onChoose={(scope) => void chooseScope(scope)}
      />
    </>
  );
}
