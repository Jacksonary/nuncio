import {
  forwardRef,
  useId,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import styles from "./Slider.module.css";

export interface SliderProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "value" | "onChange" | "size"> {
  value: number;
  min?: number;
  max?: number;
  step?: number;
  onChange: (value: number) => void;
  label?: ReactNode;
  showValue?: boolean;
  formatValue?: (value: number) => string;
}

export const Slider = forwardRef<HTMLInputElement, SliderProps>(function Slider(
  {
    value,
    min = 0,
    max = 100,
    step = 1,
    onChange,
    label,
    showValue = true,
    formatValue,
    className,
    id: idProp,
    ...rest
  },
  ref,
) {
  const generatedId = useId();
  const id = idProp ?? generatedId;
  return (
    <div className={cn(styles.field, className)}>
      {label || showValue ? (
        <div className={styles.header}>
          {label ? (
            <label htmlFor={id} className={styles.label}>
              {label}
            </label>
          ) : (
            <span />
          )}
          {showValue ? (
            <span className={styles.value} aria-live="polite">
              {formatValue ? formatValue(value) : value}
            </span>
          ) : null}
        </div>
      ) : null}
      <input
        ref={ref}
        id={id}
        type="range"
        className={styles.range}
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(event) => onChange(Number(event.target.value))}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={value}
        {...rest}
      />
    </div>
  );
});

Slider.displayName = "Slider";
