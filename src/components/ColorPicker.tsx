import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Checkmark16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { accentContrastColor, normalizeHexColor } from "@/lib/theme";
import { Popover } from "./Popover";
import styles from "./ColorPicker.module.css";

/** Fluent-inspired swatch palette (also used for calendars and lists). */
export const COLOR_PRESETS: string[] = [
  "#0f6cbd",
  "#0078d4",
  "#2b88d8",
  "#00b7c3",
  "#038387",
  "#498205",
  "#107c10",
  "#8764b8",
  "#5c2e91",
  "#c239b3",
  "#e3008c",
  "#c50f1f",
  "#d13438",
  "#da3b01",
  "#ca5010",
  "#986f0b",
  "#8e562e",
  "#69797e",
  "#4f6bed",
  "#323130",
];

export interface ColorPickerProps {
  value: string | null;
  onChange: (value: string | null) => void;
  label?: ReactNode;
  presets?: string[];
  /** Show a "follow system / no color" swatch that emits null. */
  allowNull?: boolean;
  disabled?: boolean;
  className?: string;
  id?: string;
  ariaLabel?: string;
}

export function ColorPicker({
  value,
  onChange,
  label,
  presets = COLOR_PRESETS,
  allowNull = false,
  disabled = false,
  className,
  id: idProp,
  ariaLabel,
}: ColorPickerProps) {
  const t = useT();
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const gridRef = useRef<HTMLDivElement | null>(null);
  const [open, setOpen] = useState(false);
  const wasOpen = useRef(false);
  const normalized = normalizeHexColor(value);

  // The popover is portalled, so keyboard users lose their place unless focus
  // is handed to the first swatch and given back to the trigger on close.
  useEffect(() => {
    if (open) {
      const frame = window.requestAnimationFrame(() => {
        gridRef.current?.querySelector<HTMLButtonElement>("button")?.focus();
      });
      wasOpen.current = true;
      return () => window.cancelAnimationFrame(frame);
    }
    if (wasOpen.current) {
      wasOpen.current = false;
      triggerRef.current?.focus();
    }
    return undefined;
  }, [open]);

  const choose = (color: string | null) => {
    onChange(color);
    setOpen(false);
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
        className={cn(styles.trigger, disabled && styles.disabled)}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={ariaLabel ?? (label ? undefined : t("common.chooseColor"))}
        disabled={disabled}
        onClick={() => setOpen((value) => !value)}
      >
        <span
          className={cn(styles.swatch, !normalized && styles.swatchEmpty)}
          style={normalized ? { background: normalized } : undefined}
          aria-hidden="true"
        />
        <span className={cn(styles.value, !normalized && styles.placeholder)}>
          {normalized ?? t("common.noColor")}
        </span>
      </button>

      <Popover
        open={open}
        onClose={() => setOpen(false)}
        anchor={triggerRef.current}
        placement="bottom-start"
        role="dialog"
        ariaLabel={t("common.chooseColor")}
        className={styles.picker}
      >
        <div className={styles.grid} ref={gridRef}>
          {allowNull ? (
            <button
              type="button"
              className={cn(
                styles.option,
                styles.nullOption,
                !normalized && styles.selected,
              )}
              aria-label={t("common.noColor")}
              aria-pressed={!normalized}
              onClick={() => choose(null)}
            >
              {!normalized ? (
                <span className={styles.check} aria-hidden="true">
                  <Checkmark16Regular />
                </span>
              ) : null}
            </button>
          ) : null}
          {presets.map((color) => {
            const selected = normalized === color.toLowerCase();
            const checkColor = accentContrastColor(color);
            return (
              <button
                key={color}
                type="button"
                className={cn(styles.option, selected && styles.selected)}
                style={{ background: color }}
                aria-label={color}
                aria-pressed={selected}
                onClick={() => choose(color)}
              >
                {selected ? (
                  <span
                    className={styles.check}
                    style={{ color: checkColor === "#000000" ? "#000000" : "#ffffff" }}
                    aria-hidden="true"
                  >
                    <Checkmark16Regular />
                  </span>
                ) : null}
              </button>
            );
          })}
        </div>
      </Popover>
    </div>
  );
}
