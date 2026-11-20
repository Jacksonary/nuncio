import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import {
  CalendarLtr16Regular,
  ChevronDown16Regular,
  ChevronLeft16Regular,
  ChevronRight16Regular,
  Clock16Regular,
} from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { getLocale } from "@/lib/i18n";
import {
  addDays,
  formatMonthYear,
  formatTimeOfDay,
  formatWeekday,
  isSameDay,
  parseIsoDate,
  parseLooseTime,
  parseTimeOfDay,
  startOfWeek,
  toIsoDate,
} from "@/lib/format";
import { Button } from "./Button";
import { IconButton } from "./IconButton";
import { Popover } from "./Popover";
import styles from "./DateTime.module.css";

function firstOfMonth(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), 1, 12, 0, 0, 0);
}

function formatFullDate(date: Date): string {
  return new Intl.DateTimeFormat(getLocale(), {
    year: "numeric",
    month: "short",
    day: "numeric",
  }).format(date);
}

function inRange(value: string, min?: string, max?: string): boolean {
  if (min && value < min) return false;
  if (max && value > max) return false;
  return true;
}

export interface DatePickerProps {
  value: string | null;
  onChange: (value: string | null) => void;
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  placeholder?: string;
  disabled?: boolean;
  clearable?: boolean;
  min?: string;
  max?: string;
  weekStartsOn?: number;
  className?: string;
  id?: string;
  ariaLabel?: string;
}

/** One-month calendar grid in a popover; keyboard navigable. */
export function DatePicker({
  value,
  onChange,
  label,
  hint,
  error,
  placeholder,
  disabled = false,
  clearable = false,
  min,
  max,
  weekStartsOn = 1,
  className,
  id: idProp,
  ariaLabel,
}: DatePickerProps) {
  const t = useT();
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const gridRef = useRef<HTMLDivElement | null>(null);
  const [open, setOpen] = useState(false);
  const [viewDate, setViewDate] = useState(() => firstOfMonth(parseIsoDate(value) ?? new Date()));
  const [focusIso, setFocusIso] = useState<string | null>(null);
  const wasOpen = useRef(false);

  const selectedDate = parseIsoDate(value);
  const today = new Date();

  useEffect(() => {
    if (open && focusIso) {
      window.requestAnimationFrame(() => {
        const element = gridRef.current?.querySelector<HTMLElement>(
          `[data-date="${focusIso}"]`,
        );
        element?.focus();
      });
    }
  }, [open, focusIso]);

  useEffect(() => {
    if (!open && wasOpen.current) triggerRef.current?.focus();
    wasOpen.current = open;
  }, [open]);

  const openPicker = () => {
    const base = selectedDate ?? today;
    setViewDate(firstOfMonth(base));
    setFocusIso(toIsoDate(base));
    setOpen(true);
  };

  const close = () => setOpen(false);

  const select = (date: Date) => {
    onChange(toIsoDate(date));
    close();
  };

  const moveFocus = (date: Date) => {
    setFocusIso(toIsoDate(date));
    if (date.getMonth() !== viewDate.getMonth() || date.getFullYear() !== viewDate.getFullYear()) {
      setViewDate(firstOfMonth(date));
    }
  };

  const handleGridKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const base = parseIsoDate(focusIso) ?? selectedDate ?? today;
    switch (event.key) {
      case "ArrowLeft":
        event.preventDefault();
        moveFocus(addDays(base, -1));
        break;
      case "ArrowRight":
        event.preventDefault();
        moveFocus(addDays(base, 1));
        break;
      case "ArrowUp":
        event.preventDefault();
        moveFocus(addDays(base, -7));
        break;
      case "ArrowDown":
        event.preventDefault();
        moveFocus(addDays(base, 7));
        break;
      case "Home":
        event.preventDefault();
        moveFocus(startOfWeek(base, weekStartsOn));
        break;
      case "End":
        event.preventDefault();
        moveFocus(addDays(startOfWeek(base, weekStartsOn), 6));
        break;
      case "PageUp":
        event.preventDefault();
        moveFocus(new Date(base.getFullYear(), base.getMonth() - 1, base.getDate(), 12));
        break;
      case "PageDown":
        event.preventDefault();
        moveFocus(new Date(base.getFullYear(), base.getMonth() + 1, base.getDate(), 12));
        break;
      case "Escape":
        event.preventDefault();
        event.stopPropagation();
        close();
        break;
      case "Tab":
        close();
        break;
      default:
        break;
    }
  };

  const gridStart = startOfWeek(firstOfMonth(viewDate), weekStartsOn);
  const days = Array.from({ length: 42 }, (_, index) => addDays(gridStart, index));
  const weekdays = Array.from({ length: 7 }, (_, index) => addDays(gridStart, index));

  return (
    <div className={cn(styles.field, className)}>
      {label ? (
        <label htmlFor={id} className={styles.label}>
          {label}
        </label>
      ) : null}
      <button
        ref={triggerRef}
        id={id}
        type="button"
        className={cn(
          styles.trigger,
          error && styles.invalid,
          disabled && styles.disabled,
        )}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => (open ? close() : openPicker())}
      >
        <span className={styles.icon} aria-hidden="true">
          <CalendarLtr16Regular />
        </span>
        <span className={cn(styles.value, !selectedDate && styles.placeholder)}>
          {selectedDate ? formatFullDate(selectedDate) : (placeholder ?? t("common.selectDate"))}
        </span>
        <span className={styles.icon} aria-hidden="true">
          <ChevronDown16Regular />
        </span>
      </button>

      <Popover
        open={open}
        onClose={close}
        anchor={triggerRef.current}
        placement="bottom-start"
        role="dialog"
        ariaLabel={t("common.selectDate")}
        className={styles.calendar}
        closeOnEscape={false}
      >
        <div>
          <div className={styles.calHeader}>
            <IconButton
              icon={<ChevronLeft16Regular />}
              label={t("common.previousMonth")}
              size="small"
              onClick={() =>
                setViewDate(
                  new Date(viewDate.getFullYear(), viewDate.getMonth() - 1, 1, 12),
                )
              }
            />
            <div className={styles.calTitle} aria-live="polite">
              {formatMonthYear(viewDate)}
            </div>
            <IconButton
              icon={<ChevronRight16Regular />}
              label={t("common.nextMonth")}
              size="small"
              onClick={() =>
                setViewDate(new Date(viewDate.getFullYear(), viewDate.getMonth() + 1, 1, 12))
              }
            />
          </div>
          <div className={styles.grid} role="grid">
            {weekdays.map((day) => (
              <div key={toIsoDate(day)} className={styles.weekday} role="columnheader">
                {formatWeekday(day)}
              </div>
            ))}
          </div>
          <div
            className={styles.grid}
            role="grid"
            ref={gridRef}
            onKeyDown={handleGridKeyDown}
          >
            {days.map((day) => {
              const iso = toIsoDate(day);
              const isOutside = day.getMonth() !== viewDate.getMonth();
              const isSelected = Boolean(value && iso === value);
              const isToday = isSameDay(day, today);
              const dayDisabled = !inRange(iso, min, max);
              return (
                <button
                  key={iso}
                  type="button"
                  role="gridcell"
                  data-date={iso}
                  aria-selected={isSelected}
                  aria-current={isToday ? "date" : undefined}
                  tabIndex={focusIso === iso ? 0 : -1}
                  disabled={dayDisabled}
                  className={cn(
                    styles.day,
                    isOutside && styles.outside,
                    isToday && styles.today,
                    isSelected && styles.selected,
                    dayDisabled && styles.dayDisabled,
                  )}
                  onClick={() => {
                    setFocusIso(iso);
                    select(day);
                  }}
                >
                  {day.getDate()}
                </button>
              );
            })}
          </div>
          <div className={styles.footer}>
            <Button
              size="small"
              appearance="transparent"
              onClick={() => {
                setFocusIso(toIsoDate(today));
                select(today);
              }}
            >
              {t("common.today")}
            </Button>
            {clearable && value ? (
              <Button
                size="small"
                appearance="transparent"
                onClick={() => {
                  onChange(null);
                  close();
                }}
              >
                {t("common.clear")}
              </Button>
            ) : null}
          </div>
        </div>
      </Popover>

      {error || hint ? (
        <div className={cn(styles.message, error && styles.error)} role={error ? "alert" : undefined}>
          {error || hint}
        </div>
      ) : null}
    </div>
  );
}

export interface TimePickerProps {
  value: string | null;
  onChange: (value: string | null) => void;
  /** Minute increment; default 15. */
  step?: number;
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  placeholder?: string;
  disabled?: boolean;
  clearable?: boolean;
  className?: string;
  id?: string;
  ariaLabel?: string;
}

/** Scrollable 15-minute time list in a popover. */
export function TimePicker({
  value,
  onChange,
  step = 15,
  label,
  hint,
  error,
  placeholder,
  disabled = false,
  clearable = false,
  className,
  id: idProp,
  ariaLabel,
}: TimePickerProps) {
  const t = useT();
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const manualRef = useRef<HTMLInputElement | null>(null);
  const optionRefs = useRef(new Map<number, HTMLButtonElement | null>());
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  // Typed value: the list offers 15-minute stops, but a reminder can be any
  // minute, so the field has to accept free text as well.
  const [draft, setDraft] = useState("");
  const wasOpen = useRef(false);

  const options = useMemo(() => {
    const increment = Math.max(1, Math.min(60, Math.round(step)));
    const result: number[] = [];
    for (let minutes = 0; minutes < 24 * 60; minutes += increment) result.push(minutes);
    return result;
  }, [step]);

  const currentMinutes = value ? parseTimeOfDay(value) : null;
  const selectedIndex =
    currentMinutes === null
      ? -1
      : options.findIndex((minutes) => minutes === currentMinutes);

  useEffect(() => {
    if (!open) return;
    const index = selectedIndex >= 0 ? selectedIndex : 0;
    setActiveIndex(index);
    setDraft(value ?? "");
    window.requestAnimationFrame(() => {
      optionRefs.current.get(index)?.scrollIntoView({ block: "center" });
      manualRef.current?.focus();
      manualRef.current?.select();
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open && wasOpen.current) triggerRef.current?.focus();
    wasOpen.current = open;
  }, [open]);

  const close = () => setOpen(false);

  const choose = (minutes: number) => {
    onChange(formatTimeOfDay(minutes));
    close();
  };

  const commitDraft = () => {
    const parsed = parseLooseTime(draft);
    if (!parsed) return;
    onChange(parsed);
    close();
  };

  const move = (delta: number) => {
    if (options.length === 0) return;
    const next = (activeIndex + delta + options.length) % options.length;
    setActiveIndex(next);
    optionRefs.current.get(next)?.focus();
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        move(1);
        break;
      case "ArrowUp":
        event.preventDefault();
        move(-1);
        break;
      case "Home":
        event.preventDefault();
        setActiveIndex(0);
        optionRefs.current.get(0)?.focus();
        break;
      case "End": {
        event.preventDefault();
        const last = options.length - 1;
        setActiveIndex(last);
        optionRefs.current.get(last)?.focus();
        break;
      }
      case "Enter":
        event.preventDefault();
        if (options[activeIndex] !== undefined) choose(options[activeIndex]);
        break;
      case "Escape":
        event.preventDefault();
        event.stopPropagation();
        close();
        break;
      case "Tab":
        close();
        break;
      default:
        break;
    }
  };

  return (
    <div className={cn(styles.field, className)}>
      {label ? (
        <label htmlFor={id} className={styles.label}>
          {label}
        </label>
      ) : null}
      <button
        ref={triggerRef}
        id={id}
        type="button"
        className={cn(
          styles.trigger,
          error && styles.invalid,
          disabled && styles.disabled,
        )}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => setOpen((value) => !value)}
      >
        <span className={styles.icon} aria-hidden="true">
          <Clock16Regular />
        </span>
        <span className={cn(styles.value, !value && styles.placeholder)}>
          {value ?? placeholder ?? t("common.selectTime")}
        </span>
        <span className={styles.icon} aria-hidden="true">
          <ChevronDown16Regular />
        </span>
      </button>

      <Popover
        open={open}
        onClose={close}
        anchor={triggerRef.current}
        placement="bottom-start"
        minWidth={148}
        closeOnEscape={false}
      >
        <div className={styles.timeManual}>
          <input
            ref={manualRef}
            className={styles.timeInput}
            value={draft}
            placeholder="HH:MM"
            inputMode="numeric"
            aria-label={t("common.enterTime")}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                commitDraft();
              } else if (event.key === "Escape") {
                event.preventDefault();
                event.stopPropagation();
                close();
              } else if (event.key === "ArrowDown") {
                event.preventDefault();
                const index = selectedIndex >= 0 ? selectedIndex : 0;
                setActiveIndex(index);
                optionRefs.current.get(index)?.focus();
              }
            }}
          />
        </div>
        <div
          ref={listRef}
          className={styles.timeList}
          role="listbox"
          aria-label={ariaLabel ?? t("common.selectTime")}
          onKeyDown={handleKeyDown}
        >
          {options.map((minutes, index) => (
            <button
              key={minutes}
              ref={(element) => {
                optionRefs.current.set(index, element);
              }}
              type="button"
              role="option"
              aria-selected={currentMinutes === minutes}
              tabIndex={activeIndex === index ? 0 : -1}
              className={cn(
                styles.timeOption,
                activeIndex === index && styles.timeActive,
              )}
              onMouseEnter={() => setActiveIndex(index)}
              onClick={() => choose(minutes)}
            >
              {formatTimeOfDay(minutes)}
            </button>
          ))}
        </div>
        {clearable && value ? (
          <div className={styles.footer}>
            <Button
              size="small"
              appearance="transparent"
              onClick={() => {
                onChange(null);
                close();
              }}
            >
              {t("common.clear")}
            </Button>
          </div>
        ) : null}
      </Popover>

      {error || hint ? (
        <div className={cn(styles.message, error && styles.error)} role={error ? "alert" : undefined}>
          {error || hint}
        </div>
      ) : null}
    </div>
  );
}
