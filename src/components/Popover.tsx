/**
 * Low-level positioned surface used by menus, flyouts, pickers and tooltips.
 * Renders into a portal, flips/clamps against the viewport, closes on outside
 * pointer-down and Escape. It is intentionally unstyled (no padding) so each
 * consumer can decide its own internal layout.
 */
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { cn } from "@/lib/cn";
import styles from "./Popover.module.css";

export type PopoverPlacement =
  | "bottom"
  | "bottom-start"
  | "bottom-end"
  | "top"
  | "top-start"
  | "top-end"
  | "right"
  | "right-start"
  | "right-end"
  | "left"
  | "left-start"
  | "left-end";

export interface PopoverProps {
  open: boolean;
  onClose: () => void;
  /** Element the popover is anchored to. */
  anchor?: HTMLElement | null;
  /** Alternative anchor: a screen point (context menu). */
  point?: { x: number; y: number } | null;
  placement?: PopoverPlacement;
  offset?: number;
  matchAnchorWidth?: boolean;
  minWidth?: number;
  maxHeight?: number;
  role?: string;
  id?: string;
  className?: string;
  style?: CSSProperties;
  ariaLabel?: string;
  closeOnOutsideClick?: boolean;
  closeOnEscape?: boolean;
  /** Render inline instead of a portal (used by nested submenus). */
  portal?: boolean;
  children: ReactNode;
}

interface Position {
  left: number;
  top: number;
  width?: number;
}

const VIEWPORT_MARGIN = 8;

function computePosition(
  anchorRect: { left: number; top: number; width: number; height: number },
  size: { width: number; height: number },
  placement: PopoverPlacement,
  offset: number,
): Position {
  const viewportWidth = window.innerWidth;
  const viewportHeight = window.innerHeight;
  let actual = placement;

  const spaceBelow = viewportHeight - (anchorRect.top + anchorRect.height) - offset;
  const spaceAbove = anchorRect.top - offset;
  if (placement.startsWith("bottom") && size.height > spaceBelow && spaceAbove > spaceBelow) {
    actual = placement.replace("bottom", "top") as PopoverPlacement;
  } else if (
    placement.startsWith("top") &&
    size.height > spaceAbove &&
    spaceBelow > spaceAbove
  ) {
    actual = placement.replace("top", "bottom") as PopoverPlacement;
  }

  const spaceRight = viewportWidth - (anchorRect.left + anchorRect.width) - offset;
  const spaceLeft = anchorRect.left - offset;
  if (placement.startsWith("right") && size.width > spaceRight && spaceLeft > spaceRight) {
    actual = placement.replace("right", "left") as PopoverPlacement;
  } else if (
    placement.startsWith("left") &&
    size.width > spaceLeft &&
    spaceRight > spaceLeft
  ) {
    actual = placement.replace("left", "right") as PopoverPlacement;
  }

  const [side, align] = actual.split("-") as [string, string | undefined];
  let left = 0;
  let top = 0;
  if (side === "bottom" || side === "top") {
    top = side === "bottom" ? anchorRect.top + anchorRect.height + offset : anchorRect.top - size.height - offset;
    if (align === "start") left = anchorRect.left;
    else if (align === "end") left = anchorRect.left + anchorRect.width - size.width;
    else left = anchorRect.left + (anchorRect.width - size.width) / 2;
  } else if (side === "right" || side === "left") {
    left = side === "right" ? anchorRect.left + anchorRect.width + offset : anchorRect.left - size.width - offset;
    if (align === "start") top = anchorRect.top;
    else if (align === "end") top = anchorRect.top + anchorRect.height - size.height;
    else top = anchorRect.top + (anchorRect.height - size.height) / 2;
  }

  left = Math.min(
    Math.max(VIEWPORT_MARGIN, left),
    Math.max(VIEWPORT_MARGIN, viewportWidth - size.width - VIEWPORT_MARGIN),
  );
  top = Math.min(
    Math.max(VIEWPORT_MARGIN, top),
    Math.max(VIEWPORT_MARGIN, viewportHeight - size.height - VIEWPORT_MARGIN),
  );
  return { left, top };
}

export function Popover({
  open,
  onClose,
  anchor,
  point,
  placement = "bottom-start",
  offset = 4,
  matchAnchorWidth = false,
  minWidth,
  maxHeight,
  role,
  id,
  className,
  style,
  ariaLabel,
  closeOnOutsideClick = true,
  closeOnEscape = true,
  portal = true,
  children,
}: PopoverProps) {
  const contentRef = useRef<HTMLDivElement | null>(null);
  const [position, setPosition] = useState<Position | null>(null);
  const [contentSize, setContentSize] = useState<{ width: number; height: number } | null>(null);

  const update = useCallback(() => {
    const content = contentRef.current;
    if (!content) return;
    const rect = content.getBoundingClientRect();
    const size = {
      width: Math.ceil(rect.width),
      height: Math.ceil(rect.height),
    };
    const anchorRect = point
      ? { left: point.x, top: point.y, width: 0, height: 0 }
      : anchor
        ? (() => {
            const r = anchor.getBoundingClientRect();
            return { left: r.left, top: r.top, width: r.width, height: r.height };
          })()
        : null;
    if (!anchorRect) return;
    const next = computePosition(anchorRect, size, placement, offset);
    setPosition(next);
    setContentSize(size);
  }, [anchor, point, placement, offset]);

  useLayoutEffect(() => {
    if (!open) {
      setPosition(null);
      setContentSize(null);
      return;
    }
    update();
  }, [open, update]);

  // Track content size (async fonts/images can change it).
  useEffect(() => {
    if (!open) return;
    const content = contentRef.current;
    if (!content || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => update());
    observer.observe(content);
    return () => observer.disconnect();
  }, [open, update]);

  useEffect(() => {
    if (!open) return;
    const handleScroll = (event: Event) => {
      if (contentRef.current && event.target instanceof Node && contentRef.current.contains(event.target)) {
        return;
      }
      update();
    };
    window.addEventListener("resize", update);
    window.addEventListener("scroll", handleScroll, true);
    return () => {
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", handleScroll, true);
    };
  }, [open, update]);

  useEffect(() => {
    if (!open || !closeOnOutsideClick) return;
    const handlePointerDown = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (contentRef.current?.contains(target)) return;
      if (anchor?.contains(target)) return;
      onClose();
    };
    document.addEventListener("pointerdown", handlePointerDown, true);
    return () => document.removeEventListener("pointerdown", handlePointerDown, true);
  }, [open, closeOnOutsideClick, anchor, onClose]);

  useEffect(() => {
    if (!open || !closeOnEscape) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      onClose();
    };
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [open, closeOnEscape, onClose]);

  if (!open) return null;
  if (typeof document === "undefined") return null;

  const anchorWidth = !point && anchor ? anchor.getBoundingClientRect().width : 0;
  const computedStyle: CSSProperties = {
    ...style,
    ...(position ? { left: position.left, top: position.top } : { left: 0, top: 0 }),
    ...(matchAnchorWidth && anchorWidth > 0
      ? { width: Math.max(anchorWidth, minWidth ?? 0) }
      : minWidth
        ? { minWidth }
        : {}),
    ...(maxHeight ? { maxHeight, overflowY: "auto" } : {}),
  };

  const measured = position !== null && contentSize !== null;

  const surface = (
    <div
      ref={contentRef}
      id={id}
      role={role}
      aria-label={ariaLabel}
      className={cn(
        styles.popover,
        !measured && styles.measuring,
        className,
      )}
      style={computedStyle}
      data-popover=""
    >
      {children}
    </div>
  );

  return portal ? createPortal(surface, document.body) : surface;
}
