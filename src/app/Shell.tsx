/**
 * Application shell: custom title strip, collapsible navigation pane with the
 * active module's sidebar, bottom module switcher / connectivity indicator,
 * settings flyout, global toasts and first-run onboarding.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import {
  Add24Regular,
  ArrowSync16Regular,
  Navigation24Regular,
  Settings24Regular,
  WifiOff20Regular,
} from "@fluentui/react-icons";
import { AddAccountDialog, SettingsPanel } from "@/modules/settings";
import {
  Button,
  IconButton,
  ModuleSyncDot,
  SidePanel,
  Tooltip,
  WindowControls,
  toast,
} from "@/components";
import { accounts, isIpcError, sync as syncApi } from "@/ipc/api";
import type { AppModule, SyncModule } from "@/ipc/types";
import { shouldShowFirstRun } from "@/lib/accounts";
import { installEventBridge } from "@/lib/eventBridge";
import { installHotkeys, registerHotkeys } from "@/lib/hotkeys";
import { useT } from "@/lib/i18n";
import { isMac } from "@/lib/platform";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useSettingsSync } from "@/lib/useSettings";
import { newActionLabelKeys, moduleOrder, modules } from "./modules";
import { FirstRun } from "./FirstRun";
import { ImportDialogs } from "./ImportDialogs";
import { Logo } from "./Logo";
import styles from "./Shell.module.css";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/**
 * Modules whose data is fetched by polling (PLAN §6.2). Mail is excluded: IMAP
 * accounts stay current through IDLE, so switching to the mail module does not
 * need to start a sync.
 */
const moduleSyncTargets: Partial<Record<AppModule, SyncModule>> = {
  calendar: "calendar",
  people: "contacts",
  todo: "tasks",
};

/**
 * The sync module a manual action targets. Mail has no polling entry above
 * because IDLE keeps it current, but a user-requested sync still has to name
 * its module so the backend does not widen it to every module.
 */
export function syncTargetFor(module: AppModule): SyncModule {
  return moduleSyncTargets[module] ?? "mail";
}

/** Do not re-sync a module more than once in this window when the user flips between modules. */
const MODULE_SYNC_THROTTLE_MS = 30_000;

export function Shell() {
  const t = useT();
  useSettingsSync();

  const activeModule = useShellStore((state) => state.activeModule);
  const railCollapsed = useShellStore((state) => state.railCollapsed);
  const showSettings = useShellStore((state) => state.showSettings);
  const settingsAccountId = useShellStore((state) => state.settingsAccountId);
  const showAddAccount = useShellStore((state) => state.showAddAccount);
  const setShowAddAccount = useShellStore((state) => state.setShowAddAccount);
  const firstRunSkipped = useShellStore((state) => state.firstRunSkipped);
  const toggleRail = useShellStore((state) => state.toggleRail);
  const setActiveModule = useShellStore((state) => state.setActiveModule);
  const setShowSettings = useShellStore((state) => state.setShowSettings);

  const moduleDef = modules[activeModule];
  const mac = isMac();

  const accountsQuery = useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accounts.list,
    staleTime: 60_000,
  });

  const [online, setOnline] = useState<boolean>(() =>
    typeof navigator === "undefined" ? true : navigator.onLine,
  );

  useEffect(() => installEventBridge(), []);
  useEffect(() => installHotkeys(), []);

  useEffect(() => {
    const update = () => setOnline(navigator.onLine);
    window.addEventListener("online", update);
    window.addEventListener("offline", update);
    return () => {
      window.removeEventListener("online", update);
      window.removeEventListener("offline", update);
    };
  }, []);

  const moduleSyncTimes = useRef<Partial<Record<AppModule, number>>>({});

  // PLAN §6.2: opening calendar / contacts / to-dos syncs that module right away.
  // Throttled per module so flipping between tabs does not hammer the backend.
  useEffect(() => {
    const target = moduleSyncTargets[activeModule];
    if (!target || !isTauri()) return;
    const now = Date.now();
    const previous = moduleSyncTimes.current[activeModule] ?? 0;
    if (now - previous < MODULE_SYNC_THROTTLE_MS) return;
    moduleSyncTimes.current[activeModule] = now;
    // Fire and forget: progress and failures are reported through the sync
    // status hub and module events, so this must not interrupt the user.
    syncApi.now(null, target).catch((error: unknown) => {
      // Allow the next module switch to retry after a transient failure.
      delete moduleSyncTimes.current[activeModule];
      console.warn(`[shell] background sync of ${target} failed`, error);
    });
  }, [activeModule]);

  const syncNow = useCallback(async () => {
    const module = syncTargetFor(activeModule);
    const moduleName = t(moduleDef.labelKey);
    try {
      await syncApi.now(null, module);
      toast.success(t("shell.syncDone", { module: moduleName }));
    } catch (error) {
      toast.error(t("shell.syncError", { module: moduleName }), errorMessage(error));
    }
  }, [activeModule, moduleDef.labelKey, t]);

  useEffect(() => {
    return registerHotkeys([
      {
        id: "shell.module.mail",
        keys: "Mod+1",
        handler: () => useShellStore.getState().setActiveModule("mail"),
      },
      {
        id: "shell.module.calendar",
        keys: "Mod+2",
        handler: () => useShellStore.getState().setActiveModule("calendar"),
      },
      {
        id: "shell.module.people",
        keys: "Mod+3",
        handler: () => useShellStore.getState().setActiveModule("people"),
      },
      {
        id: "shell.module.todo",
        keys: "Mod+4",
        handler: () => useShellStore.getState().setActiveModule("todo"),
      },
      {
        id: "shell.settings",
        keys: "Mod+,",
        handler: () => {
          const state = useShellStore.getState();
          state.setShowSettings(!state.showSettings);
        },
      },
      {
        id: "shell.sync",
        keys: "F5",
        allowInInputs: true,
        handler: () => {
          void syncNow();
        },
      },
    ]);
  }, [syncNow]);

  useEffect(() => {
    if (!moduleDef.hotkeys || moduleDef.hotkeys.length === 0) return;
    return registerHotkeys(moduleDef.hotkeys);
  }, [moduleDef]);

  useEffect(() => {
    document.title = `${t(moduleDef.labelKey)} — ${t("shell.appName")}`;
  }, [moduleDef.labelKey, t]);

  const newLabel = t(newActionLabelKeys[activeModule]);
  const syncNowLabel = t("shell.syncNow", { module: t(moduleDef.labelKey) });
  const onNew = moduleDef.onNew;

  // Core always seeds the built-in "local" account, so a fresh install still
  // returns one entry: onboarding depends on a *connected* account, not on an
  // empty list.
  const showFirstRun =
    accountsQuery.isSuccess &&
    shouldShowFirstRun(accountsQuery.data, firstRunSkipped);

  return (
    <div className={styles.shell}>
      <header
        className={`${styles.titleBar} ${mac ? styles.titleBarMac : styles.titleBarControls}`}
        data-tauri-drag-region
      >
        <IconButton
          icon={<Navigation24Regular />}
          label={t("shell.toggleRail")}
          onClick={toggleRail}
        />
        <span className={styles.brand} data-tauri-drag-region>
          <Logo size={20} />
          <span className={styles.brandName}>{t("shell.appName")}</span>
        </span>
        <div className={styles.dragSpacer} data-tauri-drag-region />
        {mac ? null : <WindowControls />}
      </header>

      <div className={styles.workArea}>
        <aside
          className={railCollapsed ? `${styles.nav} ${styles.rail}` : styles.nav}
          aria-label={t("shell.moduleSwitcher")}
        >
          {onNew ? (
            <div className={styles.navHeader}>
              {railCollapsed ? (
                <IconButton
                  icon={<Add24Regular />}
                  label={newLabel}
                  appearance="primary"
                  onClick={onNew}
                />
              ) : (
                <Button
                  appearance="primary"
                  icon={<Add24Regular />}
                  fullWidth
                  onClick={onNew}
                >
                  {newLabel}
                </Button>
              )}
            </div>
          ) : null}

          <div className={styles.navBody}>
            <moduleDef.Sidebar />
          </div>

          <div className={styles.navFooter}>
            <div className={styles.footerRow}>
              <button
                type="button"
                className={styles.syncNow}
                onClick={() => void syncNow()}
                aria-label={syncNowLabel}
                title={syncNowLabel}
              >
                <span className={styles.syncNowIcon} aria-hidden="true">
                  <ArrowSync16Regular />
                </span>
                <span className={styles.syncNowText}>{syncNowLabel}</span>
              </button>
              {!online ? (
                <span
                  className={styles.offlineIndicator}
                  role="status"
                  aria-label={t("shell.offlineIndicator")}
                  title={t("shell.offlineIndicator")}
                >
                  <WifiOff20Regular aria-hidden="true" />
                  <span className={styles.offlineIndicatorText}>
                    {t("shell.offlineIndicator")}
                  </span>
                </span>
              ) : null}
            </div>

            <div className={styles.switcher}>
              {moduleOrder.map((moduleId) => {
                const definition = modules[moduleId];
                const active = moduleId === activeModule;
                const button = (
                  <button
                    key={moduleId}
                    type="button"
                    className={`${styles.moduleButton} ${active ? styles.moduleButtonActive : ""}`}
                    aria-current={active ? "page" : undefined}
                    aria-label={t(definition.labelKey)}
                    onClick={() => setActiveModule(moduleId)}
                  >
                    <span className={styles.moduleIcon} aria-hidden="true">
                      <definition.Icon />
                    </span>
                    {!railCollapsed ? (
                      <span className={styles.moduleLabel}>{t(definition.labelKey)}</span>
                    ) : null}
                    <span className={styles.moduleDot}>
                      <ModuleSyncDot module={syncTargetFor(moduleId)} />
                    </span>
                  </button>
                );
                return railCollapsed ? (
                  <Tooltip
                    key={moduleId}
                    content={t(definition.labelKey)}
                    placement="right"
                    className={styles.railItemWrap}
                  >
                    {button}
                  </Tooltip>
                ) : (
                  button
                );
              })}
              <IconButton
                icon={<Settings24Regular />}
                label={t("shell.settings")}
                selected={showSettings}
                className={styles.settingsButton}
                aria-haspopup="dialog"
                onClick={() => setShowSettings(true)}
              />
            </div>
          </div>
        </aside>

        <main className={styles.main}>
          {!online ? (
            <div className={styles.offlineBanner} role="status">
              <WifiOff20Regular aria-hidden="true" />
              <span className={styles.offlineText}>{t("shell.offlineBanner")}</span>
              <Button size="small" appearance="subtle" onClick={() => void syncNow()}>
                {t("shell.offlineRetry")}
              </Button>
            </div>
          ) : null}

          <div key={activeModule} className={styles.mainBody}>
            <moduleDef.Main />
          </div>
        </main>
      </div>

      <SidePanel
        open={showSettings}
        onClose={() => setShowSettings(false)}
        title={t("shell.settings")}
        width={460}
      >
        <SettingsPanel accountId={settingsAccountId} onClose={() => setShowSettings(false)} />
      </SidePanel>

      <AddAccountDialog
        open={showAddAccount}
        onClose={() => setShowAddAccount(false)}
        onCreated={() => setShowAddAccount(false)}
      />

      <ImportDialogs />

      {showFirstRun ? <FirstRun /> : null}
    </div>
  );
}
