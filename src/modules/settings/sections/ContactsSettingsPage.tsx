/** Contacts sync settings; embedded by the Calendar, contacts & tasks page. */
import { Select } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card } from "../components/SettingsUI";

export function ContactsSettingsContent() {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <Section title={t("settings.contacts.sync")}>
      <Card>
        <SettingRow label={t("settings.contacts.poll")} description={t("settings.contacts.pollDesc")}>
          <Select
            ariaLabel={t("settings.contacts.poll")}
            value={String(settings.contacts.pollMinutes)}
            onChange={(value) => update({ contacts: { pollMinutes: Number(value) } })}
            options={[5, 10, 15, 30, 60, 120].map((minutes) => ({
              value: String(minutes),
              label: t("settings.contacts.minutes", { count: minutes }),
            }))}
          />
        </SettingRow>
      </Card>
    </Section>
  );
}
