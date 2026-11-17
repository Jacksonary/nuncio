import { useMemo, useState, type DragEvent, type MouseEvent } from "react";
import type { EventInstance } from "@/ipc/types";
import { Popover } from "@/components";
import { useLanguage, useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import {
  addDaysLocal,
  calendarDayDiff,
  formatMonthDay,
  isSameDayLocal,
  isSameMonth,
  isoWeekNumber,
  longWeekday,
  shortWeekday,
  startOfDayLocal,
} from "../dates";
import {
  eventEndDateExclusive,
  eventSpansDay,
  eventStartDate,
  formatEventChipTime,
  instanceSelectionKeyForInstance,
  isEventDeclined,
  isEventCancelled,
} from "../model";
import styles from "./MonthView.module.css";

export interface MonthViewProps {
  month: Date;
  days: Date[];
  events: EventInstance[];
  showWeekNumbers: boolean;
  selectedKey: string | null;
  onSelectEvent: (event: EventInstance, anchor: HTMLElement | null) => void;
  onOpenEvent: (event: EventInstance) => void;
  onDayClick: (day: Date) => void;
  onDayDoubleClick: (day: Date, point: { x: number; y: number }) => void;
  onMoveEvent: (event: EventInstance, start: Date, end: Date) => void;
}

const MAX_CHIPS = 3;
const DRAG_TYPE = "application/x-nuncio-calendar-event";

export function MonthView({
  month,
  days,
  events,
  showWeekNumbers,
  selectedKey,
  onSelectEvent,
  onOpenEvent,
  onDayClick,
  onDayDoubleClick,
  onMoveEvent,
}: MonthViewProps) {
  const t = useT();
  const locale = useLanguage();
  const today = useMemo(() => startOfDayLocal(new Date()), []);
  const [dragOverDay, setDragOverDay] = useState<string | null>(null);
  const [more, setMore] = useState<{ anchor: HTMLElement; list: EventInstance[]; day: Date } | null>(
    null,
  );

  const eventsByDay = useMemo(() => {
    const map = new Map<string, EventInstance[]>();
    for (const day of days) {
      const key = day.toDateString();
      const list: EventInstance[] = [];
      for (const event of events) {
        if (eventSpansDay(event, day)) list.push(event);
      }
      list.sort((a, b) => {
        if (a.allDay !== b.allDay) return a.allDay ? -1 : 1;
        const diff = eventStartDate(a).getTime() - eventStartDate(b).getTime();
        if (diff !== 0) return diff;
        return a.title.localeCompare(b.title);
      });
      map.set(key, list);
    }
    return map;
  }, [days, events]);

  const byInstanceKey = useMemo(() => {
    const map = new Map<string, EventInstance>();
    for (const event of events) map.set(event.instanceKey, event);
    return map;
  }, [events]);

  const weekdayLabels = useMemo(() => {
    const labels: string[] = [];
    for (let index = 0; index < 7; index += 1) {
      const day = days[index];
      if (day) labels.push(shortWeekday(day, locale));
    }
    return labels;
  }, [days, locale]);

  const columns = showWeekNumbers ? "28px repeat(7, minmax(0, 1fr))" : "repeat(7, minmax(0, 1fr))";

  const handleDrop = (dragEvent: DragEvent<HTMLDivElement>, day: Date) => {
    dragEvent.preventDefault();
    setDragOverDay(null);
    const key = dragEvent.dataTransfer.getData(DRAG_TYPE);
    if (!key) return;
    const event = byInstanceKey.get(key);
    if (!event) return;
    const delta = calendarDayDiff(day, eventStartDate(event));
    if (delta === 0) return;
    const newStart = addDaysLocal(eventStartDate(event), delta);
    const newEnd = addDaysLocal(eventEndDateExclusive(event), delta);
    onMoveEvent(event, newStart, newEnd);
  };

  const openMore = (event: MouseEvent<HTMLButtonElement>, list: EventInstance[], day: Date) => {
    event.stopPropagation();
    setMore({ anchor: event.currentTarget, list, day });
  };

  return (
    <div className={styles.root}>
      <div className={styles.weekdayRow} style={{ gridTemplateColumns: columns }}>
        {showWeekNumbers ? (
          <div className={styles.weekNumberHeader} aria-hidden="true">
            {t("calendar.weekNumberShort")}
          </div>
        ) : null}
        {weekdayLabels.map((label, index) => (
          <div key={`${label}-${index}`} className={styles.weekday}>
            {label}
          </div>
        ))}
      </div>

      <div className={styles.weeks}>
        {Array.from({ length: 6 }, (_, row) => {
          const rowDays = days.slice(row * 7, row * 7 + 7);
          const first = rowDays[0];
          return (
            <div
              key={row}
              className={styles.weekRow}
              style={{ gridTemplateColumns: columns }}
            >
              {showWeekNumbers && first ? (
                <div className={styles.weekNumber} aria-label={t("calendar.weekNumber", { number: isoWeekNumber(first) })}>
                  {isoWeekNumber(first)}
                </div>
              ) : null}
              {rowDays.map((day) => {
                const key = day.toDateString();
                const list = eventsByDay.get(key) ?? [];
                const visible = list.slice(0, MAX_CHIPS);
                const overflow = list.length - visible.length;
                const isToday = isSameDayLocal(day, today);
                const outside = !isSameMonth(day, month);
                return (
                  <div
                    key={key}
                    className={cn(
                      styles.cell,
                      outside && styles.outside,
                      isToday && styles.today,
                      dragOverDay === key && styles.dragOver,
                    )}
                    onDoubleClick={(event) => {
                      const target = event.target as HTMLElement;
                      if (target.closest("[data-month-event]") || target.closest("button")) return;
                      onDayDoubleClick(day, { x: event.clientX, y: event.clientY });
                    }}
                    onDragOver={(event) => {
                      if (!event.dataTransfer.types.includes(DRAG_TYPE)) return;
                      event.preventDefault();
                      event.dataTransfer.dropEffect = "move";
                      if (dragOverDay !== key) setDragOverDay(key);
                    }}
                    onDragLeave={(event) => {
                      if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
                      if (dragOverDay === key) setDragOverDay(null);
                    }}
                    onDrop={(event) => handleDrop(event, day)}
                  >
                    <div className={styles.cellHeader}>
                      <button
                        type="button"
                        className={cn(styles.dayNumber, isToday && styles.todayNumber)}
                        onClick={(event) => {
                          event.stopPropagation();
                          onDayClick(day);
                        }}
                        aria-label={longWeekday(day, locale) + " " + formatMonthDay(day, locale)}
                      >
                        {day.getDate()}
                      </button>
                    </div>
                    <div className={styles.chips}>
                      {visible.map((event) => {
                        const selected = selectedKey === instanceSelectionKeyForInstance(event);
                        return (
                          <button
                            key={event.instanceKey}
                            type="button"
                            data-month-event=""
                            draggable
                            className={cn(styles.chip, selected && styles.chipSelected)}
                            style={{ ["--event-color" as string]: event.color }}
                            data-show-as={event.showAs}
                            data-declined={isEventDeclined(event) || undefined}
                            data-cancelled={isEventCancelled(event) || undefined}
                            title={event.title || t("calendar.untitled")}
                            onClick={(clickEvent) => {
                              clickEvent.stopPropagation();
                              onSelectEvent(event, clickEvent.currentTarget);
                            }}
                            onDoubleClick={(doubleEvent) => {
                              doubleEvent.stopPropagation();
                              onOpenEvent(event);
                            }}
                            onDragStart={(dragEvent) => {
                              dragEvent.dataTransfer.setData(DRAG_TYPE, event.instanceKey);
                              dragEvent.dataTransfer.effectAllowed = "move";
                            }}
                          >
                            {!event.allDay ? (
                              <span className={styles.chipTime}>
                                {formatEventChipTime(event, t("calendar.allDay"), locale)}
                              </span>
                            ) : null}
                            <span className={styles.chipTitle}>
                              {event.title || t("calendar.untitled")}
                            </span>
                          </button>
                        );
                      })}
                      {overflow > 0 ? (
                        <button
                          type="button"
                          className={styles.moreChip}
                          onClick={(event) => openMore(event, list, day)}
                        >
                          {t("calendar.moreEvents", { count: overflow })}
                        </button>
                      ) : null}
                    </div>
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>

      {more ? (
        <Popover
          open
          onClose={() => setMore(null)}
          anchor={more.anchor}
          placement="bottom-start"
          role="dialog"
          ariaLabel={t("calendar.eventCount", { count: more.list.length })}
          minWidth={240}
          maxHeight={320}
          className={styles.morePopover}
        >
          <div className={styles.moreDate}>
            {new Intl.DateTimeFormat(locale, {
              weekday: "long",
              month: "long",
              day: "numeric",
            }).format(more.day)}
          </div>
          {more.list.map((event) => (
            <button
              key={event.instanceKey}
              type="button"
              className={styles.moreItem}
              onClick={(clickEvent) => {
                onSelectEvent(event, clickEvent.currentTarget);
                setMore(null);
              }}
              onDoubleClick={() => onOpenEvent(event)}
            >
              <span className={styles.moreSwatch} style={{ background: event.color }} />
              <span className={styles.moreTitle}>{event.title || t("calendar.untitled")}</span>
              <span className={styles.moreTime}>
                {formatEventChipTime(event, t("calendar.allDay"), locale)}
              </span>
            </button>
          ))}
        </Popover>
      ) : null}
    </div>
  );
}
