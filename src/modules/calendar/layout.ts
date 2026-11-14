/**
 * Overlap layout for the day/work-week/week time grid.
 *
 * Each event is first clipped to the visible day (multi-day events become one
 * segment per day with continuation flags). Overlapping segments are grouped
 * into connected clusters; inside a cluster every segment gets the first free
 * column. This is the standard "calendar columns" algorithm and stays O(n log n).
 */
import type { EventInstance } from "@/ipc/types";
import { addDaysLocal, startOfDayLocal } from "./dates";
import { eventEndDateExclusive, eventStartDate } from "./model";

export interface DaySegment {
  event: EventInstance;
  /** Minutes since local midnight, 0..1440. */
  startMin: number;
  endMin: number;
  continuesBefore: boolean;
  continuesAfter: boolean;
}

export interface PlacedSegment {
  segment: DaySegment;
  column: number;
  columns: number;
}

/** Clip a timed event to one local day; returns null when it does not intersect. */
export function segmentForDay(event: EventInstance, day: Date): DaySegment | null {
  if (event.allDay) return null;
  const dayStart = startOfDayLocal(day);
  const dayEnd = addDaysLocal(dayStart, 1);
  const start = eventStartDate(event);
  const end = new Date(event.end);
  if (Number.isNaN(start.getTime()) || Number.isNaN(end.getTime())) return null;
  if (end <= dayStart || start >= dayEnd) return null;

  const clampedStart = start < dayStart ? dayStart : start;
  const clampedEnd = end > dayEnd ? dayEnd : end;
  const startMin = Math.max(0, (clampedStart.getTime() - dayStart.getTime()) / 60_000);
  const endMin = Math.min(1440, (clampedEnd.getTime() - dayStart.getTime()) / 60_000);
  if (endMin <= startMin) return null;

  return {
    event,
    startMin,
    endMin,
    continuesBefore: start < dayStart,
    continuesAfter: end > dayEnd,
  };
}

/** All timed segments for a day, sorted by start (longest first on ties). */
export function segmentsForDay(events: EventInstance[], day: Date): DaySegment[] {
  const segments: DaySegment[] = [];
  for (const event of events) {
    const segment = segmentForDay(event, day);
    if (segment) segments.push(segment);
  }
  segments.sort((a, b) => a.startMin - b.startMin || b.endMin - a.endMin);
  return segments;
}

const EPSILON = 0.001;

/**
 * Assign columns to segments. Segments that do not touch share columns
 * independently (columns resets per cluster).
 */
export function layoutSegments(segments: DaySegment[]): PlacedSegment[] {
  const sorted = [...segments].sort(
    (a, b) => a.startMin - b.startMin || b.endMin - a.endMin,
  );
  const result: PlacedSegment[] = [];
  let cluster: DaySegment[] = [];
  let clusterEnd = Number.NEGATIVE_INFINITY;

  const flush = () => {
    if (cluster.length === 0) return;
    const columnEnds: number[] = [];
    const assigned: { segment: DaySegment; column: number }[] = [];
    for (const segment of cluster) {
      let column = columnEnds.findIndex((end) => end <= segment.startMin + EPSILON);
      if (column === -1) {
        column = columnEnds.length;
        columnEnds.push(segment.endMin);
      } else {
        columnEnds[column] = segment.endMin;
      }
      assigned.push({ segment, column });
    }
    const columns = Math.max(1, columnEnds.length);
    for (const item of assigned) {
      result.push({ segment: item.segment, column: item.column, columns });
    }
    cluster = [];
    clusterEnd = Number.NEGATIVE_INFINITY;
  };

  for (const segment of sorted) {
    if (cluster.length > 0 && segment.startMin >= clusterEnd - EPSILON) {
      flush();
    }
    cluster.push(segment);
    clusterEnd = Math.max(clusterEnd, segment.endMin);
  }
  flush();

  return result.sort(
    (a, b) => a.segment.startMin - b.segment.startMin || a.column - b.column,
  );
}

/** Convenience for views: layout + sort events for a single day. */
export function layoutDay(events: EventInstance[], day: Date): PlacedSegment[] {
  return layoutSegments(segmentsForDay(events, day));
}

/** Events overlapping the day that are all-day (used by the all-day row). */
export function allDayEventsForDay(events: EventInstance[], day: Date): EventInstance[] {
  const target = startOfDayLocal(day);
  const end = addDaysLocal(target, 1);
  const result: EventInstance[] = [];
  for (const event of events) {
    if (!event.allDay) continue;
    const start = eventStartDate(event);
    // All-day ends are "YYYY-MM-DD" calendar dates (exclusive). Parsing them
    // with `new Date()` would read them as UTC midnight and shift the event by
    // the local UTC offset, making it spill into the following day west of
    // UTC and lose a day east of it. `eventEndDateExclusive` uses
    // `parseDateOnly` (local midnight) instead.
    const exclusiveEnd = eventEndDateExclusive(event);
    const effectiveEnd =
      exclusiveEnd.getTime() > start.getTime() ? exclusiveEnd : addDaysLocal(start, 1);
    if (effectiveEnd > target && start < end) result.push(event);
  }
  return result.sort((a, b) => {
    const aStart = eventStartDate(a);
    const bStart = eventStartDate(b);
    if (aStart.getTime() !== bStart.getTime()) return aStart.getTime() - bStart.getTime();
    return a.title.localeCompare(b.title);
  });
}

/** Minimum visual height (minutes) for very short events. */
export function displayStartMin(segment: DaySegment): number {
  return Math.max(0, Math.min(1440, segment.startMin));
}

export function displayEndMin(segment: DaySegment): number {
  return Math.max(displayStartMin(segment), Math.min(1440, Math.max(segment.endMin, segment.startMin + 1)));
}
