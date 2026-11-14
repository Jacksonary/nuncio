/**
 * Calendar module entry point: Windows 11 Calendar-style mini month + calendar
 * list sidebar, hand-written day / work-week / week / month / year / agenda
 * views, event peek, full editor, quick create and recurring-event scopes.
 */
import { CalendarLtr24Regular } from "@fluentui/react-icons";
import type { ModuleDefinition } from "@/app/moduleTypes";
import type { AppSettings, Calendar, CalendarView } from "@/ipc/types";
import type { HotkeyDefinition } from "@/lib/hotkeys";
import { queryClient } from "@/lib/queryClient";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { DEFAULT_SETTINGS } from "@/lib/useSettings";
import { CalendarSidebar } from "./Sidebar";
import { CalendarMain } from "./Main";
import {
  DEFAULT_CALENDAR_SETTINGS,
  addMinutes,
  formatDateOnly,
  parseDateOnly,
  shiftView,
  startOfDayLocal,
  type CalendarSettings,
} from "./dates";
import { nextHalfHour } from "./model";
import { useCalendarUiStore } from "./uiStore";

function readCalendarSettings(): CalendarSettings {
  const settings =
    queryClient.getQueryData<AppSettings>(queryKeys.settings()) ?? DEFAULT_SETTINGS;
  return { ...DEFAULT_CALENDAR_SETTINGS, ...settings.calendar };
}

function defaultCalendarId(): string | null {
  const list = queryClient.getQueryData<Calendar[]>(queryKeys.calendars()) ?? [];
  const writable = list.filter((calendar) => !calendar.isReadOnly);
  return (
    writable.find((calendar) => calendar.isVisible)?.id ?? writable[0]?.id ?? null
  );
}

/** New event at the next half hour with the configured default duration. */
function startNewEvent(): void {
  const settings = readCalendarSettings();
  const start = nextHalfHour(new Date());
  const duration = Math.max(15, settings.defaultEventDurationMinutes);
  useCalendarUiStore.getState().openCreate({
    start,
    end: addMinutes(start, duration),
    allDay: false,
    calendarId: defaultCalendarId(),
  });
}

function currentAnchor(): Date {
  const selection = useShellStore.getState().selection.calendar;
  return parseDateOnly(selection.date) ?? startOfDayLocal(new Date());
}

function currentView(): CalendarView {
  const selection = useShellStore.getState().selection.calendar;
  return selection.view ?? readCalendarSettings().defaultView;
}

function setView(view: CalendarView): void {
  useShellStore.getState().setSelection("calendar", { view });
}

function goToday(): void {
  useShellStore.getState().setSelection("calendar", {
    date: formatDateOnly(startOfDayLocal(new Date())),
  });
}

function stepPeriod(direction: 1 | -1): void {
  const settings = readCalendarSettings();
  const next = shiftView(currentView(), currentAnchor(), direction, settings);
  useShellStore.getState().setSelection("calendar", { date: formatDateOnly(next) });
}

function deleteSelected(): void {
  if (!useShellStore.getState().selection.calendar.eventId) return;
  useCalendarUiStore.getState().requestDeleteSelected();
}

const hotkeys: HotkeyDefinition[] = [
  { id: "calendar.view.day", keys: "Mod+Shift+D", handler: () => setView("day") },
  { id: "calendar.view.workWeek", keys: "Mod+Shift+E", handler: () => setView("workWeek") },
  { id: "calendar.view.week", keys: "Mod+Shift+W", handler: () => setView("week") },
  { id: "calendar.view.month", keys: "Mod+Shift+M", handler: () => setView("month") },
  { id: "calendar.view.year", keys: "Mod+Shift+Y", handler: () => setView("year") },
  { id: "calendar.view.agenda", keys: "Mod+Shift+A", handler: () => setView("agenda") },
  { id: "calendar.today", keys: "T", handler: goToday },
  {
    id: "calendar.previous",
    keys: "ArrowLeft",
    handler: () => stepPeriod(-1),
    preventDefault: true,
  },
  {
    id: "calendar.next",
    keys: "ArrowRight",
    handler: () => stepPeriod(1),
    preventDefault: true,
  },
  { id: "calendar.newEvent", keys: "Mod+N", handler: startNewEvent, preventDefault: true },
  {
    id: "calendar.deleteEvent",
    keys: "Delete",
    handler: deleteSelected,
    preventDefault: true,
  },
];

export const calendarModule: ModuleDefinition = {
  id: "calendar",
  labelKey: "shell.calendar",
  Icon: CalendarLtr24Regular,
  Sidebar: CalendarSidebar,
  Main: CalendarMain,
  onNew: startNewEvent,
  hotkeys,
};

export default calendarModule;
