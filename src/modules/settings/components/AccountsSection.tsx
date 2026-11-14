/** Account management list with drag reordering and per-account details. */
import { useState } from "react";
import {
  Add24Regular,
  ArrowDown24Regular,
  ArrowUp24Regular,
  ReOrderDotsVertical24Regular,
} from "@fluentui/react-icons";
import { Avatar, Badge, Button, IconButton, toast } from "@/components";
import { useT } from "@/lib/i18n";
import type { Account } from "@/ipc/types";
import type { AccountsSectionProps } from "@/app/moduleTypes";
import { cn } from "@/lib/cn";
import { hasConnectedAccount, isLocalAccount } from "@/lib/accounts";
import { useAccounts, useReorderAccounts } from "../lib/hooks";
import { describeError } from "../lib/errors";
import { AddAccountDialog } from "../AddAccountDialog";
import { AccountDetail } from "./AccountDetail";
import { Section, SettingsPage, Banner } from "./SettingsUI";
import styles from "../settings.module.css";

export function AccountsSection({
  onAddAccount,
  onBack,
}: AccountsSectionProps & { onBack?: () => void }) {
  const t = useT();
  const accountsQuery = useAccounts();
  const reorder = useReorderAccounts();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [dragId, setDragId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);

  const accounts = accountsQuery.data ?? [];
  const selected = selectedId ? accounts.find((account) => account.id === selectedId) : null;

  const requestAdd = () => {
    if (onAddAccount) onAddAccount();
    else setDialogOpen(true);
  };

  const commitOrder = (ids: string[]) => {
    reorder.mutate(ids, {
      onError: (error) => toast.error(t("settings.accounts.reorderFailed"), describeError(error, t)),
    });
  };

  const move = (id: string, delta: number) => {
    const ids = accounts.map((account) => account.id);
    const from = ids.indexOf(id);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= ids.length) return;
    ids.splice(from, 1);
    ids.splice(to, 0, id);
    commitOrder(ids);
  };

  const handleDrop = (targetId: string) => {
    const sourceId = dragId;
    setDragId(null);
    setDragOverId(null);
    if (!sourceId || sourceId === targetId) return;
    const ids = accounts.map((account) => account.id);
    const from = ids.indexOf(sourceId);
    const to = ids.indexOf(targetId);
    if (from < 0 || to < 0) return;
    ids.splice(from, 1);
    ids.splice(to, 0, sourceId);
    commitOrder(ids);
  };

  if (selected) {
    return <AccountDetail account={selected} onBack={() => setSelectedId(null)} />;
  }

  return (
    <SettingsPage
      title={t("settings.accounts.title")}
      subtitle={t("settings.accounts.subtitle")}
      onBack={onBack}
      actions={
        <Button
          appearance="primary"
          size="small"
          icon={<Add24Regular />}
          onClick={requestAdd}
        >
          {t("settings.accounts.add")}
        </Button>
      }
    >
      {accountsQuery.isError ? (
        <Banner tone="error" title={t("settings.accounts.loadFailed")}>
          {describeError(accountsQuery.error, t)}
        </Banner>
      ) : null}

      {/* The built-in local account always exists, so an empty list is not the
          signal for "first run" — a configured (non-local) account is. */}
      {!accountsQuery.isLoading && !hasConnectedAccount(accounts) ? (
        <Banner tone="info" title={t("settings.accounts.emptyTitle")}>
          {t("settings.accounts.empty")}
        </Banner>
      ) : null}

      <Section>
        <div className={styles.accountList}>
          {accounts.map((account, index) => {
            const local = isLocalAccount(account);
            return (
              <div
                key={account.id}
                className={cn(
                  styles.accountRow,
                  local && styles.accountRowBuiltin,
                  dragOverId === account.id && dragId !== account.id && styles.accountRowDragOver,
                )}
                onDragOver={(event) => {
                  if (!dragId) return;
                  event.preventDefault();
                  if (dragOverId !== account.id) setDragOverId(account.id);
                }}
                onDragLeave={() => {
                  if (dragOverId === account.id) setDragOverId(null);
                }}
                onDrop={(event) => {
                  event.preventDefault();
                  handleDrop(account.id);
                }}
              >
                {local ? (
                  // The built-in local account is not reorderable.
                  <span className={styles.dragSpacer} aria-hidden="true" />
                ) : (
                  <span
                    className={styles.dragHandle}
                    draggable
                    aria-hidden="true"
                    title={t("settings.accounts.dragHint")}
                    onDragStart={(event) => {
                      setDragId(account.id);
                      event.dataTransfer.effectAllowed = "move";
                      try {
                        event.dataTransfer.setData("text/plain", account.id);
                      } catch {
                        // Some webviews disallow custom drag data; internal state is enough.
                      }
                    }}
                    onDragEnd={() => {
                      setDragId(null);
                      setDragOverId(null);
                    }}
                  >
                    <ReOrderDotsVertical24Regular />
                  </span>
                )}

                <button
                  type="button"
                  className={styles.accountMain}
                  onClick={() => setSelectedId(account.id)}
                >
                  <Avatar name={account.displayName} email={account.email} size={36} />
                  <span className={styles.accountMeta}>
                    <span className={styles.accountName}>{account.displayName || account.email}</span>
                    <span className={styles.accountEmail}>{account.email}</span>
                  </span>
                </button>

                <div className={styles.accountSide}>
                  {local ? (
                    <Badge appearance="neutral" title={t("settings.accounts.builtinHint")}>
                      {t("settings.accounts.builtin")}
                    </Badge>
                  ) : null}
                  <StateBadge account={account} />
                  {local ? null : (
                    <>
                      <IconButton
                        icon={<ArrowUp24Regular />}
                        label={t("settings.accounts.moveUp", { name: account.displayName })}
                        size="small"
                        appearance="subtle"
                        disabled={index === 0}
                        onClick={() => move(account.id, -1)}
                      />
                      <IconButton
                        icon={<ArrowDown24Regular />}
                        label={t("settings.accounts.moveDown", { name: account.displayName })}
                        size="small"
                        appearance="subtle"
                        disabled={index === accounts.length - 1}
                        onClick={() => move(account.id, 1)}
                      />
                    </>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </Section>

      <p className={styles.hint}>{t("settings.accounts.hint")}</p>

      {!onAddAccount ? (
        <AddAccountDialog
          open={dialogOpen}
          onClose={() => setDialogOpen(false)}
          onCreated={(account) => {
            setDialogOpen(false);
            setSelectedId(account.id);
          }}
        />
      ) : null}
    </SettingsPage>
  );
}

function StateBadge({ account }: { account: Account }) {
  const t = useT();
  switch (account.state) {
    case "needsReauth":
      return <Badge appearance="danger">{t("settings.account.state.needsReauth")}</Badge>;
    case "error":
      return <Badge appearance="warning">{t("settings.account.state.error")}</Badge>;
    case "syncing":
      return <Badge appearance="accent">{t("settings.account.state.syncing")}</Badge>;
    case "disabled":
      return <Badge appearance="neutral">{t("settings.account.state.disabled")}</Badge>;
    case "offline":
      return <Badge appearance="neutral">{t("settings.account.state.offline")}</Badge>;
    default:
      return null;
  }
}
