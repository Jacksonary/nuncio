import type { RecurrenceEditScope } from "@/ipc/types";
import { Button, Dialog } from "@/components";
import { useT } from "@/lib/i18n";
import styles from "./ScopeDialog.module.css";

export interface ScopeDialogProps {
  open: boolean;
  mode: "edit" | "delete";
  onCancel: () => void;
  onChoose: (scope: RecurrenceEditScope) => void;
}

const SCOPES: RecurrenceEditScope[] = ["this", "thisAndFollowing", "all"];

/** "This event / This and following / All events" chooser for recurring edits. */
export function ScopeDialog({ open, mode, onCancel, onChoose }: ScopeDialogProps) {
  const t = useT();
  return (
    <Dialog
      open={open}
      onClose={onCancel}
      title={t(mode === "delete" ? "calendar.scope.deleteTitle" : "calendar.scope.title")}
      width={420}
      footer={
        <Button appearance="subtle" onClick={onCancel}>
          {t("common.cancel")}
        </Button>
      }
    >
      <p className={styles.message}>
        {t(mode === "delete" ? "calendar.scope.deleteMessage" : "calendar.scope.message")}
      </p>
      <div className={styles.options}>
        {SCOPES.map((scope) => (
          <Button
            key={scope}
            appearance="outline"
            fullWidth
            className={styles.option}
            onClick={() => onChoose(scope)}
          >
            {t(
              scope === "this"
                ? "calendar.scope.this"
                : scope === "thisAndFollowing"
                  ? "calendar.scope.thisAndFollowing"
                  : "calendar.scope.all",
            )}
          </Button>
        ))}
      </div>
    </Dialog>
  );
}
