/** First-run welcome shown when no accounts are configured yet. */
import { useState } from "react";
import { Add24Regular, CheckmarkCircle16Regular } from "@fluentui/react-icons";
import { AddAccountDialog } from "@/modules/settings";
import { Button } from "@/components";
import { useT } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { Logo } from "./Logo";
import styles from "./FirstRun.module.css";

const FEATURE_KEYS = [
  "shell.firstRun.featureLocal",
  "shell.firstRun.featureUnified",
  "shell.firstRun.featureModules",
];

export function FirstRun() {
  const t = useT();
  const setFirstRunSkipped = useShellStore((state) => state.setFirstRunSkipped);
  const [addOpen, setAddOpen] = useState(false);

  return (
    <div className={styles.layer}>
      <div className={styles.dragStrip} data-tauri-drag-region />
      <div
        className={styles.card}
        role="dialog"
        aria-modal="true"
        aria-labelledby="first-run-title"
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
            autoFocus
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
