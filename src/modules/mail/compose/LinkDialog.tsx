/**
 * Insert/edit link dialog (Ctrl+K). Validates the URL scheme and supports both
 * applying a link to the selection and inserting a new linked text.
 */
import { useEffect, useState } from "react";
import { Dialog, Input } from "@/components";
import { Button } from "@/components";
import { useT } from "@/lib/i18n";
import { normalizeLinkUrl } from "./logic";
import styles from "./ComposeView.module.css";

export interface LinkDialogProps {
  open: boolean;
  initialUrl: string;
  initialText: string;
  hasSelection: boolean;
  onClose: () => void;
  onSubmit: (url: string, text: string) => void;
  onRemove: () => void;
}

export function LinkDialog({
  open,
  initialUrl,
  initialText,
  hasSelection,
  onClose,
  onSubmit,
  onRemove,
}: LinkDialogProps) {
  const t = useT();
  const [url, setUrl] = useState(initialUrl);
  const [text, setText] = useState(initialText);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setUrl(initialUrl);
    setText(initialText);
    setError(null);
  }, [open, initialUrl, initialText]);

  const submit = () => {
    const normalized = normalizeLinkUrl(url);
    if (!normalized) {
      setError(t("compose.error.invalidLink"));
      return;
    }
    onSubmit(normalized, text.trim());
    onClose();
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("compose.dialog.linkTitle")}
      width={440}
      footer={
        <div className={styles.dialogFooter}>
          {initialUrl ? (
            <Button
              appearance="transparent"
              onClick={() => {
                onRemove();
                onClose();
              }}
            >
              {t("compose.dialog.linkRemove")}
            </Button>
          ) : null}
          <span className={styles.dialogSpacer} />
          <Button appearance="subtle" onClick={onClose}>
            {t("compose.dialog.cancel")}
          </Button>
          <Button appearance="primary" onClick={submit}>
            {t("compose.dialog.insert")}
          </Button>
        </div>
      }
    >
      <div className={styles.linkDialogBody}>
        <Input
          label={t("compose.dialog.linkUrl")}
          value={url}
          onChange={(event) => {
            setUrl(event.target.value);
            setError(null);
          }}
          error={error ?? undefined}
          placeholder="https://"
          autoFocus
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              submit();
            }
          }}
        />
        {hasSelection ? null : (
          <Input
            label={t("compose.dialog.linkText")}
            value={text}
            onChange={(event) => setText(event.target.value)}
            placeholder={url || "https://"}
          />
        )}
      </div>
    </Dialog>
  );
}
