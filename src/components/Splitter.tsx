/**
 * Two-pane resizable splitter with a persisted size (localStorage).
 * The handle is focusable and adjustable with arrow keys.
 */
import {
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import styles from "./Splitter.module.css";

const STORAGE_PREFIX = "nuncio.split.";
/** Width/height of the drag handle; keep in sync with .handle in Splitter.module.css. */
const HANDLE_SIZE = 7;
/** Room the secondary pane always keeps, so the handle never gets pushed out of view. */
const SECONDARY_MIN = 200;

function readStoredSize(storageKey: string, fallback: number): number {
  try {
    const raw = localStorage.getItem(`${STORAGE_PREFIX}${storageKey}`);
    const value = raw ? Number(raw) : Number.NaN;
    return Number.isFinite(value) && value > 0 ? value : fallback;
  } catch {
    return fallback;
  }
}

export interface SplitterProps {
  /** Stable key used to persist the pane size. */
  storageKey: string;
  primary: ReactNode;
  secondary: ReactNode;
  defaultSize?: number;
  min?: number;
  max?: number;
  /** "horizontal" = panes side by side; "vertical" = stacked. */
  direction?: "horizontal" | "vertical";
  className?: string;
  primaryClassName?: string;
  secondaryClassName?: string;
  handleLabel?: string;
  /** Reset to defaultSize on double click. */
  resetOnDoubleClick?: boolean;
}

export function Splitter({
  storageKey,
  primary,
  secondary,
  defaultSize = 320,
  min = 180,
  max = 760,
  direction = "horizontal",
  className,
  primaryClassName,
  secondaryClassName,
  handleLabel,
  resetOnDoubleClick = true,
}: SplitterProps) {
  const t = useT();
  const horizontal = direction === "horizontal";
  const [size, setSize] = useState(() => readStoredSize(storageKey, defaultSize));
  const sizeRef = useRef(size);
  const rootRef = useRef<HTMLDivElement>(null);
  const [containerSize, setContainerSize] = useState<number | null>(null);
  const containerSizeRef = useRef<number | null>(null);
  const [dragging, setDragging] = useState(false);
  const dragRef = useRef(false);
  const startRef = useRef({ position: 0, size: 0 });

  useEffect(() => {
    sizeRef.current = size;
  }, [size]);

  // The container can shrink without a window resize (side panels, module
  // switches), so observe the root itself rather than listening to the window.
  useEffect(() => {
    const element = rootRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      const next = horizontal ? rect.width : rect.height;
      containerSizeRef.current = next;
      // Re-clamping mid-drag would pull the pane out from under the pointer;
      // the new limit is published when the drag ends instead.
      if (!dragRef.current) setContainerSize(next);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [horizontal]);

  const clamp = (value: number) => Math.min(max, Math.max(min, value));

  /** Largest primary size that still fits next to the handle and a usable secondary pane. */
  const limitFor = (measured: number | null) =>
    measured == null ? max : Math.min(max, Math.max(0, measured - HANDLE_SIZE - SECONDARY_MIN));

  const persist = () => {
    try {
      localStorage.setItem(`${STORAGE_PREFIX}${storageKey}`, String(Math.round(sizeRef.current)));
    } catch {
      // Storage may be unavailable; the in-memory size still works.
    }
  };

  const updateSize = (next: number) => {
    // Clamp before persisting: a size chosen while the window is small should
    // not be restored later as an overflow on the next launch.
    const clamped = Math.min(clamp(next), limitFor(containerSizeRef.current));
    sizeRef.current = clamped;
    setSize(clamped);
  };

  const handlePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    dragRef.current = true;
    startRef.current = {
      position: horizontal ? event.clientX : event.clientY,
      // Start from what is actually on screen so a clamped stored size does
      // not leave a dead zone before the handle begins to move.
      size: Math.min(sizeRef.current, limitFor(containerSizeRef.current)),
    };
    setDragging(true);
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const handlePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return;
    const position = horizontal ? event.clientX : event.clientY;
    updateSize(startRef.current.size + (position - startRef.current.position));
  };

  const endDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragRef.current) return;
    dragRef.current = false;
    setDragging(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    // Publish any resize observed while dragging was suppressed above.
    setContainerSize(containerSizeRef.current);
    persist();
  };

  const appliedSize = Math.min(size, limitFor(containerSize));
  const primaryStyle: CSSProperties = horizontal
    ? { width: appliedSize }
    : { height: appliedSize };

  return (
    <div ref={rootRef} className={cn(styles.splitter, !horizontal && styles.vertical, className)}>
      <div className={cn(styles.primary, primaryClassName)} style={primaryStyle}>
        {primary}
      </div>
      <div
        className={cn(styles.handle, dragging && styles.dragging)}
        role="separator"
        tabIndex={0}
        aria-orientation={horizontal ? "vertical" : "horizontal"}
        aria-label={handleLabel ?? t("common.dragToResize")}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={Math.round(size)}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onDoubleClick={() => {
          if (!resetOnDoubleClick) return;
          updateSize(defaultSize);
          persist();
        }}
        onKeyDown={(event) => {
          const step = event.shiftKey ? 64 : 16;
          if (horizontal && (event.key === "ArrowLeft" || event.key === "ArrowRight")) {
            event.preventDefault();
            updateSize(sizeRef.current + (event.key === "ArrowLeft" ? -step : step));
            persist();
          } else if (!horizontal && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
            event.preventDefault();
            updateSize(sizeRef.current + (event.key === "ArrowUp" ? -step : step));
            persist();
          } else if (event.key === "Home") {
            event.preventDefault();
            updateSize(min);
            persist();
          } else if (event.key === "End") {
            event.preventDefault();
            updateSize(max);
            persist();
          }
        }}
      />
      <div className={cn(styles.secondary, secondaryClassName)}>{secondary}</div>
    </div>
  );
}
