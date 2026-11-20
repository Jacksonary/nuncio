/** Calendar, contacts & tasks: the three PIM module settings on one page. */
import { useT } from "@/lib/i18n";
import { SettingsPage, SectionGroup } from "../components/SettingsUI";
import { CalendarSettingsContent } from "./CalendarSettingsPage";
import { ContactsSettingsContent } from "./ContactsSettingsPage";
import { TasksSettingsContent } from "./TasksSettingsPage";

export function CalendarContactsTasksPage({ onBack }: { onBack: () => void }) {
  const t = useT();

  return (
    <SettingsPage title={t("settings.pim.title")} subtitle={t("settings.pim.subtitle")} onBack={onBack}>
      <SectionGroup title={t("settings.calendar.title")} hint={t("settings.calendar.subtitle")}>
        <CalendarSettingsContent />
      </SectionGroup>

      <SectionGroup title={t("settings.contacts.title")} hint={t("settings.contacts.subtitle")}>
        <ContactsSettingsContent />
      </SectionGroup>

      <SectionGroup title={t("settings.tasks.title")} hint={t("settings.tasks.subtitle")}>
        <TasksSettingsContent />
      </SectionGroup>
    </SettingsPage>
  );
}
