/**
 * Toast system usable from React (useToast) and from plain modules
 * (`toast.error(...)` in the event bridge). Toasts can carry actions such as
 * "Undo" or reminder snooze/dismiss buttons.
 */
import { useCallback, useSyncExternalStore, type ReactNode } from "react";
import { createPortal } from "react-dom";
import {
  Alert24Regular,
  CheckmarkCircle24Regular,
  Dismiss16Regular,
  ErrorCircle24Regular,
  Info24Regular,
} from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Button } from "./Button";
import { IconButton } from "./IconButton";
import styles from "./Toast.module.css";

export type ToastType = "info" | "success" | "error" | "reminder";

export interface ToastAction {
  label: string;
  onClick?: () => void;
  primary?: boolean;
}

export interface ToastOptions {
  type?: ToastType;
  title: string;
  message?: string;
  actions?: ToastAction[];
  /** Auto-dismiss delay in ms; 0 keeps it open until dismissed. */
  duration?: number;
}

export interface ToastItem extends Required<Omit<ToastOptions, "message" | "actions">> {
  id: number;
  message?: string;
  actions?: ToastAction[];
}

const DEFAULT_DURATION: Record<ToastType, number> = {
  info: 5000,
  success: 4000,
  error: 8000,
  reminder: 0,
};

let items: ToastItem[] = [];
let nextId = 1;
const listeners = new Set<() => void>();
const timers = new Map<number, number>();

function emit(): void {
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getSnapshot(): ToastItem[] {
  return items;
}

function clearTimer(id: number): void {
  const timer = timers.get(id);
  if (timer !== undefined) {
    window.clearTimeout(timer);
    timers.delete(id);
  }
}

function dismiss(id: number): void {
  clearTimer(id);
  const next = items.filter((item) => item.id !== id);
  if (next.length === items.length) return;
  items = next;
  emit();
}

function show(options: ToastOptions): number {
  const type = options.type ?? "info";
  const id = nextId++;
  const item: ToastItem = {
    id,
    type,
    title: options.title,
    message: options.message,
    actions: options.actions,
    duration: options.duration ?? DEFAULT_DURATION[type],
  };
  items = [...items, item];
  emit();
  if (item.duration > 0) {
    const timer = window.setTimeout(() => dismiss(id), item.duration);
    timers.set(id, timer);
  }
  return id;
}

function showOfType(
  type: ToastType,
  title: string,
  message?: string,
  extra?: Partial<Omit<ToastOptions, "type" | "title" | "message">>,
): number {
  return show({ type, title, message, ...extra });
}

export const toast = {
  show,
  info: (title: string, message?: string, extra?: Partial<ToastOptions>) =>
    showOfType("info", title, message, extra),
  success: (title: string, message?: string, extra?: Partial<ToastOptions>) =>
    showOfType("success", title, message, extra),
  error: (title: string, message?: string, extra?: Partial<ToastOptions>) =>
    showOfType("error", title, message, extra),
  reminder: (title: string, message?: string, extra?: Partial<ToastOptions>) =>
    showOfType("reminder", title, message, extra),
  dismiss,
  clear: () => {
    for (const item of items) clearTimer(item.id);
    items = [];
    emit();
  },
};

export interface ToastApi {
  show: (options: ToastOptions) => number;
  info: typeof toast.info;
  success: typeof toast.success;
  error: typeof toast.error;
  reminder: typeof toast.reminder;
  dismiss: (id: number) => void;
  clear: () => void;
}

/** Hook returning the toast API (stable identity). */
export function useToast(): ToastApi {
  return toast;
}

const TOAST_ICON: Record<ToastType, ReactNode> = {
  info: <Info24Regular />,
  success: <CheckmarkCircle24Regular />,
  error: <ErrorCircle24Regular />,
  reminder: <Alert24Regular />,
};

function ToastSurface({ item }: { item: ToastItem }) {
  const t = useT();
  const dismiss = useCallback(() => toast.dismiss(item.id), [item.id]);
  return (
    <div
      role={item.type === "error" || item.type === "reminder" ? "alert" : "status"}
      aria-live={item.type === "error" ? "assertive" : "polite"}
      className={cn(
        styles.toast,
        item.type === "error" && styles.error,
        item.type === "success" && styles.success,
        item.type === "reminder" && styles.reminder,
      )}
    >
      <span className={styles.icon} aria-hidden="true">
        {TOAST_ICON[item.type]}
      </span>
      <div className={styles.content}>
        <div className={styles.title}>{item.title}</div>
        {item.message ? <div className={styles.message}>{item.message}</div> : null}
        {item.actions && item.actions.length > 0 ? (
          <div className={styles.actions}>
            {item.actions.map((action, index) => (
              <Button
                key={`${action.label}-${index}`}
                size="small"
                appearance={action.primary ? "primary" : "subtle"}
                onClick={() => {
                  action.onClick?.();
                  toast.dismiss(item.id);
                }}
              >
                {action.label}
              </Button>
            ))}
          </div>
        ) : null}
      </div>
      <IconButton
        icon={<Dismiss16Regular />}
        label={t("common.dismiss")}
        size="small"
        className={styles.dismiss}
        onClick={dismiss}
      />
    </div>
  );
}

/** Renders the toast stack; mount once near the app root. */
export function ToastViewport() {
  const toasts = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  const t = useT();
  if (typeof document === "undefined") return null;
  return createPortal(
    <div className={styles.viewport} aria-label={t("common.notifications")}>
      {toasts.map((item) => (
        <ToastSurface key={item.id} item={item} />
      ))}
    </div>,
    document.body,
  );
}

export interface ToastProviderProps {
  children: ReactNode;
}

/** Convenience provider: renders children plus the viewport. */
export function ToastProvider({ children }: ToastProviderProps) {
  return (
    <>
      {children}
      <ToastViewport />
    </>
  );
}
