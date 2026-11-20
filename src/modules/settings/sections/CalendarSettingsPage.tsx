/** Calendar settings; embedded by the Calendar, contacts & tasks page. */
import { Checkbox, Select, Switch, TimePicker } from "@/components";
import { useT } from "@/lib/i18n";
import type { CalendarView } from "@/ipc/types";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

const VIEWS: CalendarView[] = ["day", "workWeek", "week", "month", "year", "agenda"];
const REMINDER_MINUTES = [0, 5, 10, 15, 30, 60, 120, 1440];
const DURATIONS = [15, 30, 45, 60, 90, 120, 180, 240];

export function CalendarSettingsContent() {
  const t = useT();
  const { settings, update } = useSettings();

  const toggleWorkDay = (day: number, checked: boolean) => {
    const next = checked
      ? Array.from(new Set([...settings.calendar.workDays, day])).sort((a, b) => a - b)
      : settings.calendar.workDays.filter((item) => item !== day);
    update({ calendar: { workDays: next } });
  };

  return (
    <>
      <Section title={t("settings.calendar.views")}>
        <Card>
          <SettingRow label={t("settings.calendar.defaultView")} description={t("settings.calendar.defaultViewDesc")}>
            <Select
              ariaLabel={t("settings.calendar.defaultView")}
              value={settings.calendar.defaultView}
              onChange={(value) =>
                update({ calendar: { defaultView: value as CalendarView } })
              }
              options={VIEWS.map((view) => ({
                value: view,
                label: t(`settings.calendar.view.${view}`),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.calendar.firstDay")} description={t("settings.calendar.firstDayDesc")}>
            <Select
              ariaLabel={t("settings.calendar.firstDay")}
              value={String(settings.calendar.firstDayOfWeek)}
              onChange={(value) => update({ calendar: { firstDayOfWeek: Number(value) } })}
              options={Array.from({ length: 7 }, (_, index) => ({
                value: String(index),
                label: t(`settings.weekday.${index}`),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.calendar.weekNumbers")}>
            <Switch
              checked={settings.calendar.showWeekNumbers}
              onChange={(checked) => update({ calendar: { showWeekNumbers: checked } })}
              aria-label={t("settings.calendar.weekNumbersToggle")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.calendar.workHours")} hint={t("settings.calendar.workHoursHint")}>
        <Card>
          <StackRow>
            <div className={styles.rowLabel}>{t("settings.calendar.workDays")}</div>
            <div className={styles.chipRow}>
              {[1, 2, 3, 4, 5, 6, 0].map((day) => (
                <Checkbox
                  key={day}
                  label={t(`settings.weekday.${day}`)}
                  checked={settings.calendar.workDays.includes(day)}
                  onChange={(event) => toggleWorkDay(day, event.target.checked)}
                />
              ))}
            </div>
          </StackRow>
          <StackRow>
            <div className={styles.fieldGrid}>
              <TimePicker
                label={t("settings.calendar.workStart")}
                value={settings.calendar.workHoursStart}
                onChange={(value) =>
                  update({ calendar: { workHoursStart: value ?? settings.calendar.workHoursStart } })
                }
              />
              <TimePicker
                label={t("settings.calendar.workEnd")}
                value={settings.calendar.workHoursEnd}
                onChange={(value) =>
                  update({ calendar: { workHoursEnd: value ?? settings.calendar.workHoursEnd } })
                }
              />
            </div>
          </StackRow>
        </Card>
      </Section>

      <Section title={t("settings.calendar.defaults")}>
        <Card>
          <SettingRow label={t("settings.calendar.defaultReminder")} description={t("settings.calendar.defaultReminderDesc")}>
            <Select
              ariaLabel={t("settings.calendar.defaultReminder")}
              value={String(settings.calendar.defaultReminderMinutes)}
              onChange={(value) =>
                update({ calendar: { defaultReminderMinutes: Number(value) } })
              }
              options={REMINDER_MINUTES.map((minutes) => ({
                value: String(minutes),
                label:
                  minutes === 0
                    ? t("settings.calendar.noReminder")
                    : t("settings.calendar.minutesBefore", { count: minutes }),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.calendar.defaultDuration")}>
            <Select
              ariaLabel={t("settings.calendar.defaultDuration")}
              value={String(settings.calendar.defaultEventDurationMinutes)}
              onChange={(value) =>
                update({ calendar: { defaultEventDurationMinutes: Number(value) } })
              }
              options={DURATIONS.map((minutes) => ({
                value: String(minutes),
                label: t("settings.calendar.minutes", { count: minutes }),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.calendar.poll")} description={t("settings.calendar.pollDesc")}>
            <Select
              ariaLabel={t("settings.calendar.poll")}
              value={String(settings.calendar.pollMinutes)}
              onChange={(value) => update({ calendar: { pollMinutes: Number(value) } })}
              options={[5, 10, 15, 30, 60, 120].map((minutes) => ({
                value: String(minutes),
                label: t("settings.calendar.minutes", { count: minutes }),
              }))}
            />
          </SettingRow>
        </Card>
      </Section>
    </>
  );
}
