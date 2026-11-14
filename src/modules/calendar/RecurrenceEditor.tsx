import type { Weekday } from "@/ipc/types";
import { DatePicker, Input, RadioGroup, Select, ToggleButton } from "@/components";
import { useLanguage, useT } from "@/lib/i18n";
import { formatDateOnly } from "./dates";
import {
  WEEKDAYS,
  describeRepeat,
  ruleFromRepeat,
  weekdayLabel,
  weekdayOf,
  type RepeatEndMode,
  type RepeatMode,
  type RepeatState,
} from "./recurrence";
import styles from "./RecurrenceEditor.module.css";

export interface RecurrenceEditorProps {
  value: RepeatState;
  onChange: (value: RepeatState) => void;
  /** Local start date/time of the event; anchors weekday/month-day choices. */
  start: Date;
}

const MODES: RepeatMode[] = [
  "none",
  "daily",
  "weekdays",
  "weekly",
  "monthly",
  "monthlyNth",
  "yearly",
];

const MODE_KEYS: Record<RepeatMode, string> = {
  none: "calendar.repeat.none",
  daily: "calendar.repeat.daily",
  weekdays: "calendar.repeat.weekdays",
  weekly: "calendar.repeat.weekly",
  monthly: "calendar.repeat.monthly",
  monthlyNth: "calendar.repeat.monthlyNth",
  yearly: "calendar.repeat.yearly",
};

const INTERVAL_KEYS: Record<RepeatMode, string> = {
  none: "",
  daily: "calendar.editor.everyDays",
  weekdays: "",
  weekly: "calendar.editor.everyWeeks",
  monthly: "calendar.editor.everyMonths",
  monthlyNth: "calendar.editor.everyMonths",
  yearly: "calendar.editor.everyYears",
};

export function RecurrenceEditor({ value, onChange, start }: RecurrenceEditorProps) {
  const t = useT();
  const locale = useLanguage();
  const weekStart = weekdayOf(start);
  const monthName = new Intl.DateTimeFormat(locale, { month: "long" }).format(start);

  const patch = (next: Partial<RepeatState>) => onChange({ ...value, ...next });

  const modeOptions = MODES.map((mode) => ({ value: mode, label: t(MODE_KEYS[mode]) }));
  const nthOptions = Array.from({ length: 5 }, (_, index) => ({
    value: String(index + 1),
    label: t("calendar.editor.nthOption", { nth: index + 1 }),
  }));

  const intervalLabelKey = INTERVAL_KEYS[value.mode];
  const showInterval = Boolean(intervalLabelKey);
  const showWeekdayPicker = value.mode === "weekly";
  const showNth = value.mode === "monthlyNth";

  const endOptions: { value: RepeatEndMode; label: string }[] = [
    { value: "never", label: t("calendar.editor.endNever") },
    {
      value: "after",
      label: t("calendar.editor.endAfter", { count: Math.max(1, value.count) }),
    },
    { value: "until", label: t("calendar.editor.endUntil") },
  ];

  const toggleWeekday = (weekday: Weekday) => {
    const has = value.byWeekday.includes(weekday);
    const next = has
      ? value.byWeekday.filter((entry) => entry !== weekday)
      : [...value.byWeekday, weekday];
    patch({ byWeekday: next });
  };

  return (
    <div className={styles.editor}>
      <div className={styles.row}>
        <Select
          label={t("calendar.editor.repeat")}
          value={value.mode}
          onChange={(mode) => {
            const typed = mode as RepeatMode;
            if (typed === "weekly" && value.byWeekday.length === 0) {
              patch({ mode: typed, byWeekday: [weekStart] });
            } else {
              patch({ mode: typed });
            }
          }}
          options={modeOptions}
          className={styles.modeSelect}
        />
        {showInterval ? (
          <Input
            type="number"
            min={1}
            max={99}
            value={String(value.interval)}
            label={t(intervalLabelKey, { n: value.interval })}
            onChange={(event) => {
              const parsed = Number.parseInt(event.target.value, 10);
              patch({ interval: Number.isFinite(parsed) ? Math.max(1, Math.min(99, parsed)) : 1 });
            }}
            className={styles.interval}
          />
        ) : null}
      </div>

      {showWeekdayPicker ? (
        <div className={styles.weekdays}>
          <span className={styles.legend}>{t("calendar.editor.onDays")}</span>
          <div className={styles.weekdayRow} role="group" aria-label={t("calendar.editor.onDays")}>
            {WEEKDAYS.map((weekday) => (
              <ToggleButton
                key={weekday}
                pressed={value.byWeekday.includes(weekday)}
                onPressedChange={() => toggleWeekday(weekday)}
                size="small"
                aria-label={weekdayLabel(weekday, locale)}
                className={styles.weekday}
              >
                {weekdayLabel(weekday, locale)}
              </ToggleButton>
            ))}
          </div>
        </div>
      ) : null}

      {showNth ? (
        <div className={styles.row}>
          <Select
            label={t("calendar.editor.monthlyLabel")}
            value={String(value.nth)}
            onChange={(nth) => patch({ nth: Number.parseInt(nth, 10) || 1 })}
            options={nthOptions}
            className={styles.modeSelect}
          />
          <div className={styles.staticField}>
            <span className={styles.legend}>{t("calendar.editor.onDays")}</span>
            <span className={styles.staticValue}>{weekdayLabel(weekStart, locale)}</span>
          </div>
        </div>
      ) : null}

      {value.mode === "monthly" ? (
        <p className={styles.caption}>
          {t("calendar.editor.monthlyOn", { day: start.getDate() })}
        </p>
      ) : null}

      {value.mode === "yearly" ? (
        <p className={styles.caption}>
          {t("calendar.repeat.summary.yearly", { month: monthName, day: start.getDate() })}
        </p>
      ) : null}

      {value.mode !== "none" ? (
        <div className={styles.endSection}>
          <RadioGroup
            label={t("calendar.editor.ends")}
            value={value.endMode}
            onChange={(mode) => {
              const endMode = mode as RepeatEndMode;
              if (endMode === "until" && !value.until) {
                patch({ endMode, until: formatDateOnly(start) });
              } else {
                patch({ endMode });
              }
            }}
            options={endOptions}
            orientation="vertical"
            name="calendar-repeat-end"
          />
          {value.endMode === "after" ? (
            <Input
              type="number"
              min={1}
              max={999}
              value={String(value.count)}
              label={t("calendar.editor.occurrences")}
              onChange={(event) => {
                const parsed = Number.parseInt(event.target.value, 10);
                patch({ count: Number.isFinite(parsed) ? Math.max(1, Math.min(999, parsed)) : 1 });
              }}
              className={styles.countInput}
            />
          ) : null}
          {value.endMode === "until" ? (
            <DatePicker
              label={t("calendar.editor.endUntil")}
              value={value.until}
              onChange={(next) => patch({ until: next })}
              clearable={false}
              min={formatDateOnly(start)}
            />
          ) : null}
        </div>
      ) : null}

      {value.mode !== "none" ? (
        <p className={styles.summary}>
          {describeRepeat(value, start, { t, locale }, ruleFromRepeat(value, start))}
        </p>
      ) : null}
    </div>
  );
}
