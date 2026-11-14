/**
 * Attachment strip: chips with file name/size, inline badge, remove button and
 * a warning when the total exceeds 25 MB.
 */
import {
  Dismiss16Regular,
  DocumentText16Regular,
  Image16Regular,
} from "@fluentui/react-icons";
import { Badge, Button } from "@/components";
import type { DraftAttachment } from "@/ipc/types";
import { cn } from "@/lib/cn";
import { formatFileSize } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { summarizeAttachments } from "./logic";
import styles from "./ComposeView.module.css";

export interface AttachmentsBarProps {
  attachments: DraftAttachment[];
  onRemove: (attachment: DraftAttachment) => void;
  onAdd: () => void;
  compact?: boolean;
}

export function AttachmentsBar({
  attachments,
  onRemove,
  onAdd,
  compact = false,
}: AttachmentsBarProps) {
  const t = useT();
  const summary = summarizeAttachments(attachments);

  return (
    <div className={cn(styles.attachments, compact && styles.attachmentsCompact)}>
      <div className={styles.attachmentList}>
        {attachments.map((attachment) => {
          const inline = Boolean(attachment.contentId);
          const Icon = inline ? Image16Regular : DocumentText16Regular;
          return (
            <span
              key={attachment.id}
              className={cn(styles.attachmentChip, inline && styles.attachmentInline)}
              title={`${attachment.filename} (${formatFileSize(attachment.size)})`}
            >
              <span className={styles.attachmentIcon} aria-hidden="true">
                <Icon />
              </span>
              <span className={styles.attachmentName}>{attachment.filename}</span>
              {inline ? (
                <Badge appearance="neutral">{t("compose.attach.inline")}</Badge>
              ) : null}
              <span className={styles.attachmentSize}>
                {formatFileSize(attachment.size)}
              </span>
              <button
                type="button"
                className={styles.attachmentRemove}
                aria-label={t("compose.attach.remove", { name: attachment.filename })}
                onClick={() => onRemove(attachment)}
              >
                <Dismiss16Regular />
              </button>
            </span>
          );
        })}
      </div>
      <div className={styles.attachmentActions}>
        {summary.total > 0 ? (
          <span
            className={cn(styles.attachmentTotal, summary.tooLarge && styles.attachmentWarning)}
            title={
              summary.tooLarge
                ? t("compose.attach.tooLarge", { size: summary.formatted })
                : undefined
            }
          >
            {summary.tooLarge
              ? t("compose.attach.tooLarge", { size: summary.formatted })
              : t("compose.attach.total", { size: summary.formatted })}
          </span>
        ) : null}
        <Button
          size="small"
          appearance="transparent"
          icon={<DocumentText16Regular />}
          onClick={onAdd}
        >
          {t("compose.attach.add")}
        </Button>
      </div>
    </div>
  );
}
