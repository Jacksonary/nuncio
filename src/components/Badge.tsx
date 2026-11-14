import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./Badge.module.css";

export type BadgeAppearance = "accent" | "neutral" | "danger" | "success" | "warning" | "outline";

export interface BadgeProps {
  children: ReactNode;
  appearance?: BadgeAppearance;
  className?: string;
  title?: string;
}

export function Badge({
  children,
  appearance = "accent",
  className,
  title,
}: BadgeProps) {
  return (
    <span className={cn(styles.badge, styles[appearance], className)} title={title}>
      {children}
    </span>
  );
}

export interface CountBadgeProps {
  count: number;
  max?: number;
  appearance?: BadgeAppearance;
  showZero?: boolean;
  /** Accessible description, e.g. "12 unread". */
  label?: string;
  className?: string;
}

/** Numeric pill; hidden when count is 0 unless showZero. */
export function CountBadge({
  count,
  max = 99,
  appearance = "accent",
  showZero = false,
  label,
  className,
}: CountBadgeProps) {
  if (!Number.isFinite(count) || (count <= 0 && !showZero)) return null;
  const display = count > max ? `${max}+` : String(count);
  return (
    <span
      className={cn(styles.badge, styles.count, styles[appearance], className)}
      aria-label={label ?? String(count)}
    >
      {display}
    </span>
  );
}

export interface DotBadgeProps {
  color?: string;
  size?: number;
  className?: string;
  label?: string;
}

/** Small status dot. */
export function DotBadge({ color, size = 8, className, label }: DotBadgeProps) {
  return (
    <span
      className={cn(styles.badge, className)}
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      style={{
        width: size,
        height: size,
        minHeight: size,
        padding: 0,
        borderRadius: "50%",
        background: color ?? "var(--accent)",
      }}
    />
  );
}
