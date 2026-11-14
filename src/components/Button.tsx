import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./Button.module.css";

export type ButtonAppearance = "primary" | "subtle" | "transparent" | "outline" | "danger";
export type ButtonSize = "small" | "medium" | "large";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  appearance?: ButtonAppearance;
  size?: ButtonSize;
  icon?: ReactNode;
  trailingIcon?: ReactNode;
  loading?: boolean;
  fullWidth?: boolean;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    appearance = "subtle",
    size = "medium",
    icon,
    trailingIcon,
    loading = false,
    fullWidth = false,
    className,
    children,
    disabled,
    type = "button",
    ...rest
  },
  ref,
) {
  const appearanceClass =
    appearance === "primary"
      ? styles.primary
      : appearance === "transparent"
        ? styles.transparent
        : appearance === "outline"
          ? styles.outline
          : appearance === "danger"
            ? styles.danger
            : styles.subtle;
  const sizeClass =
    size === "small" ? styles.small : size === "large" ? styles.large : undefined;
  return (
    <button
      ref={ref}
      type={type}
      className={cn(
        styles.button,
        appearanceClass,
        sizeClass,
        fullWidth && styles.fullWidth,
        className,
      )}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...rest}
    >
      {loading ? <span className={styles.spinner} aria-hidden="true" /> : null}
      {!loading && icon ? (
        <span className={styles.icon} aria-hidden="true">
          {icon}
        </span>
      ) : null}
      {children !== undefined && children !== null ? (
        <span className={styles.label}>{children}</span>
      ) : null}
      {trailingIcon ? (
        <span className={styles.icon} aria-hidden="true">
          {trailingIcon}
        </span>
      ) : null}
    </button>
  );
});

Button.displayName = "Button";

export interface DangerButtonProps extends Omit<ButtonProps, "appearance"> {
  filled?: boolean;
}

/** Convenience wrapper for destructive actions. */
export const DangerButton = forwardRef<HTMLButtonElement, DangerButtonProps>(
  function DangerButton({ filled = false, className, ...rest }, ref) {
    return (
      <Button
        ref={ref}
        appearance={filled ? "danger" : "subtle"}
        className={cn(filled ? styles.dangerFilled : styles.danger, className)}
        {...rest}
      />
    );
  },
);

DangerButton.displayName = "DangerButton";
