import type { CSSProperties, ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./Card.module.css";

export interface CardProps {
  children: ReactNode;
  padded?: boolean;
  interactive?: boolean;
  selected?: boolean;
  flat?: boolean;
  elevated?: boolean;
  onClick?: () => void;
  onDoubleClick?: () => void;
  className?: string;
  style?: CSSProperties;
  ariaLabel?: string;
}

/** Generic surface card used by settings sections, composer panes, etc. */
export function Card({
  children,
  padded = true,
  interactive = false,
  selected = false,
  flat = false,
  elevated = false,
  onClick,
  onDoubleClick,
  className,
  style,
  ariaLabel,
}: CardProps) {
  const clickable = interactive || Boolean(onClick);
  return (
    <div
      className={cn(
        styles.card,
        padded && styles.padded,
        clickable && styles.interactive,
        selected && styles.selected,
        flat && styles.flat,
        elevated && styles.elevated,
        className,
      )}
      style={style}
      role={clickable ? "button" : undefined}
      tabIndex={clickable ? 0 : undefined}
      aria-label={ariaLabel}
      aria-pressed={selected || undefined}
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      onKeyDown={
        clickable
          ? (event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                onClick?.();
              }
            }
          : undefined
      }
    >
      {children}
    </div>
  );
}
