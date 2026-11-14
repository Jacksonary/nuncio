import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./Kbd.module.css";

export interface KbdProps {
  children: ReactNode;
  className?: string;
  label?: string;
}

/** A keycap. Pass a shortcut string (already formatted) as children. */
export function Kbd({ children, className, label }: KbdProps) {
  return (
    <kbd className={cn(styles.kbd, className)} aria-label={label}>
      {children}
    </kbd>
  );
}
