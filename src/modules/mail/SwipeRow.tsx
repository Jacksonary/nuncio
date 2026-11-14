/**
 * Horizontal swipe gesture wrapper for message rows.
 *
 * Supports pointer input (touch, pen and — after a short hold, so it does not
 * steal drag-to-folder — mouse) plus trackpad two-finger horizontal wheel
 * deltas. A gesture only becomes a swipe once the horizontal intent exceeds
 * 24 px and clearly dominates the vertical movement; otherwise vertical
 * scrolling and native row dragging are left untouched.
 *
 * The action is revealed while dragging and executed on release past the
 * commit distance. `prefers-reduced-motion` disables the snap animation (CSS).
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
  type WheelEvent as ReactWheelEvent,
} from "react";
import type { HoverAction } from "@/ipc/types";
import { cn } from "@/lib/cn";
import styles from "./mail.module.css";

/** Horizontal movement (px) before a gesture is treated as a swipe. */
const HORIZONTAL_INTENT = 24;
/** Release distance (px) that commits the action. */
const COMMIT_DISTANCE = 72;
/** Maximum offset the row content is allowed to travel. */
const MAX_OFFSET = 112;
/** Hold before a mouse drag becomes a swipe instead of drag-to-folder. */
const MOUSE_HOLD_MS = 260;
/** Accumulated wheel delta that triggers a trackpad swipe. */
const WHEEL_THRESHOLD = 72;
/** Ignore further wheel swipes for this long (momentum scrolling). */
const WHEEL_LOCK_MS = 600;
const WHEEL_IDLE_MS = 220;

export type SwipeTone = "accent" | "danger" | "neutral";

export interface SwipeActionDescriptor {
  action: HoverAction;
  label: string;
  icon: ReactNode;
  tone: SwipeTone;
}

export interface SwipeableRowProps {
  /** Action revealed when the content is swiped right. */
  swipeRight: SwipeActionDescriptor | null;
  /** Action revealed when the content is swiped left. */
  swipeLeft: SwipeActionDescriptor | null;
  onAction: (action: HoverAction) => void;
  disabled?: boolean;
  className?: string;
  children: ReactNode;
}

interface GestureState {
  pointerId: number;
  startX: number;
  startY: number;
  horizontal: boolean;
  captured: boolean;
  element: HTMLElement;
}

function interactiveTarget(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    target.closest(
      "button, a, input, textarea, select, [role='button'], [role='checkbox'], [role='menuitem'], [contenteditable='true']",
    ) !== null
  );
}

function toneClass(tone: SwipeTone): string {
  if (tone === "danger") return styles.swipeToneDanger;
  if (tone === "neutral") return styles.swipeToneNeutral;
  return styles.swipeToneAccent;
}

export function SwipeableRow({
  swipeRight,
  swipeLeft,
  onAction,
  disabled = false,
  className,
  children,
}: SwipeableRowProps) {
  const [offset, setOffset] = useState(0);
  const [dragging, setDragging] = useState(false);
  const gestureRef = useRef<GestureState | null>(null);
  const offsetRef = useRef(0);
  const holdTimer = useRef<number | null>(null);
  const suppressDragRef = useRef(false);
  const suppressClickRef = useRef(0);
  const wheelSum = useRef(0);
  const wheelTimer = useRef<number | null>(null);
  const wheelLockUntil = useRef(0);

  const clearHoldTimer = useCallback(() => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current);
      holdTimer.current = null;
    }
  }, []);

  useEffect(() => {
    return () => {
      clearHoldTimer();
      if (wheelTimer.current !== null) window.clearTimeout(wheelTimer.current);
    };
  }, [clearHoldTimer]);

  const handlePointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (disabled) return;
      if (event.pointerType === "mouse" && event.button !== 0) return;
      if (interactiveTarget(event.target)) return;
      if (gestureRef.current) return;
      gestureRef.current = {
        pointerId: event.pointerId,
        startX: event.clientX,
        startY: event.clientY,
        horizontal: false,
        captured: false,
        element: event.currentTarget,
      };
      // Mouse drags normally become native drag-to-folder almost immediately.
      // Holding briefly lets an intentional horizontal swipe win instead.
      holdTimer.current = window.setTimeout(() => {
        suppressDragRef.current = true;
      }, MOUSE_HOLD_MS);
    },
    [disabled],
  );

  const handlePointerMove = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const gesture = gestureRef.current;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      const dx = event.clientX - gesture.startX;
      const dy = event.clientY - gesture.startY;

      if (!gesture.horizontal) {
        const horizontalIntent =
          Math.abs(dx) > HORIZONTAL_INTENT &&
          Math.abs(dx) > Math.abs(dy) * 1.2;
        if (!horizontalIntent) return;
        gesture.horizontal = true;
        suppressDragRef.current = true;
        try {
          event.currentTarget.setPointerCapture(event.pointerId);
          gesture.captured = true;
        } catch {
          // Capture is an optimisation; the gesture still works without it.
        }
        setDragging(true);
      }

      if (!gesture.horizontal) return;
      if (event.cancelable) event.preventDefault();

      const descriptor = dx > 0 ? swipeRight : swipeLeft;
      const next = descriptor
        ? Math.max(-MAX_OFFSET, Math.min(MAX_OFFSET, dx))
        : 0;
      offsetRef.current = next;
      setOffset(next);
    },
    [swipeLeft, swipeRight],
  );

  const finishGesture = useCallback(
    (commit: boolean) => {
      const gesture = gestureRef.current;
      if (!gesture) return;
      clearHoldTimer();
      // Clear the gesture first: releasing capture may synchronously deliver
      // `lostpointercapture`, which must see an already-finished gesture.
      gestureRef.current = null;
      const captured = gesture.captured;
      gesture.captured = false;
      if (captured) {
        try {
          gesture.element.releasePointerCapture(gesture.pointerId);
        } catch {
          // Pointer already released.
        }
      }
      const finalOffset = offsetRef.current;
      let action: HoverAction | null = null;
      if (commit && gesture.horizontal && Math.abs(finalOffset) >= COMMIT_DISTANCE) {
        action = finalOffset > 0 ? swipeRight?.action ?? null : swipeLeft?.action ?? null;
      }
      offsetRef.current = 0;
      setOffset(0);
      setDragging(false);
      if (action) {
        suppressClickRef.current = Date.now() + 500;
        onAction(action);
      }
      window.setTimeout(() => {
        suppressDragRef.current = false;
      }, 0);
    },
    [clearHoldTimer, onAction, swipeLeft, swipeRight],
  );

  const handlePointerUp = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const gesture = gestureRef.current;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      finishGesture(true);
    },
    [finishGesture],
  );

  const handlePointerCancel = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const gesture = gestureRef.current;
      if (!gesture || gesture.pointerId !== event.pointerId) return;
      finishGesture(false);
    },
    [finishGesture],
  );

  const handleLostCapture = useCallback(() => {
    // A cancelled native drag also ends capture; treat it as an aborted swipe.
    if (gestureRef.current) finishGesture(false);
  }, [finishGesture]);

  const handleDragStartCapture = useCallback((event: ReactDragEvent<HTMLDivElement>) => {
    if (!suppressDragRef.current) return;
    event.preventDefault();
    event.stopPropagation();
  }, []);

  const handleClickCapture = useCallback((event: ReactMouseEvent<HTMLDivElement>) => {
    if (Date.now() >= suppressClickRef.current) return;
    event.preventDefault();
    event.stopPropagation();
  }, []);

  const handleWheel = useCallback(
    (event: ReactWheelEvent<HTMLDivElement>) => {
      if (disabled) return;
      // deltaMode: 0 = pixels, 1 = lines, 2 = pages.
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1;
      const dx = event.deltaX * unit;
      const dy = event.deltaY * unit;
      if (Math.abs(dx) <= Math.abs(dy)) return;
      const now = Date.now();
      if (now < wheelLockUntil.current) return;

      wheelSum.current += dx;
      if (wheelTimer.current !== null) window.clearTimeout(wheelTimer.current);
      wheelTimer.current = window.setTimeout(() => {
        wheelSum.current = 0;
      }, WHEEL_IDLE_MS);

      if (Math.abs(wheelSum.current) < WHEEL_THRESHOLD) return;
      // Positive deltaX = content scrolled right = fingers swiped left.
      const leftward = wheelSum.current > 0;
      const descriptor = leftward ? swipeLeft : swipeRight;
      wheelSum.current = 0;
      if (!descriptor) return;
      wheelLockUntil.current = now + WHEEL_LOCK_MS;
      onAction(descriptor.action);
    },
    [disabled, onAction, swipeLeft, swipeRight],
  );

  const descriptor = offset > 0 ? swipeRight : offset < 0 ? swipeLeft : null;
  const progress = Math.min(1, Math.abs(offset) / MAX_OFFSET);

  return (
    <div
      className={cn(styles.swipeRow, className)}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={handlePointerUp}
      onPointerCancel={handlePointerCancel}
      onLostPointerCapture={handleLostCapture}
      onDragStartCapture={handleDragStartCapture}
      onClickCapture={handleClickCapture}
      onWheel={handleWheel}
    >
      {descriptor ? (
        <div
          className={cn(
            styles.swipeActionLayer,
            offset > 0 ? styles.swipeActionStart : styles.swipeActionEnd,
          )}
          style={{ width: Math.abs(offset) }}
          aria-hidden="true"
        >
          <div className={cn(styles.swipeAction, toneClass(descriptor.tone))}>
            <span className={styles.swipeActionIcon}>{descriptor.icon}</span>
            <span
              className={styles.swipeActionLabel}
              style={{ opacity: Math.max(0, (progress - 0.35) / 0.65) }}
            >
              {descriptor.label}
            </span>
          </div>
        </div>
      ) : null}
      <div
        className={cn(styles.swipeContent, !dragging && styles.swipeContentSnap)}
        style={{ transform: `translateX(${offset}px)` }}
      >
        {children}
      </div>
    </div>
  );
}
