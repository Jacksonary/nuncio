import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./ListItem.module.css";

export type ListDensity = "compact" | "normal" | "spacious";

export interface ListItemProps {
  primary: ReactNode;
  secondary?: ReactNode;
  tertiary?: ReactNode;
  leading?: ReactNode;
  trailing?: ReactNode;
  /** Timestamp or short meta shown in the trailing column. */
  timestamp?: ReactNode;
  selected?: boolean;
  unread?: boolean;
  disabled?: boolean;
  density?: ListDensity;
  onClick?: () => void;
  onDoubleClick?: () => void;
  className?: string;
  role?: string;
  ariaLabel?: string;
}

/**
 * Generic two-line list row (mail list, contacts list, task list items).
 * Interactive rows are keyboard operable.
 */
export function ListItem({
  primary,
  secondary,
  tertiary,
  leading,
  trailing,
  timestamp,
  selected = false,
  unread = false,
  disabled = false,
  density = "normal",
  onClick,
  onDoubleClick,
  className,
  role,
  ariaLabel,
}: ListItemProps) {
  const interactive = Boolean(onClick) && !disabled;
  const endColumn = trailing || timestamp;
  return (
    <div
      className={cn(
        styles.item,
        density === "compact" && styles.compact,
        density === "spacious" && styles.spacious,
        interactive && styles.interactive,
        selected && styles.selected,
        unread && styles.unread,
        disabled && styles.disabled,
        className,
      )}
      role={role ?? (interactive ? "button" : undefined)}
      tabIndex={interactive ? 0 : undefined}
      aria-label={ariaLabel}
      aria-current={selected || undefined}
      aria-disabled={disabled || undefined}
      onClick={interactive ? onClick : undefined}
      onDoubleClick={onDoubleClick}
      onKeyDown={
        interactive
          ? (event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                onClick?.();
              }
            }
          : undefined
      }
    >
      {leading ? <div className={styles.leading}>{leading}</div> : null}
      <div className={styles.main}>
        <div className={styles.primaryRow}>
          <span className={styles.primary}>{primary}</span>
          {timestamp ? <span className={styles.timestamp}>{timestamp}</span> : null}
        </div>
        {secondary || tertiary ? (
          <div className={styles.secondary}>
            <span className={styles.secondaryText}>{secondary}</span>
            {tertiary ? (
              <>
                <span aria-hidden="true">·</span>
                <span className={styles.secondaryText}>{tertiary}</span>
              </>
            ) : null}
          </div>
        ) : null}
      </div>
      {endColumn ? (
        <div className={styles.endColumn}>
          {trailing ? <div className={styles.trailing}>{trailing}</div> : null}
        </div>
      ) : null}
    </div>
  );
}
