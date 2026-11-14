import { type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import styles from "./Progress.module.css";

export interface SpinnerProps {
  size?: "tiny" | "small" | "medium" | "large";
  label?: string;
  className?: string;
}

export function Spinner({ size = "small", label, className }: SpinnerProps) {
  const t = useT();
  return (
    <span
      className={cn(styles.spinner, styles[size], className)}
      role="progressbar"
      aria-label={label ?? t("common.loading")}
    />
  );
}

export interface ProgressBarProps {
  /** 0–100. Ignored when `indeterminate` is true. */
  value?: number;
  max?: number;
  indeterminate?: boolean;
  label?: ReactNode;
  showValue?: boolean;
  className?: string;
}

export function ProgressBar({
  value = 0,
  max = 100,
  indeterminate = false,
  label,
  showValue = false,
  className,
}: ProgressBarProps) {
  const percent = max > 0 ? Math.max(0, Math.min(100, (value / max) * 100)) : 0;
  return (
    <div
      className={cn(styles.progress, className)}
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={max}
      aria-valuenow={indeterminate ? undefined : value}
    >
      {label || showValue ? (
        <div className={styles.progressHeader}>
          <span>{label}</span>
          {showValue && !indeterminate ? <span>{Math.round(percent)}%</span> : null}
        </div>
      ) : null}
      <div className={styles.track}>
        {indeterminate ? (
          <div className={styles.indeterminate} />
        ) : (
          <div className={styles.bar} style={{ width: `${percent}%` }} />
        )}
      </div>
    </div>
  );
}
