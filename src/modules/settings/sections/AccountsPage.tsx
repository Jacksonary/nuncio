/**
 * Accounts: the account list with per-account detail, signatures and the
 * advanced OAuth client-id settings collapsed at the bottom.
 */
import { useState } from "react";
import { useT } from "@/lib/i18n";
import type { Signature } from "@/ipc/types";
import { useAccounts } from "../lib/hooks";
import { AccountDetail } from "../components/AccountDetail";
import { AccountsContent } from "../components/AccountsSection";
import { Disclosure, Section, SettingsPage } from "../components/SettingsUI";
import { AdvancedContent } from "./AdvancedPage";
import { SignatureEditorPage, SignaturesContent } from "./SignaturesPage";

/** What the page shows instead of the list: one account or one signature. */
type SubView =
  | { kind: "account"; accountId: string }
  | { kind: "signature"; signature: Signature | null }
  | null;

export function AccountsPage({
  onBack,
  initialAccountId,
}: {
  onBack: () => void;
  initialAccountId?: string | null;
}) {
  const t = useT();
  const accountsQuery = useAccounts();
  // A deep link to one account lands on that account, not the list.
  const [view, setView] = useState<SubView>(
    initialAccountId ? { kind: "account", accountId: initialAccountId } : null,
  );

  const selected =
    view?.kind === "account"
      ? (accountsQuery.data ?? []).find((account) => account.id === view.accountId)
      : undefined;

  if (view?.kind === "signature") {
    return <SignatureEditorPage signature={view.signature} onBack={() => setView(null)} />;
  }

  if (selected) {
    return <AccountDetail account={selected} onBack={() => setView(null)} />;
  }

  return (
    <SettingsPage
      title={t("settings.accounts.title")}
      subtitle={t("settings.accounts.subtitle")}
      onBack={onBack}
    >
      <AccountsContent
        onSelect={(accountId) => setView({ kind: "account", accountId })}
      />
      <SignaturesContent onEdit={(signature) => setView({ kind: "signature", signature })} />
      <Section>
        <Disclosure label={t("settings.home.advanced")}>
          <AdvancedContent />
        </Disclosure>
      </Section>
    </SettingsPage>
  );
}
