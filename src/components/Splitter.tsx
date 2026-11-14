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
  const [dragging, setDragging] = useState(false);
  const dragRef = useRef(false);
  const startRef = useRef({ position: 0, size: 0 });

  useEffect(() => {
    sizeRef.current = size;
  }, [size]);

  const clamp = (value: number) => Math.min(max, Math.max(min, value));

  const persist = () => {
    try {
      localStorage.setItem(`${STORAGE_PREFIX}${storageKey}`, String(Math.round(sizeRef.current)));
    } catch {
      // Storage may be unavailable; the in-memory size still works.
    }
  };

  const updateSize = (next: number) => {
    const clamped = clamp(next);
    sizeRef.current = clamped;
    setSize(clamped);
  };

  const handlePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    dragRef.current = true;
    startRef.current = {
      position: horizontal ? event.clientX : event.clientY,
      size: sizeRef.current,
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
    persist();
  };

  const primaryStyle: CSSProperties = horizontal ? { width: size } : { height: size };

  return (
    <div className={cn(styles.splitter, !horizontal && styles.vertical, className)}>
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
