/**
 * Horizontal swipe gesture wrapper for message rows.
 *
 * Touch and pen input only. A mouse or trackpad never starts a swipe: on a
 * desktop a two-finger horizontal scroll or a held drag is easy to trigger by
 * accident, and the action here (delete / archive) is destructive. The
 * desktop has hover actions, the context menu, the bulk bar and shortcuts for
 * the same operations. A touch gesture only becomes a swipe once the
 * horizontal intent exceeds 24 px and clearly dominates the vertical movement,
 * so vertical scrolling is left untouched.
 *
 * The action is revealed while dragging and executed on release past the
 * commit distance. `prefers-reduced-motion` disables the snap animation (CSS).
 */
import {
  useCallback,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
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
  const suppressDragRef = useRef(false);
  const suppressClickRef = useRef(0);

  const handlePointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (disabled) return;
      // Touch and pen only; see the file comment.
      if (event.pointerType !== "touch" && event.pointerType !== "pen") return;
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
    [onAction, swipeLeft, swipeRight],
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
