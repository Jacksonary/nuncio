/** Notifications: global switches, sound/preview and per-account toggles. */
import { Avatar, Switch, toast } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { useAccounts, useUpdateAccount } from "../lib/hooks";
import { describeError } from "../lib/errors";
import { SettingRow, Section, Card, SettingsPage } from "../components/SettingsUI";
import styles from "../settings.module.css";

export function NotificationsPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();
  const accountsQuery = useAccounts();
  const updateAccount = useUpdateAccount();

  const toggleAccount = (id: string, checked: boolean) => {
    updateAccount.mutate(
      { id, patch: { notificationsEnabled: checked } },
      {
        onError: (error) => {
          toast.error(t("settings.notifications.accountFailed"), describeError(error, t));
        },
      },
    );
  };

  return (
    <SettingsPage
      title={t("settings.notifications.title")}
      subtitle={t("settings.notifications.subtitle")}
      onBack={onBack}
    >
      <Section title={t("settings.notifications.general")}>
        <Card>
          <SettingRow label={t("settings.notifications.enabled")} description={t("settings.notifications.enabledDesc")}>
            <Switch
              checked={settings.notifications.enabled}
              onChange={(checked) => update({ notifications: { enabled: checked } })}
              aria-label={t("settings.notifications.enabledToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.notifications.sound")} description={t("settings.notifications.soundDesc")}>
            <Switch
              checked={settings.notifications.sound}
              disabled={!settings.notifications.enabled}
              onChange={(checked) => update({ notifications: { sound: checked } })}
              aria-label={t("settings.notifications.soundToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.notifications.preview")} description={t("settings.notifications.previewDesc")}>
            <Switch
              checked={settings.notifications.showPreview}
              disabled={!settings.notifications.enabled}
              onChange={(checked) => update({ notifications: { showPreview: checked } })}
              aria-label={t("settings.notifications.previewToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.notifications.eventReminders")} description={t("settings.notifications.eventRemindersDesc")}>
            <Switch
              checked={settings.notifications.eventReminders}
              disabled={!settings.notifications.enabled}
              onChange={(checked) => update({ notifications: { eventReminders: checked } })}
              aria-label={t("settings.notifications.eventRemindersToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.notifications.taskReminders")} description={t("settings.notifications.taskRemindersDesc")}>
            <Switch
              checked={settings.notifications.taskReminders}
              disabled={!settings.notifications.enabled}
              onChange={(checked) => update({ notifications: { taskReminders: checked } })}
              aria-label={t("settings.notifications.taskRemindersToggle")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.notifications.perAccount")} hint={t("settings.notifications.perAccountHint")}>
        <Card>
          {accountsQuery.data && accountsQuery.data.filter((account) => account.provider !== "local").length > 0 ? (
            accountsQuery.data
              .filter((account) => account.provider !== "local")
              .map((account) => (
                <div key={account.id} className={styles.row}>
                  <div className={styles.rowText}>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <Avatar name={account.displayName} email={account.email} size={24} />
                      <span className={styles.rowLabel}>{account.displayName}</span>
                    </div>
                    <div className={styles.rowDesc}>{account.email}</div>
                  </div>
                  <div className={styles.rowControl}>
                    <Switch
                      checked={account.notificationsEnabled}
                      disabled={!settings.notifications.enabled || updateAccount.isPending}
                      onChange={(checked) => toggleAccount(account.id, checked)}
                      aria-label={t("settings.notifications.accountToggle", { name: account.displayName })}
                    />
                  </div>
                </div>
              ))
          ) : (
            <div className={styles.emptyHint}>{t("settings.notifications.noAccounts")}</div>
          )}
        </Card>
      </Section>
    </SettingsPage>
  );
}
