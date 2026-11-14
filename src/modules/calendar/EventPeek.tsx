import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  CalendarLtr20Regular,
  Clock16Regular,
  Delete20Regular,
  Edit20Regular,
  Link20Regular,
  Location20Regular,
  LockClosed16Regular,
  People20Regular,
  Video20Regular,
} from "@fluentui/react-icons";
import type { Attendee, AttendeeStatus, Calendar, CalendarEvent } from "@/ipc/types";
import { Avatar, Button, IconButton, Popover, Spinner, toast } from "@/components";
import { events, isIpcError } from "@/ipc/api";
import { queryKeys } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { formatTime } from "@/lib/format";
import { formatEventTimeRange } from "./model";
import { mapUrlFor, openExternal } from "./openExternal";
import styles from "./EventPeek.module.css";

export interface EventPeekProps {
  eventId: string;
  instanceStart: string | null;
  anchor: HTMLElement | null;
  point: { x: number; y: number } | null;
  calendars: Calendar[];
  onClose: () => void;
  onEdit: (event: CalendarEvent) => void;
  onDelete: (event: CalendarEvent) => void;
  onResponded?: () => void;
}

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

const STATUS_KEYS: Record<AttendeeStatus, string> = {
  needsAction: "calendar.attendeeStatus.needsAction",
  accepted: "calendar.attendeeStatus.accepted",
  declined: "calendar.attendeeStatus.declined",
  tentative: "calendar.attendeeStatus.tentative",
  delegated: "calendar.attendeeStatus.delegated",
};

function attendeeName(attendee: Attendee): string {
  return attendee.name?.trim() || attendee.email;
}

function reminderLabel(t: (key: string, vars?: Record<string, string | number>) => string, minutes: number): string {
  if (minutes <= 0) return t("calendar.reminder.atStart");
  if (minutes % 1440 === 0) return t("calendar.reminder.days", { count: minutes / 1440 });
  if (minutes % 60 === 0) return t("calendar.reminder.hours", { count: minutes / 60 });
  return t("calendar.reminder.minutes", { count: minutes });
}

export function EventPeek({
  eventId,
  instanceStart,
  anchor,
  point,
  calendars,
  onClose,
  onEdit,
  onDelete,
  onResponded,
}: EventPeekProps) {
  const t = useT();
  const locale = useLanguage();
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: queryKeys.event(eventId, instanceStart),
    queryFn: () => events.get(eventId, instanceStart),
    staleTime: 15_000,
  });

  const respond = useMutation({
    mutationFn: (status: AttendeeStatus) => events.respond(eventId, status, null, true),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.event(eventId, instanceStart) });
      void queryClient.invalidateQueries({ queryKey: ["events"] });
      toast.success(t("calendar.toast.responded"));
      onResponded?.();
    },
    onError: (error) => {
      toast.error(t("calendar.error.action"), errorMessage(error));
    },
  });

  const event = query.data;
  const calendar = event ? calendars.find((item) => item.id === event.calendarId) : undefined;
  const color = event?.color ?? calendar?.color ?? "var(--accent)";
  const otherAttendees = event
    ? event.attendees.filter(
        (attendee) =>
          !event.organizer || attendee.email.toLowerCase() !== event.organizer.email.toLowerCase(),
      )
    : [];

  return (
    <Popover
      open
      onClose={onClose}
      anchor={anchor}
      point={point}
      placement="bottom-start"
      role="dialog"
      ariaLabel={event?.title || t("calendar.peek.edit")}
      minWidth={320}
      maxHeight={520}
      className={styles.popover}
    >
      {query.isLoading ? (
        <div className={styles.loading}>
          <Spinner size="small" label={t("common.loading")} />
        </div>
      ) : query.isError || !event ? (
        <div className={styles.error}>
          <p>{t("calendar.error.load")}</p>
          <Button size="small" onClick={() => void query.refetch()}>
            {t("calendar.error.retry")}
          </Button>
        </div>
      ) : (
        <div className={styles.content}>
          <div className={styles.colorBar} style={{ background: color }} aria-hidden="true" />
          <div className={styles.header}>
            <div className={styles.titleRow}>
              <h2 className={styles.title}>{event.title || t("calendar.untitled")}</h2>
              {event.isPrivate ? (
                <span className={styles.lock} title={t("calendar.peek.private")}>
                  <LockClosed16Regular aria-hidden="true" />
                </span>
              ) : null}
            </div>
            <div className={styles.time}>
              <Clock16Regular aria-hidden="true" />
              <span>{formatEventTimeRange(event, t("calendar.allDay"), locale)}</span>
            </div>
          </div>

          {event.location ? (
            <button
              type="button"
              className={styles.linkRow}
              onClick={() => openExternal(mapUrlFor(event.location))}
              title={t("calendar.peek.openMap")}
            >
              <Location20Regular aria-hidden="true" />
              <span className={styles.linkText}>{event.location}</span>
            </button>
          ) : null}

          {event.onlineMeetingUrl ? (
            <button
              type="button"
              className={styles.linkRow}
              onClick={() => openExternal(event.onlineMeetingUrl)}
            >
              <Video20Regular aria-hidden="true" />
              <span className={styles.linkText}>{t("calendar.peek.joinMeeting")}</span>
              <Link20Regular aria-hidden="true" />
            </button>
          ) : null}

          <div className={styles.metaRow}>
            <CalendarLtr20Regular aria-hidden="true" />
            <span className={styles.swatch} style={{ background: color }} aria-hidden="true" />
            <span className={styles.metaText}>
              {calendar?.name ?? t("calendar.peek.calendar", { name: "—" })}
            </span>
          </div>

          {event.reminders.length > 0 ? (
            <div className={styles.metaRow}>
              <Clock16Regular aria-hidden="true" />
              <span className={styles.metaText}>
                {t("calendar.peek.reminder", {
                  value: event.reminders
                    .map((minutes) => reminderLabel(t, minutes))
                    .join(", "),
                })}
              </span>
            </div>
          ) : null}

          {event.organizer ? (
            <div className={styles.section}>
              <div className={styles.sectionTitle}>{t("calendar.peek.organizer")}</div>
              <div className={styles.attendeeRow}>
                <Avatar name={event.organizer.name} email={event.organizer.email} size={24} />
                <span className={styles.attendeeName}>{attendeeName(event.organizer)}</span>
                <span className={styles.attendeeStatus}>
                  {t(STATUS_KEYS[event.organizer.status])}
                </span>
              </div>
            </div>
          ) : null}

          {event.attendees.length > 0 ? (
            <div className={styles.section}>
              <div className={styles.sectionTitle}>
                <People20Regular aria-hidden="true" />
                {t("calendar.peek.attendeesCount", { count: event.attendees.length })}
              </div>
              <div className={styles.attendeeList}>
                {otherAttendees.slice(0, 5).map((attendee) => (
                  <div key={attendee.email} className={styles.attendeeRow}>
                    <Avatar name={attendee.name} email={attendee.email} size={24} />
                    <span className={styles.attendeeName}>{attendeeName(attendee)}</span>
                    <span
                      className={cn(
                        styles.attendeeStatus,
                        attendee.status === "accepted" && styles.statusAccepted,
                        attendee.status === "declined" && styles.statusDeclined,
                        attendee.status === "tentative" && styles.statusTentative,
                      )}
                    >
                      {t(STATUS_KEYS[attendee.status])}
                    </span>
                  </div>
                ))}
                {otherAttendees.length > 5 ? (
                  <div className={styles.moreAttendees}>
                    +{otherAttendees.length - 5}
                  </div>
                ) : null}
              </div>
            </div>
          ) : null}

          {event.attendees.length > 0 || event.isOrganizer ? (
            <div className={styles.responseRow} role="group" aria-label={t("calendar.peek.yourResponse")}>
              {(
                [
                  ["accepted", "calendar.peek.accept"],
                  ["tentative", "calendar.peek.tentative"],
                  ["declined", "calendar.peek.decline"],
                ] as [AttendeeStatus, string][]
              ).map(([status, labelKey]) => (
                <Button
                  key={status}
                  size="small"
                  appearance={event.myResponse === status ? "primary" : "outline"}
                  loading={respond.isPending && respond.variables === status}
                  onClick={() => respond.mutate(status)}
                >
                  {t(labelKey)}
                </Button>
              ))}
            </div>
          ) : null}

          <div className={styles.footer}>
            <Button
              size="small"
              icon={<Edit20Regular />}
              onClick={() => onEdit(event)}
            >
              {t("calendar.peek.edit")}
            </Button>
            <span className={styles.spacer} />
            <IconButton
              size="small"
              icon={<Delete20Regular />}
              label={t("calendar.peek.delete")}
              disabled={Boolean(calendar?.isReadOnly)}
              onClick={() => onDelete(event)}
            />
          </div>

          <div className={styles.footerHint}>
            {!event.allDay ? (
              <>
                <span>{formatTime(new Date(event.start), { locale })}</span>
                <span aria-hidden="true">·</span>
              </>
            ) : null}
            <span>{event.timeZone}</span>
          </div>
        </div>
      )}
    </Popover>
  );
}
