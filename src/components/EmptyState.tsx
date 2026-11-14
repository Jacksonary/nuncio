import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./EmptyState.module.css";

export interface EmptyStateProps {
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  compact?: boolean;
  className?: string;
}

/** Centered empty/placeholder state for panes and lists. */
export function EmptyState({
  icon,
  title,
  description,
  actions,
  compact = false,
  className,
}: EmptyStateProps) {
  return (
    <div className={cn(styles.empty, compact && styles.compact, className)}>
      {icon ? (
        <span className={styles.icon} aria-hidden="true">
          {icon}
        </span>
      ) : null}
      <div className={styles.title}>{title}</div>
      {description ? <div className={styles.description}>{description}</div> : null}
      {actions ? <div className={styles.actions}>{actions}</div> : null}
    </div>
  );
}
