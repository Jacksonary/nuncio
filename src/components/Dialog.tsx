/**
 * Modal dialog with a focus trap, Escape handling and focus restoration.
 * `confirm()` / `ConfirmHost` provide an imperative async confirmation helper.
 */
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useSyncExternalStore,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { createPortal } from "react-dom";
import { Dismiss16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Button, DangerButton } from "./Button";
import { IconButton } from "./IconButton";
import styles from "./Dialog.module.css";

const FOCUSABLE =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

export interface DialogProps {
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  /** Leading footer content (e.g. a checkbox). */
  footerLeading?: ReactNode;
  icon?: ReactNode;
  width?: number | string;
  dismissible?: boolean;
  closeOnBackdrop?: boolean;
  /** Allow Escape to close. Turn off for wizards that hold user input. */
  closeOnEscape?: boolean;
  className?: string;
  bodyClassName?: string;
  ariaLabel?: string;
  initialFocusRef?: RefObject<HTMLElement | null>;
}

export function Dialog({
  open,
  onClose,
  title,
  children,
  footer,
  footerLeading,
  icon,
  width = 520,
  dismissible = true,
  closeOnBackdrop = true,
  closeOnEscape = true,
  className,
  bodyClassName,
  ariaLabel,
  initialFocusRef,
}: DialogProps) {
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
        initialFocusRef?.current ??
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
  }, [open, initialFocusRef]);

  const handleKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      if (event.key === "Escape") {
        if (!dismissible || !closeOnEscape) return;
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
      const active = document.activeElement;
      if (event.shiftKey) {
        if (active === first || active === panel) {
          event.preventDefault();
          last.focus();
        }
      } else if (active === last) {
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
        className={cn(styles.panel, className)}
        style={{ width }}
        onKeyDown={handleKeyDown}
      >
        <div className={styles.header}>
          {icon ? (
            <span className={styles.headerIcon} aria-hidden="true">
              {icon}
            </span>
          ) : null}
          <div id={titleId} className={styles.title}>
            {title}
          </div>
          {dismissible ? (
            <IconButton
              icon={<Dismiss16Regular />}
              label={t("common.close")}
              size="small"
              className={styles.close}
              onClick={onClose}
            />
          ) : null}
        </div>
        <div ref={bodyRef} className={cn(styles.body, bodyClassName)}>{children}</div>
        {footer || footerLeading ? (
          <div className={cn(styles.footer, footerLeading && styles.footerWithLeading)}>
            {footerLeading ? <div className={styles.leading}>{footerLeading}</div> : null}
            {footer ? <div className={styles.actions}>{footer}</div> : null}
          </div>
        ) : null}
      </div>
    </div>,
    document.body,
  );
}

export interface ConfirmOptions {
  title: ReactNode;
  message?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
  /** Focus the confirm button instead of cancel. */
  focusConfirm?: boolean;
}

interface PendingConfirm extends ConfirmOptions {
  resolve: (value: boolean) => void;
}

let pendingConfirm: PendingConfirm | null = null;
const confirmListeners = new Set<() => void>();

function emitConfirmChange(): void {
  for (const listener of confirmListeners) listener();
}

function subscribeConfirm(listener: () => void): () => void {
  confirmListeners.add(listener);
  return () => confirmListeners.delete(listener);
}

function getConfirmSnapshot(): PendingConfirm | null {
  return pendingConfirm;
}

/** Imperative async confirm. Resolves false when another confirm opens. */
export function confirm(options: ConfirmOptions): Promise<boolean> {
  if (pendingConfirm) {
    pendingConfirm.resolve(false);
    pendingConfirm = null;
  }
  return new Promise<boolean>((resolve) => {
    pendingConfirm = { ...options, resolve };
    emitConfirmChange();
  });
}

export interface ConfirmDialogProps {
  open: boolean;
  title: ReactNode;
  message?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  focusConfirm?: boolean;
}

export function ConfirmDialog({
  open,
  title,
  message,
  confirmLabel,
  cancelLabel,
  danger = false,
  onConfirm,
  onCancel,
  focusConfirm = false,
}: ConfirmDialogProps) {
  const t = useT();
  const confirmRef = useRef<HTMLButtonElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  return (
    <Dialog
      open={open}
      onClose={onCancel}
      title={title}
      width={440}
      dismissible
      closeOnBackdrop={false}
      initialFocusRef={focusConfirm ? confirmRef : cancelRef}
      footer={
        <>
          <Button ref={cancelRef} appearance="subtle" onClick={onCancel}>
            {cancelLabel ?? t("common.cancel")}
          </Button>
          {danger ? (
            <DangerButton ref={confirmRef} filled onClick={onConfirm}>
              {confirmLabel ?? t("common.delete")}
            </DangerButton>
          ) : (
            <Button ref={confirmRef} appearance="primary" onClick={onConfirm}>
              {confirmLabel ?? t("common.ok")}
            </Button>
          )}
        </>
      }
    >
      {typeof message === "string" ? <p className={styles.message}>{message}</p> : message}
    </Dialog>
  );
}

/** Mount once (App) so `confirm()` works from anywhere. */
export function ConfirmHost() {
  const pending = useSyncExternalStore(
    subscribeConfirm,
    getConfirmSnapshot,
    getConfirmSnapshot,
  );

  if (!pending) return null;

  const finish = (value: boolean) => {
    const current = pendingConfirm;
    pendingConfirm = null;
    emitConfirmChange();
    current?.resolve(value);
  };

  return (
    <ConfirmDialog
      open
      title={pending.title}
      message={pending.message}
      confirmLabel={pending.confirmLabel}
      cancelLabel={pending.cancelLabel}
      danger={pending.danger}
      focusConfirm={pending.focusConfirm}
      onConfirm={() => finish(true)}
      onCancel={() => finish(false)}
    />
  );
}
