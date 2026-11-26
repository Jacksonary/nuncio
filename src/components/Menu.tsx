/**
 * Fluent-style menu with keyboard navigation, submenus and shortcut hints.
 *
 * Keyboard: Up/Down/Home/End move, Right opens a submenu, Left closes it,
 * Enter/Space activates, Tab/Escape close. Menus restore focus to the trigger.
 */
import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { Checkmark16Regular, ChevronRight16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { formatShortcut } from "@/lib/hotkeys";
import { isMac } from "@/lib/platform";
import { type PopoverPlacement, Popover } from "./Popover";
import { IconButton } from "./IconButton";
import styles from "./Menu.module.css";

export interface MenuItemDef {
  id: string;
  label: ReactNode;
  icon?: ReactNode;
  /** Shortcut spec (e.g. "Mod+Shift+R") rendered on the right. */
  shortcut?: string;
  disabled?: boolean;
  checked?: boolean;
  danger?: boolean;
  submenu?: MenuEntry[];
  onSelect?: () => void;
  /** Keep the menu open after activation (e.g. multi-select filters). */
  keepOpen?: boolean;
}

export interface MenuSeparatorDef {
  type: "separator";
  id?: string;
}

export interface MenuHeaderDef {
  type: "header";
  id?: string;
  label: ReactNode;
}

export type MenuEntry = MenuItemDef | MenuSeparatorDef | MenuHeaderDef;

export function isMenuItem(entry: MenuEntry): entry is MenuItemDef {
  return !("type" in entry) || (entry.type !== "separator" && entry.type !== "header");
}

export interface MenuProps {
  open: boolean;
  onClose: () => void;
  items: MenuEntry[];
  anchor?: HTMLElement | null;
  point?: { x: number; y: number } | null;
  placement?: PopoverPlacement;
  minWidth?: number;
  className?: string;
  ariaLabel?: string;
}

interface MenuLevelProps {
  items: MenuEntry[];
  onClose: () => void;
  autoFocus?: boolean;
  onBack?: () => void;
}

function MenuLevel({ items, onClose, autoFocus = false, onBack }: MenuLevelProps) {
  const t = useT();
  const mac = isMac();
  const [activeIndex, setActiveIndex] = useState(-1);
  const [submenu, setSubmenu] = useState<{
    index: number;
    element: HTMLElement;
    focusFirst: boolean;
  } | null>(null);
  const refs = useRef(new Map<number, HTMLButtonElement | null>());

  const focusableIndexes = items
    .map((entry, index) => (isMenuItem(entry) && !entry.disabled ? index : -1))
    .filter((index) => index >= 0);

  const focusIndex = (index: number) => {
    const button = refs.current.get(index);
    if (!button) return;
    setActiveIndex(index);
    button.focus();
  };

  useEffect(() => {
    if (!autoFocus) return;
    const first = focusableIndexes[0];
    if (first !== undefined) focusIndex(first);
    // Only run when the level mounts.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const move = (delta: number) => {
    if (focusableIndexes.length === 0) return;
    const current = focusableIndexes.indexOf(activeIndex);
    const nextPosition =
      current === -1
        ? delta > 0
          ? 0
          : focusableIndexes.length - 1
        : (current + delta + focusableIndexes.length) % focusableIndexes.length;
    focusIndex(focusableIndexes[nextPosition]);
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        event.stopPropagation();
        move(1);
        break;
      case "ArrowUp":
        event.preventDefault();
        event.stopPropagation();
        move(-1);
        break;
      case "Home":
        event.preventDefault();
        event.stopPropagation();
        if (focusableIndexes.length > 0) focusIndex(focusableIndexes[0]);
        break;
      case "End":
        event.preventDefault();
        event.stopPropagation();
        if (focusableIndexes.length > 0) {
          focusIndex(focusableIndexes[focusableIndexes.length - 1]);
        }
        break;
      case "ArrowRight": {
        const entry = items[activeIndex];
        if (entry && isMenuItem(entry) && entry.submenu && entry.submenu.length > 0) {
          event.preventDefault();
          event.stopPropagation();
          const element = refs.current.get(activeIndex) ?? null;
          if (element) setSubmenu({ index: activeIndex, element, focusFirst: true });
        }
        break;
      }
      case "ArrowLeft":
        if (onBack) {
          event.preventDefault();
          event.stopPropagation();
          onBack();
        }
        break;
      case "Tab":
        event.preventDefault();
        onClose();
        break;
      default:
        break;
    }
  };

  const submenuEntry = submenu ? items[submenu.index] : null;

  return (
    <div className={styles.menuLevel} role="none" onKeyDown={handleKeyDown}>
      {items.map((entry, index) => {
        if ("type" in entry && entry.type === "separator") {
          return (
            <div
              key={entry.id ?? `separator-${index}`}
              className={styles.separator}
              role="separator"
            />
          );
        }
        if ("type" in entry && entry.type === "header") {
          return (
            <div
              key={entry.id ?? `header-${index}`}
              className={styles.header}
              role="presentation"
            >
              {entry.label}
            </div>
          );
        }
        const item = entry;
        const hasSubmenu = Boolean(item.submenu && item.submenu.length > 0);
        const open = submenu?.index === index;
        return (
          <button
            key={item.id}
            ref={(element) => {
              refs.current.set(index, element);
            }}
            type="button"
            role={item.checked === undefined ? "menuitem" : "menuitemcheckbox"}
            aria-checked={item.checked}
            aria-haspopup={hasSubmenu ? "menu" : undefined}
            aria-expanded={hasSubmenu ? open : undefined}
            tabIndex={activeIndex === index ? 0 : -1}
            disabled={item.disabled}
            className={cn(
              styles.item,
              activeIndex === index && styles.itemActive,
              item.danger && styles.danger,
            )}
            onMouseEnter={(event) => {
              setActiveIndex(index);
              if (hasSubmenu) {
                setSubmenu({ index, element: event.currentTarget, focusFirst: false });
              } else {
                setSubmenu(null);
              }
            }}
            onFocus={() => setActiveIndex(index)}
            onClick={(event) => {
              if (hasSubmenu) {
                event.preventDefault();
                setSubmenu(
                  open
                    ? null
                    : { index, element: event.currentTarget, focusFirst: true },
                );
                return;
              }
              item.onSelect?.();
              if (!item.keepOpen) onClose();
            }}
          >
            <span className={styles.icon} aria-hidden="true">
              {item.checked ? <Checkmark16Regular /> : item.icon}
            </span>
            <span className={styles.label}>{item.label}</span>
            {item.shortcut ? (
              <span className={styles.shortcut} aria-label={t("common.specialKeys")}>
                {formatShortcut(item.shortcut, mac)}
              </span>
            ) : null}
            {hasSubmenu ? (
              <span className={styles.chevron} aria-hidden="true">
                <ChevronRight16Regular />
              </span>
            ) : null}
          </button>
        );
      })}

      {submenu && submenuEntry && isMenuItem(submenuEntry) && submenuEntry.submenu ? (
        <Popover
          open
          portal={false}
          onClose={() => setSubmenu(null)}
          anchor={submenu.element}
          placement="right-start"
          offset={2}
          role="menu"
          ariaLabel={t("common.menu")}
          className={styles.menu}
        >
          <MenuLevel
            items={submenuEntry.submenu}
            onClose={onClose}
            autoFocus={submenu.focusFirst}
            onBack={() => {
              const element = refs.current.get(submenu.index);
              setSubmenu(null);
              if (element) {
                window.requestAnimationFrame(() => element.focus());
              }
            }}
          />
        </Popover>
      ) : null}
    </div>
  );
}

export function Menu({
  open,
  onClose,
  items,
  anchor,
  point,
  placement = "bottom-start",
  minWidth = 220,
  className,
  ariaLabel,
}: MenuProps) {
  const t = useT();
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (open) {
      previousFocus.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
    } else if (previousFocus.current) {
      const target = previousFocus.current;
      previousFocus.current = null;
      target.focus?.();
    }
  }, [open]);

  return (
    <Popover
      open={open}
      onClose={onClose}
      anchor={anchor}
      point={point}
      placement={placement}
      minWidth={minWidth}
      role="menu"
      ariaLabel={ariaLabel ?? t("common.menu")}
      className={cn(styles.menu, className)}
      dismissOnAnyClick
    >
      <MenuLevel items={items} onClose={onClose} autoFocus />
    </Popover>
  );
}

export interface MenuButtonProps {
  label: string;
  icon: ReactNode;
  items: MenuEntry[];
  appearance?: "subtle" | "transparent" | "primary";
  size?: "small" | "medium" | "large";
  className?: string;
  /** When true the button shows the menu next to a text label instead. */
  tooltip?: ReactNode | false;
  disabled?: boolean;
}

/** An icon button that opens a menu. */
export function MenuButton({
  label,
  icon,
  items,
  appearance = "transparent",
  size = "medium",
  className,
  tooltip,
  disabled,
}: MenuButtonProps) {
  const buttonRef = useRef<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  return (
    <span className={styles.menuButtonWrap}>
      <IconButton
        ref={buttonRef}
        icon={icon}
        label={label}
        appearance={appearance}
        size={size}
        tooltip={tooltip}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        className={className}
        onClick={() => setOpen((value) => !value)}
      />
      <Menu
        open={open}
        onClose={() => setOpen(false)}
        items={items}
        anchor={buttonRef.current}
        ariaLabel={label}
      />
    </span>
  );
}

export interface ContextMenuProps {
  items: MenuEntry[];
  children: ReactNode;
  disabled?: boolean;
  className?: string;
}

/** Wraps content with a right-click menu at the pointer position. */
export function ContextMenu({ items, children, disabled = false, className }: ContextMenuProps) {
  const [point, setPoint] = useState<{ x: number; y: number } | null>(null);
  return (
    <div
      className={cn(styles.contextWrapper, className)}
      onContextMenu={(event) => {
        if (disabled) return;
        event.preventDefault();
        event.stopPropagation();
        setPoint({ x: event.clientX, y: event.clientY });
      }}
    >
      {children}
      <Menu
        open={point !== null}
        point={point}
        onClose={() => setPoint(null)}
        items={items}
      />
    </div>
  );
}
