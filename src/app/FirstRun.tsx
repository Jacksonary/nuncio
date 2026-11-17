/** First-run welcome shown when no accounts are configured yet. */
import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { Add24Regular, CheckmarkCircle16Regular } from "@fluentui/react-icons";
import { AddAccountDialog } from "@/modules/settings";
import { Button } from "@/components";
import { useT } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { Logo } from "./Logo";
import styles from "./FirstRun.module.css";

const FOCUSABLE =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

const FEATURE_KEYS = [
  "shell.firstRun.featureLocal",
  "shell.firstRun.featureUnified",
  "shell.firstRun.featureModules",
];

export function FirstRun() {
  const t = useT();
  const setFirstRunSkipped = useShellStore((state) => state.setFirstRunSkipped);
  const [addOpen, setAddOpen] = useState(false);
  const cardRef = useRef<HTMLDivElement | null>(null);

  // The card blocks the whole app, so keep Tab inside it; focus starts on the
  // card itself, mirroring Dialog's focus handling.
  useEffect(() => {
    cardRef.current?.focus();
  }, []);

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "Tab") return;
    const card = cardRef.current;
    if (!card) return;
    const focusable = Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
      (element) => element.offsetParent !== null || element === document.activeElement,
    );
    if (focusable.length === 0) {
      event.preventDefault();
      card.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const active = document.activeElement;
    if (event.shiftKey) {
      if (active === first || active === card) {
        event.preventDefault();
        last.focus();
      }
    } else if (active === last) {
      event.preventDefault();
      first.focus();
    }
  };

  return (
    <div className={styles.layer}>
      <div className={styles.dragStrip} data-tauri-drag-region />
      <div
        ref={cardRef}
        className={styles.card}
        role="dialog"
        aria-modal="true"
        aria-labelledby="first-run-title"
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <Logo size={64} title={t("shell.appName")} />
        <h1 id="first-run-title" className={styles.title}>
          {t("shell.firstRun.title")}
        </h1>
        <p className={styles.subtitle}>{t("shell.firstRun.subtitle")}</p>

        <ul className={styles.features}>
          {FEATURE_KEYS.map((key) => (
            <li key={key} className={styles.feature}>
              <span className={styles.featureIcon} aria-hidden="true">
                <CheckmarkCircle16Regular />
              </span>
              <span>{t(key)}</span>
            </li>
          ))}
        </ul>

        <div className={styles.actions}>
          <Button
            appearance="primary"
            size="large"
            icon={<Add24Regular />}
            onClick={() => setAddOpen(true)}
          >
            {t("shell.firstRun.addAccount")}
          </Button>
          <Button appearance="transparent" onClick={() => setFirstRunSkipped(true)}>
            {t("shell.firstRun.skip")}
          </Button>
        </div>

        <p className={styles.privacy}>{t("shell.firstRun.privacy")}</p>
      </div>

      <AddAccountDialog
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onCreated={() => {
          setAddOpen(false);
          setFirstRunSkipped(true);
        }}
      />
    </div>
  );
}
