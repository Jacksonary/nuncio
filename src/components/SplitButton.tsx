import { useRef, useState, type ReactNode } from "react";
import { ChevronDown16Regular } from "@fluentui/react-icons";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { Button, type ButtonSize } from "./Button";
import { IconButton } from "./IconButton";
import { Menu, type MenuEntry } from "./Menu";
import styles from "./SplitButton.module.css";

export interface SplitButtonProps {
  label: ReactNode;
  icon?: ReactNode;
  onPrimary: () => void;
  items: MenuEntry[];
  appearance?: "primary" | "subtle";
  size?: ButtonSize;
  disabled?: boolean;
  menuLabel?: string;
  className?: string;
}

/**
 * Primary action plus a dropdown of related actions
 * (e.g. "Send" / "Send later", "New event" / "New meeting").
 */
export function SplitButton({
  label,
  icon,
  onPrimary,
  items,
  appearance = "primary",
  size = "medium",
  disabled = false,
  menuLabel,
  className,
}: SplitButtonProps) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  return (
    <span
      className={cn(
        styles.split,
        size === "small" && styles.small,
        size === "large" && styles.large,
        className,
      )}
    >
      <Button
        appearance={appearance}
        size={size}
        icon={icon}
        onClick={onPrimary}
        disabled={disabled}
        className={styles.primary}
      >
        {label}
      </Button>
      <IconButton
        ref={triggerRef}
        icon={<ChevronDown16Regular />}
        label={menuLabel ?? t("common.moreOptions")}
        appearance={appearance === "primary" ? "primary" : "subtle"}
        size={size}
        disabled={disabled}
        className={styles.menuTrigger}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      />
      <Menu
        open={open}
        onClose={() => setOpen(false)}
        items={items}
        anchor={triggerRef.current}
        placement="bottom-end"
      />
    </span>
  );
}
