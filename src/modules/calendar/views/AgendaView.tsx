import { useMemo } from "react";
import type { EventInstance } from "@/ipc/types";
import { EmptyState } from "@/components";
import { CalendarLtr24Regular } from "@fluentui/react-icons";
import { useLanguage, useT } from "@/lib/i18n";
import {
  addDaysLocal,
  calendarDayDiff,
  formatAgendaDay,
  startOfDayLocal,
} from "../dates";
import {
  formatEventTimeRange,
  instanceSelectionKeyForInstance,
  isEventDeclined,
  isEventCancelled,
  eventStartsOnDay,
} from "../model";
import { cn } from "@/lib/cn";
import styles from "./AgendaView.module.css";

export interface AgendaViewProps {
  anchor: Date;
  events: EventInstance[];
  selectedKey: string | null;
  onSelectEvent: (event: EventInstance, anchor: HTMLElement | null) => void;
  onOpenEvent: (event: EventInstance) => void;
}

export function AgendaView({
  anchor,
  events,
  selectedKey,
  onSelectEvent,
  onOpenEvent,
}: AgendaViewProps) {
  const t = useT();
  const locale = useLanguage();
  const today = useMemo(() => startOfDayLocal(new Date()), []);
  const start = startOfDayLocal(anchor);

  const groups = useMemo(() => {
    const result: { day: Date; events: EventInstance[] }[] = [];
    for (let index = 0; index < 30; index += 1) {
      const day = addDaysLocal(start, index);
      const list = events
        .filter((event) => eventStartsOnDay(event, day))
        .sort((a, b) => {
          if (a.allDay !== b.allDay) return a.allDay ? -1 : 1;
          const startDiff = new Date(a.start).getTime() - new Date(b.start).getTime();
          if (startDiff !== 0) return startDiff;
          return a.title.localeCompare(b.title);
        });
      if (list.length > 0) result.push({ day, events: list });
    }
    return result;
  }, [events, start]);

  if (groups.length === 0) {
    return (
      <EmptyState
        icon={<CalendarLtr24Regular />}
        title={t("calendar.view.agenda")}
        description={t("calendar.agenda.empty")}
      />
    );
  }

  const dayLabel = (day: Date): string => {
    const diff = calendarDayDiff(day, today);
    if (diff === 0) return t("calendar.agenda.today");
    if (diff === 1) return t("calendar.agenda.tomorrow");
    if (diff === -1) return t("common.yesterday");
    const raw = formatAgendaDay(day, locale, today);
    return raw === "__today__"
      ? t("calendar.agenda.today")
      : raw === "__tomorrow__"
        ? t("calendar.agenda.tomorrow")
        : raw;
  };

  return (
    <div className={styles.root}>
      {groups.map((group) => (
        <section key={group.day.toDateString()} className={styles.day}>
          <h3 className={styles.dayHeader}>
            <span className={styles.dayName}>{dayLabel(group.day)}</span>
            <span className={styles.dayDate}>
              {new Intl.DateTimeFormat(locale, { month: "long", day: "numeric" }).format(
                group.day,
              )}
            </span>
          </h3>
          <div className={styles.rows}>
            {group.events.map((event) => (
              <button
                key={event.instanceKey}
                type="button"
                className={cn(
                  styles.row,
                  selectedKey === instanceSelectionKeyForInstance(event) && styles.selected,
                )}
                data-show-as={event.showAs}
                data-declined={isEventDeclined(event) || undefined}
                data-cancelled={isEventCancelled(event) || undefined}
                onClick={(clickEvent) => onSelectEvent(event, clickEvent.currentTarget)}
                onDoubleClick={() => onOpenEvent(event)}
              >
                <span
                  className={styles.colorBar}
                  style={{ background: event.color }}
                  aria-hidden="true"
                />
                <span className={styles.time}>
                  {formatEventTimeRange(event, t("calendar.allDay"), locale)}
                </span>
                <span className={styles.body}>
                  <span className={styles.title}>{event.title || t("calendar.untitled")}</span>
                  {event.location ? (
                    <span className={styles.location}>{event.location}</span>
                  ) : null}
                </span>
              </button>
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}
