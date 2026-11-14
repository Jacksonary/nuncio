import { Fragment, useCallback, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { isTauri } from "@tauri-apps/api/core";
import {
  Add20Regular,
  ArrowDownload20Regular,
  CalendarLtr20Regular,
  ChevronLeft16Regular,
  ChevronRight16Regular,
  Color20Regular,
  Edit20Regular,
  Delete20Regular,
  Globe16Regular,
  LockClosed16Regular,
} from "@fluentui/react-icons";
import type { Calendar } from "@/ipc/types";
import type { MenuEntry } from "@/components";
import {
  Button,
  Checkbox,
  ContextMenu,
  IconButton,
  Menu,
  confirm,
  toast,
} from "@/components";
import { accounts as accountsApi, calendars as calendarsApi, events, isIpcError } from "@/ipc/api";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { useLanguage, useT } from "@/lib/i18n";
import { useAppSettings } from "@/lib/useSettings";
import { useShellStore } from "@/lib/store";
import { cn } from "@/lib/cn";
import {
  addDaysLocal,
  addMonthsLocal,
  formatDateOnly,
  formatMonthLong,
  isSameDayLocal,
  isSameMonth,
  isoWeekNumber,
  monthGridDays,
  parseDateOnly,
  startOfDayLocal,
  startOfMonth,
  weekdayHeaders,
} from "./dates";
import { eventSpansDay } from "./model";
import {
  ColorCalendarDialog,
  CreateCalendarDialog,
  ImportCalendarDialog,
  RenameCalendarDialog,
  SubscribeCalendarDialog,
} from "./CalendarDialogs";
import styles from "./Sidebar.module.css";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

type DialogState =
  | { kind: "create" }
  | { kind: "subscribe" }
  | { kind: "import" }
  | { kind: "rename"; calendar: Calendar }
  | { kind: "color"; calendar: Calendar }
  | null;

export function CalendarSidebar() {
  const t = useT();
  const locale = useLanguage();
  const client = useQueryClient();
  const settings = useAppSettings();
  const calendarSettings = settings.calendar;
  const selection = useShellStore((state) => state.selection.calendar);
  const setSelection = useShellStore((state) => state.setSelection);

  const accountsQuery = useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
    staleTime: 60_000,
  });
  const calendarsQuery = useQuery({
    queryKey: queryKeys.calendars(),
    queryFn: calendarsApi.list,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });

  const accounts = accountsQuery.data ?? [];
  const calendars = calendarsQuery.data ?? [];
  const today = useMemo(() => startOfDayLocal(new Date()), []);
  const anchor = parseDateOnly(selection.date) ?? today;
  const [miniMonth, setMiniMonth] = useState(() => startOfMonth(anchor));
  const [addMenu, setAddMenu] = useState<{ open: boolean; anchor: HTMLElement | null }>({
    open: false,
    anchor: null,
  });
  const [dialog, setDialog] = useState<DialogState>(null);

  useEffect(() => {
    setMiniMonth(startOfMonth(anchor));
    // Re-sync when the anchor moves outside the visible mini month.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [anchor.getFullYear(), anchor.getMonth()]);

  const gridDays = useMemo(
    () => monthGridDays(miniMonth, calendarSettings.firstDayOfWeek),
    [miniMonth, calendarSettings.firstDayOfWeek],
  );
  const gridFrom = gridDays[0] ?? startOfMonth(miniMonth);
  const gridTo = addDaysLocal(gridDays[gridDays.length - 1] ?? gridFrom, 1);

  const visibleIds = useMemo(
    () => calendars.filter((calendar) => calendar.isVisible).map((calendar) => calendar.id).sort(),
    [calendars],
  );
  const rangeCalendarIds = useMemo<IdList>(() => {
    if (calendars.length === 0) return null;
    return visibleIds.length === calendars.length ? null : visibleIds;
  }, [calendars.length, visibleIds]);

  const miniEventsQuery = useQuery({
    queryKey: queryKeys.events(gridFrom.toISOString(), gridTo.toISOString(), rangeCalendarIds),
    queryFn: () => events.range(gridFrom.toISOString(), gridTo.toISOString(), rangeCalendarIds),
    enabled: calendarsQuery.isSuccess && visibleIds.length > 0,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });

  const dotsByDay = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const event of miniEventsQuery.data ?? []) {
      for (const day of gridDays) {
        if (!eventSpansDay(event, day)) continue;
        const key = formatDateOnly(day);
        const list = map.get(key) ?? [];
        if (!list.includes(event.color) && list.length < 3) list.push(event.color);
        map.set(key, list);
      }
    }
    return map;
  }, [miniEventsQuery.data, gridDays]);

  const setVisible = useMutation({
    mutationFn: ({ id, visible }: { id: string; visible: boolean }) =>
      calendarsApi.setVisible(id, visible),
    onMutate: async ({ id, visible }) => {
      await client.cancelQueries({ queryKey: queryKeys.calendars() });
      const previous = client.getQueryData<Calendar[]>(queryKeys.calendars());
      client.setQueryData<Calendar[]>(queryKeys.calendars(), (current) =>
        current?.map((calendar) =>
          calendar.id === id ? { ...calendar, isVisible: visible } : calendar,
        ),
      );
      return { previous };
    },
    onError: (error, _variables, context) => {
      if (context?.previous) client.setQueryData(queryKeys.calendars(), context.previous);
      toast.error(t("calendar.error.action"), errorMessage(error));
    },
    onSettled: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.events });
    },
  });

  const goToDay = useCallback(
    (day: Date) => {
      let view = selection.view ?? calendarSettings.defaultView;
      if (view === "year" || view === "agenda") view = "month";
      setMiniMonth(startOfMonth(day));
      setSelection("calendar", { date: formatDateOnly(day), view });
    },
    [calendarSettings.defaultView, selection.view, setSelection],
  );

  const weekdays = useMemo(
    () => weekdayHeaders(locale, calendarSettings.firstDayOfWeek),
    [locale, calendarSettings.firstDayOfWeek],
  );

  const groups = useMemo(() => {
    const byAccount = new Map<string, Calendar[]>();
    for (const calendar of calendars) {
      const list = byAccount.get(calendar.accountId) ?? [];
      list.push(calendar);
      byAccount.set(calendar.accountId, list);
    }
    const ordered: { accountId: string; label: string; color: string; calendars: Calendar[] }[] = [];
    const sortedAccounts = [...accounts].sort((a, b) => a.sortOrder - b.sortOrder);
    for (const account of sortedAccounts) {
      const list = byAccount.get(account.id);
      if (!list || list.length === 0) continue;
      ordered.push({
        accountId: account.id,
        label: account.displayName || account.email,
        color: account.color,
        calendars: list,
      });
      byAccount.delete(account.id);
    }
    for (const [accountId, list] of byAccount) {
      ordered.push({ accountId, label: accountId, color: "var(--text-tertiary)", calendars: list });
    }
    return ordered;
  }, [accounts, calendars]);

  const exportCalendar = useCallback(
    async (calendar: Calendar) => {
      if (!isTauri()) {
        toast.error(t("calendar.error.action"), t("calendar.toast.notAvailable"));
        return;
      }
      try {
        const path = await saveFileDialog({
          defaultPath: `${calendar.name}.ics`,
          filters: [{ name: "iCalendar", extensions: ["ics"] }],
        });
        if (!path) return;
        await calendarsApi.exportIcs(calendar.id, path);
        toast.success(t("calendar.toast.exported"), calendar.name);
      } catch (error) {
        toast.error(t("calendar.error.action"), errorMessage(error));
      }
    },
    [t],
  );

  const removeCalendar = useCallback(
    async (calendar: Calendar) => {
      const ok = await confirm({
        title: t("calendar.dialog.delete.title"),
        message: t("calendar.dialog.delete.message", { name: calendar.name }),
        confirmLabel: t("calendar.dialog.delete.confirm"),
        cancelLabel: t("common.cancel"),
        danger: true,
      });
      if (!ok) return;
      try {
        await calendarsApi.delete(calendar.id);
        toast.success(t("calendar.toast.calendarDeleted"), calendar.name);
        void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
        void client.invalidateQueries({ queryKey: queryKeyPrefixes.events });
      } catch (error) {
        toast.error(t("calendar.error.action"), errorMessage(error));
      }
    },
    [client, t],
  );

  const calendarMenu = (calendar: Calendar): MenuEntry[] => [
    {
      id: "rename",
      label: t("calendar.sidebar.renameCalendar"),
      icon: <Edit20Regular />,
      disabled: calendar.isReadOnly,
      onSelect: () => setDialog({ kind: "rename", calendar }),
    },
    {
      id: "color",
      label: t("calendar.sidebar.changeColor"),
      icon: <Color20Regular />,
      disabled: calendar.isReadOnly,
      onSelect: () => setDialog({ kind: "color", calendar }),
    },
    {
      id: "export",
      label: t("calendar.sidebar.exportIcs"),
      icon: <ArrowDownload20Regular />,
      onSelect: () => void exportCalendar(calendar),
    },
    { type: "separator" },
    {
      id: "delete",
      label: t("calendar.sidebar.deleteCalendar"),
      icon: <Delete20Regular />,
      danger: true,
      disabled: calendar.isReadOnly,
      onSelect: () => void removeCalendar(calendar),
    },
  ];

  const addMenuItems: MenuEntry[] = [
    {
      id: "create",
      label: t("calendar.sidebar.createCalendar"),
      icon: <CalendarLtr20Regular />,
      onSelect: () => setDialog({ kind: "create" }),
    },
    {
      id: "subscribe",
      label: t("calendar.sidebar.subscribe"),
      icon: <Globe16Regular />,
      onSelect: () => setDialog({ kind: "subscribe" }),
    },
    {
      id: "import",
      label: t("calendar.sidebar.import"),
      icon: <ArrowDownload20Regular />,
      onSelect: () => setDialog({ kind: "import" }),
    },
  ];

  return (
    <div className={styles.sidebar}>
      <section className={styles.miniSection} aria-label={t("calendar.today")}>
        <div className={styles.miniHeader}>
          <span className={styles.miniTitle}>{formatMonthLong(miniMonth, locale)}</span>
          <span className={styles.miniNav}>
            <IconButton
              size="small"
              icon={<ChevronLeft16Regular />}
              label={t("common.previousMonth")}
              onClick={() => setMiniMonth(addMonthsLocal(miniMonth, -1))}
            />
            <IconButton
              size="small"
              icon={<ChevronRight16Regular />}
              label={t("common.nextMonth")}
              onClick={() => setMiniMonth(addMonthsLocal(miniMonth, 1))}
            />
          </span>
        </div>
        <div
          className={styles.miniGrid}
          style={{
            gridTemplateColumns: calendarSettings.showWeekNumbers
              ? "24px repeat(7, minmax(0, 1fr))"
              : "repeat(7, minmax(0, 1fr))",
          }}
        >
          {calendarSettings.showWeekNumbers ? <span className={styles.miniWeekHeading} /> : null}
          {weekdays.map((label, index) => (
            <span key={`${label}-${index}`} className={styles.miniWeekday}>
              {label}
            </span>
          ))}
          {Array.from({ length: 6 }, (_, row) => {
            const rowDays = gridDays.slice(row * 7, row * 7 + 7);
            const first = rowDays[0];
            return (
              <Fragment key={`week-${row}`}>
                {calendarSettings.showWeekNumbers ? (
                  <span className={styles.miniWeekNumber}>
                    {first ? isoWeekNumber(first) : ""}
                  </span>
                ) : null}
                {rowDays.map((day) => {
                  const key = formatDateOnly(day);
                  const outside = !isSameMonth(day, miniMonth);
                  const isToday = isSameDayLocal(day, today);
                  const selected = isSameDayLocal(day, anchor);
                  const dots = dotsByDay.get(key) ?? [];
                  return (
                    <button
                      key={key}
                      type="button"
                      className={cn(
                        styles.miniDay,
                        outside && styles.miniOutside,
                        isToday && styles.miniToday,
                        selected && styles.miniSelected,
                      )}
                      onClick={() => goToDay(day)}
                      aria-label={new Intl.DateTimeFormat(locale, {
                        month: "long",
                        day: "numeric",
                      }).format(day)}
                      aria-current={isToday ? "date" : undefined}
                    >
                      <span className={styles.miniDayNumber}>{day.getDate()}</span>
                      {dots.length > 0 ? (
                        <span className={styles.miniDots} aria-hidden="true">
                          {dots.map((color) => (
                            <span
                              key={color}
                              className={styles.miniDot}
                              style={{ background: color }}
                            />
                          ))}
                        </span>
                      ) : null}
                    </button>
                  );
                })}
              </Fragment>
            );
          })}
        </div>
      </section>

      <section className={styles.listSection}>
        <h2 className={styles.sectionTitle}>{t("calendar.sidebar.calendars")}</h2>
        <div className={styles.calendarScroll}>
          {calendarsQuery.isLoading ? (
            <p className={styles.placeholder}>{t("common.loading")}</p>
          ) : calendars.length === 0 ? (
            <p className={styles.placeholder}>{t("calendar.sidebar.noCalendars")}</p>
          ) : (
            groups.map((group) => (
              <div key={group.accountId} className={styles.group}>
                <div className={styles.groupTitle}>
                  <span className={styles.groupDot} style={{ background: group.color }} aria-hidden="true" />
                  <span className={styles.groupName}>{group.label}</span>
                </div>
                {group.calendars.map((calendar) => (
                  <ContextMenu key={calendar.id} items={calendarMenu(calendar)}>
                    <div className={styles.calendarRow} data-hidden={!calendar.isVisible || undefined}>
                      <Checkbox
                        checked={calendar.isVisible}
                        onChange={(event) =>
                          setVisible.mutate({ id: calendar.id, visible: event.target.checked })
                        }
                        label={
                          <span className={styles.calendarLabel}>
                            <span
                              className={styles.calendarSwatch}
                              style={{ background: calendar.color }}
                              aria-hidden="true"
                            />
                            <span className={styles.calendarName}>{calendar.name}</span>
                            {calendar.subscriptionUrl ? (
                              <Globe16Regular
                                className={styles.calendarBadge}
                                title={t("calendar.sidebar.subscribed")}
                              />
                            ) : null}
                            {calendar.isReadOnly ? (
                              <LockClosed16Regular
                                className={styles.calendarBadge}
                                title={t("calendar.sidebar.readOnly")}
                              />
                            ) : null}
                          </span>
                        }
                        className={styles.calendarCheckbox}
                        aria-label={t("calendar.sidebar.showCalendar", { name: calendar.name })}
                      />
                    </div>
                  </ContextMenu>
                ))}
              </div>
            ))
          )}
        </div>
        <div className={styles.addRow}>
          <Button
            appearance="transparent"
            size="small"
            icon={<Add20Regular />}
            onClick={(event) => setAddMenu({ open: true, anchor: event.currentTarget })}
            fullWidth
            className={styles.addButton}
          >
            {t("calendar.sidebar.addCalendar")}
          </Button>
          <Menu
            open={addMenu.open}
            onClose={() => setAddMenu({ open: false, anchor: null })}
            anchor={addMenu.anchor}
            items={addMenuItems}
            placement="top-start"
            ariaLabel={t("calendar.sidebar.addCalendar")}
          />
        </div>
      </section>

      <CreateCalendarDialog
        open={dialog?.kind === "create"}
        onClose={() => setDialog(null)}
        accounts={accounts}
      />
      <SubscribeCalendarDialog
        open={dialog?.kind === "subscribe"}
        onClose={() => setDialog(null)}
      />
      <ImportCalendarDialog
        open={dialog?.kind === "import"}
        onClose={() => setDialog(null)}
        calendars={calendars}
      />
      <RenameCalendarDialog
        open={dialog?.kind === "rename"}
        onClose={() => setDialog(null)}
        calendar={dialog?.kind === "rename" ? dialog.calendar : null}
      />
      <ColorCalendarDialog
        open={dialog?.kind === "color"}
        onClose={() => setDialog(null)}
        calendar={dialog?.kind === "color" ? dialog.calendar : null}
      />
    </div>
  );
}

type IdList = string[] | null;
