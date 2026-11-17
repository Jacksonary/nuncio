import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import {
  CalendarLtr24Regular,
  ChevronLeft20Regular,
  ChevronRight20Regular,
  Clock16Regular,
} from "@fluentui/react-icons";
import type { Calendar, CalendarEvent, CalendarView, EventInput, EventInstance, RecurrenceEditScope } from "@/ipc/types";
import {
  Button,
  EmptyState,
  IconButton,
  Popover,
  SearchBox,
  Spinner,
  Tabs,
  confirm,
  toast,
} from "@/components";
import { events, isIpcError, on, sync } from "@/ipc/api";
import { queryClient } from "@/lib/queryClient";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { parseTimeOfDay } from "@/lib/format";
import { useAppSettings } from "@/lib/useSettings";
import { useShellStore } from "@/lib/store";
import {
  DEFAULT_CALENDAR_SETTINGS,
  addMinutes,
  combineDateAndMinutes,
  formatDateOnly,
  formatRangeTitle,
  parseDateOnly,
  rangeForView,
  shiftView,
  startOfDayLocal,
  type CalendarSettings,
} from "./dates";
import {
  buildEventInput,
  eventStartDate,
  formatEventChipTime,
  instanceSelectionKey,
  rangeToInputInZone,
} from "./model";
import {
  deleteEventSeries,
  isRecurringEvent,
  updateEventSeries,
} from "./actions";
import { TimeGridView } from "./views/TimeGridView";import { MonthView } from "./views/MonthView";
import { YearView } from "./views/YearView";
import { AgendaView } from "./views/AgendaView";
import { EventPeek } from "./EventPeek";
import { EventEditor } from "./EventEditor";
import { QuickCreate } from "./QuickCreate";
import { ScopeDialog } from "./ScopeDialog";
import { useCalendarUiStore, type NewEventSeed } from "./uiStore";
import { calendars as calendarsApi } from "@/ipc/api";
import styles from "./Main.module.css";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

interface PeekState {
  eventId: string;
  instanceStart: string | null;
  anchor: HTMLElement | null;
  point: { x: number; y: number } | null;
}

type Pending =
  | {
      kind: "update";
      event: CalendarEvent;
      input: EventInput;
      instanceStart: string | null;
    }
  | {
      kind: "delete";
      event: CalendarEvent;
      instanceStart: string | null;
    };

interface QuickState {
  seed: NewEventSeed;
  point: { x: number; y: number } | null;
}

export function CalendarMain() {
  const t = useT();
  const locale = useLanguage();
  const settingsApp = useAppSettings();
  const settings: CalendarSettings = { ...DEFAULT_CALENDAR_SETTINGS, ...settingsApp.calendar };
  const selection = useShellStore((state) => state.selection.calendar);
  const setSelection = useShellStore((state) => state.setSelection);
  const editor = useCalendarUiStore((state) => state.editor);
  const closeEditor = useCalendarUiStore((state) => state.closeEditor);
  const deleteRequest = useCalendarUiStore((state) => state.deleteRequest);
  const openEditRequest = useCalendarUiStore((state) => state.openEdit);

  const rootRef = useRef<HTMLDivElement | null>(null);
  const searchRef = useRef<HTMLDivElement | null>(null);
  const lastDeleteRequest = useRef(0);

  const [peek, setPeek] = useState<PeekState | null>(null);
  const [quick, setQuick] = useState<QuickState | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [searchText, setSearchText] = useState("");
  const [debounced, setDebounced] = useState("");

  const calendarsQuery = useQuery({
    queryKey: queryKeys.calendars(),
    queryFn: calendarsApi.list,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
  const calendars: Calendar[] = calendarsQuery.data ?? [];

  const today = startOfDayLocal(new Date());
  const anchor = parseDateOnly(selection.date) ?? today;
  const view: CalendarView = selection.view ?? settings.defaultView;
  const range = rangeForView(view, anchor, settings);

  const visibleIds = useMemo(
    () => calendars.filter((calendar) => calendar.isVisible).map((calendar) => calendar.id).sort(),
    [calendars],
  );
  const rangeCalendarIds = useMemo<string[] | null>(() => {
    if (calendars.length === 0) return null;
    return visibleIds.length === calendars.length ? null : visibleIds;
  }, [calendars.length, visibleIds]);

  const fromIso = range.from.toISOString();
  const toIso = range.to.toISOString();
  const eventsQuery = useQuery({
    queryKey: queryKeys.events(fromIso, toIso, rangeCalendarIds),
    queryFn: () => events.range(fromIso, toIso, rangeCalendarIds),
    enabled: calendarsQuery.isSuccess && visibleIds.length > 0,
    staleTime: 20_000,
    refetchInterval: settings.pollMinutes > 0 ? settings.pollMinutes * 60_000 : false,
  });
  const eventList = eventsQuery.data ?? [];

  useEffect(() => {
    if (!isTauri()) return;
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    on("calendar://changed", () => {
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.events });
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.event });
    })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch((error: unknown) => {
        console.warn("[calendar] failed to listen for calendar://changed", error);
      });
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    const pollMs = Math.max(1, settings.pollMinutes) * 60_000;

    // PLAN §6.2: opening a module syncs it immediately. Only hit the backend
    // when the last calendar sync is older than the configured poll interval
    // (or never ran), and stay silent on failure — sync status surfaces it.
    const syncOnOpen = async () => {
      let stale = true;
      try {
        const statuses = await sync.status();
        const calendarStatuses = statuses.filter((status) => status.module === "calendar");
        if (calendarStatuses.some((status) => status.state === "syncing")) return;
        let latest = 0;
        for (const status of calendarStatuses) {
          if (!status.lastSyncAt) continue;
          const parsed = Date.parse(status.lastSyncAt);
          if (!Number.isNaN(parsed)) latest = Math.max(latest, parsed);
        }
        stale = latest === 0 || Date.now() - latest >= pollMs;
      } catch {
        // Status may be unavailable while the backend starts; sync anyway.
      }
      if (cancelled || !stale) return;
      await sync.now(null, "calendar");
    };

    void syncOnOpen().catch((error: unknown) => {
      console.debug("[calendar] sync on open failed", error);
    });
    return () => {
      cancelled = true;
    };
    // Only on module mount; the shell remounts Main when the module changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => setDebounced(searchText.trim()), 250);
    return () => window.clearTimeout(timer);
  }, [searchText]);

  const searchQuery = useQuery({
    queryKey: ["events", "search", debounced] as const,
    queryFn: () => events.search(debounced, 20),
    enabled: debounced.length >= 2,
    staleTime: 30_000,
  });
  const searchOpen = debounced.length >= 2;

  const selectedKey =
    selection.eventId && selection.eventId.length > 0
      ? instanceSelectionKey(selection.eventId, selection.instanceStart)
      : null;

  const navigate = (date: Date, nextView?: CalendarView) => {
    setPeek(null);
    setSelection("calendar", { date: formatDateOnly(date), view: nextView ?? view });
  };

  const goToday = () => {
    const now = startOfDayLocal(new Date());
    setPeek(null);
    setSelection("calendar", { date: formatDateOnly(now) });
  };

  const goPrevious = () => {
    setPeek(null);
    setSelection("calendar", { date: formatDateOnly(shiftView(view, anchor, -1, settings)) });
  };

  const goNext = () => {
    setPeek(null);
    setSelection("calendar", { date: formatDateOnly(shiftView(view, anchor, 1, settings)) });
  };

  const changeView = (nextView: CalendarView) => {
    setPeek(null);
    setSelection("calendar", { view: nextView });
  };

  const openPeek = (instance: EventInstance, anchorElement: HTMLElement | null) => {
    setPeek({
      eventId: instance.eventId,
      instanceStart: instance.start,
      anchor: anchorElement,
      point: null,
    });
    setSelection("calendar", {
      eventId: instance.eventId,
      instanceStart: instance.start,
    });
  };

  const openEditorFor = (instance: EventInstance) => {
    setPeek(null);
    openEditRequest(instance.eventId, instance.start);
  };

  // Open the peek when the selection is set from outside (reminders, deep links).
  useEffect(() => {
    const eventId = selection.eventId;
    if (!eventId) return;
    setPeek((current) => {
      if (current && current.eventId === eventId && current.instanceStart === selection.instanceStart) {
        return current;
      }
      const rect = rootRef.current?.getBoundingClientRect();
      const point = rect
        ? { x: rect.left + rect.width * 0.55, y: rect.top + Math.min(140, rect.height / 2) }
        : null;
      return { eventId, instanceStart: selection.instanceStart, anchor: null, point };
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selection.eventId, selection.instanceStart]);

  const openQuickCreate = (seed: NewEventSeed, point: { x: number; y: number } | null) => {
    setQuick({ seed, point });
  };

  const quickCreateDefault = (day: Date, point: { x: number; y: number } | null) => {
    const startMinutes = parseTimeOfDay(settings.workHoursStart) ?? 9 * 60;
    const start = combineDateAndMinutes(day, startMinutes);
    const end = addMinutes(start, Math.max(15, settings.defaultEventDurationMinutes));
    openQuickCreate({ start, end, allDay: false, calendarId: null }, point);
  };

  const handleMove = async (instance: EventInstance, newStart: Date, newEnd: Date) => {
    try {
      const full = await events.get(instance.eventId, instance.start);
      const input = buildEventInput(full);
      const nextRange = rangeToInputInZone(newStart, newEnd, full.allDay, full.timeZone);
      input.start = nextRange.start;
      input.end = nextRange.end;
      if (isRecurringEvent(full)) {
        setPending({ kind: "update", event: full, input, instanceStart: instance.start });
        return;
      }
      await updateEventSeries(full, input, "all", instance.start);
      toast.success(t("calendar.toast.updated"), full.title || t("calendar.untitled"));
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    }
  };

  const requestDelete = async (event: CalendarEvent, instanceStart: string | null) => {
    const title = event.title || t("calendar.untitled");
    if (isRecurringEvent(event)) {
      setPending({ kind: "delete", event, instanceStart });
      return;
    }
    const ok = await confirm({
      title: t("calendar.confirm.deleteTitle"),
      message: t("calendar.confirm.deleteMessage", { title }),
      confirmLabel: t("calendar.confirm.delete"),
      cancelLabel: t("common.cancel"),
      danger: true,
    });
    if (!ok) return;
    try {
      await deleteEventSeries(event, "all", instanceStart, event.attendees.length > 0);
      toast.success(t("calendar.toast.deleted"));
      setPeek(null);
      closeEditor();
      setSelection("calendar", { eventId: null, instanceStart: null });
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    }
  };

  useEffect(() => {
    if (deleteRequest === lastDeleteRequest.current) return;
    lastDeleteRequest.current = deleteRequest;
    const eventId = selection.eventId;
    if (!eventId) return;
    void (async () => {
      try {
        const full = await events.get(eventId, selection.instanceStart);
        await requestDelete(full, selection.instanceStart);
      } catch (error) {
        toast.error(t("calendar.error.action"), errorMessage(error));
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deleteRequest]);

  const resolvePending = async (scope: RecurrenceEditScope) => {
    if (!pending) return;
    try {
      if (pending.kind === "update") {
        await updateEventSeries(
          pending.event,
          pending.input,
          scope,
          pending.instanceStart,
        );
        toast.success(t("calendar.toast.updated"), pending.event.title || t("calendar.untitled"));
      } else {
        await deleteEventSeries(
          pending.event,
          scope,
          pending.instanceStart,
          pending.event.attendees.length > 0,
        );
        toast.success(t("calendar.toast.deleted"));
        setPeek(null);
        closeEditor();
        setSelection("calendar", { eventId: null, instanceStart: null });
      }
    } catch (error) {
      toast.error(t("calendar.error.action"), errorMessage(error));
    } finally {
      setPending(null);
    }
  };

  const pickSearchResult = (instance: EventInstance) => {
    const day = startOfDayLocal(eventStartDate(instance));
    const nextView = view === "year" || view === "agenda" ? "month" : view;
    setSearchText("");
    setDebounced("");
    setSelection("calendar", {
      date: formatDateOnly(day),
      view: nextView,
      eventId: instance.eventId,
      instanceStart: instance.start,
    });
  };

  const rangeTitle = formatRangeTitle(view, anchor, range, locale);

  const viewItems = useMemo(
    () => [
      { id: "day", label: t("calendar.view.day") },
      { id: "workWeek", label: t("calendar.view.workWeek") },
      { id: "week", label: t("calendar.view.week") },
      { id: "month", label: t("calendar.view.month") },
      { id: "year", label: t("calendar.view.year") },
      { id: "agenda", label: t("calendar.view.agenda") },
    ],
    [t],
  );

  const noCalendars = calendarsQuery.isSuccess && calendars.length === 0;
  const noVisibleCalendars = calendars.length > 0 && visibleIds.length === 0;

  const renderView = () => {
    if (noCalendars) {
      return (
        <EmptyState
          icon={<CalendarLtr24Regular />}
          title={t("calendar.sidebar.noCalendars")}
        />
      );
    }
    if (noVisibleCalendars) {
      return (
        <EmptyState
          icon={<CalendarLtr24Regular />}
          title={t("calendar.noVisibleCalendars")}
          description={t("calendar.sidebar.calendars")}
        />
      );
    }
    if (eventsQuery.isError) {
      return (
        <EmptyState
          icon={<CalendarLtr24Regular />}
          title={t("calendar.error.load")}
          description={eventsQuery.error ? errorMessage(eventsQuery.error) : undefined}
          actions={
            <Button size="small" onClick={() => void eventsQuery.refetch()}>
              {t("calendar.error.retry")}
            </Button>
          }
        />
      );
    }
    if (view === "month") {
      return (
        <MonthView
          month={anchor}
          days={range.days}
          events={eventList}
          showWeekNumbers={settings.showWeekNumbers}
          selectedKey={selectedKey}
          onSelectEvent={openPeek}
          onOpenEvent={openEditorFor}
          onDayClick={(day) => navigate(day, "day")}
          onDayDoubleClick={quickCreateDefault}
          onMoveEvent={handleMove}
        />
      );
    }
    if (view === "year") {
      return (
        <YearView
          year={anchor.getFullYear()}
          events={eventList}
          weekStartsOn={settings.firstDayOfWeek}
          onNavigate={(date, nextView) => navigate(date, nextView)}
        />
      );
    }
    if (view === "agenda") {
      return (
        <AgendaView
          anchor={anchor}
          events={eventList}
          selectedKey={selectedKey}
          onSelectEvent={openPeek}
          onOpenEvent={openEditorFor}
        />
      );
    }
    return (
      <TimeGridView
        view={view}
        days={range.days}
        events={eventList}
        settings={settings}
        selectedKey={selectedKey}
        onSelectEvent={openPeek}
        onOpenEvent={openEditorFor}
        onMoveEvent={handleMove}
        onQuickCreate={openQuickCreate}
      />
    );
  };

  return (
    <div className={styles.root} ref={rootRef}>
      <div className={styles.toolbar}>
        <Button size="small" appearance="subtle" onClick={goToday}>
          {t("calendar.today")}
        </Button>
        <span className={styles.navGroup}>
          <IconButton
            size="small"
            icon={<ChevronLeft20Regular />}
            label={t("calendar.previous")}
            onClick={goPrevious}
          />
          <IconButton
            size="small"
            icon={<ChevronRight20Regular />}
            label={t("calendar.next")}
            onClick={goNext}
          />
        </span>
        <h1 className={styles.title}>{rangeTitle}</h1>
        {eventsQuery.isFetching ? <Spinner size="tiny" /> : null}
        <span className={styles.spacer} />
        <div className={styles.searchWrap} ref={searchRef}>
          <SearchBox
            value={searchText}
            onChange={setSearchText}
            placeholder={t("calendar.search.placeholder")}
            onSearch={(value) => setDebounced(value.trim())}
            aria-label={t("calendar.search.placeholder")}
          />
        </div>
        <Tabs
          items={viewItems}
          value={view}
          onChange={(id) => changeView(id as CalendarView)}
          appearance="pill"
          ariaLabel={t("calendar.view.label")}
        />
      </div>

      <div className={styles.content}>
        {eventsQuery.isLoading && calendars.length > 0 && visibleIds.length > 0 ? (
          <div className={styles.loading}>
            <Spinner label={t("common.loading")} />
          </div>
        ) : (
          renderView()
        )}
      </div>

      {peek ? (
        <EventPeek
          eventId={peek.eventId}
          instanceStart={peek.instanceStart}
          anchor={peek.anchor}
          point={peek.point}
          calendars={calendars}
          onClose={() => setPeek(null)}
          onEdit={(event) => {
            const recurring = event.recurrence !== null || event.seriesId !== null;
            const instance = recurring
              ? (peek?.instanceStart ?? event.recurrenceId ?? null)
              : null;
            setPeek(null);
            openEditRequest(event.seriesId ?? event.id, instance);
          }}
          onDelete={(event) => void requestDelete(event, peek?.instanceStart ?? event.recurrenceId ?? null)}
        />
      ) : null}

      {quick ? (
        <QuickCreate
          seed={quick.seed}
          point={quick.point}
          calendars={calendars}
          defaultReminderMinutes={settings.defaultReminderMinutes}
          onClose={() => setQuick(null)}
          onOpenEditor={(seed) => {
            setQuick(null);
            useCalendarUiStore.getState().openCreate(seed);
          }}
        />
      ) : null}

      {editor ? (
        <EventEditor
          request={editor}
          calendars={calendars}
          settings={settings}
          instanceStart={editor.mode === "edit" ? editor.instanceStart : null}
          onClose={closeEditor}
          onSaved={(event) => {
            setPeek(null);
            void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.events });
            void queryClient.invalidateQueries({ queryKey: queryKeys.event(event.id, null) });
          }}
          onDeleted={() => {
            setPeek(null);
            closeEditor();
            setSelection("calendar", { eventId: null, instanceStart: null });
          }}
        />
      ) : null}

      <ScopeDialog
        open={pending !== null}
        mode={pending?.kind === "delete" ? "delete" : "edit"}
        onCancel={() => setPending(null)}
        onChoose={(scope) => void resolvePending(scope)}
      />

      {searchOpen ? (
        <Popover
          open
          onClose={() => setDebounced("")}
          anchor={searchRef.current}
          placement="bottom-end"
          role="dialog"
          ariaLabel={t("calendar.search.results")}
          minWidth={340}
          maxHeight={420}
          matchAnchorWidth={false}
          className={styles.searchPopover}
        >
          <div className={styles.searchHeader}>{t("calendar.search.results")}</div>
          {searchQuery.isLoading ? (
            <div className={styles.searchEmpty}>
              <Spinner size="small" label={t("common.loading")} />
            </div>
          ) : (searchQuery.data ?? []).length === 0 ? (
            <div className={styles.searchEmpty}>{t("calendar.search.noResults")}</div>
          ) : (
            <div className={styles.searchResults}>
              {(searchQuery.data ?? []).map((instance) => (
                <button
                  key={instance.instanceKey}
                  type="button"
                  className={styles.searchItem}
                  onClick={() => pickSearchResult(instance)}
                >
                  <span className={styles.searchColor} style={{ background: instance.color }} />
                  <span className={styles.searchBody}>
                    <span className={styles.searchTitle}>
                      {instance.title || t("calendar.untitled")}
                    </span>
                    <span className={styles.searchMeta}>
                      <Clock16Regular aria-hidden="true" />
                      {new Intl.DateTimeFormat(locale, {
                        month: "short",
                        day: "numeric",
                      }).format(eventStartDate(instance))}
                      {" · "}
                      {formatEventChipTime(instance, t("calendar.allDay"), locale)}
                      {instance.location ? ` · ${instance.location}` : ""}
                    </span>
                  </span>
                </button>
              ))}
            </div>
          )}
        </Popover>
      ) : null}
    </div>
  );
}

