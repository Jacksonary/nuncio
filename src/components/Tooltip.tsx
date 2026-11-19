/**
 * Hover/focus tooltip. Wraps its child in an inline-flex span, opens after a
 * short delay and closes immediately on leave/blur. The tooltip content is
 * rendered in a portal so it can escape clipped panes.
 */
import {
  cloneElement,
  isValidElement,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { Popover, type PopoverPlacement } from "./Popover";
import styles from "./Tooltip.module.css";

/**
 * True when focus landed on the element the way a keyboard user would put it
 * there. Browsers expose that as `:focus-visible`: it does not match when the
 * focus was moved by script after a mouse interaction, which is exactly the
 * auto-focus-the-first-control case. If the selector is unsupported, keep the
 * old behaviour (show) rather than hide the tooltip from keyboard users.
 */
export function isKeyboardFocus(target: Pick<Element, "matches"> | EventTarget | null): boolean {
  if (!target || typeof (target as Element).matches !== "function") return true;
  try {
    return (target as Element).matches(":focus-visible");
  } catch {
    return true;
  }
}

export interface TooltipProps {
  content: ReactNode;
  children: ReactNode;
  placement?: PopoverPlacement;
  delay?: number;
  disabled?: boolean;
  /** Allow pointer interaction with the tooltip content (rich tooltips). */
  interactive?: boolean;
  className?: string;
  /** Extra class applied to the tooltip surface. */
  contentClassName?: string;
}

export function Tooltip({
  content,
  children,
  placement = "top",
  delay = 500,
  disabled = false,
  interactive = false,
  className,
  contentClassName,
}: TooltipProps) {
  const id = useId();
  const anchorRef = useRef<HTMLSpanElement | null>(null);
  const timer = useRef<number | null>(null);
  const [open, setOpen] = useState(false);

  const clearTimer = () => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
  };

  const show = (immediate = false) => {
    if (disabled) return;
    clearTimer();
    if (immediate) {
      setOpen(true);
      return;
    }
    timer.current = window.setTimeout(() => {
      setOpen(true);
      timer.current = null;
    }, delay);
  };

  const hide = () => {
    clearTimer();
    setOpen(false);
  };

  useEffect(() => clearTimer, []);

  useEffect(() => {
    if (disabled) setOpen(false);
  }, [disabled]);

  let trigger = children;
  if (open && isValidElement(children)) {
    trigger = cloneElement(
      children as ReactElement<{ "aria-describedby"?: string }>,
      { "aria-describedby": id },
    );
  }

  return (
    <>
      <span
        ref={anchorRef}
        className={cn(styles.anchor, className)}
        onMouseEnter={() => show()}
        onMouseLeave={hide}
        onFocus={(event) => {
          // Only a keyboard focus explains itself. A panel or dialog that
          // moves focus to its first control on open is not the user pointing
          // at it, and a bubble popping up unprompted ("Back" over the arrow
          // the moment settings opens) is just noise.
          if (isKeyboardFocus(event.target)) show();
        }}
        onBlur={hide}
        onPointerDown={hide}
        onContextMenu={hide}
      >
        {trigger}
      </span>
      <Popover
        open={open && !disabled}
        onClose={hide}
        anchor={anchorRef.current}
        placement={placement}
        offset={6}
        role="tooltip"
        id={id}
        className={cn(styles.tooltip, interactive && styles.interactive, contentClassName)}
        style={{ zIndex: "var(--z-tooltip)" }}
        closeOnOutsideClick={false}
        closeOnEscape={false}
      >
        {content}
      </Popover>
    </>
  );
}

export interface RichTooltipProps extends Omit<TooltipProps, "content" | "interactive"> {
  title: ReactNode;
  description?: ReactNode;
  footer?: ReactNode;
}

/** Tooltip with a title, description and optional footer/actions. */
export function RichTooltip({
  title,
  description,
  footer,
  children,
  ...rest
}: RichTooltipProps) {
  return (
    <Tooltip
      {...rest}
      interactive={Boolean(footer)}
      content={
        <div>
          <div className={styles.title}>{title}</div>
          {description ? <div className={styles.description}>{description}</div> : null}
          {footer ? <div className={styles.footer}>{footer}</div> : null}
        </div>
      }
    >
      {children}
    </Tooltip>
  );
}
