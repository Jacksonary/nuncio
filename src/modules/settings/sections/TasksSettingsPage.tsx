/** To Do sync settings; embedded by the Calendar, contacts & tasks page. */
import { Select } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card } from "../components/SettingsUI";

export function TasksSettingsContent() {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <Section title={t("settings.tasks.sync")}>
      <Card>
        <SettingRow label={t("settings.tasks.poll")} description={t("settings.tasks.pollDesc")}>
          <Select
            ariaLabel={t("settings.tasks.poll")}
            value={String(settings.tasks.pollMinutes)}
            onChange={(value) => update({ tasks: { pollMinutes: Number(value) } })}
            options={[5, 10, 15, 30, 60, 120].map((minutes) => ({
              value: String(minutes),
              label: t("settings.tasks.minutes", { count: minutes }),
            }))}
          />
        </SettingRow>
      </Card>
    </Section>
  );
}
