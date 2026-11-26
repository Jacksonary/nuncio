/**
 * Shared bulk-action bar used by list panes (Mail, People, …): one accent row
 * with an optional leading control, the module's actions, and the selection
 * count plus an optional clear button pinned to the right edge.
 */
import type { ReactNode } from "react";
import { Dismiss16Regular } from "@fluentui/react-icons";
import { useT } from "@/lib/i18n";
import { IconButton } from "./IconButton";
import styles from "./BulkBar.module.css";

export interface BulkBarProps {
  /** Accessible name for the toolbar. */
  ariaLabel: string;
  /** Optional leading control, e.g. the mail select-all toggle. */
  leading?: ReactNode;
  /** Selected item count; the count label is hidden when 0 or omitted. */
  selectedCount?: number;
  /** Shows the clear button when provided. */
  onClear?: () => void;
  /** Module-specific action buttons. */
  children?: ReactNode;
}

export function BulkBar({
  ariaLabel,
  leading,
  selectedCount = 0,
  onClear,
  children,
}: BulkBarProps) {
  const t = useT();
  return (
    <div className={styles.bulkBar} role="toolbar" aria-label={ariaLabel}>
      {leading}
      {children}
      {selectedCount > 0 ? (
        <span className={styles.count}>
          {t("common.selectedCount", { count: selectedCount })}
        </span>
      ) : null}
      {onClear ? (
        <IconButton
          icon={<Dismiss16Regular />}
          label={t("common.cancel")}
          size="small"
          onClick={onClear}
        />
      ) : null}
    </div>
  );
}
