/** Settings flyout content: a Win11 Mail style category list with sub-pages. */
import { useState, type ReactNode } from "react";
import {
  Alert24Regular,
  CalendarLtr24Regular,
  ChevronRight24Regular,
  Database24Regular,
  Filter24Regular,
  Info24Regular,
  MailRead24Regular,
  PaintBrush24Regular,
  People24Regular,
  Person24Regular,
  Settings24Regular,
  Signature24Regular,
  TaskListLtr24Regular,
  Wrench24Regular,
} from "@fluentui/react-icons";
import { Button } from "@/components";
import { useT } from "@/lib/i18n";
import type { SettingsPanelProps } from "@/app/moduleTypes";
import { AccountsSection } from "./components/AccountsSection";
import { AppearancePage } from "./sections/AppearancePage";
import { ReadingPage } from "./sections/ReadingPage";
import { RulesPage } from "./sections/RulesPage";
import { NotificationsPage } from "./sections/NotificationsPage";
import { CalendarSettingsPage } from "./sections/CalendarSettingsPage";
import { ContactsSettingsPage } from "./sections/ContactsSettingsPage";
import { TasksSettingsPage } from "./sections/TasksSettingsPage";
import { SignaturesPage } from "./sections/SignaturesPage";
import { GeneralPage } from "./sections/GeneralPage";
import { DataPage } from "./sections/DataPage";
import { AdvancedPage } from "./sections/AdvancedPage";
import { AboutPage } from "./sections/AboutPage";
import styles from "./settings.module.css";

type SectionId =
  | "home"
  | "accounts"
  | "signatures"
  | "appearance"
  | "reading"
  | "rules"
  | "notifications"
  | "calendar"
  | "contacts"
  | "tasks"
  | "general"
  | "data"
  | "advanced"
  | "about";

interface HomeItem {
  id: Exclude<SectionId, "home">;
  icon: ReactNode;
  titleKey: string;
  descKey: string;
}

const HOME_ITEMS: HomeItem[] = [
  {
    id: "accounts",
    icon: <Person24Regular />,
    titleKey: "settings.home.accounts",
    descKey: "settings.home.accountsDesc",
  },
  {
    id: "signatures",
    icon: <Signature24Regular />,
    titleKey: "settings.home.signatures",
    descKey: "settings.home.signaturesDesc",
  },
  {
    id: "appearance",
    icon: <PaintBrush24Regular />,
    titleKey: "settings.home.appearance",
    descKey: "settings.home.appearanceDesc",
  },
  {
    id: "reading",
    icon: <MailRead24Regular />,
    titleKey: "settings.home.reading",
    descKey: "settings.home.readingDesc",
  },
  {
    id: "rules",
    icon: <Filter24Regular />,
    titleKey: "settings.home.rules",
    descKey: "settings.home.rulesDesc",
  },
  {
    id: "notifications",
    icon: <Alert24Regular />,
    titleKey: "settings.home.notifications",
    descKey: "settings.home.notificationsDesc",
  },
  {
    id: "calendar",
    icon: <CalendarLtr24Regular />,
    titleKey: "settings.home.calendar",
    descKey: "settings.home.calendarDesc",
  },
  {
    id: "contacts",
    icon: <People24Regular />,
    titleKey: "settings.home.contacts",
    descKey: "settings.home.contactsDesc",
  },
  {
    id: "tasks",
    icon: <TaskListLtr24Regular />,
    titleKey: "settings.home.tasks",
    descKey: "settings.home.tasksDesc",
  },
  {
    id: "general",
    icon: <Settings24Regular />,
    titleKey: "settings.home.general",
    descKey: "settings.home.generalDesc",
  },
  {
    id: "data",
    icon: <Database24Regular />,
    titleKey: "settings.home.data",
    descKey: "settings.home.dataDesc",
  },
  {
    id: "advanced",
    icon: <Wrench24Regular />,
    titleKey: "settings.home.advanced",
    descKey: "settings.home.advancedDesc",
  },
  {
    id: "about",
    icon: <Info24Regular />,
    titleKey: "settings.home.about",
    descKey: "settings.home.aboutDesc",
  },
];

export function SettingsPanel({ onClose }: SettingsPanelProps) {
  const t = useT();
  const [section, setSection] = useState<SectionId>("home");

  if (section === "home") {
    return (
      <div className={styles.root}>
        <div className={styles.homeIntro}>
          <div className={styles.homeSubtitle}>{t("settings.home.subtitle")}</div>
        </div>
        <nav className={styles.homeList} aria-label={t("settings.home.nav")}>
          {HOME_ITEMS.map((item) => (
            <button
              key={item.id}
              type="button"
              className={styles.homeRow}
              onClick={() => setSection(item.id)}
            >
              <span className={styles.homeIcon} aria-hidden="true">
                {item.icon}
              </span>
              <span className={styles.homeText}>
                <span className={styles.homeRowTitle}>{t(item.titleKey)}</span>
                <span className={styles.homeRowDesc}>{t(item.descKey)}</span>
              </span>
              <span className={styles.homeChevron} aria-hidden="true">
                <ChevronRight24Regular />
              </span>
            </button>
          ))}
        </nav>
        <div className={styles.homeFooter}>
          <Button appearance="subtle" onClick={onClose}>
            {t("common.close")}
          </Button>
        </div>
      </div>
    );
  }

  const back = () => setSection("home");

  switch (section) {
    case "accounts":
      return <AccountsSection onBack={back} />;
    case "signatures":
      return <SignaturesPage onBack={back} />;
    case "appearance":
      return <AppearancePage onBack={back} />;
    case "reading":
      return <ReadingPage onBack={back} />;
    case "rules":
      return <RulesPage onBack={back} />;
    case "notifications":
      return <NotificationsPage onBack={back} />;
    case "calendar":
      return <CalendarSettingsPage onBack={back} />;
    case "contacts":
      return <ContactsSettingsPage onBack={back} />;
    case "tasks":
      return <TasksSettingsPage onBack={back} />;
    case "general":
      return <GeneralPage onBack={back} />;
    case "data":
      return <DataPage onBack={back} onOpenAbout={() => setSection("about")} />;
    case "advanced":
      return <AdvancedPage onBack={back} />;
    case "about":
      return <AboutPage onBack={back} />;
    default:
      return null;
  }
}
