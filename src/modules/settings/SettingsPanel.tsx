/** Settings flyout content: a Win11 Mail style category list with sub-pages. */
import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
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
import { useSidePanelCloseGuard, isDraftTextField } from "@/components/SidePanel";
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

// Closing is the SidePanel header's job; the prop stays in the panel contract
// because the shell still passes it.
export function SettingsPanel({ accountId }: SettingsPanelProps) {
  const t = useT();
  // A deep link to one account should land on the accounts page with that
  // account already open, rather than on the index.
  const [section, setSection] = useState<SectionId>(accountId ? "accounts" : "home");
  const [dirty, setDirty] = useState(false);
  const closeGuard = useSidePanelCloseGuard();
  const queryClient = useQueryClient();
  const rootRef = useRef<HTMLDivElement | null>(null);
  // Fields the user typed into since the current page opened. They are dropped
  // once a save payload proves their value reached the backend, so the shared
  // close guard does not keep warning after a successful save.
  const draftFields = useRef(new Set<HTMLElement>());

  useEffect(() => {
    closeGuard?.setDirty(dirty);
    return () => closeGuard?.setDirty(false);
  }, [closeGuard, dirty]);

  // A field that left the page (an inline editor that was cancelled, a section
  // that collapsed) has nothing left to lose. Without this it stays in
  // `draftFields` and the panel keeps warning about an input nobody can see.
  useEffect(() => {
    const root = rootRef.current;
    if (!root || typeof MutationObserver === "undefined") return;
    const observer = new MutationObserver(() => {
      if (pruneDetachedFields(draftFields.current)) {
        setDirty(draftFields.current.size > 0);
      }
    });
    observer.observe(root, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    return queryClient.getMutationCache().subscribe((event) => {
      if (event.type !== "updated" || event.action.type !== "success") return;
      const variables = event.mutation.state.variables;
      let saved = false;
      for (const field of draftFields.current) {
        if (payloadCarriesDraft(variables, fieldValue(field))) {
          draftFields.current.delete(field);
          saved = true;
        }
      }
      if (saved) setDirty(draftFields.current.size > 0);
    });
  }, [queryClient]);

  const openSection = (next: SectionId) => {
    // Navigation discards the current page (and its draft), so the next panel
    // close must not warn about fields that are no longer mounted.
    draftFields.current.clear();
    setDirty(false);
    setSection(next);
  };

  const handleInput = (event: FormEvent<HTMLDivElement>) => {
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    // Nested dialogs render through a portal outside this container; their
    // drafts are protected by their own Dialog close handling.
    if (!rootRef.current?.contains(target)) return;
    if (!isDraftTextField(target)) return;
    draftFields.current.add(target);
    if (!dirty) setDirty(true);
  };

  const back = () => openSection("home");

  let content: ReactNode = null;
  if (section === "home") {
    content = (
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
              onClick={() => openSection(item.id)}
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
      </div>
    );
  } else {
    switch (section) {
      case "accounts":
        content = <AccountsSection onBack={back} initialAccountId={accountId} />;
        break;
      case "signatures":
        content = <SignaturesPage onBack={back} />;
        break;
      case "appearance":
        content = <AppearancePage onBack={back} />;
        break;
      case "reading":
        content = <ReadingPage onBack={back} />;
        break;
      case "rules":
        content = <RulesPage onBack={back} />;
        break;
      case "notifications":
        content = <NotificationsPage onBack={back} />;
        break;
      case "calendar":
        content = <CalendarSettingsPage onBack={back} />;
        break;
      case "contacts":
        content = <ContactsSettingsPage onBack={back} />;
        break;
      case "tasks":
        content = <TasksSettingsPage onBack={back} />;
        break;
      case "general":
        content = <GeneralPage onBack={back} />;
        break;
      case "data":
        content = <DataPage onBack={back} onOpenAbout={() => openSection("about")} />;
        break;
      case "advanced":
        content = <AdvancedPage onBack={back} />;
        break;
      case "about":
        content = <AboutPage onBack={back} />;
        break;
    }
  }

  return (
    // `display: contents` keeps the wrapper out of the layout while still
    // giving the panel one place to observe text input for the close guard.
    <div ref={rootRef} style={{ display: "contents" }} onInput={handleInput}>
      {content}
    </div>
  );
}

/**
 * Drop fields that are no longer attached to the document. Returns whether
 * anything was removed, so the caller only recomputes `dirty` when needed.
 */
export function pruneDetachedFields(fields: Set<{ isConnected: boolean }>): boolean {
  let removed = false;
  for (const field of Array.from(fields)) {
    if (!field.isConnected) {
      fields.delete(field);
      removed = true;
    }
  }
  return removed;
}

function fieldValue(field: HTMLElement): string {
  if (field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) {
    return field.value;
  }
  return field.textContent ?? "";
}

/**
 * True when a successful mutation's variables carry the text the user typed.
 * Settings drafts are submitted as a whole form, so a payload containing the
 * typed value proves that field was saved. Empty values match any write: there
 * is no text to restore, and some forms clear the input after saving it.
 */
export function payloadCarriesDraft(variables: unknown, draft: string): boolean {
  const needle = draft.trim();
  if (needle.length === 0) return true;
  return variablesCarry(variables, needle, new Set<unknown>());
}

function variablesCarry(value: unknown, needle: string, seen: Set<unknown>): boolean {
  if (typeof value === "string" || typeof value === "number") {
    return String(value).trim() === needle;
  }
  if (typeof value !== "object" || value === null || seen.has(value)) return false;
  seen.add(value);
  if (Array.isArray(value)) {
    return value.some((item) => variablesCarry(item, needle, seen));
  }
  return Object.values(value as Record<string, unknown>).some((item) =>
    variablesCarry(item, needle, seen),
  );
}
