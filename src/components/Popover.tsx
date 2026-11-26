/**
 * Low-level positioned surface used by menus, flyouts, pickers and tooltips.
 * Renders into a portal, flips/clamps against the viewport, closes on outside
 * pointer-down and Escape. It is intentionally unstyled (no padding) so each
 * consumer can decide its own internal layout.
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { cn } from "@/lib/cn";
import styles from "./Popover.module.css";

/** The only DOM capability containment checks need (keeps the helpers testable). */
export interface SurfaceContainment {
  contains(target: unknown): boolean;
}

export interface OpenSurface {
  token: symbol;
  depth: number;
  element: SurfaceContainment | null;
}

/**
 * Every open popover surface. Nested surfaces are portalled to <body>, so an
 * ancestor cannot discover them through the DOM; this registry (plus the React
 * nesting depth) is how Escape picks the innermost surface and how an ancestor
 * recognises a pointer-down inside a descendant portal.
 */
const openSurfaces: OpenSurface[] = [];

export function isInsideSurface(
  target: unknown,
  self: SurfaceContainment | null | undefined,
  descendants: Iterable<SurfaceContainment>,
): boolean {
  if (self?.contains(target)) return true;
  for (const surface of descendants) {
    if (surface.contains(target)) return true;
  }
  return false;
}

/**
 * Escape belongs to the deepest open surface that contains the focused target,
 * so a Select/date picker inside a form closes itself instead of the form.
 */
export function isInnermostSurfaceAt(
  surfaces: readonly OpenSurface[],
  token: symbol,
  target: unknown,
): boolean {
  const own = surfaces.find((surface) => surface.token === token);
  if (!own) return false;
  return !surfaces.some(
    (surface) =>
      surface.token !== token &&
      surface.depth > own.depth &&
      surface.element?.contains(target) === true,
  );
}

interface PopoverNesting {
  /** Depth a popover rendered at this point would receive. */
  depth: number;
  register: (element: HTMLElement) => () => void;
}

const PopoverNestingContext = createContext<PopoverNesting | null>(null);

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
  /**
   * Render a full-window click-away layer under the surface. Needed for menus:
   * a click over the sandboxed mail iframe never reaches this document, so the
   * outside-pointerdown listener cannot see it.
   */
  dismissOnAnyClick?: boolean;
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
  dismissOnAnyClick = false,
  portal = true,
  children,
}: PopoverProps) {
  const contentRef = useRef<HTMLDivElement | null>(null);
  const [position, setPosition] = useState<Position | null>(null);
  const [contentSize, setContentSize] = useState<{ width: number; height: number } | null>(null);

  const parentNesting = useContext(PopoverNestingContext);
  const depth = parentNesting?.depth ?? 0;
  const surfaceToken = useRef<symbol>(Symbol("popover")).current;
  const descendantSurfaces = useRef(new Set<SurfaceContainment>());

  const registerDescendant = useCallback(
    (element: HTMLElement) => {
      descendantSurfaces.current.add(element);
      // Bubble it up too so a grandparent with an intermediate portal between
      // them still considers the click inside.
      const unregisterFromParent = parentNesting?.register(element);
      return () => {
        descendantSurfaces.current.delete(element);
        unregisterFromParent?.();
      };
    },
    [parentNesting],
  );

  const nesting = useMemo(
    () => ({ depth: depth + 1, register: registerDescendant }),
    [depth, registerDescendant],
  );

  useEffect(() => {
    if (!open) return;
    const close = () => onClose();
    window.addEventListener("nuncio:module-changed", close);
    return () => window.removeEventListener("nuncio:module-changed", close);
  }, [open, onClose]);

  useEffect(() => {
    if (!open) return;
    const entry: OpenSurface = { token: surfaceToken, depth, element: contentRef.current };
    openSurfaces.push(entry);
    return () => {
      const index = openSurfaces.indexOf(entry);
      if (index !== -1) openSurfaces.splice(index, 1);
    };
  }, [open, depth, surfaceToken]);

  // Let the enclosing popover treat this portal as part of its own surface.
  useEffect(() => {
    if (!open || !parentNesting || !contentRef.current) return;
    return parentNesting.register(contentRef.current);
  }, [open, parentNesting]);

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
      if (isInsideSurface(target, contentRef.current, descendantSurfaces.current)) return;
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
      // Only the innermost surface reacts. Otherwise Escape meant for a nested
      // Select/date picker would also close the popover or dialog hosting it.
      if (!isInnermostSurfaceAt(openSurfaces, surfaceToken, event.target)) return;
      event.preventDefault();
      event.stopPropagation();
      onClose();
    };
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [open, closeOnEscape, onClose, surfaceToken]);

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
    <PopoverNestingContext.Provider value={nesting}>
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
    </PopoverNestingContext.Provider>
  );

  // A point-anchored surface (context menu) must be dismissible by a click
  // anywhere. A click over the sandboxed mail iframe never reaches this
  // document's listeners, so it needs an in-document layer to catch it: the
  // click dismisses the menu instead of reaching what is underneath.
  const catcher = point || dismissOnAnyClick ? (
    <div
      className={styles.catcher}
      aria-hidden="true"
      onPointerDown={(event) => {
        event.preventDefault();
        onClose();
      }}
    />
  ) : null;

  const tree = (
    <>
      {catcher}
      {surface}
    </>
  );
  return portal ? createPortal(tree, document.body) : tree;
}
