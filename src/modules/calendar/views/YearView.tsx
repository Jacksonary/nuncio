import { useMemo } from "react";
import type { CalendarView, EventInstance } from "@/ipc/types";
import { useLanguage } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import {
  addDaysLocal,
  isSameDayLocal,
  isSameMonth,
  monthGridDays,
  startOfDayLocal,
  startOfMonth,
  weekdayHeaders,
} from "../dates";
import { eventLastDayInclusive, eventStartDate } from "../model";
import styles from "./YearView.module.css";

export interface YearViewProps {
  year: number;
  events: EventInstance[];
  weekStartsOn: number;
  onNavigate: (date: Date, view: CalendarView) => void;
}

export function YearView({ year, events, weekStartsOn, onNavigate }: YearViewProps) {
  const locale = useLanguage();
  const today = useMemo(() => startOfDayLocal(new Date()), []);
  const narrowDays = useMemo(() => weekdayHeaders(locale, weekStartsOn), [locale, weekStartsOn]);

  const colorsByDay = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const event of events) {
      let cursor = startOfDayLocal(eventStartDate(event));
      const last = startOfDayLocal(eventLastDayInclusive(event));
      let guard = 0;
      while (cursor <= last && guard < 400) {
        if (cursor.getFullYear() === year) {
          const key = cursor.toDateString();
          const list = map.get(key) ?? [];
          if (!list.includes(event.color) && list.length < 3) list.push(event.color);
          map.set(key, list);
        }
        cursor = addDaysLocal(cursor, 1);
        guard += 1;
      }
    }
    return map;
  }, [events, year]);

  const months = useMemo(
    () => Array.from({ length: 12 }, (_, index) => startOfMonth(new Date(year, index, 1))),
    [year],
  );

  return (
    <div className={styles.root}>
      {months.map((month) => {
        const days = monthGridDays(month, weekStartsOn);
        return (
          <div key={month.getMonth()} className={styles.month}>
            <button
              type="button"
              className={styles.monthTitle}
              onClick={() => onNavigate(month, "month")}
            >
              {new Intl.DateTimeFormat(locale, { month: "long" }).format(month)}
            </button>
            <div className={styles.weekdayHeader} aria-hidden="true">
              {narrowDays.map((label, index) => (
                <span key={`${label}-${index}`}>{label}</span>
              ))}
            </div>
            <div className={styles.grid}>
              {days.map((day) => {
                const outside = !isSameMonth(day, month);
                const isToday = isSameDayLocal(day, today);
                const colors = colorsByDay.get(day.toDateString()) ?? [];
                return (
                  <button
                    key={day.toISOString()}
                    type="button"
                    className={cn(
                      styles.day,
                      outside && styles.outside,
                      isToday && styles.today,
                      colors.length > 0 && styles.hasEvents,
                    )}
                    onClick={() => onNavigate(day, "day")}
                    tabIndex={outside ? -1 : 0}
                    aria-label={new Intl.DateTimeFormat(locale, {
                      month: "long",
                      day: "numeric",
                    }).format(day)}
                  >
                    <span className={styles.dayNumber}>{day.getDate()}</span>
                    {colors.length > 0 ? (
                      <span className={styles.dots} aria-hidden="true">
                        {colors.map((color) => (
                          <span
                            key={color}
                            className={styles.dot}
                            style={{ background: color }}
                          />
                        ))}
                      </span>
                    ) : null}
                  </button>
                );
              })}
            </div>
          </div>
        );
      })}
    </div>
  );
}
