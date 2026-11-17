/**
 * One recipient line (To/Cc/Bcc): chips input plus drag-and-drop of existing
 * chips between the three lists. Dragging a chip onto another field moves the
 * address instead of duplicating it.
 */
import { useCallback, useRef, useState, type DragEvent, type ReactNode } from "react";
import { Avatar, TokenInput } from "@/components";
import type { Address } from "@/ipc/types";
import { cn } from "@/lib/cn";
import { formatAddress, formatAddressFull } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { parseRecipients, type RecipientField as RecipientFieldName } from "./logic";
import styles from "./ComposeHeader.module.css";

export const ADDRESS_MIME = "application/x-nuncio-address";
export const SOURCE_MIME = "application/x-nuncio-source";

export interface RecipientFieldProps {
  field: RecipientFieldName;
  label: string;
  value: Address[];
  onChange: (value: Address[]) => void;
  onDropAddress: (address: Address, from: RecipientFieldName | null) => void;
  suggestions?: (query: string) => Address[] | Promise<Address[]>;
  error?: ReactNode;
  trailing?: ReactNode;
  autoFocus?: boolean;
}

export function RecipientField({
  field,
  label,
  value,
  onChange,
  onDropAddress,
  suggestions,
  error,
  trailing,
  autoFocus,
}: RecipientFieldProps) {
  const t = useT();
  const [dragOver, setDragOver] = useState(false);
  const dragDepth = useRef(0);

  const handleDragEnter = useCallback((event: DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes(ADDRESS_MIME)) return;
    dragDepth.current += 1;
    setDragOver(true);
  }, []);

  const handleDragLeave = useCallback(() => {
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragOver(false);
  }, []);

  const handleDragOver = useCallback((event: DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes(ADDRESS_MIME)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
  }, []);

  const handleDrop = useCallback(
    (event: DragEvent<HTMLDivElement>) => {
      dragDepth.current = 0;
      setDragOver(false);
      if (!event.dataTransfer.types.includes(ADDRESS_MIME)) return;
      event.preventDefault();
      const raw = event.dataTransfer.getData(ADDRESS_MIME);
      if (!raw) return;
      let address: Address | null = null;
      try {
        const parsed = JSON.parse(raw) as Partial<Address>;
        if (parsed && typeof parsed.email === "string") {
          address = {
            name: typeof parsed.name === "string" ? parsed.name : null,
            email: parsed.email,
          };
        }
      } catch {
        // Fall back to "Name <email>" / plain e-mail text.
        const parsed = parseRecipients(raw);
        address = parsed.valid[0] ?? null;
      }
      if (!address) return;
      const sourceRaw = event.dataTransfer.getData(SOURCE_MIME);
      const source =
        sourceRaw === "to" || sourceRaw === "cc" || sourceRaw === "bcc"
          ? (sourceRaw as RecipientFieldName)
          : null;
      onDropAddress(address, source);
    },
    [onDropAddress],
  );

  const renderToken = useCallback(
    (address: Address) => (
      <span
        className={styles.dragToken}
        draggable
        title={t("compose.dragRecipient")}
        onDragStart={(event) => {
          event.dataTransfer.setData(ADDRESS_MIME, JSON.stringify(address));
          event.dataTransfer.setData(SOURCE_MIME, field);
          event.dataTransfer.effectAllowed = "move";
        }}
      >
        <Avatar name={address.name} email={address.email} size={18} />
        <span className={styles.dragTokenLabel} title={formatAddressFull(address)}>
          {formatAddress(address)}
        </span>
      </span>
    ),
    [field, t],
  );

  return (
    <div
      className={cn(styles.row, dragOver && styles.dropTarget)}
      onDragEnter={handleDragEnter}
      onDragLeave={handleDragLeave}
      onDragOver={handleDragOver}
      onDrop={handleDrop}
    >
      <span className={styles.label}>{label}</span>
      <div className={styles.control}>
        <TokenInput
          value={value}
          onChange={onChange}
          suggestions={suggestions}
          ariaLabel={label}
          autoFocus={autoFocus}
          showAvatars={false}
          renderToken={renderToken}
          error={error}
          className={styles.tokenInput}
        />
      </div>
      {trailing ? <div className={styles.trailing}>{trailing}</div> : null}
    </div>
  );
}
