import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";
import type { CalendarView, EventInstance } from "@/ipc/types";
import { Popover } from "@/components";
import { useLanguage, useT } from "@/lib/i18n";
import { formatTime } from "@/lib/format";
import { cn } from "@/lib/cn";
import {
  HOUR_HEIGHT,
  SNAP_MINUTES,
  addDaysLocal,
  combineDateAndMinutes,
  isSameDayLocal,
  isWorkDay,
  isoWeekNumber,
  shortWeekday,
  snapDelta,
  snapMinutes,
  startOfDayLocal,
  workHourBounds,
  type CalendarSettings,
} from "../dates";
import {
  allDayEventsForDay,
  displayEndMin,
  displayStartMin,
  layoutSegments,
  segmentsForDay,
  type PlacedSegment,
} from "../layout";
import {
  eventStartDate,
  formatEventChipTime,
  formatEventTimeRange,
  instanceSelectionKeyForInstance,
  isEventCancelled,
  isEventDeclined,
  eventEndDateExclusive,
} from "../model";
import type { NewEventSeed } from "../uiStore";
import styles from "./TimeGrid.module.css";

export interface TimeGridViewProps {
  view: Extract<CalendarView, "day" | "workWeek" | "week">;
  days: Date[];
  events: EventInstance[];
  settings: CalendarSettings;
  selectedKey: string | null;
  onSelectEvent: (event: EventInstance, anchor: HTMLElement | null) => void;
  onOpenEvent: (event: EventInstance) => void;
  onMoveEvent: (event: EventInstance, start: Date, end: Date) => void;
  onQuickCreate: (seed: NewEventSeed, point: { x: number; y: number }) => void;
}

interface DragState {
  mode: "move" | "resize";
  event: EventInstance;
  day: Date;
  startX: number;
  startY: number;
  deltaMin: number;
  dayDelta: number;
  dayIndex: number;
  columnWidth: number;
  originStart: Date;
  originEnd: Date;
  continuesBefore: boolean;
  continuesAfter: boolean;
  pointerId: number;
}

interface CreateDragState {
  dayIndex: number;
  anchorMin: number;
  currentMin: number;
  pointerId: number;
}

function errorFallback(event: EventInstance): string {
  return event.title || "";
}

export function TimeGridView({
  view,
  days,
  events,
  settings,
  selectedKey,
  onSelectEvent,
  onOpenEvent,
  onMoveEvent,
  onQuickCreate,
}: TimeGridViewProps) {
  const t = useT();
  const locale = useLanguage();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [drag, setDragState] = useState<DragState | null>(null);
  const dragRef = useRef<DragState | null>(null);
  const lastDragAt = useRef(0);
  const [createDrag, setCreateDragState] = useState<CreateDragState | null>(null);
  const createDragRef = useRef<CreateDragState | null>(null);
  const [more, setMore] = useState<{
    anchor: HTMLElement;
    list: EventInstance[];
    day: Date;
  } | null>(null);

  const setDrag = (next: DragState | null) => {
    dragRef.current = next;
    setDragState(next);
  };
  const setCreateDrag = (next: CreateDragState | null) => {
    createDragRef.current = next;
    setCreateDragState(next);
  };

  const work = workHourBounds(settings);
  const today = useMemo(() => startOfDayLocal(new Date()), []);
  const [nowMinutes, setNowMinutes] = useState(() => {
    const now = new Date();
    return now.getHours() * 60 + now.getMinutes();
  });

  useEffect(() => {
    const timer = window.setInterval(() => {
      const now = new Date();
      setNowMinutes(now.getHours() * 60 + now.getMinutes());
    }, 60_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    // Scroll the grid so the morning is visible on first load.
    const element = scrollRef.current;
    if (!element) return;
    const targetMinutes = Math.min(8 * 60, work.start);
    element.scrollTop = Math.max(0, (targetMinutes / 60) * HOUR_HEIGHT - 8);
    // Only on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const dayLayouts = useMemo(
    () =>
      days.map((day) => ({
        day,
        placed: layoutSegments(segmentsForDay(events, day)),
        allDay: allDayEventsForDay(events, day),
      })),
    [days, events],
  );

  const showWeekNumber = settings.showWeekNumbers && days.length > 1;
  const gridStyle = {
    gridTemplateColumns: `var(--time-gutter) repeat(${days.length}, minmax(0, 1fr))`,
  };

  const startEventDrag = (
    event: ReactPointerEvent<HTMLButtonElement>,
    instance: EventInstance,
    segment: PlacedSegment["segment"],
    day: Date,
  ) => {
    if (event.button !== 0) return;
    event.stopPropagation();
    const rect = event.currentTarget.getBoundingClientRect();
    const column = event.currentTarget.closest("[data-day-column]") as HTMLElement | null;
    const columnRect = column?.getBoundingClientRect() ?? null;
    const resize = event.clientY >= rect.bottom - 8;
    event.currentTarget.setPointerCapture(event.pointerId);
    lastDragAt.current = 0;
    setDrag({
      mode: resize ? "resize" : "move",
      event: instance,
      day: startOfDayLocal(day),
      startX: event.clientX,
      startY: event.clientY,
      deltaMin: 0,
      dayDelta: 0,
      dayIndex: days.findIndex((entry) => entry.getTime() === startOfDayLocal(day).getTime()),
      columnWidth: columnRect?.width ?? 0,
      originStart: eventStartDate(instance),
      originEnd: eventEndDateExclusive(instance),
      continuesBefore: segment.continuesBefore,
      continuesAfter: segment.continuesAfter,
      pointerId: event.pointerId,
    });
  };

  const moveEventDrag = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const current = dragRef.current;
    if (!current || current.pointerId !== event.pointerId) return;
    const rawMinutes = ((event.clientY - current.startY) / HOUR_HEIGHT) * 60;
    const delta = snapDelta(rawMinutes, SNAP_MINUTES);
    let dayDelta = 0;
    if (current.mode === "move" && current.columnWidth > 1 && current.dayIndex >= 0) {
      dayDelta = Math.round((event.clientX - current.startX) / current.columnWidth);
      dayDelta = Math.max(-current.dayIndex, Math.min(days.length - 1 - current.dayIndex, dayDelta));
    }
    if (delta !== current.deltaMin || dayDelta !== current.dayDelta) {
      setDrag({ ...current, deltaMin: delta, dayDelta });
    }
  };

  const endEventDrag = (event: ReactPointerEvent<HTMLButtonElement>) => {
    const current = dragRef.current;
    if (!current || current.pointerId !== event.pointerId) return;
    setDrag(null);
    if (current.deltaMin === 0 && (current.mode === "resize" || current.dayDelta === 0)) {
      onSelectEvent(current.event, event.currentTarget);
      return;
    }
    lastDragAt.current = Date.now();
    const dayStart = current.day;
    const dayEnd = addDaysLocal(dayStart, 1);
    const dayShift = current.mode === "move" ? current.dayDelta : 0;
    const baseStart = dayShift ? addDaysLocal(current.originStart, dayShift) : current.originStart;
    const baseEnd = dayShift ? addDaysLocal(current.originEnd, dayShift) : current.originEnd;
    if (current.mode === "move") {
      let start = new Date(baseStart.getTime() + current.deltaMin * 60_000);
      let end = new Date(baseEnd.getTime() + current.deltaMin * 60_000);
      if (dayShift === 0 && !current.continuesBefore && !current.continuesAfter) {
        const duration = end.getTime() - start.getTime();
        if (start < dayStart) {
          start = dayStart;
          end = new Date(dayStart.getTime() + duration);
        }
        if (end > dayEnd) {
          end = dayEnd;
          start = new Date(dayEnd.getTime() - duration);
        }
      }
      onMoveEvent(current.event, start, end);
    } else {
      let end = new Date(current.originEnd.getTime() + current.deltaMin * 60_000);
      const minimum = new Date(current.originStart.getTime() + 15 * 60_000);
      if (end < minimum) end = minimum;
      if (!current.continuesAfter && end > dayEnd) end = dayEnd;
      onMoveEvent(current.event, current.originStart, end);
    }
  };

  const cancelDrag = () => setDrag(null);

  const startCreate = (event: ReactPointerEvent<HTMLDivElement>, dayIndex: number) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const rect = event.currentTarget.getBoundingClientRect();
    const minutes = snapMinutes(((event.clientY - rect.top) / rect.height) * 1440, SNAP_MINUTES);
    event.currentTarget.setPointerCapture(event.pointerId);
    setCreateDrag({
      dayIndex,
      anchorMin: minutes,
      currentMin: minutes,
      pointerId: event.pointerId,
    });
  };

  const moveCreate = (event: ReactPointerEvent<HTMLDivElement>) => {
    const current = createDragRef.current;
    if (!current || current.pointerId !== event.pointerId) return;
    const rect = event.currentTarget.getBoundingClientRect();
    const minutes = snapMinutes(((event.clientY - rect.top) / rect.height) * 1440, SNAP_MINUTES);
    if (minutes !== current.currentMin) setCreateDrag({ ...current, currentMin: minutes });
  };

  const endCreate = (event: ReactPointerEvent<HTMLDivElement>) => {
    const current = createDragRef.current;
    if (!current || current.pointerId !== event.pointerId) return;
    setCreateDrag(null);
    const day = days[current.dayIndex];
    if (!day) return;
    const min = Math.min(current.anchorMin, current.currentMin);
    const max = Math.max(current.anchorMin, current.currentMin);
    let startMin = min;
    let endMin = max;
    if (endMin - startMin < SNAP_MINUTES) {
      const duration = Math.max(SNAP_MINUTES, settings.defaultEventDurationMinutes);
      startMin = Math.min(min, 1440 - duration);
      endMin = startMin + duration;
    }
    const start = combineDateAndMinutes(day, startMin);
    const end = combineDateAndMinutes(day, endMin);
    onQuickCreate(
      { start, end, allDay: false, calendarId: null },
      { x: event.clientX, y: event.clientY },
    );
  };

  const cancelCreate = () => setCreateDrag(null);

  const renderPlaced = (placed: PlacedSegment, day: Date) => {
    const segment = placed.segment;
    const instance = segment.event;
    const activeEventDrag =
      dragRef.current && dragRef.current.event.instanceKey === instance.instanceKey
        ? dragRef.current
        : null;
    const dragging = activeEventDrag !== null;
    const delta = activeEventDrag?.deltaMin ?? 0;
    let startMin =
      displayStartMin(segment) +
      (activeEventDrag?.mode === "move" ? delta : 0);
    let endMin = displayEndMin(segment) + (dragging ? delta : 0);
    if (activeEventDrag?.mode === "resize") startMin = displayStartMin(segment);
    if (endMin - startMin < 1) endMin = startMin + 1;
    const top = (startMin / 60) * HOUR_HEIGHT;
    const height = Math.max(18, ((endMin - startMin) / 60) * HOUR_HEIGHT);
    const compact = height < 34;
    const selected = selectedKey === instanceSelectionKeyForInstance(instance);
    return (
      <button
        key={instance.instanceKey}
        type="button"
        className={cn(
          styles.event,
          compact && styles.eventCompact,
          selected && styles.eventSelected,
          dragging && styles.eventDragging,
          segment.continuesBefore && styles.continuesBefore,
          segment.continuesAfter && styles.continuesAfter,
        )}
        style={{
          top,
          height,
          left: `calc(${(placed.column / placed.columns) * 100}% + 2px)`,
          width: `calc(${100 / placed.columns}% - 4px)`,
          ["--event-color" as string]: instance.color,
        }}
        data-show-as={instance.showAs}
        data-status={instance.status}
        data-declined={isEventDeclined(instance) || undefined}
        data-cancelled={isEventCancelled(instance) || undefined}
        aria-label={`${errorFallback(instance)} ${formatEventTimeRange(
          instance,
          t("calendar.allDay"),
          locale,
        )}`}
        onPointerDown={(event) => startEventDrag(event, instance, segment, day)}
        onPointerMove={moveEventDrag}
        onPointerUp={endEventDrag}
        onPointerCancel={cancelDrag}
        onDoubleClick={(event) => {
          event.stopPropagation();
          if (Date.now() - lastDragAt.current < 250) return;
          onOpenEvent(instance);
        }}
        onKeyDown={(event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            onSelectEvent(instance, event.currentTarget);
          }
        }}
      >
        <span className={styles.eventTitle}>{instance.title || t("calendar.untitled")}</span>
        {!compact ? (
          <span className={styles.eventTime}>
            {formatEventTimeRange(instance, t("calendar.allDay"), locale)}
          </span>
        ) : null}
        {!compact && instance.location && height > 52 ? (
          <span className={styles.eventLocation}>{instance.location}</span>
        ) : null}
        <span className={styles.resizeHandle} aria-hidden="true" />
      </button>
    );
  };

  return (
    <div className={cn(styles.root, view === "day" && styles.dayView)}>
      <div className={styles.headerRow} style={gridStyle}>
        <div className={styles.gutterHeader}>
          {showWeekNumber ? (
            <span className={styles.weekNumberHeader}>
              {t("calendar.weekNumberShort")}
              {isoWeekNumber(days[0] ?? today)}
            </span>
          ) : null}
        </div>
        {days.map((day) => {
          const isToday = isSameDayLocal(day, today);
          return (
            <div key={day.toISOString()} className={styles.headerCell}>
              <span className={styles.weekday}>{shortWeekday(day, locale)}</span>
              <span className={cn(styles.dayNumber, isToday && styles.todayNumber)}>
                {day.getDate()}
              </span>
            </div>
          );
        })}
      </div>

      <div className={styles.allDayRow} style={gridStyle}>
        <div className={styles.allDayLabel}>{t("calendar.allDay")}</div>
        {days.map((day) => {
          const list = dayLayouts.find((layout) => layout.day.getTime() === day.getTime())?.allDay ?? [];
          const visible = list.slice(0, 2);
          return (
            <div key={day.toISOString()} className={styles.allDayCell}>
              {visible.map((event) => (
                <button
                  key={event.instanceKey}
                  type="button"
                  className={cn(
                    styles.allDayChip,
                    selectedKey === instanceSelectionKeyForInstance(event) && styles.allDayChipSelected,
                  )}
                  style={{ ["--event-color" as string]: event.color }}
                  data-show-as={event.showAs}
                  data-declined={isEventDeclined(event) || undefined}
                  onClick={(event_) => onSelectEvent(event, event_.currentTarget)}
                  onDoubleClick={(event_) => {
                    event_.stopPropagation();
                    onOpenEvent(event);
                  }}
                >
                  <span className={styles.allDayChipTitle}>
                    {event.title || t("calendar.untitled")}
                  </span>
                </button>
              ))}
              {list.length > visible.length ? (
                <button
                  type="button"
                  className={styles.moreChip}
                  onClick={(event_) =>
                    setMore({ anchor: event_.currentTarget, list, day })
                  }
                >
                  {t("calendar.moreEvents", { count: list.length - visible.length })}
                </button>
              ) : null}
            </div>
          );
        })}
      </div>

      <div className={styles.scroll} ref={scrollRef}>
        <div className={styles.body} style={gridStyle}>
          <div className={styles.timeColumn}>
            {Array.from({ length: 24 }, (_, hour) => (
              <div key={hour} className={styles.hourLabel} style={{ top: hour * HOUR_HEIGHT }}>
                {formatTime(new Date(2000, 0, 1, hour, 0), { locale })}
              </div>
            ))}
          </div>
          {dayLayouts.map((layout, dayIndex) => {
            const isToday = isSameDayLocal(layout.day, today);
            const working = isWorkDay(layout.day, settings);
            const create = createDrag?.dayIndex === dayIndex ? createDrag : null;
            const createStart = create ? Math.min(create.anchorMin, create.currentMin) : 0;
            const createEnd = create ? Math.max(create.anchorMin, create.currentMin) : 0;
            return (
              <div
                key={layout.day.toISOString()}
                className={cn(
                  styles.dayColumn,
                  isToday && styles.todayColumn,
                  drag &&
                    drag.mode === "move" &&
                    drag.dayIndex + drag.dayDelta === dayIndex &&
                    drag.dayDelta !== 0 &&
                    styles.dropTarget,
                )}
                data-weekend={!working || undefined}
                data-day-column=""
              >
                {working ? (
                  <div
                    className={styles.workShade}
                    style={{
                      top: (work.start / 60) * HOUR_HEIGHT,
                      height: ((work.end - work.start) / 60) * HOUR_HEIGHT,
                    }}
                    aria-hidden="true"
                  />
                ) : null}
                <div
                  className={styles.createLayer}
                  onPointerDown={(event) => startCreate(event, dayIndex)}
                  onPointerMove={moveCreate}
                  onPointerUp={endCreate}
                  onPointerCancel={cancelCreate}
                  aria-hidden="true"
                />
                {create ? (
                  <div
                    className={styles.createGhost}
                    style={{
                      top: (createStart / 60) * HOUR_HEIGHT,
                      height: Math.max(6, ((createEnd - createStart) / 60) * HOUR_HEIGHT),
                    }}
                    aria-hidden="true"
                  />
                ) : null}
                {layout.placed.map((placed) => renderPlaced(placed, layout.day))}
                {isToday ? (
                  <div
                    className={styles.nowLine}
                    style={{ top: (nowMinutes / 60) * HOUR_HEIGHT }}
                    aria-hidden="true"
                  >
                    <span className={styles.nowDot} />
                  </div>
                ) : null}
              </div>
            );
          })}
        </div>
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
              onClick={(event_) => {
                onSelectEvent(event, event_.currentTarget);
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
