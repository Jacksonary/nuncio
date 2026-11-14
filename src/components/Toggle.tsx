import {
  forwardRef,
  useEffect,
  useId,
  useRef,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import styles from "./Toggle.module.css";

export interface CheckboxProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "size"> {
  label?: ReactNode;
  description?: ReactNode;
  indeterminate?: boolean;
}

export const Checkbox = forwardRef<HTMLInputElement, CheckboxProps>(function Checkbox(
  { label, description, indeterminate = false, className, disabled, ...rest },
  ref,
) {
  const innerRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    if (innerRef.current) innerRef.current.indeterminate = indeterminate;
  }, [indeterminate]);
  return (
    <label className={cn(styles.row, disabled && styles.rowDisabled, className)}>
      <input
        ref={(element) => {
          innerRef.current = element;
          if (typeof ref === "function") ref(element);
          else if (ref) ref.current = element;
        }}
        type="checkbox"
        className={styles.native}
        disabled={disabled}
        {...rest}
      />
      <span className={styles.box} aria-hidden="true" />
      {label || description ? (
        <span className={styles.body}>
          {label ? <span className={styles.label}>{label}</span> : null}
          {description ? <span className={styles.description}>{description}</span> : null}
        </span>
      ) : null}
    </label>
  );
});

Checkbox.displayName = "Checkbox";

export interface RadioProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "size"> {
  label?: ReactNode;
  description?: ReactNode;
}

export const Radio = forwardRef<HTMLInputElement, RadioProps>(function Radio(
  { label, description, className, disabled, ...rest },
  ref,
) {
  return (
    <label className={cn(styles.row, disabled && styles.rowDisabled, className)}>
      <input
        ref={ref}
        type="radio"
        className={styles.native}
        disabled={disabled}
        {...rest}
      />
      <span className={cn(styles.box, styles.radioBox)} aria-hidden="true" />
      {label || description ? (
        <span className={styles.body}>
          {label ? <span className={styles.label}>{label}</span> : null}
          {description ? <span className={styles.description}>{description}</span> : null}
        </span>
      ) : null}
    </label>
  );
});

Radio.displayName = "Radio";

export interface RadioOption {
  value: string;
  label: ReactNode;
  description?: ReactNode;
  disabled?: boolean;
}

export interface RadioGroupProps {
  label?: ReactNode;
  value: string | null;
  onChange: (value: string) => void;
  options: RadioOption[];
  name?: string;
  orientation?: "vertical" | "horizontal";
  className?: string;
}

export function RadioGroup({
  label,
  value,
  onChange,
  options,
  name,
  orientation = "vertical",
  className,
}: RadioGroupProps) {
  const generatedName = useId();
  const groupName = name ?? generatedName;
  const labelId = `${groupName}-label`;
  return (
    <fieldset
      className={cn(
        styles.group,
        orientation === "horizontal" && styles.groupHorizontal,
        className,
      )}
      role="radiogroup"
      aria-labelledby={label ? labelId : undefined}
    >
      {label ? (
        <legend id={labelId} className={styles.groupLabel}>
          {label}
        </legend>
      ) : null}
      {options.map((option) => (
        <Radio
          key={option.value}
          name={groupName}
          value={option.value}
          label={option.label}
          description={option.description}
          disabled={option.disabled}
          checked={option.value === value}
          onChange={() => onChange(option.value)}
        />
      ))}
    </fieldset>
  );
}

export interface SwitchProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "size" | "onChange"> {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: ReactNode;
}

export const Switch = forwardRef<HTMLInputElement, SwitchProps>(function Switch(
  { checked, onChange, label, className, disabled, ...rest },
  ref,
) {
  return (
    <label className={cn(styles.switchRow, disabled && styles.rowDisabled, className)}>
      <input
        ref={ref}
        type="checkbox"
        role="switch"
        className={styles.switchNative}
        checked={checked}
        disabled={disabled}
        onChange={(event) => onChange(event.target.checked)}
        {...rest}
      />
      <span className={styles.track} aria-hidden="true" />
      {label ? <span className={styles.switchLabel}>{label}</span> : null}
    </label>
  );
});

Switch.displayName = "Switch";
