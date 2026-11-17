/**
 * Per-account settings page: identity fields, servers, credentials, sync
 * options, folder selection, aliases and deletion.
 */
import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Add24Regular,
  Delete24Regular,
  Edit24Regular,
  Key24Regular,
} from "@fluentui/react-icons";
import {
  Avatar,
  Badge,
  Button,
  Checkbox,
  ColorPicker,
  IconButton,
  Input,
  Select,
  Switch,
  confirm,
  toast,
} from "@/components";
import { useT } from "@/lib/i18n";
import { accounts } from "@/ipc/api";
import type { Account, AccountPatch, Folder, Identity, SentCopyMode } from "@/ipc/types";
import { isValidEmail } from "@/lib/format";
import {
  useDavPasswordStatus,
  useDeleteAccount,
  useDeleteIdentity,
  useFolders,
  useIdentities,
  useReauthAccount,
  useSetDavPassword,
  useSetFolderSync,
  useSignatures,
  useUpdateAccount,
  useUpdateAccountPassword,
  useUpsertIdentity,
} from "../lib/hooks";
import { describeError } from "../lib/errors";
import { newId } from "../lib/id";
import { ServerFields } from "./ServerFields";
import {
  SYNC_WINDOW_DAYS,
  isValidPort,
  serverDraftFromConfig,
  serverFromDraft,
  type ServerDraft,
} from "../lib/validation";
import {
  Banner,
  Card,
  Hint,
  Section,
  SettingsPage,
  SettingRow,
  StackRow,
} from "./SettingsUI";
import styles from "../settings.module.css";

export function AccountDetail({
  account,
  onBack,
}: {
  account: Account;
  onBack: () => void;
}) {
  const t = useT();
  const updateAccount = useUpdateAccount();
  const deleteAccount = useDeleteAccount();
  const reauth = useReauthAccount();
  const updatePassword = useUpdateAccountPassword();
  const [displayName, setDisplayName] = useState(account.displayName);

  useEffect(() => {
    setDisplayName(account.displayName);
  }, [account.displayName, account.id]);

  const isLocal = account.provider === "local";
  const nameDirty = displayName.trim() !== account.displayName && displayName.trim().length > 0;

  const saveName = () => {
    if (!nameDirty) return;
    updateAccount.mutate(
      { id: account.id, patch: { displayName: displayName.trim() } },
      {
        onError: (error) => toast.error(t("settings.account.saveFailed"), describeError(error, t)),
      },
    );
  };

  const patchAccount = (patch: AccountPatch) => {
    updateAccount.mutate(
      { id: account.id, patch },
      {
        onError: (error) => toast.error(t("settings.account.saveFailed"), describeError(error, t)),
      },
    );
  };

  const doReauth = async () => {
    try {
      await reauth.mutateAsync(account.id);
      toast.success(t("settings.account.reauthSuccess"));
    } catch (error) {
      toast.error(t("settings.account.reauthFailed"), describeError(error, t));
    }
  };

  const onDelete = async () => {
    const ok = await confirm({
      title: t("settings.account.deleteTitle", { name: account.displayName || account.email }),
      message: t("settings.account.deleteMessage"),
      confirmLabel: t("settings.account.deleteConfirm"),
      danger: true,
    });
    if (!ok) return;
    try {
      await deleteAccount.mutateAsync(account.id);
      toast.success(t("settings.account.deleted"));
      onBack();
    } catch (error) {
      toast.error(t("settings.account.deleteFailed"), describeError(error, t));
    }
  };

  return (
    <SettingsPage
      title={account.displayName || account.email}
      subtitle={account.email}
      onBack={onBack}
    >
      {account.state === "needsReauth" ? (
        <Banner
          tone="warning"
          title={t("settings.account.needsReauth")}
          actions={
            account.authKind === "oauth" ? (
              <Button appearance="primary" size="small" loading={reauth.isPending} onClick={() => void doReauth()}>
                {t("settings.account.reauth")}
              </Button>
            ) : (
              <Button
                appearance="primary"
                size="small"
                onClick={() => {
                  const element = document.getElementById("account-password");
                  element?.focus();
                }}
              >
                {t("settings.account.enterPassword")}
              </Button>
            )
          }
        >
          {account.stateMessage ?? t("settings.account.needsReauthDesc")}
        </Banner>
      ) : null}
      {account.state === "error" && account.stateMessage ? (
        <Banner tone="error" title={t("settings.account.errorState")}>
          {account.stateMessage}
        </Banner>
      ) : null}
      {isLocal ? (
        <Banner tone="info" title={t("settings.account.localTitle")}>
          {t("settings.account.localDesc")}
        </Banner>
      ) : null}

      <Section title={t("settings.account.general")}>
        <Card>
          <StackRow>
            <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
              <Avatar name={account.displayName} email={account.email} size={44} />
              <div style={{ display: "flex", flexDirection: "column" }}>
                <span className={styles.rowLabel}>{account.displayName}</span>
                <span className={styles.rowDesc}>{account.email}</span>
              </div>
            </div>
          </StackRow>
          <StackRow>
            <div className={styles.inlineFields}>
              <Input
                label={t("settings.account.displayName")}
                value={displayName}
                onChange={(event) => setDisplayName(event.target.value)}
                onBlur={saveName}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    saveName();
                  }
                }}
              />
              <Button
                appearance="primary"
                size="small"
                disabled={!nameDirty}
                loading={updateAccount.isPending}
                onClick={saveName}
              >
                {t("common.save")}
              </Button>
            </div>
          </StackRow>
          <StackRow>
            <ColorPicker
              label={t("settings.account.color")}
              value={account.color}
              onChange={(color) => {
                if (color) patchAccount({ color });
              }}
            />
          </StackRow>
          <SettingRow label={t("settings.account.provider")}>
            <Badge appearance="neutral">{providerLabel(account.provider, t)}</Badge>
          </SettingRow>
          <SettingRow label={t("settings.account.email")}>
            <span className={styles.mono}>{account.email}</span>
          </SettingRow>
        </Card>
      </Section>

      {!isLocal ? (
        <Section title={t("settings.account.syncOptions")}>
          <Card>
            {account.mailEnabled ? (
              <SettingRow label={t("settings.account.syncWindow")} description={t("settings.account.syncWindowDesc")}>
                <Select
                  ariaLabel={t("settings.account.syncWindow")}
                  value={String(account.syncWindowDays)}
                  onChange={(value) => patchAccount({ syncWindowDays: Number(value) })}
                  options={((SYNC_WINDOW_DAYS as readonly number[]).includes(account.syncWindowDays)
                    ? [...SYNC_WINDOW_DAYS]
                    : [...SYNC_WINDOW_DAYS, account.syncWindowDays].sort((a, b) => a - b)
                  ).map((days) => ({
                    value: String(days),
                    label:
                      days === 0
                        ? t("settings.account.syncEverything")
                        : t("settings.account.syncDays", { count: days }),
                  }))}
                />
              </SettingRow>
            ) : null}
            <SettingRow label={t("settings.account.mail")} description={t("settings.account.mailDesc")}>
              <Switch
                checked={account.mailEnabled}
                onChange={(checked) => patchAccount({ mailEnabled: checked })}
                aria-label={t("settings.account.mail")}
              />
            </SettingRow>
            {account.oauthProvider !== "microsoft" ? (
              <SentCopySetting account={account} patch={patchAccount} />
            ) : null}
            <SettingRow label={t("settings.account.calendar")} description={t("settings.account.calendarDesc")}>
              <Switch
                checked={account.calendarEnabled}
                disabled={!account.capabilities.calendar}
                onChange={(checked) => patchAccount({ calendarEnabled: checked })}
                aria-label={t("settings.account.calendar")}
              />
            </SettingRow>
            <SettingRow label={t("settings.account.contacts")} description={t("settings.account.contactsDesc")}>
              <Switch
                checked={account.contactsEnabled}
                disabled={!account.capabilities.contacts}
                onChange={(checked) => patchAccount({ contactsEnabled: checked })}
                aria-label={t("settings.account.contacts")}
              />
            </SettingRow>
            <SettingRow label={t("settings.account.tasks")} description={t("settings.account.tasksDesc")}>
              <Switch
                checked={account.tasksEnabled}
                disabled={!account.capabilities.tasks}
                onChange={(checked) => patchAccount({ tasksEnabled: checked })}
                aria-label={t("settings.account.tasks")}
              />
            </SettingRow>
            <SettingRow label={t("settings.account.notifications")} description={t("settings.account.notificationsDesc")}>
              <Switch
                checked={account.notificationsEnabled}
                onChange={(checked) => patchAccount({ notificationsEnabled: checked })}
                aria-label={t("settings.account.notifications")}
              />
            </SettingRow>
            <SettingRow label={t("settings.account.includeInUnified")} description={t("settings.account.includeInUnifiedDesc")}>
              <Switch
                checked={account.includeInUnified}
                onChange={(checked) => patchAccount({ includeInUnified: checked })}
                aria-label={t("settings.account.includeInUnified")}
              />
            </SettingRow>
          </Card>
        </Section>
      ) : null}

      {!isLocal ? <ServerSettingsCard account={account} /> : null}

      {!isLocal ? (
        <Section title={t("settings.account.credentials")}>
          <Card>
            {account.authKind === "password" ? (
              <PasswordReset account={account} />
            ) : (
              <SettingRow
                label={t("settings.account.oauthAccount")}
                description={t("settings.account.oauthAccountDesc")}
              >
                <Button
                  icon={<Key24Regular />}
                  loading={reauth.isPending}
                  onClick={() => void doReauth()}
                >
                  {t("settings.account.reauth")}
                </Button>
              </SettingRow>
            )}
            <SettingRow label={t("settings.account.lastState")} description={account.stateMessage ?? undefined}>
              <Badge appearance={account.state === "ok" ? "success" : "warning"}>
                {t(`settings.account.state.${account.state}`)}
              </Badge>
            </SettingRow>
          </Card>
        </Section>
      ) : null}

      {!isLocal && account.mailEnabled ? <FolderSyncCard account={account} /> : null}

      {!isLocal ? <IdentitiesCard account={account} /> : null}

      {!isLocal ? (
        <Section title={t("settings.account.dangerZone")}>
          <Card className={styles.dangerZone}>
            <StackRow>
              <div className={styles.rowLabel}>{t("settings.account.deleteTitle", { name: account.displayName || account.email })}</div>
              <div className={styles.rowDesc}>{t("settings.account.deleteMessage")}</div>
              <div className={styles.actions}>
                <Button appearance="danger" icon={<Delete24Regular />} loading={deleteAccount.isPending} onClick={() => void onDelete()}>
                  {t("settings.account.deleteConfirm")}
                </Button>
              </div>
            </StackRow>
          </Card>
        </Section>
      ) : null}
    </SettingsPage>
  );
}

/* ───────────────────────── Servers ───────────────────────── */

/**
 * Provider-directory facts for an address. The server card and the Sent-copy
 * row both need them; react-query dedupes the calls by key.
 */
function useProviderInfo(email: string) {
  return useQuery({
    queryKey: ["accountProviderInfo", email],
    queryFn: () => accounts.providerInfo(email),
    staleTime: 5 * 60_000,
  });
}

/**
 * Who files the Sent copy. Hidden for Microsoft accounts: Graph sendMail
 * always files the copy itself, so the setting would have no effect.
 */
function SentCopySetting({
  account,
  patch,
}: {
  account: Account;
  patch: (patch: AccountPatch) => void;
}) {
  const t = useT();
  const providerInfo = useProviderInfo(account.email).data ?? null;
  const value = account.sentCopy ?? "auto";
  // "configurable" behaves like "client" (Nuncio appends) but the provider's
  // web switch can change that, so the user has to be told.
  const configurableHint =
    providerInfo?.sentCopy === "configurable"
      ? t("settings.account.sentCopyConfigurableHint", { name: providerInfo.name })
      : null;
  const description =
    value === "auto"
      ? providerInfo?.sentCopy === "server"
        ? t("settings.account.sentCopyAutoServer", { name: providerInfo.name })
        : t("settings.account.sentCopyAutoClient")
      : null;

  return (
    <SettingRow
      label={t("settings.account.sentCopy")}
      description={
        description || configurableHint ? (
          <>
            {description ? <div>{description}</div> : null}
            {configurableHint ? <div>{configurableHint}</div> : null}
          </>
        ) : undefined
      }
    >
      <Select
        ariaLabel={t("settings.account.sentCopy")}
        value={value}
        onChange={(next) =>
          patch({ sentCopy: next === "auto" ? null : (next as SentCopyMode) })
        }
        options={[
          { value: "auto", label: t("settings.account.sentCopyAuto") },
          { value: "server", label: t("settings.account.sentCopyServer") },
          { value: "client", label: t("settings.account.sentCopyClient") },
        ]}
      />
    </SettingRow>
  );
}

function ServerSettingsCard({ account }: { account: Account }) {
  const t = useT();
  const updateAccount = useUpdateAccount();
  const [imap, setImap] = useState<ServerDraft | null>(() => (account.imap ? serverDraftFromConfig(account.imap) : null));
  const [smtp, setSmtp] = useState<ServerDraft | null>(() => (account.smtp ? serverDraftFromConfig(account.smtp) : null));
  const [pop3, setPop3] = useState<ServerDraft | null>(() => (account.pop3 ? serverDraftFromConfig(account.pop3) : null));
  const [caldavUrl, setCaldavUrl] = useState(account.caldavUrl ?? "");
  const [carddavUrl, setCarddavUrl] = useState(account.carddavUrl ?? "");
  const [caldavPassword, setCaldavPassword] = useState("");
  const [carddavPassword, setCarddavPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [detecting, setDetecting] = useState(false);
  // Google/Microsoft accounts sync calendars and contacts through their APIs;
  // the DAV fields would never be read, so they are replaced by a hint.
  const isApiMail = account.oauthProvider === "google" || account.oauthProvider === "microsoft";
  const providerInfo = useProviderInfo(account.email).data ?? null;
  const davStatus = useDavPasswordStatus(account.id, !isApiMail);
  const setDavPassword = useSetDavPassword();

  useEffect(() => {
    setImap(account.imap ? serverDraftFromConfig(account.imap) : null);
    setSmtp(account.smtp ? serverDraftFromConfig(account.smtp) : null);
    setPop3(account.pop3 ? serverDraftFromConfig(account.pop3) : null);
    setCaldavUrl(account.caldavUrl ?? "");
    setCarddavUrl(account.carddavUrl ?? "");
    // Password inputs are write-only: never prefill them from a secret.
    setCaldavPassword("");
    setCarddavPassword("");
    setError(null);
  }, [
    account.id,
    account.imap,
    account.smtp,
    account.pop3,
    account.caldavUrl,
    account.carddavUrl,
  ]);

  const hasMailServers = Boolean(imap || smtp || pop3);

  const save = async () => {
    setError(null);
    for (const [label, draft] of [
      ["IMAP", imap],
      ["SMTP", smtp],
      ["POP3", pop3],
    ] as const) {
      if (!draft) continue;
      if (!draft.host.trim()) {
        setError(t("settings.account.serverHostRequired", { server: label }));
        return;
      }
      if (!isValidPort(draft.port)) {
        setError(t("settings.account.serverPortInvalid", { server: label }));
        return;
      }
    }
    if (!isApiMail) {
      for (const [label, value] of [
        ["CalDAV", caldavUrl],
        ["CardDAV", carddavUrl],
      ] as const) {
        if (value.trim() && !/^https?:\/\//i.test(value.trim())) {
          setError(t("settings.account.davUrlInvalid", { server: label }));
          return;
        }
      }
    }
    const patch: AccountPatch = {
      imap: imap ? serverFromDraft(imap, account.email) : null,
      smtp: smtp ? serverFromDraft(smtp, account.email) : null,
      pop3: pop3 ? serverFromDraft(pop3, account.email) : null,
      // Leave the stored DAV values alone for API accounts: the fields are
      // not rendered, so sending null here would silently wipe them.
      ...(isApiMail
        ? {}
        : {
            caldavUrl: caldavUrl.trim() || null,
            carddavUrl: carddavUrl.trim() || null,
          }),
    };
    try {
      await updateAccount.mutateAsync({ id: account.id, patch });
      // Only fields the user actually typed into are sent; an untouched empty
      // field must never clear a stored secret (the "clear" action does that).
      for (const [service, password] of [
        ["calendar", caldavPassword.trim()],
        ["contacts", carddavPassword.trim()],
      ] as const) {
        if (password) {
          await setDavPassword.mutateAsync({ id: account.id, service, password });
        }
      }
      setCaldavPassword("");
      setCarddavPassword("");
      toast.success(t("settings.account.serversSaved"));
    } catch (err) {
      setError(describeError(err, t));
    }
  };

  const clearDavPassword = async (service: "calendar" | "contacts", label: string) => {
    setError(null);
    try {
      await setDavPassword.mutateAsync({ id: account.id, service, password: null });
      if (service === "calendar") setCaldavPassword("");
      else setCarddavPassword("");
      toast.success(t("settings.account.davPasswordCleared", { server: label }));
    } catch (err) {
      setError(describeError(err, t));
    }
  };

  // Fills only blank fields so a detected endpoint never overwrites a URL the
  // user entered by hand. Saving stays an explicit action.
  const detectDav = async () => {
    setDetecting(true);
    try {
      const result = await accounts.discover(account.email);
      let filled = false;
      if (!caldavUrl.trim() && result.caldavUrl) {
        setCaldavUrl(result.caldavUrl);
        filled = true;
      }
      if (!carddavUrl.trim() && result.carddavUrl) {
        setCarddavUrl(result.carddavUrl);
        filled = true;
      }
      if (!filled) toast.info(t("settings.account.davDetectNothing"));
    } catch (err) {
      toast.error(t("settings.account.davDetectFailed"), describeError(err, t));
    } finally {
      setDetecting(false);
    }
  };

  return (
    <Section title={t("settings.account.servers")} hint={t("settings.account.serversHint")}>
      <Card>
        {hasMailServers ? (
          <>
            {imap ? <ServerFields label="IMAP" draft={imap} onChange={setImap} /> : null}
            {smtp ? <ServerFields label="SMTP" draft={smtp} onChange={setSmtp} /> : null}
            {pop3 ? <ServerFields label="POP3" draft={pop3} onChange={setPop3} /> : null}
          </>
        ) : null}
        {isApiMail ? (
          <Hint>{t("settings.account.davOauthHint")}</Hint>
        ) : (
          <>
            <StackRow>
              <Input
                label="CalDAV"
                value={caldavUrl}
                placeholder={providerInfo?.caldavUrl ?? "https://example.com/caldav/"}
                spellCheck={false}
                onChange={(event) => setCaldavUrl(event.target.value)}
              />
            </StackRow>
            <StackRow>
              <div className={styles.inlineFields}>
                <Input
                  type="password"
                  label={t("settings.account.davPasswordLabel", { server: "CalDAV" })}
                  value={caldavPassword}
                  autoComplete="new-password"
                  placeholder={
                    davStatus.data?.calendar
                      ? t("settings.account.davPasswordStored")
                      : t("settings.account.davPasswordFallback")
                  }
                  onChange={(event) => setCaldavPassword(event.target.value)}
                />
                {davStatus.data?.calendar ? (
                  <Button
                    size="small"
                    disabled={setDavPassword.isPending}
                    onClick={() => void clearDavPassword("calendar", "CalDAV")}
                  >
                    {t("settings.account.davPasswordClear")}
                  </Button>
                ) : null}
              </div>
            </StackRow>
            <StackRow>
              <Input
                label="CardDAV"
                value={carddavUrl}
                placeholder={providerInfo?.carddavUrl ?? "https://example.com/carddav/"}
                spellCheck={false}
                onChange={(event) => setCarddavUrl(event.target.value)}
              />
            </StackRow>
            <StackRow>
              <div className={styles.inlineFields}>
                <Input
                  type="password"
                  label={t("settings.account.davPasswordLabel", { server: "CardDAV" })}
                  value={carddavPassword}
                  autoComplete="new-password"
                  placeholder={
                    davStatus.data?.contacts
                      ? t("settings.account.davPasswordStored")
                      : t("settings.account.davPasswordFallback")
                  }
                  onChange={(event) => setCarddavPassword(event.target.value)}
                />
                {davStatus.data?.contacts ? (
                  <Button
                    size="small"
                    disabled={setDavPassword.isPending}
                    onClick={() => void clearDavPassword("contacts", "CardDAV")}
                  >
                    {t("settings.account.davPasswordClear")}
                  </Button>
                ) : null}
              </div>
            </StackRow>
            <StackRow>
              <p className={styles.hint}>{t("settings.account.davPasswordHint")}</p>
              {providerInfo?.caldavUrl || providerInfo?.carddavUrl ? (
                <p className={styles.hint}>
                  {t("settings.account.davDefaultHint", { name: providerInfo.name })}
                </p>
              ) : null}
              <div className={styles.actions}>
                <Button size="small" loading={detecting} onClick={() => void detectDav()}>
                  {t("settings.account.davDetect")}
                </Button>
              </div>
            </StackRow>
          </>
        )}
        <StackRow>
          {error ? <p className={styles.errorText}>{error}</p> : null}
          <div className={styles.actions}>
            <Button appearance="primary" loading={updateAccount.isPending} onClick={() => void save()}>
              {t("settings.account.saveServers")}
            </Button>
          </div>
        </StackRow>
      </Card>
      {!hasMailServers ? <Hint>{t("settings.account.davOnlyHint")}</Hint> : null}
    </Section>
  );
}

/* ─────────────────────── Password reset ──────────────────────── */

function PasswordReset({ account }: { account: Account }) {
  const t = useT();
  const updatePassword = useUpdateAccountPassword();
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    if (!password) {
      setError(t("settings.account.passwordRequired"));
      return;
    }
    setError(null);
    try {
      await updatePassword.mutateAsync({ id: account.id, password });
      setPassword("");
      toast.success(t("settings.account.passwordSaved"));
    } catch (err) {
      setError(describeError(err, t));
    }
  };

  return (
    <StackRow>
      <div className={styles.inlineFields}>
        <Input
          id="account-password"
          type="password"
          label={t("settings.account.newPassword")}
          value={password}
          autoComplete="new-password"
          error={error ?? undefined}
          onChange={(event) => {
            setPassword(event.target.value);
            setError(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") void save();
          }}
        />
        <Button appearance="primary" size="small" loading={updatePassword.isPending} onClick={() => void save()}>
          {t("settings.account.updatePassword")}
        </Button>
      </div>
      <p className={styles.hint}>{t("settings.account.passwordHint")}</p>
    </StackRow>
  );
}

/* ─────────────────────── Folder selection ────────────────────── */

function FolderSyncCard({ account }: { account: Account }) {
  const t = useT();
  const foldersQuery = useFolders(account.id);
  const setFolderSync = useSetFolderSync();

  const folders = useMemo(() => {
    const list = [...(foldersQuery.data ?? [])];
    const byId = new Map(list.map((folder) => [folder.id, folder]));
    const depth = (folder: Folder): number => {
      let level = 0;
      let current = folder.parentId ? byId.get(folder.parentId) : undefined;
      while (current && level < 12) {
        level += 1;
        current = current.parentId ? byId.get(current.parentId) : undefined;
      }
      return level;
    };
    return list
      .filter((folder) => folder.isSelectable)
      .map((folder) => ({ folder, level: depth(folder) }))
      .sort((a, b) => a.folder.sortOrder - b.folder.sortOrder || a.folder.name.localeCompare(b.folder.name));
  }, [foldersQuery.data]);

  return (
    <Section title={t("settings.account.folders")} hint={t("settings.account.foldersHint")}>
      <Card>
        {foldersQuery.isLoading ? (
          <div className={styles.emptyHint}>{t("common.loading")}</div>
        ) : folders.length === 0 ? (
          <div className={styles.emptyHint}>{t("settings.account.noFolders")}</div>
        ) : (
          <div className={styles.folderList}>
            {folders.map(({ folder, level }) => (
              <div key={folder.id} className={styles.folderRow} style={{ paddingLeft: 12 + level * 16 }}>
                <span className={styles.folderName}>{folder.name}</span>
                <Switch
                  checked={folder.syncEnabled}
                  onChange={(checked) =>
                    setFolderSync.mutate(
                      { id: folder.id, enabled: checked, accountId: account.id },
                      {
                        onError: (error) =>
                          toast.error(t("settings.account.folderToggleFailed"), describeError(error, t)),
                      },
                    )
                  }
                  aria-label={t("settings.account.folderToggle", { name: folder.name })}
                />
              </div>
            ))}
          </div>
        )}
      </Card>
    </Section>
  );
}

/* ─────────────────────── Identities ─────────────────────────── */

function IdentitiesCard({ account }: { account: Account }) {
  const t = useT();
  const identitiesQuery = useIdentities(account.id);
  const signaturesQuery = useSignatures();
  const upsert = useUpsertIdentity();
  const remove = useDeleteIdentity();
  const [draft, setDraft] = useState<Identity | null>(null);
  const [error, setError] = useState<string | null>(null);

  const signatureOptions = useMemo(
    () => [
      { value: "none", label: t("settings.account.noSignature") },
      ...(signaturesQuery.data ?? [])
        .filter((signature) => signature.accountId === null || signature.accountId === account.id)
        .map((signature) => ({ value: signature.id, label: signature.name })),
    ],
    [signaturesQuery.data, account.id, t],
  );

  const startNew = () => {
    setError(null);
    setDraft({
      id: newId(),
      accountId: account.id,
      name: account.displayName,
      email: account.email,
      replyTo: null,
      signatureId: null,
      isDefault: (identitiesQuery.data ?? []).length === 0,
    });
  };

  const save = async () => {
    if (!draft) return;
    if (!draft.name.trim()) {
      setError(t("settings.account.identityNameRequired"));
      return;
    }
    if (!isValidEmail(draft.email.trim())) {
      setError(t("common.invalidEmail"));
      return;
    }
    if (draft.replyTo && !isValidEmail(draft.replyTo.trim())) {
      setError(t("settings.account.replyToInvalid"));
      return;
    }
    setError(null);
    try {
      await upsert.mutateAsync({
        ...draft,
        name: draft.name.trim(),
        email: draft.email.trim(),
        replyTo: draft.replyTo?.trim() || null,
      });
      toast.success(t("settings.account.identitySaved"));
      setDraft(null);
    } catch (err) {
      setError(describeError(err, t));
    }
  };

  const onDelete = async (identity: Identity) => {
    const ok = await confirm({
      title: t("settings.account.identityDeleteTitle", { email: identity.email }),
      message: t("settings.account.identityDeleteMessage"),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await remove.mutateAsync(identity.id);
      toast.success(t("settings.account.identityDeleted"));
    } catch (err) {
      toast.error(t("settings.account.identityDeleteFailed"), describeError(err, t));
    }
  };

  return (
    <Section
      title={t("settings.account.identities")}
      hint={t("settings.account.identitiesHint")}
      actions={
        <Button size="small" appearance="subtle" icon={<Add24Regular />} onClick={startNew}>
          {t("settings.account.addIdentity")}
        </Button>
      }
    >
      <Card>
        {(identitiesQuery.data ?? []).length === 0 && !draft ? (
          <div className={styles.emptyHint}>{t("settings.account.noIdentities")}</div>
        ) : null}
        {(identitiesQuery.data ?? []).map((identity) => (
          <div key={identity.id} className={styles.row}>
            <div className={styles.rowText}>
              <div className={styles.rowLabel}>
                {identity.name} {identity.isDefault ? <Badge appearance="accent">{t("settings.account.defaultIdentity")}</Badge> : null}
              </div>
              <div className={styles.rowDesc}>{identity.email}</div>
              {identity.replyTo ? (
                <div className={styles.rowDesc}>{t("settings.account.replyToValue", { email: identity.replyTo })}</div>
              ) : null}
            </div>
            <div className={styles.rowControl}>
              <IconButton
                icon={<Edit24Regular />}
                label={t("common.edit")}
                size="small"
                appearance="subtle"
                onClick={() => {
                  setError(null);
                  setDraft(identity);
                }}
              />
              <IconButton
                icon={<Delete24Regular />}
                label={t("common.delete")}
                size="small"
                appearance="subtle"
                onClick={() => void onDelete(identity)}
              />
            </div>
          </div>
        ))}

        {draft ? (
          <StackRow>
            <div className={styles.sectionTitle}>
              {identitiesQuery.data?.some((item) => item.id === draft.id)
                ? t("settings.account.editIdentity")
                : t("settings.account.newIdentity")}
            </div>
            <div className={styles.fieldGrid}>
              <Input
                label={t("settings.account.identityName")}
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              />
              <Input
                label={t("settings.account.identityEmail")}
                value={draft.email}
                spellCheck={false}
                onChange={(event) => setDraft({ ...draft, email: event.target.value })}
              />
              <div className={styles.span2}>
                <Input
                  label={t("settings.account.replyTo")}
                  value={draft.replyTo ?? ""}
                  placeholder={t("settings.account.replyToPlaceholder")}
                  spellCheck={false}
                  onChange={(event) => setDraft({ ...draft, replyTo: event.target.value || null })}
                />
              </div>
              <div className={styles.span2}>
                <Select
                  label={t("settings.account.signature")}
                  value={draft.signatureId ?? "none"}
                  onChange={(value) => setDraft({ ...draft, signatureId: value === "none" ? null : value })}
                  options={signatureOptions}
                />
              </div>
            </div>
            <Checkbox
              label={t("settings.account.defaultIdentity")}
              description={t("settings.account.defaultIdentityDesc")}
              checked={draft.isDefault}
              onChange={(event) => setDraft({ ...draft, isDefault: event.target.checked })}
            />
            {error ? <p className={styles.errorText}>{error}</p> : null}
            <div className={styles.actions}>
              <Button appearance="subtle" onClick={() => setDraft(null)}>
                {t("common.cancel")}
              </Button>
              <Button appearance="primary" loading={upsert.isPending} onClick={() => void save()}>
                {t("common.save")}
              </Button>
            </div>
          </StackRow>
        ) : null}
      </Card>
    </Section>
  );
}

function providerLabel(provider: Account["provider"], t: (key: string) => string): string {
  return t(`settings.provider.${provider}`);
}

