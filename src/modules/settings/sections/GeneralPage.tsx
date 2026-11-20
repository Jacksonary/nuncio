/**
 * General: appearance and startup behaviour plus the ISPdb privacy switch.
 *
 * Updates moved to the About Nuncio page (`AboutPage`), which is now a
 * top-level entry rather than a sub-page of this one.
 */
import { Switch, toast } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { describeError } from "../lib/errors";
import { setAutostart } from "../lib/desktop";
import { Banner, Card, Section, SettingsPage, SettingRow } from "../components/SettingsUI";
import { AppearanceContent } from "./AppearancePage";

export function GeneralPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();

  const changeAutostart = async (checked: boolean) => {
    try {
      await setAutostart(checked);
      update({ launchAtStartup: checked });
    } catch (err) {
      toast.error(t("settings.general.autostartFailed"), describeError(err, t));
    }
  };

  return (
    <SettingsPage title={t("settings.general.title")} subtitle={t("settings.general.subtitle")} onBack={onBack}>
      <AppearanceContent />

      <Section title={t("settings.general.startup")}>
        <Card>
          <SettingRow label={t("settings.general.launchAtStartup")} description={t("settings.general.launchAtStartupDesc")}>
            <Switch
              checked={settings.launchAtStartup}
              onChange={(checked) => void changeAutostart(checked)}
              aria-label={t("settings.general.launchAtStartup")}
            />
          </SettingRow>
          <SettingRow label={t("settings.general.closeToTray")} description={t("settings.general.closeToTrayDesc")}>
            <Switch
              checked={settings.closeToTray}
              onChange={(checked) => update({ closeToTray: checked })}
              aria-label={t("settings.general.closeToTray")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.general.privacy")}>
        <Card>
          <SettingRow label={t("settings.general.ispdb")} description={t("settings.general.ispdbDesc")}>
            <Switch
              checked={settings.privacy.allowIspdbLookup}
              onChange={(checked) => update({ privacy: { allowIspdbLookup: checked } })}
              aria-label={t("settings.general.ispdb")}
            />
          </SettingRow>
        </Card>
        <Banner tone="info">{t("settings.general.privacyNote")}</Banner>
      </Section>
    </SettingsPage>
  );
}
