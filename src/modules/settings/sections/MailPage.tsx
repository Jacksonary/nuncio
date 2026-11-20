/** Mail: reading behaviour and rules on one page. */
import { useState } from "react";
import { useT } from "@/lib/i18n";
import type { MailRule } from "@/ipc/types";
import { createRule } from "../lib/ruleModel";
import { SettingsPage, SectionGroup } from "../components/SettingsUI";
import { ReadingContent } from "./ReadingPage";
import { RuleEditorPage, RulesContent } from "./RulesPage";

export function MailPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  // The rule editor replaces the page's body, like the old standalone page.
  const [draft, setDraft] = useState<{ rule: MailRule; isNew: boolean } | null>(null);

  if (draft) {
    return <RuleEditorPage rule={draft.rule} isNew={draft.isNew} onBack={() => setDraft(null)} />;
  }

  return (
    <SettingsPage title={t("settings.mail.title")} subtitle={t("settings.mail.subtitle")} onBack={onBack}>
      <SectionGroup title={t("settings.reading.title")} hint={t("settings.reading.subtitle")}>
        <ReadingContent />
      </SectionGroup>

      <SectionGroup title={t("settings.rules.title")} hint={t("settings.rules.subtitle")}>
        <RulesContent
          onEdit={(rule) => setDraft({ rule: rule ?? createRule(), isNew: rule === null })}
        />
      </SectionGroup>
    </SettingsPage>
  );
}
