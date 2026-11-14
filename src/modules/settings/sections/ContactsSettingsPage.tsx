/** Contacts settings page. */
import { Select } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, SettingsPage } from "../components/SettingsUI";

export function ContactsSettingsPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <SettingsPage title={t("settings.contacts.title")} subtitle={t("settings.contacts.subtitle")} onBack={onBack}>
      <Section title={t("settings.contacts.sorting")}>
        <Card>
          <SettingRow label={t("settings.contacts.sortBy")} description={t("settings.contacts.sortByDesc")}>
            <Select
              ariaLabel={t("settings.contacts.sortBy")}
              value={settings.contacts.sortBy}
              onChange={(value) =>
                update({ contacts: { sortBy: value as typeof settings.contacts.sortBy } })
              }
              options={[
                { value: "firstName", label: t("settings.contacts.sortFirstName") },
                { value: "lastName", label: t("settings.contacts.sortLastName") },
              ]}
            />
          </SettingRow>
          <SettingRow label={t("settings.contacts.displayOrder")} description={t("settings.contacts.displayOrderDesc")}>
            <Select
              ariaLabel={t("settings.contacts.displayOrder")}
              value={settings.contacts.displayOrder}
              onChange={(value) =>
                update({
                  contacts: { displayOrder: value as typeof settings.contacts.displayOrder },
                })
              }
              options={[
                { value: "firstLast", label: t("settings.contacts.orderFirstLast") },
                { value: "lastFirst", label: t("settings.contacts.orderLastFirst") },
              ]}
            />
          </SettingRow>
        </Card>
      </Section>
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
    </SettingsPage>
  );
}
