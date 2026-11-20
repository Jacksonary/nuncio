/**
 * Update controls: the automatic-check switch plus a manual "check now" row
 * with its inline checking / available / downloading / ready / error states.
 *
 * Shared so the About page can host it directly; it used to live inside the
 * General page before updates and About were merged into one top-level entry.
 */
import { useRef, useState } from "react";
import { ProgressBar, Spinner, Switch } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { describeUpdaterError } from "../lib/errors";
import { isDesktop, relaunchApp } from "../lib/desktop";
import { runAutoUpdateCheck } from "../lib/autoUpdate";
import { markAutoCheckDone } from "../lib/updateThrottle";
import { useAppInfo } from "../lib/hooks";
import { Banner, Card, Section, SettingRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

interface UpdateHandle {
  version: string;
  currentVersion: string;
  date?: string;
  body?: string;
  downloadAndInstall: (onEvent?: (event: { event: string; data?: unknown }) => void) => Promise<void>;
}

type UpdatePhase = "idle" | "checking" | "uptodate" | "available" | "downloading" | "ready" | "error";

export function UpdateSection() {
  const t = useT();
  const { settings, update } = useSettings();
  const appInfo = useAppInfo();
  const [phase, setPhase] = useState<UpdatePhase>("idle");
  const [error, setError] = useState<string | null>(null);
  const [updateInfo, setUpdateInfo] = useState<{ version: string; notes?: string; date?: string } | null>(
    null,
  );
  const updateRef = useRef<UpdateHandle | null>(null);

  const changeAutoCheck = (checked: boolean) => {
    update({ updates: { autoCheck: checked } });
    // Give the switch immediate effect instead of waiting for the next launch.
    // The background check is throttled and reports through a toast.
    if (checked) void runAutoUpdateCheck({ assumeEnabled: true });
  };

  const checkForUpdates = async () => {
    if (!isDesktop()) {
      setPhase("error");
      setError(t("settings.general.updaterDesktopOnly"));
      return;
    }
    setPhase("checking");
    setError(null);
    setUpdateInfo(null);
    try {
      const { check } = await import("@tauri-apps/plugin-updater");
      const found = await check({ timeout: 30_000 });
      // A manual check satisfies the automatic 24 h window.
      markAutoCheckDone();
      if (!found) {
        setPhase("uptodate");
        return;
      }
      updateRef.current = found as unknown as UpdateHandle;
      setUpdateInfo({ version: found.version, notes: found.body ?? undefined, date: found.date });
      setPhase("available");
    } catch (err) {
      setPhase("error");
      setError(describeUpdaterError(err, t));
    }
  };

  const installUpdate = async () => {
    const handle = updateRef.current;
    if (!handle) return;
    setPhase("downloading");
    setError(null);
    try {
      await handle.downloadAndInstall();
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(describeUpdaterError(err, t));
    }
  };

  return (
    <Section title={t("settings.general.updates")}>
      <Card>
        <SettingRow label={t("settings.general.autoCheck")} description={t("settings.general.autoCheckDesc")}>
          <Switch
            checked={settings.updates.autoCheck}
            onChange={changeAutoCheck}
            aria-label={t("settings.general.autoCheck")}
          />
        </SettingRow>
        <SettingRow
          label={t("settings.general.checkNow")}
          description={t("settings.general.currentVersion", { version: appInfo.data?.version ?? "—" })}
        >
          <button
            type="button"
            className={styles.linkButton}
            onClick={() => void checkForUpdates()}
            disabled={phase === "checking" || phase === "downloading"}
          >
            {t("settings.general.checkNowButton")}
          </button>
        </SettingRow>
      </Card>

      {phase === "checking" ? (
        <div className={styles.spinnerRow}>
          <Spinner size="small" />
          <span>{t("settings.general.checking")}</span>
        </div>
      ) : null}

      {phase === "uptodate" ? (
        <Banner tone="success" title={t("settings.general.upToDate")}>
          {t("settings.general.upToDateDesc")}
        </Banner>
      ) : null}

      {phase === "available" && updateInfo ? (
        <Banner
          tone="info"
          title={t("settings.general.updateAvailable", { version: updateInfo.version })}
          actions={
            <div className={styles.actions}>
              <button type="button" className={styles.linkButton} onClick={() => setPhase("idle")}>
                {t("common.dismiss")}
              </button>
              <button
                type="button"
                className={styles.linkButton}
                onClick={() => void installUpdate()}
              >
                {t("settings.general.downloadInstall")}
              </button>
            </div>
          }
        >
          <div>{t("settings.general.updateAvailableDesc")}</div>
          {updateInfo.date ? (
            <div className={styles.hint}>
              {t("settings.general.released", { date: updateInfo.date })}
            </div>
          ) : null}
          {updateInfo.notes ? <div className={styles.hint}>{updateInfo.notes}</div> : null}
        </Banner>
      ) : null}

      {phase === "downloading" ? (
        <div className={styles.rowStack}>
          {/* `ProgressBar` renders `label` as its own visible heading, so a
              sibling label would render the same text twice. */}
          <ProgressBar indeterminate label={t("settings.general.downloading")} />
        </div>
      ) : null}

      {phase === "ready" ? (
        <Banner
          tone="success"
          title={t("settings.general.installed")}
          actions={
            <div className={styles.actions}>
              <button type="button" className={styles.linkButton} onClick={() => void relaunchApp()}>
                {t("settings.general.restart")}
              </button>
            </div>
          }
        >
          {t("settings.general.installedDesc")}
        </Banner>
      ) : null}

      {phase === "error" && error ? (
        <Banner tone="warning" title={t("settings.general.updaterFailed")}>
          {error}
        </Banner>
      ) : null}
    </Section>
  );
}
