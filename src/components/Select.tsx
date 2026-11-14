import {
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { Checkmark16Regular, ChevronDown16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Popover } from "./Popover";
import styles from "./Select.module.css";

export interface SelectOption {
  value: string;
  label: ReactNode;
  disabled?: boolean;
  icon?: ReactNode;
}

export interface SelectProps {
  value: string | null;
  onChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  disabled?: boolean;
  size?: "small" | "medium";
  className?: string;
  id?: string;
  ariaLabel?: string;
}

/** Accessible custom select (button + listbox popover), themed by tokens. */
export function Select({
  value,
  onChange,
  options,
  placeholder,
  label,
  hint,
  error,
  disabled = false,
  size = "medium",
  className,
  id: idProp,
  ariaLabel,
}: SelectProps) {
  const t = useT();
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const listId = `${id}-listbox`;
  const messageId = `${id}-message`;
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const optionRefs = useRef(new Map<number, HTMLButtonElement | null>());
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const wasOpen = useRef(false);

  useEffect(() => {
    if (!open && wasOpen.current) triggerRef.current?.focus();
    wasOpen.current = open;
  }, [open]);

  const selectedIndex = options.findIndex((option) => option.value === value);
  const selected = selectedIndex >= 0 ? options[selectedIndex] : null;

  const enabledIndexes = options
    .map((option, index) => (option.disabled ? -1 : index))
    .filter((index) => index >= 0);

  const focusOption = (index: number) => {
    const button = optionRefs.current.get(index);
    if (button) {
      setActiveIndex(index);
      button.focus();
    }
  };

  useEffect(() => {
    if (!open) return;
    const initial =
      enabledIndexes.indexOf(selectedIndex) >= 0 ? selectedIndex : enabledIndexes[0];
    if (initial !== undefined) {
      window.requestAnimationFrame(() => focusOption(initial));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const close = (restoreFocus = true) => {
    setOpen(false);
    if (restoreFocus) {
      window.requestAnimationFrame(() => triggerRef.current?.focus());
    }
  };

  const choose = (option: SelectOption) => {
    if (option.disabled) return;
    onChange(option.value);
    close();
  };

  const moveActive = (delta: number) => {
    if (enabledIndexes.length === 0) return;
    const current = enabledIndexes.indexOf(activeIndex);
    const next =
      current === -1
        ? delta > 0
          ? 0
          : enabledIndexes.length - 1
        : (current + delta + enabledIndexes.length) % enabledIndexes.length;
    focusOption(enabledIndexes[next]);
  };

  const handleListKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        moveActive(1);
        break;
      case "ArrowUp":
        event.preventDefault();
        moveActive(-1);
        break;
      case "Home":
        event.preventDefault();
        if (enabledIndexes.length) focusOption(enabledIndexes[0]);
        break;
      case "End":
        event.preventDefault();
        if (enabledIndexes.length) focusOption(enabledIndexes[enabledIndexes.length - 1]);
        break;
      case "Enter":
        event.preventDefault();
        if (activeIndex >= 0) choose(options[activeIndex]);
        break;
      case "Escape":
      case "Tab":
        event.preventDefault();
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
          size === "small" && styles.small,
          error && styles.invalid,
          disabled && styles.disabled,
        )}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-label={ariaLabel}
        aria-describedby={error || hint ? messageId : undefined}
        disabled={disabled}
        onClick={() => setOpen((value) => !value)}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            setOpen(true);
          }
        }}
      >
        {selected?.icon ? (
          <span className={styles.optionIcon} aria-hidden="true">
            {selected.icon}
          </span>
        ) : null}
        <span className={cn(styles.value, !selected && styles.placeholder)}>
          {selected ? selected.label : (placeholder ?? t("common.select"))}
        </span>
        <span className={styles.chevron} aria-hidden="true">
          <ChevronDown16Regular />
        </span>
      </button>

      <Popover
        open={open}
        onClose={() => close()}
        anchor={triggerRef.current}
        placement="bottom-start"
        matchAnchorWidth
        minWidth={180}
        role="listbox"
        id={listId}
        ariaLabel={ariaLabel ?? t("common.select")}
        className={styles.list}
        closeOnEscape={false}
      >
        <div onKeyDown={handleListKeyDown} role="presentation">
          {options.map((option, index) => (
            <button
              key={option.value}
              ref={(element) => {
                optionRefs.current.set(index, element);
              }}
              type="button"
              role="option"
              aria-selected={option.value === value}
              disabled={option.disabled}
              tabIndex={activeIndex === index ? 0 : -1}
              className={cn(
                styles.option,
                activeIndex === index && styles.optionActive,
              )}
              onMouseEnter={() => setActiveIndex(index)}
              onClick={() => choose(option)}
            >
              {option.icon ? (
                <span className={styles.optionIcon} aria-hidden="true">
                  {option.icon}
                </span>
              ) : null}
              <span className={styles.value}>{option.label}</span>
              {option.value === value ? (
                <span className={styles.check} aria-hidden="true">
                  <Checkmark16Regular />
                </span>
              ) : null}
            </button>
          ))}
        </div>
      </Popover>

      {error || hint ? (
        <div
          id={messageId}
          className={cn(styles.message, error && styles.error)}
          role={error ? "alert" : undefined}
        >
          {error || hint}
        </div>
      ) : null}
    </div>
  );
}
