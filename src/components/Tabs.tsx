import { useId, useRef, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import styles from "./Tabs.module.css";

export interface TabItem {
  id: string;
  label: ReactNode;
  icon?: ReactNode;
  count?: number;
  disabled?: boolean;
}

export interface TabsProps {
  items: TabItem[];
  value: string;
  onChange: (id: string) => void;
  appearance?: "underline" | "pill";
  className?: string;
  ariaLabel?: string;
}

/** Pivot/tab strip with roving focus and arrow-key navigation. */
export function Tabs({
  items,
  value,
  onChange,
  appearance = "underline",
  className,
  ariaLabel,
}: TabsProps) {
  const id = useId();
  const refs = useRef(new Map<string, HTMLButtonElement | null>());
  const enabled = items.filter((item) => !item.disabled);

  const focusTab = (tabId: string) => {
    refs.current.get(tabId)?.focus();
    onChange(tabId);
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (enabled.length === 0) return;
    const index = enabled.findIndex((item) => item.id === value);
    switch (event.key) {
      case "ArrowRight":
      case "ArrowDown": {
        event.preventDefault();
        const next = enabled[(Math.max(0, index) + 1) % enabled.length];
        focusTab(next.id);
        break;
      }
      case "ArrowLeft":
      case "ArrowUp": {
        event.preventDefault();
        const previous = enabled[(index - 1 + enabled.length) % enabled.length];
        focusTab(previous.id);
        break;
      }
      case "Home":
        event.preventDefault();
        focusTab(enabled[0].id);
        break;
      case "End":
        event.preventDefault();
        focusTab(enabled[enabled.length - 1].id);
        break;
      default:
        break;
    }
  };

  return (
    <div
      className={cn(styles.tabs, appearance === "pill" ? styles.pill : styles.underline, className)}
      role="tablist"
      aria-label={ariaLabel}
      onKeyDown={handleKeyDown}
    >
      {items.map((item) => {
        const active = item.id === value;
        return (
          <button
            key={item.id}
            ref={(element) => {
              refs.current.set(item.id, element);
            }}
            id={`${id}-${item.id}`}
            type="button"
            role="tab"
            aria-selected={active}
            aria-controls={`${id}-${item.id}-panel`}
            tabIndex={active ? 0 : -1}
            disabled={item.disabled}
            className={cn(styles.tab, active && styles.active)}
            onClick={() => onChange(item.id)}
          >
            {item.icon ? (
              <span className={styles.icon} aria-hidden="true">
                {item.icon}
              </span>
            ) : null}
            <span>{item.label}</span>
            {item.count !== undefined ? <span className={styles.count}>{item.count}</span> : null}
          </button>
        );
      })}
    </div>
  );
}

/** Helper for the panel side of a Tabs instance. */
export function tabPanelProps(baseId: string, tabId: string) {
  return {
    id: `${baseId}-${tabId}-panel`,
    role: "tabpanel" as const,
    "aria-labelledby": `${baseId}-${tabId}`,
    tabIndex: 0,
  };
}
