/**
 * Calendar write helpers shared by the main view, the editor and the peek.
 * They keep the query cache coherent after each mutation and centralize the
 * recurring-event targeting rules.
 */
import type {
  CalendarEvent,
  EventInput,
  RecurrenceEditScope,
} from "@/ipc/types";
import { events } from "@/ipc/api";
import { queryClient } from "@/lib/queryClient";
import { queryKeyPrefixes, queryKeys } from "@/lib/queryKeys";

export function invalidateCalendarData(): void {
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.events });
  void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.event });
}

export function isRecurringEvent(event: CalendarEvent): boolean {
  return event.recurrence !== null || event.seriesId !== null;
}

/** Master event id and the occurrence to target for "this" / "following". */
export function recurrenceTarget(
  event: CalendarEvent,
  instanceStart: string | null,
): { eventId: string; instanceStart: string } {
  return {
    eventId: event.seriesId ?? event.id,
    instanceStart: event.recurrenceId ?? instanceStart ?? event.start,
  };
}

export async function updateEventSeries(
  event: CalendarEvent,
  input: EventInput,
  scope: RecurrenceEditScope,
  instanceStart: string | null,
): Promise<CalendarEvent> {
  const target = recurrenceTarget(event, instanceStart);
  const result = await events.update(
    target.eventId,
    input,
    scope,
    scope === "all" ? null : target.instanceStart,
  );
  invalidateCalendarData();
  void queryClient.invalidateQueries({
    queryKey: queryKeys.event(target.eventId, target.instanceStart),
  });
  return result;
}

export async function deleteEventSeries(
  event: CalendarEvent,
  scope: RecurrenceEditScope,
  instanceStart: string | null,
  notifyAttendees: boolean,
): Promise<void> {
  const target = recurrenceTarget(event, instanceStart);
  await events.delete(
    target.eventId,
    scope,
    scope === "all" ? null : target.instanceStart,
    notifyAttendees,
  );
  invalidateCalendarData();
}
