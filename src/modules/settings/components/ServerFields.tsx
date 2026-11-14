/** Host / port / security / username fields for one mail server. */
import { Input, Select } from "@/components";
import { useT } from "@/lib/i18n";
import type { Security } from "@/ipc/types";
import type { ServerDraft } from "../lib/validation";
import { StackRow } from "./SettingsUI";
import styles from "../settings.module.css";

export interface ServerFieldsProps {
  label: string;
  draft: ServerDraft;
  onChange: (draft: ServerDraft) => void;
  disabled?: boolean;
}

export function ServerFields({ label, draft, onChange, disabled }: ServerFieldsProps) {
  const t = useT();
  const patch = (next: Partial<ServerDraft>) => onChange({ ...draft, ...next });
  return (
    <StackRow>
      <div className={styles.sectionTitle}>{label}</div>
      <div className={styles.fieldGrid}>
        <div className={styles.span2}>
          <Input
            label={t("settings.account.serverHost")}
            value={draft.host}
            spellCheck={false}
            disabled={disabled}
            onChange={(event) => patch({ host: event.target.value })}
          />
        </div>
        <Input
          label={t("settings.account.serverPort")}
          value={draft.port}
          inputMode="numeric"
          disabled={disabled}
          onChange={(event) => patch({ port: event.target.value })}
        />
        <Select
          label={t("settings.account.serverSecurity")}
          value={draft.security}
          disabled={disabled}
          onChange={(value) => patch({ security: value as Security })}
          options={[
            { value: "tls", label: "TLS" },
            { value: "starttls", label: "STARTTLS" },
            { value: "none", label: t("settings.account.securityNone") },
          ]}
        />
        <div className={styles.span2}>
          <Input
            label={t("settings.account.serverUsername")}
            value={draft.username}
            placeholder={t("settings.account.serverUsernamePlaceholder")}
            spellCheck={false}
            autoComplete="off"
            disabled={disabled}
            onChange={(event) => patch({ username: event.target.value })}
          />
        </div>
      </div>
    </StackRow>
  );
}
