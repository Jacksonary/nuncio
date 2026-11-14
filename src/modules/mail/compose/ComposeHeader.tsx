/**
 * Header fields: sender identity, To/Cc/Bcc recipient lines and subject.
 * Cc/Bcc can be revealed with the links next to the To field.
 */
import { useMemo, type ReactNode } from "react";
import { Dismiss16Regular } from "@fluentui/react-icons";
import { IconButton, Input, Select, type SelectOption } from "@/components";
import type { Account, Address, Draft, Identity } from "@/ipc/types";
import { formatAddress } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { invalidRecipients, type RecipientField as RecipientFieldName } from "./logic";
import { RecipientField } from "./RecipientField";
import styles from "./ComposeHeader.module.css";

export interface ComposeHeaderProps {
  draft: Draft;
  accounts: Account[];
  identities: Identity[];
  identitiesLoading: boolean;
  showCc: boolean;
  showBcc: boolean;
  onToggleCc: () => void;
  onToggleBcc: () => void;
  onPatch: (patch: Partial<Draft>) => void;
  onIdentitySelected: (identity: Identity | null) => void;
  onMoveRecipient: (
    address: Address,
    from: RecipientFieldName,
    to: RecipientFieldName,
  ) => void;
  suggestions: (query: string) => Address[] | Promise<Address[]>;
  autoFocusTo?: boolean;
}

function accountOf(accounts: Account[], accountId: string): Account | null {
  return accounts.find((account) => account.id === accountId) ?? null;
}

function ColorDot({ color }: { color: string | null | undefined }) {
  return (
    <span
      className={styles.colorDot}
      style={color ? { background: color } : undefined}
      aria-hidden="true"
    />
  );
}

export function ComposeHeader({
  draft,
  accounts,
  identities,
  identitiesLoading,
  showCc,
  showBcc,
  onToggleCc,
  onToggleBcc,
  onPatch,
  onIdentitySelected,
  onMoveRecipient,
  suggestions,
  autoFocusTo = false,
}: ComposeHeaderProps) {
  const t = useT();

  const { options, value } = useMemo(() => {
    const items: SelectOption[] = identities.map((identity) => {
      const account = accountOf(accounts, identity.accountId);
      return {
        value: `identity:${identity.id}`,
        icon: <ColorDot color={account?.color} />,
        label: (
          <span className={styles.identityOption}>
            <span className={styles.identityName}>
              {identity.name || identity.email}
            </span>
            {identity.name ? (
              <span className={styles.identityEmail}>{identity.email}</span>
            ) : null}
          </span>
        ),
      };
    });

    const currentEmail = draft.from.email.trim().toLowerCase();
    const explicit = draft.identityId
      ? identities.find((identity) => identity.id === draft.identityId)
      : undefined;
    const matching = identities.find(
      (identity) => identity.email.trim().toLowerCase() === currentEmail,
    );
    let selectedValue: string;
    if (explicit) {
      selectedValue = `identity:${explicit.id}`;
    } else if (matching) {
      selectedValue = `identity:${matching.id}`;
    } else {
      selectedValue = `manual:${draft.from.email}`;
      items.unshift({
        value: selectedValue,
        icon: <ColorDot color={accountOf(accounts, draft.accountId)?.color} />,
        label: (
          <span className={styles.identityOption}>
            <span className={styles.identityName}>
              {draft.from.name || draft.from.email || t("compose.identity.manual")}
            </span>
            <span className={styles.identityEmail}>{draft.from.email}</span>
          </span>
        ),
      });
    }
    return { options: items, value: selectedValue };
  }, [accounts, draft.accountId, draft.from, draft.identityId, identities, t]);

  const handleIdentityChange = (next: string) => {
    if (next.startsWith("identity:")) {
      const id = next.slice("identity:".length);
      const identity = identities.find((item) => item.id === id) ?? null;
      if (!identity) return;
      onPatch({
        identityId: identity.id,
        accountId: identity.accountId,
        from: { name: identity.name || null, email: identity.email },
      });
      onIdentitySelected(identity);
      return;
    }
    if (next.startsWith("manual:")) {
      const email = next.slice("manual:".length);
      onPatch({
        identityId: null,
        from: { name: null, email },
      });
      onIdentitySelected(null);
    }
  };

  const invalidTo = invalidRecipients(draft.to);
  const invalidCc = invalidRecipients(draft.cc);
  const invalidBcc = invalidRecipients(draft.bcc);

  const invalidError = (emails: string[]): ReactNode =>
    emails.length > 0
      ? t("compose.error.invalidRecipients", { emails: emails.join(", ") })
      : undefined;

  const recipientTrailing: ReactNode =
    !showCc || !showBcc ? (
      <div className={styles.toggles}>
        {!showCc ? (
          <button type="button" className={styles.linkButton} onClick={onToggleCc}>
            {draft.cc.length > 0
              ? t("compose.ccCount", { count: draft.cc.length })
              : t("compose.showCc")}
          </button>
        ) : null}
        {!showBcc ? (
          <button type="button" className={styles.linkButton} onClick={onToggleBcc}>
            {draft.bcc.length > 0
              ? t("compose.bccCount", { count: draft.bcc.length })
              : t("compose.showBcc")}
          </button>
        ) : null}
      </div>
    ) : null;

  const hideButton = (label: string, onHide: () => void): ReactNode => (
    <IconButton
      icon={<Dismiss16Regular />}
      label={label}
      size="small"
      onClick={onHide}
    />
  );

  return (
    <div className={styles.header}>
      <div className={styles.row}>
        <span className={styles.label}>{t("compose.from")}</span>
        <div className={styles.control}>
          <Select
            value={value}
            onChange={handleIdentityChange}
            options={options}
            ariaLabel={t("compose.from")}
            placeholder={
              identitiesLoading ? t("compose.loading") : formatAddress(draft.from)
            }
            disabled={identitiesLoading || options.length === 0}
            size="small"
          />
        </div>
      </div>

      <RecipientField
        field="to"
        label={t("compose.to")}
        value={draft.to}
        onChange={(to) => onPatch({ to })}
        onDropAddress={(address, from) => {
          if (from) onMoveRecipient(address, from, "to");
        }}
        suggestions={suggestions}
        error={invalidError(invalidTo)}
        trailing={recipientTrailing}
        autoFocus={autoFocusTo}
      />

      {showCc ? (
        <RecipientField
          field="cc"
          label={t("compose.cc")}
          value={draft.cc}
          onChange={(cc) => onPatch({ cc })}
          onDropAddress={(address, from) => {
            if (from) onMoveRecipient(address, from, "cc");
          }}
          suggestions={suggestions}
          error={invalidError(invalidCc)}
          trailing={hideButton(t("compose.hideCc"), onToggleCc)}
        />
      ) : null}

      {showBcc ? (
        <RecipientField
          field="bcc"
          label={t("compose.bcc")}
          value={draft.bcc}
          onChange={(bcc) => onPatch({ bcc })}
          onDropAddress={(address, from) => {
            if (from) onMoveRecipient(address, from, "bcc");
          }}
          suggestions={suggestions}
          error={invalidError(invalidBcc)}
          trailing={hideButton(t("compose.hideBcc"), onToggleBcc)}
        />
      ) : null}

      <div className={styles.row}>
        <span className={styles.label}>{t("compose.subject")}</span>
        <div className={styles.control}>
          <Input
            value={draft.subject}
            onChange={(event) => onPatch({ subject: event.target.value })}
            placeholder={t("compose.subjectPlaceholder")}
            aria-label={t("compose.subject")}
            autoComplete="off"
            spellCheck={false}
            containerClassName={styles.subjectField}
          />
        </div>
      </div>
    </div>
  );
}
