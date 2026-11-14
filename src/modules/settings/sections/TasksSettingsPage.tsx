/** To Do settings page. */
import { Select, Switch } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, SettingsPage } from "../components/SettingsUI";

export function TasksSettingsPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <SettingsPage title={t("settings.tasks.title")} subtitle={t("settings.tasks.subtitle")} onBack={onBack}>
      <Section title={t("settings.tasks.behavior")}>
        <Card>
          <SettingRow label={t("settings.tasks.showCompleted")} description={t("settings.tasks.showCompletedDesc")}>
            <Switch
              checked={settings.tasks.showCompleted}
              onChange={(checked) => update({ tasks: { showCompleted: checked } })}
              aria-label={t("settings.tasks.showCompletedToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.tasks.newOnTop")} description={t("settings.tasks.newOnTopDesc")}>
            <Switch
              checked={settings.tasks.newTasksOnTop}
              onChange={(checked) => update({ tasks: { newTasksOnTop: checked } })}
              aria-label={t("settings.tasks.newOnTopToggle")}
            />
          </SettingRow>
        </Card>
      </Section>
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
    </SettingsPage>
  );
}
