import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Tooltip } from "./Tooltip";
import styles from "./IconButton.module.css";

export interface IconButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children" | "aria-label"> {
  icon: ReactNode;
  /** Accessible name; also the default tooltip text. */
  label: string;
  appearance?: "subtle" | "transparent" | "primary";
  size?: "small" | "medium" | "large";
  selected?: boolean;
  /**
   * An action is in flight. The icon keeps its look and turns on the spot, and
   * clicks are ignored until it clears. Unlike `disabled` the icon is not dimmed,
   * so the motion stays readable.
   */
  busy?: boolean;
  /** Override the tooltip; `false` disables it. */
  tooltip?: ReactNode | false;
}

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  {
    icon,
    label,
    appearance = "transparent",
    size = "medium",
    selected,
    busy = false,
    tooltip,
    className,
    disabled,
    onClick,
    type = "button",
    ...rest
  },
  ref,
) {
  const button = (
    <button
      ref={ref}
      type={type}
      className={cn(
        styles.iconButton,
        size === "small" ? styles.small : size === "large" ? styles.large : undefined,
        appearance === "subtle"
          ? styles["appearance-subtle"]
          : appearance === "primary"
            ? styles["appearance-primary"]
            : undefined,
        selected && styles.selected,
        busy && styles.busy,
        className,
      )}
      aria-label={label}
      aria-busy={busy || undefined}
      onClick={busy ? undefined : onClick}
      aria-pressed={selected === undefined ? undefined : selected}
      disabled={disabled}
      {...rest}
    >
      <span className={styles.icon} aria-hidden="true">
        {icon}
      </span>
    </button>
  );

  if (tooltip === false) return button;
  return (
    <Tooltip content={tooltip ?? label} placement="top" delay={450}>
      {button}
    </Tooltip>
  );
});

IconButton.displayName = "IconButton";
