/** Signatures: CRUD with a small rich editor and per-account assignment. */
import { useState } from "react";
import { Add24Regular, Delete24Regular, Edit24Regular } from "@fluentui/react-icons";
import {
  Badge,
  Button,
  IconButton,
  Input,
  Select,
  Switch,
  confirm,
  toast,
} from "@/components";
import { useT } from "@/lib/i18n";
import type { Signature } from "@/ipc/types";
import { useAccounts, useDeleteSignature, useSignatures, useUpsertSignature } from "../lib/hooks";
import { describeError } from "../lib/errors";
import { htmlToPlainText, sanitizeSignatureHtml } from "../lib/richText";
import { newId } from "../lib/id";
import { RichTextEditor } from "../components/RichTextEditor";
import { Card, SectionGroup, SettingsPage, SettingRow, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

interface SignatureDraft {
  id: string;
  isNew: boolean;
  name: string;
  accountId: string | null;
  html: string;
  useForNew: boolean;
  useForReply: boolean;
}

function toDraft(signature: Signature | null): SignatureDraft {
  return {
    id: signature?.id ?? newId(),
    isNew: signature === null,
    name: signature?.name ?? "",
    accountId: signature?.accountId ?? null,
    html: signature?.html ?? "",
    useForNew: signature?.useForNew ?? true,
    useForReply: signature?.useForReply ?? false,
  };
}

/** Signature editor; the Accounts page swaps its list out for this page. */
export function SignatureEditorPage({
  signature: initial,
  onBack,
}: {
  /** null creates a new signature. */
  signature: Signature | null;
  onBack: () => void;
}) {
  const t = useT();
  const accountsQuery = useAccounts();
  const upsert = useUpsertSignature();
  const remove = useDeleteSignature();
  const [draft, setDraft] = useState<SignatureDraft>(toDraft(initial));
  const [nameError, setNameError] = useState<string | null>(null);

  const patchDraft = (patch: Partial<SignatureDraft>) => {
    setDraft((current) => ({ ...current, ...patch }));
  };

  const save = async () => {
    if (!draft.name.trim()) {
      setNameError(t("settings.signatures.nameRequired"));
      return;
    }
    setNameError(null);
    const signature: Signature = {
      id: draft.id,
      accountId: draft.accountId,
      name: draft.name.trim(),
      html: sanitizeSignatureHtml(draft.html),
      useForNew: draft.useForNew,
      useForReply: draft.useForReply,
    };
    try {
      await upsert.mutateAsync(signature);
      toast.success(t("settings.signatures.saved", { name: signature.name }));
      onBack();
    } catch (error) {
      toast.error(t("settings.signatures.saveFailed"), describeError(error, t));
    }
  };

  const onDelete = async (signature: Signature) => {
    const ok = await confirm({
      title: t("settings.signatures.deleteTitle", { name: signature.name }),
      message: t("settings.signatures.deleteMessage"),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await remove.mutateAsync(signature.id);
      toast.success(t("settings.signatures.deleted"));
    } catch (error) {
      toast.error(t("settings.signatures.deleteFailed"), describeError(error, t));
    }
  };

  return (
    <SettingsPage
      title={draft.isNew ? t("settings.signatures.newTitle") : t("settings.signatures.editTitle")}
      subtitle={t("settings.signatures.editorSubtitle")}
      onBack={onBack}
    >
      <Card>
        <StackRow>
          <Input
            label={t("settings.signatures.name")}
            value={draft.name}
            autoFocus
            error={nameError ?? undefined}
            onChange={(event) => {
              patchDraft({ name: event.target.value });
              setNameError(null);
            }}
          />
        </StackRow>
        <StackRow>
          <Select
            label={t("settings.signatures.account")}
            value={draft.accountId ?? "all"}
            onChange={(value) => patchDraft({ accountId: value === "all" ? null : value })}
            options={[
              { value: "all", label: t("settings.signatures.allAccounts") },
              ...(accountsQuery.data ?? [])
                .filter((account) => account.provider !== "local")
                .map((account) => ({
                  value: account.id,
                  label: account.displayName || account.email,
                })),
            ]}
          />
        </StackRow>
        <StackRow>
          <div className={styles.rowLabel}>{t("settings.signatures.content")}</div>
          <RichTextEditor
            value={draft.html}
            onChange={(html) => patchDraft({ html })}
            placeholder={t("settings.signatures.contentPlaceholder")}
            ariaLabel={t("settings.signatures.content")}
          />
        </StackRow>
        <SettingRow label={t("settings.signatures.useForNew")} description={t("settings.signatures.useForNewDesc")}>
          <Switch
            checked={draft.useForNew}
            onChange={(checked) => patchDraft({ useForNew: checked })}
            aria-label={t("settings.signatures.useForNew")}
          />
        </SettingRow>
        <SettingRow label={t("settings.signatures.useForReply")} description={t("settings.signatures.useForReplyDesc")}>
          <Switch
            checked={draft.useForReply}
            onChange={(checked) => patchDraft({ useForReply: checked })}
            aria-label={t("settings.signatures.useForReply")}
          />
        </SettingRow>
      </Card>
      <div className={styles.actionsBetween}>
        <div>
          {initial ? (
            <Button
              appearance="danger"
              icon={<Delete24Regular />}
              onClick={() => void onDelete(initial)}
            >
              {t("common.delete")}
            </Button>
          ) : null}
        </div>
        <div className={styles.actions}>
          <Button appearance="subtle" onClick={onBack}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={upsert.isPending} onClick={() => void save()}>
            {t("common.save")}
          </Button>
        </div>
      </div>
    </SettingsPage>
  );
}

/** Signature list; embedded by the Accounts page. */
export function SignaturesContent({
  onEdit,
}: {
  /** null opens a new (empty) signature in the editor. */
  onEdit: (signature: Signature | null) => void;
}) {
  const t = useT();
  const signaturesQuery = useSignatures();
  const accountsQuery = useAccounts();
  const remove = useDeleteSignature();

  const accountName = (accountId: string | null) => {
    if (!accountId) return t("settings.signatures.allAccounts");
    const account = accountsQuery.data?.find((item) => item.id === accountId);
    return account ? account.displayName || account.email : accountId;
  };

  const onDelete = async (signature: Signature) => {
    const ok = await confirm({
      title: t("settings.signatures.deleteTitle", { name: signature.name }),
      message: t("settings.signatures.deleteMessage"),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await remove.mutateAsync(signature.id);
      toast.success(t("settings.signatures.deleted"));
    } catch (error) {
      toast.error(t("settings.signatures.deleteFailed"), describeError(error, t));
    }
  };

  return (
    <SectionGroup
      title={t("settings.signatures.title")}
      hint={t("settings.signatures.hint")}
      actions={
        <Button
          appearance="subtle"
          size="small"
          icon={<Add24Regular />}
          onClick={() => onEdit(null)}
        >
          {t("settings.signatures.new")}
        </Button>
      }
    >
      {signaturesQuery.data && signaturesQuery.data.length > 0 ? (
        <div className={styles.ruleList}>
          {signaturesQuery.data.map((signature) => (
            <div key={signature.id} className={styles.signatureCard}>
              <div className={styles.signatureHeader}>
                <div className={styles.signatureName}>{signature.name}</div>
                <div className={styles.listActions}>
                  <IconButton
                    icon={<Edit24Regular />}
                    label={t("common.edit")}
                    size="small"
                    appearance="subtle"
                    onClick={() => onEdit(signature)}
                  />
                  <IconButton
                    icon={<Delete24Regular />}
                    label={t("common.delete")}
                    size="small"
                    appearance="subtle"
                    onClick={() => void onDelete(signature)}
                  />
                </div>
              </div>
              <div className={styles.signaturePreview}>
                {htmlToPlainText(signature.html) || t("settings.signatures.emptyPreview")}
              </div>
              <div className={styles.badgeRow}>
                <Badge appearance="neutral">{accountName(signature.accountId)}</Badge>
                {signature.useForNew ? (
                  <Badge appearance="accent">{t("settings.signatures.useForNewBadge")}</Badge>
                ) : null}
                {signature.useForReply ? (
                  <Badge appearance="accent">{t("settings.signatures.useForReplyBadge")}</Badge>
                ) : null}
              </div>
            </div>
          ))}
        </div>
      ) : (
        <Card>
          <div className={styles.emptyHint}>
            {signaturesQuery.isLoading ? t("common.loading") : t("settings.signatures.empty")}
          </div>
        </Card>
      )}
    </SectionGroup>
  );
}

