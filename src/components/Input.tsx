import {
  forwardRef,
  useId,
  type InputHTMLAttributes,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";
import { Dismiss16Regular, Search16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { IconButton } from "./IconButton";
import { Kbd } from "./Kbd";
import styles from "./Input.module.css";

export type FieldSize = "small" | "medium" | "large";

export interface InputProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "size"> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  icon?: ReactNode;
  suffix?: ReactNode;
  size?: FieldSize;
  containerClassName?: string;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  {
    label,
    hint,
    error,
    icon,
    suffix,
    size = "medium",
    containerClassName,
    className,
    id: idProp,
    disabled,
    required,
    ...rest
  },
  ref,
) {
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const messageId = `${id}-message`;
  return (
    <div className={cn(styles.field, containerClassName)}>
      {label ? (
        <label htmlFor={id} className={styles.label}>
          {label}
          {required ? (
            <span className={styles.required} aria-hidden="true">
              *
            </span>
          ) : null}
        </label>
      ) : null}
      <div
        className={cn(
          styles.inputWrap,
          size === "small" ? styles.small : size === "large" ? styles.large : undefined,
          error ? styles.invalid : undefined,
          disabled ? styles.disabled : undefined,
        )}
      >
        {icon ? (
          <span className={styles.icon} aria-hidden="true">
            {icon}
          </span>
        ) : null}
        <input
          ref={ref}
          id={id}
          className={cn(styles.input, className)}
          disabled={disabled}
          required={required}
          aria-invalid={error ? true : undefined}
          aria-describedby={error || hint ? messageId : undefined}
          {...rest}
        />
        {suffix ? <span className={styles.suffix}>{suffix}</span> : null}
      </div>
      {error || hint ? (
        <div
          id={messageId}
          className={cn(styles.message, error ? styles.error : undefined)}
          role={error ? "alert" : undefined}
        >
          {error || hint}
        </div>
      ) : null}
    </div>
  );
});

Input.displayName = "Input";

export interface SearchBoxProps
  extends Omit<InputProps, "icon" | "onChange" | "value" | "type" | "suffix"> {
  value: string;
  onChange: (value: string) => void;
  onSearch?: (value: string) => void;
  /** Shortcut hint shown inside the box, e.g. "Mod+E". */
  shortcut?: string;
}

export const SearchBox = forwardRef<HTMLInputElement, SearchBoxProps>(
  function SearchBox(
    {
      value,
      onChange,
      onSearch,
      shortcut,
      placeholder,
      label,
      onKeyDown,
      ...rest
    },
    ref,
  ) {
    const t = useT();
    return (
      <Input
        ref={ref}
        type="search"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder ?? t("common.search")}
        label={label}
        icon={<Search16Regular />}
        suffix={
          <>
            {shortcut ? (
              <span className={styles.searchShortcut} aria-hidden="true">
                <Kbd>{shortcut}</Kbd>
              </span>
            ) : null}
            {value ? (
              <IconButton
                icon={<Dismiss16Regular />}
                label={t("common.clear")}
                size="small"
                onClick={() => onChange("")}
              />
            ) : null}
          </>
        }
        {...rest}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            onSearch?.(value);
          }
          onKeyDown?.(event);
        }}
      />
    );
  },
);

SearchBox.displayName = "SearchBox";

export interface TextAreaProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  counter?: boolean;
  containerClassName?: string;
  noResize?: boolean;
}

export const TextArea = forwardRef<HTMLTextAreaElement, TextAreaProps>(function TextArea(
  {
    label,
    hint,
    error,
    counter = false,
    containerClassName,
    className,
    id: idProp,
    disabled,
    required,
    maxLength,
    value,
    noResize = false,
    ...rest
  },
  ref,
) {
  const generatedId = useId();
  const id = idProp ?? generatedId;
  const messageId = `${id}-message`;
  const length = typeof value === "string" ? value.length : 0;
  return (
    <div className={cn(styles.field, containerClassName)}>
      {label ? (
        <label htmlFor={id} className={styles.label}>
          {label}
          {required ? (
            <span className={styles.required} aria-hidden="true">
              *
            </span>
          ) : null}
        </label>
      ) : null}
      <div
        className={cn(
          styles.inputWrap,
          styles.textareaWrap,
          error ? styles.invalid : undefined,
          disabled ? styles.disabled : undefined,
        )}
      >
        <textarea
          ref={ref}
          id={id}
          className={cn(styles.textarea, noResize && styles.noResize, className)}
          disabled={disabled}
          required={required}
          maxLength={maxLength}
          value={value}
          aria-invalid={error ? true : undefined}
          aria-describedby={error || hint ? messageId : undefined}
          {...rest}
        />
      </div>
      {counter && maxLength ? (
        <div className={styles.counter}>
          <span>
            {length}/{maxLength}
          </span>
        </div>
      ) : null}
      {error || hint ? (
        <div
          id={messageId}
          className={cn(styles.message, error ? styles.error : undefined)}
          role={error ? "alert" : undefined}
        >
          {error || hint}
        </div>
      ) : null}
    </div>
  );
});

TextArea.displayName = "TextArea";
