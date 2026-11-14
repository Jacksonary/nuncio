/**
 * Right-side flyout panel (settings, details). Focus-trapped, dismissed with
 * Escape or the close button, restores focus to the trigger.
 */
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Dismiss16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { IconButton } from "./IconButton";
import styles from "./SidePanel.module.css";

const FOCUSABLE =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

export interface SidePanelProps {
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
  side?: "right" | "left";
  dismissible?: boolean;
  closeOnBackdrop?: boolean;
  className?: string;
  bodyClassName?: string;
  ariaLabel?: string;
}

export function SidePanel({
  open,
  onClose,
  title,
  children,
  footer,
  width,
  side = "right",
  dismissible = true,
  closeOnBackdrop = true,
  className,
  bodyClassName,
  ariaLabel,
}: SidePanelProps) {
  const t = useT();
  const titleId = useId();
  const panelRef = useRef<HTMLDivElement | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open) return;
    previousFocus.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const timer = window.requestAnimationFrame(() => {
      const target =
        bodyRef.current?.querySelector<HTMLElement>(FOCUSABLE) ??
        panelRef.current?.querySelector<HTMLElement>(FOCUSABLE) ??
        panelRef.current;
      target?.focus();
    });
    return () => {
      window.cancelAnimationFrame(timer);
      previousFocus.current?.focus?.();
      previousFocus.current = null;
    };
  }, [open]);

  const handleKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      if (event.key === "Escape") {
        if (!dismissible) return;
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key !== "Tab") return;
      const panel = panelRef.current;
      if (!panel) return;
      const focusable = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
        (element) => element.offsetParent !== null || element === document.activeElement,
      );
      if (focusable.length === 0) {
        event.preventDefault();
        panel.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    },
    [dismissible, onClose],
  );

  if (!open || typeof document === "undefined") return null;

  return createPortal(
    <div
      className={styles.overlay}
      onPointerDown={(event) => {
        if (closeOnBackdrop && dismissible && event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={title ? titleId : undefined}
        aria-label={title ? undefined : ariaLabel}
        tabIndex={-1}
        className={cn(styles.panel, side === "left" && styles.left, className)}
        style={width ? { width } : undefined}
        onKeyDown={handleKeyDown}
      >
        <div className={styles.header}>
          <div id={titleId} className={styles.title}>
            {title}
          </div>
          {dismissible ? (
            <IconButton
              icon={<Dismiss16Regular />}
              label={t("common.close")}
              size="small"
              onClick={onClose}
            />
          ) : null}
        </div>
        <div ref={bodyRef} className={cn(styles.body, bodyClassName)}>{children}</div>
        {footer ? <div className={styles.footer}>{footer}</div> : null}
      </div>
    </div>,
    document.body,
  );
}
