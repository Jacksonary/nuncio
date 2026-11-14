/**
 * Command bar / ribbon with automatic overflow into a "more" menu.
 * Items are measured off-screen and the ones that do not fit move into the
 * overflow menu (items marked overflow="always" always go there).
 */
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { MoreHorizontal16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Button } from "./Button";
import { IconButton } from "./IconButton";
import { Menu, type MenuEntry } from "./Menu";
import styles from "./CommandBar.module.css";

export interface CommandBarItem {
  id: string;
  label: ReactNode;
  icon?: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  danger?: boolean;
  appearance?: "primary" | "subtle" | "transparent";
  /** "always" hides it in the overflow menu; "never" keeps it visible. */
  overflow?: "auto" | "always" | "never";
  title?: string;
}

export interface CommandBarProps {
  items: CommandBarItem[];
  className?: string;
  ariaLabel?: string;
  overflowLabel?: string;
  compact?: boolean;
  /** Ribbon variant: slightly larger spacing between command groups. */
  ribbon?: boolean;
}

const GAP = 4;
const OVERFLOW_BUTTON_WIDTH = 36;

export function CommandBar({
  items,
  className,
  ariaLabel,
  overflowLabel,
  compact = false,
  ribbon = false,
}: CommandBarProps) {
  const t = useT();
  const containerRef = useRef<HTMLDivElement | null>(null);
  const measureRef = useRef<HTMLDivElement | null>(null);
  const [containerWidth, setContainerWidth] = useState(0);
  const [widths, setWidths] = useState<number[]>([]);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    setContainerWidth(element.getBoundingClientRect().width);
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) setContainerWidth(entry.contentRect.width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const row = measureRef.current;
    if (!row) return;
    const measured = Array.from(row.children).map(
      (child) => (child as HTMLElement).getBoundingClientRect().width,
    );
    setWidths((previous) => {
      if (
        previous.length === measured.length &&
        previous.every((value, index) => Math.abs(value - measured[index]) < 0.5)
      ) {
        return previous;
      }
      return measured;
    });
  }, [items, containerWidth]);

  const autoItems = items
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.overflow !== "always" && item.overflow !== "never");
  const alwaysOverflow = items.filter((item) => item.overflow === "always");
  const neverItems = items.filter((item) => item.overflow === "never");

  let visibleAutoCount = autoItems.length;
  if (containerWidth > 0 && widths.length === items.length) {
    const neverWidth = neverItems.reduce((sum, item) => {
      const index = items.indexOf(item);
      return sum + (widths[index] ?? 0) + GAP;
    }, 0);
    let used = neverWidth;
    let count = 0;
    for (let position = 0; position < autoItems.length; position += 1) {
      const width = widths[autoItems[position].index] ?? 0;
      const remainingAuto = autoItems.length - position - 1;
      const hasOverflowAfter = remainingAuto > 0 || alwaysOverflow.length > 0;
      const reserved = hasOverflowAfter ? OVERFLOW_BUTTON_WIDTH + GAP : 0;
      if (used + width + GAP + reserved <= containerWidth) {
        used += width + GAP;
        count += 1;
      } else {
        break;
      }
    }
    visibleAutoCount = count;
  }

  const visible: CommandBarItem[] = [];
  let autoSeen = 0;
  for (const item of items) {
    if (item.overflow === "always") continue;
    if (item.overflow === "never") {
      visible.push(item);
      continue;
    }
    if (autoSeen < visibleAutoCount) visible.push(item);
    autoSeen += 1;
  }
  const visibleIds = new Set(visible.map((item) => item.id));
  const overflowItems = items.filter((item) => !visibleIds.has(item.id));

  const menuItems: MenuEntry[] = overflowItems.map((item) => ({
    id: item.id,
    label: item.label,
    icon: item.icon,
    disabled: item.disabled,
    danger: item.danger,
    onSelect: item.onClick,
  }));

  return (
    <div
      ref={containerRef}
      className={cn(styles.commandBar, ribbon && styles.ribbon, className)}
      role="toolbar"
      aria-label={ariaLabel}
    >
      <div className={styles.visible}>
        {visible.map((item) => (
          <Button
            key={item.id}
            size={compact ? "small" : "medium"}
            appearance={item.appearance ?? "transparent"}
            icon={item.icon}
            disabled={item.disabled}
            onClick={item.onClick}
            title={item.title}
          >
            {item.label}
          </Button>
        ))}
      </div>
      <div className={styles.overflow}>
        <OverflowMenu
          items={menuItems}
          label={overflowLabel ?? t("common.moreOptions")}
          hidden={overflowItems.length === 0}
        />
      </div>

      <div className={styles.measureRow} ref={measureRef} aria-hidden="true">
        {items.map((item) => (
          <Button
            key={item.id}
            size={compact ? "small" : "medium"}
            appearance={item.appearance ?? "transparent"}
            icon={item.icon}
            className={styles.measureButton}
            tabIndex={-1}
          >
            {item.label}
          </Button>
        ))}
      </div>
    </div>
  );
}

interface OverflowMenuProps {
  items: MenuEntry[];
  label: string;
  hidden: boolean;
}

function OverflowMenu({ items, label, hidden }: OverflowMenuProps) {
  const buttonRef = useRef<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  if (hidden) return null;
  return (
    <>
      <IconButton
        ref={buttonRef}
        icon={<MoreHorizontal16Regular />}
        label={label}
        size="small"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      />
      <Menu
        open={open}
        onClose={() => setOpen(false)}
        items={items}
        anchor={buttonRef.current}
        placement="bottom-end"
      />
    </>
  );
}

export type RibbonProps = CommandBarProps;

/** Alias with ribbon spacing for composer/toolbar strips. */
export function Ribbon(props: RibbonProps) {
  return <CommandBar {...props} ribbon />;
}
