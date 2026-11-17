/** Data: storage usage, cache, encrypted backup export/import, logs. */
import { useState } from "react";
import {
  ArrowDownload24Regular,
  ArrowUpload24Regular,
  DocumentArrowDown24Regular,
  FolderOpen24Regular,
  HardDrive24Regular,
  Open24Regular,
} from "@fluentui/react-icons";
import { useQueryClient } from "@tanstack/react-query";
import {
  Button,
  Checkbox,
  Dialog,
  Input,
  ProgressBar,
  Spinner,
  confirm,
  toast,
} from "@/components";
import { useT } from "@/lib/i18n";
import { app } from "@/ipc/api";
import { formatFileSize } from "@/lib/format";
import { queryKeyPrefixes } from "@/lib/queryKeys";
import { describeError } from "../lib/errors";
import { isDesktop, pickOpenPath, pickSavePath, relaunchApp } from "../lib/desktop";
import { useInvalidateAllData, useStorageUsage } from "../lib/hooks";
import { Banner, Card, Section, SettingsPage, SettingRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

type BackupMode = "export" | "import";

export function DataPage({
  onBack,
  onOpenAbout,
}: {
  onBack: () => void;
  onOpenAbout?: () => void;
}) {
  const t = useT();
  const client = useQueryClient();
  const usage = useStorageUsage();
  const invalidateAll = useInvalidateAllData();
  const [backupMode, setBackupMode] = useState<BackupMode | null>(null);
  const [busy, setBusy] = useState(false);
  const [openLogBusy, setOpenLogBusy] = useState(false);

  const total = usage.data
    ? usage.data.databaseBytes + usage.data.mimeCacheBytes + usage.data.attachmentBytes
    : 0;

  const clearCache = async () => {
    const ok = await confirm({
      title: t("settings.data.clearCacheTitle"),
      message: t("settings.data.clearCacheMessage"),
      confirmLabel: t("settings.data.clearCache"),
      danger: true,
    });
    if (!ok) return;
    if (!isDesktop()) {
      toast.info(t("settings.data.desktopOnly"));
      return;
    }
    try {
      await app.clearCache();
      await client.invalidateQueries({ queryKey: ["storageUsage"] });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.messages });
      toast.success(t("settings.data.cacheCleared"));
    } catch (error) {
      toast.error(t("settings.data.clearCacheFailed"), describeError(error, t));
    }
  };

  const openLogs = async () => {
    if (!isDesktop()) {
      toast.info(t("settings.data.desktopOnly"));
      return;
    }
    setOpenLogBusy(true);
    try {
      await app.openLogDir();
    } catch (error) {
      toast.error(t("settings.data.openLogsFailed"), describeError(error, t));
    } finally {
      setOpenLogBusy(false);
    }
  };

  return (
    <SettingsPage title={t("settings.data.title")} subtitle={t("settings.data.subtitle")} onBack={onBack}>
      <Section
        title={t("settings.data.storage")}
        actions={
          <Button size="small" appearance="subtle" onClick={() => void usage.refetch()} loading={usage.isFetching}>
            {t("settings.data.refresh")}
          </Button>
        }
      >
        {usage.isLoading ? (
          <div className={styles.spinnerRow}>
            <Spinner size="small" />
            <span>{t("common.loading")}</span>
          </div>
        ) : usage.data ? (
          <>
            <div className={styles.storageGrid}>
              <div className={styles.storageCell}>
                <span className={styles.storageLabel}>{t("settings.data.database")}</span>
                <span className={styles.storageValue}>{formatFileSize(usage.data.databaseBytes)}</span>
              </div>
              <div className={styles.storageCell}>
                <span className={styles.storageLabel}>{t("settings.data.mimeCache")}</span>
                <span className={styles.storageValue}>{formatFileSize(usage.data.mimeCacheBytes)}</span>
              </div>
              <div className={styles.storageCell}>
                <span className={styles.storageLabel}>{t("settings.data.attachments")}</span>
                <span className={styles.storageValue}>{formatFileSize(usage.data.attachmentBytes)}</span>
              </div>
              <div className={styles.storageCell}>
                <span className={styles.storageLabel}>{t("settings.data.total")}</span>
                <span className={styles.storageValue}>{formatFileSize(total)}</span>
              </div>
            </div>
            <SettingRow
              label={t("settings.data.clearCache")}
              description={t("settings.data.clearCacheDesc")}
            >
              <Button appearance="subtle" icon={<HardDrive24Regular />} onClick={() => void clearCache()}>
                {t("settings.data.clearCacheAction")}
              </Button>
            </SettingRow>
          </>
        ) : (
          <Banner tone="warning" title={t("settings.data.storageUnavailable")}>
            {describeError(usage.error, t)}
          </Banner>
        )}
      </Section>

      <Section title={t("settings.data.backup")} hint={t("settings.data.backupHint")}>
        <Card>
          <SettingRow label={t("settings.data.export")} description={t("settings.data.exportDesc")}>
            <Button
              appearance="primary"
              icon={<ArrowDownload24Regular />}
              onClick={() => setBackupMode("export")}
            >
              {t("settings.data.exportAction")}
            </Button>
          </SettingRow>
          <SettingRow label={t("settings.data.import")} description={t("settings.data.importDesc")}>
            <Button icon={<ArrowUpload24Regular />} onClick={() => setBackupMode("import")}>
              {t("settings.data.importAction")}
            </Button>
          </SettingRow>
        </Card>
        <Banner tone="info">{t("settings.data.backupEncryption")}</Banner>
      </Section>

      <Section title={t("settings.data.diagnostics")}>
        <Card>
          <SettingRow label={t("settings.data.logs")} description={t("settings.data.logsDesc")}>
            <Button
              icon={<FolderOpen24Regular />}
              loading={openLogBusy}
              onClick={() => void openLogs()}
            >
              {t("settings.data.openLogs")}
            </Button>
          </SettingRow>
          {onOpenAbout ? (
            <SettingRow label={t("settings.data.about")} description={t("settings.data.aboutDesc")}>
              <Button icon={<Open24Regular />} onClick={onOpenAbout}>
                {t("settings.data.openAbout")}
              </Button>
            </SettingRow>
          ) : null}
        </Card>
      </Section>

      <BackupDialog
        mode={backupMode}
        onClose={() => setBackupMode(null)}
        busy={busy}
        setBusy={setBusy}
        onImported={() => {
          invalidateAll();
          setBackupMode(null);
          toast.success(t("settings.data.importSuccess"), undefined, {
            duration: 0,
            actions: [{ label: t("settings.general.restart"), primary: true, onClick: () => void relaunchApp() }],
          });
        }}
      />
    </SettingsPage>
  );
}

interface BackupDialogProps {
  mode: BackupMode | null;
  onClose: () => void;
  busy: boolean;
  setBusy: (value: boolean) => void;
  onImported: () => void;
}

function BackupDialog({ mode, onClose, busy, setBusy, onImported }: BackupDialogProps) {
  const t = useT();
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [includeCredentials, setIncludeCredentials] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setPassword("");
    setConfirmation("");
    setIncludeCredentials(false);
    setError(null);
  };

  const close = () => {
    if (busy) return;
    reset();
    onClose();
  };

  const submit = async () => {
    if (!mode) return;
    setError(null);
    if (!isDesktop()) {
      setError(t("settings.data.desktopOnly"));
      return;
    }
    if (password.length < 8) {
      setError(t("settings.data.passwordTooShort"));
      return;
    }
    if (mode === "export" && password !== confirmation) {
      setError(t("settings.data.passwordMismatch"));
      return;
    }

    setBusy(true);
    try {
      if (mode === "export") {
        const path = await pickSavePath(
          `nuncio-backup-${new Date().toISOString().slice(0, 10)}.nuncio-backup`,
          t("settings.data.export"),
        );
        if (!path) {
          setBusy(false);
          return;
        }
        await app.backupExport(path, password, includeCredentials);
        toast.success(t("settings.data.exportSuccess"));
        reset();
        onClose();
      } else {
        const path = await pickOpenPath(t("settings.data.import"));
        if (!path) {
          setBusy(false);
          return;
        }
        const ok = await confirm({
          title: t("settings.data.importConfirmTitle"),
          message: t("settings.data.importConfirmMessage"),
          confirmLabel: t("settings.data.import"),
          danger: true,
        });
        if (!ok) {
          setBusy(false);
          return;
        }
        await app.backupImport(path, password);
        reset();
        onImported();
      }
    } catch (err) {
      setError(describeError(err, t));
    } finally {
      setBusy(false);
    }
  };

  const exportMode = mode === "export";

  return (
    <Dialog
      open={mode !== null}
      onClose={close}
      title={exportMode ? t("settings.data.exportTitle") : t("settings.data.importTitle")}
      icon={exportMode ? <DocumentArrowDown24Regular /> : <ArrowUpload24Regular />}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" onClick={close} disabled={busy}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={busy} onClick={() => void submit()}>
            {exportMode ? t("settings.data.export") : t("settings.data.import")}
          </Button>
        </>
      }
    >
      <div className={styles.rowStack}>
        <p className={styles.hint}>
          {exportMode ? t("settings.data.exportPasswordHint") : t("settings.data.importPasswordHint")}
        </p>
        <Input
          type="password"
          label={t("settings.data.password")}
          value={password}
          autoFocus
          autoComplete="new-password"
          onChange={(event) => {
            setPassword(event.target.value);
            setError(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter" && exportMode) void submit();
          }}
        />
        {exportMode ? (
          <>
            <Input
              type="password"
              label={t("settings.data.passwordConfirm")}
              value={confirmation}
              autoComplete="new-password"
              onChange={(event) => {
                setConfirmation(event.target.value);
                setError(null);
              }}
            />
            <Checkbox
              label={t("settings.data.includeCredentials")}
              description={t("settings.data.includeCredentialsDesc")}
              checked={includeCredentials}
              onChange={(event) => setIncludeCredentials(event.target.checked)}
            />
          </>
        ) : null}
        {busy ? <ProgressBar indeterminate label={t("common.loading")} /> : null}
        {error ? <p className={styles.errorText}>{error}</p> : null}
      </div>
    </Dialog>
  );
}
