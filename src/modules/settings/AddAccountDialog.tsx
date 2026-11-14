/**
 * Add-account wizard: provider selection → authentication (OAuth or password
 * with discovery) → account options → connection test → create.
 */
import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  CalendarLtr24Regular,
  CheckmarkCircle16Regular,
  ChevronDown24Regular,
  ChevronUp24Regular,
  Cloud24Regular,
  ErrorCircle16Regular,
  Globe24Regular,
  Mail24Regular,
  Server24Regular,
} from "@fluentui/react-icons";
import {
  Button,
  Dialog,
  Input,
  Select,
  Spinner as UiSpinner,
  Switch,
  toast,
} from "@/components";
import { translateMaybeKey, useT } from "@/lib/i18n";
import type {
  AuthKind,
  ConnectionTestResult,
  DiscoveryResult,
  OAuthProvider,
  ProviderKind,
} from "@/ipc/types";
import type { AddAccountDialogProps } from "@/app/moduleTypes";
import { accounts, isIpcError } from "@/ipc/api";
import { isValidEmail } from "@/lib/format";
import { useSettings } from "@/lib/useSettings";
import { hasConnectedAccount } from "@/lib/accounts";
import { useAccounts, useAppInfo, useCreateAccount } from "./lib/hooks";
import { describeError, isErrorCode } from "./lib/errors";
import { openExternal } from "./lib/desktop";
import {
  clientSecretPatch,
  hasStoredClientSecret,
} from "./lib/clientSecret";
import {
  SYNC_WINDOW_DAYS,
  buildNewAccountInput,
  createAccountForm,
  defaultDisplayName,
  emptyServerDraft,
  serverDraftFromConfig,
  validateAccountForm,
  type AccountFormErrors,
  type AccountFormState,
  type AccountProtocol,
} from "./lib/validation";
import { ServerFields } from "./components/ServerFields";
import { Banner as UiBanner } from "./components/SettingsUI";
import styles from "./settings.module.css";

type TileId =
  | "outlook"
  | "gmail"
  | "icloud"
  | "yahoo"
  | "qq"
  | "netease"
  | "imap"
  | "dav";

interface ProviderTile {
  id: TileId;
  provider: ProviderKind;
  authKind: AuthKind;
  oauthProvider: OAuthProvider | null;
  protocol: AccountProtocol;
  davOnly: boolean;
  icon: ReactNode;
  titleKey: string;
  descKey: string;
}

const TILES: ProviderTile[] = [
  {
    id: "outlook",
    provider: "outlook",
    authKind: "oauth",
    oauthProvider: "microsoft",
    protocol: "imap",
    davOnly: false,
    icon: <Mail24Regular />,
    titleKey: "settings.addAccount.providerOutlook",
    descKey: "settings.addAccount.providerOutlookDesc",
  },
  {
    id: "gmail",
    provider: "gmail",
    authKind: "oauth",
    oauthProvider: "google",
    protocol: "imap",
    davOnly: false,
    icon: <Mail24Regular />,
    titleKey: "settings.addAccount.providerGmail",
    descKey: "settings.addAccount.providerGmailDesc",
  },
  {
    id: "icloud",
    provider: "icloud",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: false,
    icon: <Cloud24Regular />,
    titleKey: "settings.addAccount.providerIcloud",
    descKey: "settings.addAccount.providerIcloudDesc",
  },
  {
    id: "yahoo",
    provider: "yahoo",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: false,
    icon: <Globe24Regular />,
    titleKey: "settings.addAccount.providerYahoo",
    descKey: "settings.addAccount.providerYahooDesc",
  },
  {
    id: "qq",
    provider: "qq",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: false,
    icon: <Mail24Regular />,
    titleKey: "settings.addAccount.providerQq",
    descKey: "settings.addAccount.providerQqDesc",
  },
  {
    id: "netease",
    provider: "netease",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: false,
    icon: <Mail24Regular />,
    titleKey: "settings.addAccount.providerNetease",
    descKey: "settings.addAccount.providerNeteaseDesc",
  },
  {
    id: "imap",
    provider: "imap",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: false,
    icon: <Server24Regular />,
    titleKey: "settings.addAccount.providerOther",
    descKey: "settings.addAccount.providerOtherDesc",
  },
  {
    id: "dav",
    provider: "imap",
    authKind: "password",
    oauthProvider: null,
    protocol: "imap",
    davOnly: true,
    icon: <CalendarLtr24Regular />,
    titleKey: "settings.addAccount.providerDav",
    descKey: "settings.addAccount.providerDavDesc",
  },
];

const HELP_URLS: Partial<Record<TileId, string>> = {
  qq: "https://mail.qq.com",
  netease: "https://mail.163.com",
  icloud: "https://appleid.apple.com/account/manage",
  yahoo: "https://login.yahoo.com/account/security",
};

const GOOGLE_CREDENTIALS_URL = "https://console.cloud.google.com/apis/credentials";
const MICROSOFT_ENTRA_URL = "https://entra.microsoft.com/";

type WizardStep = "provider" | "email" | "oauth" | "details";

function tileForOAuth(provider: OAuthProvider): ProviderTile {
  return provider === "google" ? TILES[1] : TILES[0];
}

export function AddAccountDialog({ open, onClose, onCreated }: AddAccountDialogProps) {
  const t = useT();
  const { settings, update: updateSettings } = useSettings();
  const accountsQuery = useAccounts();
  const appInfo = useAppInfo();
  const createAccount = useCreateAccount();

  const [step, setStep] = useState<WizardStep>("provider");
  const [tileId, setTileId] = useState<TileId | null>(null);
  const [form, setForm] = useState<AccountFormState>(() => createAccountForm());
  const [emailError, setEmailError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);
  const [discovery, setDiscovery] = useState<DiscoveryResult | null>(null);
  const [discoverError, setDiscoverError] = useState<string | null>(null);
  const [oauthHint, setOauthHint] = useState("");
  const [oauthBusy, setOauthBusy] = useState(false);
  const [oauthError, setOauthError] = useState<string | null>(null);
  const oauthRunRef = useRef(0);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [fieldErrors, setFieldErrors] = useState<AccountFormErrors>({});
  const [formError, setFormError] = useState<string | null>(null);
  const [testBusy, setTestBusy] = useState(false);
  const [testResult, setTestResult] = useState<ConnectionTestResult | null>(null);
  const [googleClientId, setGoogleClientId] = useState(settings.advanced.googleClientId ?? "");
  // The backend only reports the mask, so start empty: an empty field means
  // "keep the stored secret" and is omitted from the patch.
  const [googleClientSecret, setGoogleClientSecret] = useState("");
  const [microsoftClientId, setMicrosoftClientId] = useState(
    settings.advanced.microsoftClientId ?? "",
  );

  const tile = tileId ? (TILES.find((item) => item.id === tileId) ?? null) : null;
  const oauthNeeded = tile?.authKind === "oauth";
  const isDav = tile?.davOnly ?? false;
  const localAccountOnly =
    !accountsQuery.isLoading && !hasConnectedAccount(accountsQuery.data);
  const storedGoogleSecret = hasStoredClientSecret(settings.advanced.googleClientSecret);

  useEffect(() => {
    if (!open) return;
    setStep("provider");
    setTileId(null);
    setForm(createAccountForm());
    setEmailError(null);
    setDiscovering(false);
    setDiscovery(null);
    setDiscoverError(null);
    setOauthHint("");
    setOauthBusy(false);
    setOauthError(null);
    setAdvancedOpen(false);
    setFieldErrors({});
    setFormError(null);
    setTestBusy(false);
    setTestResult(null);
    setGoogleClientId(settings.advanced.googleClientId ?? "");
    setGoogleClientSecret("");
    setMicrosoftClientId(settings.advanced.microsoftClientId ?? "");
    oauthRunRef.current += 1;
    // Reset only when the dialog opens; later settings edits must not wipe input.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const patchForm = (patch: Partial<AccountFormState>) => {
    setForm((current) => ({ ...current, ...patch }));
    setFormError(null);
    setFieldErrors({});
  };

  const chooseTile = (next: ProviderTile) => {
    setTileId(next.id);
    setForm(
      createAccountForm({
        provider: next.provider,
        authKind: next.authKind,
        oauthProvider: next.oauthProvider,
        protocol: next.protocol,
        mailEnabled: !next.davOnly,
        calendarEnabled: true,
        contactsEnabled: true,
        tasksEnabled: !next.davOnly,
      }),
    );
    setDiscovery(null);
    setDiscoverError(null);
    setTestResult(null);
    setOauthError(null);
    setFieldErrors({});
    setFormError(null);
    setAdvancedOpen(next.davOnly);
  };

  const goToAuth = () => {
    if (!tile) return;
    setOauthHint(form.email);
    setStep(tile.authKind === "oauth" ? "oauth" : "email");
  };

  const applyDiscovery = (result: DiscoveryResult) => {
    setForm((current) => {
      const protocol: AccountProtocol =
        result.pop3 && !result.imap ? "pop3" : current.protocol;
      return {
        ...current,
        provider: result.provider,
        authKind: result.authKind,
        oauthProvider: result.oauthProvider,
        displayName: current.displayName || defaultDisplayName(current.email),
        protocol,
        imap: result.imap ? serverDraftFromConfig(result.imap) : current.imap,
        smtp: result.smtp ? serverDraftFromConfig(result.smtp) : current.smtp,
        pop3: result.pop3 ? serverDraftFromConfig(result.pop3) : current.pop3,
        caldavUrl: result.caldavUrl ?? current.caldavUrl,
        carddavUrl: result.carddavUrl ?? current.carddavUrl,
      };
    });
  };

  const discover = async () => {
    const email = form.email.trim();
    if (!isValidEmail(email)) {
      setEmailError(t("common.invalidEmail"));
      return;
    }
    setEmailError(null);
    setDiscoverError(null);
    setDiscovering(true);
    try {
      const result = await accounts.discover(email);
      setDiscovery(result);
      applyDiscovery(result);
      if (result.authKind === "oauth" && result.oauthProvider) {
        const oauthTile = tileForOAuth(result.oauthProvider);
        setTileId(oauthTile.id);
        setForm((current) => ({
          ...current,
          provider: oauthTile.provider,
          authKind: "oauth",
          oauthProvider: result.oauthProvider,
          displayName: current.displayName || defaultDisplayName(email),
        }));
        setOauthHint(email);
        setStep("oauth");
      } else {
        setStep("details");
      }
    } catch (error) {
      const domain = email.split("@")[1] ?? "";
      setDiscovery(null);
      setDiscoverError(describeError(error, t));
      setForm((current) => ({
        ...current,
        displayName: current.displayName || defaultDisplayName(email),
        imap:
          current.imap.host || !domain
            ? current.imap
            : emptyServerDraft({ host: `imap.${domain}`, port: "993", security: "tls" }),
        smtp:
          current.smtp.host || !domain
            ? current.smtp
            : emptyServerDraft({ host: `smtp.${domain}`, port: "587", security: "starttls" }),
        pop3:
          current.pop3.host || !domain
            ? current.pop3
            : emptyServerDraft({ host: `pop3.${domain}`, port: "995", security: "tls" }),
      }));
      setStep("details");
    } finally {
      setDiscovering(false);
    }
  };

  const startOauth = async () => {
    if (!tile?.oauthProvider) return;
    const run = oauthRunRef.current + 1;
    oauthRunRef.current = run;
    setOauthBusy(true);
    setOauthError(null);
    try {
      const result = await accounts.oauthLogin(
        tile.oauthProvider,
        oauthHint.trim() ? oauthHint.trim() : null,
      );
      if (oauthRunRef.current !== run) return;
      setForm((current) => ({
        ...current,
        authKind: "oauth",
        oauthProvider: result.provider,
        oauthSessionId: result.sessionId,
        email: result.email,
        displayName: result.displayName || defaultDisplayName(result.email),
      }));
      setTileId(tileForOAuth(result.provider).id);
      setStep("details");
    } catch (error) {
      if (oauthRunRef.current !== run) return;
      if (isErrorCode(error, "cancelled")) {
        setOauthError(null);
      } else {
        setOauthError(describeError(error, t));
      }
    } finally {
      if (oauthRunRef.current === run) setOauthBusy(false);
    }
  };

  const cancelOauth = () => {
    oauthRunRef.current += 1;
    setOauthBusy(false);
    void accounts.oauthCancel().catch(() => {
      // The pending login may already be gone; nothing else to do.
    });
  };

  const close = () => {
    if (oauthBusy) cancelOauth();
    onClose();
  };

  const validate = (): boolean => {
    const errors = validateAccountForm(form);
    setFieldErrors(errors);
    if (Object.keys(errors).length > 0) {
      if (
        errors.imapHost ||
        errors.imapPort ||
        errors.smtpHost ||
        errors.smtpPort ||
        errors.pop3Host ||
        errors.pop3Port ||
        errors.caldavUrl ||
        errors.carddavUrl
      ) {
        setAdvancedOpen(true);
      }
      setFormError(t("settings.addAccount.fixFields"));
      return false;
    }
    setFormError(null);
    return true;
  };

  const testConnection = async () => {
    if (!validate()) return;
    setTestBusy(true);
    setTestResult(null);
    setFormError(null);
    try {
      const result = await accounts.test(buildNewAccountInput(form));
      setTestResult(result);
      if (result.error) {
        toast.error(t("settings.addAccount.testFailed"), result.error);
      }
    } catch (error) {
      setFormError(describeError(error, t));
    } finally {
      setTestBusy(false);
    }
  };

  const submit = async () => {
    if (!validate()) return;
    try {
      const account = await createAccount.mutateAsync(buildNewAccountInput(form));
      toast.success(t("settings.addAccount.created", { email: account.email }));
      onCreated?.(account);
      onClose();
    } catch (error) {
      if (isIpcError(error) && error.code === "conflict") {
        setFormError(t("settings.addAccount.alreadyExists"));
      } else {
        setFormError(describeError(error, t));
      }
    }
  };

  const missingClient =
    oauthNeeded && appInfo.isSuccess
      ? tile?.oauthProvider === "google"
        ? !(appInfo.data?.buildHasGoogleClient || settings.advanced.googleClientId)
        : !(appInfo.data?.buildHasMicrosoftClient || settings.advanced.microsoftClientId)
      : false;

  const saveClientIds = () => {
    const advanced: {
      googleClientId: string | null;
      microsoftClientId: string | null;
      googleClientSecret?: string;
    } = {
      googleClientId: googleClientId.trim() || null,
      microsoftClientId: microsoftClientId.trim() || null,
    };
    // Only send a secret the user actually typed; the mask is never written back.
    const secret = clientSecretPatch(googleClientSecret);
    if (secret !== undefined) advanced.googleClientSecret = secret;
    updateSettings({ advanced });
    setGoogleClientSecret("");
    toast.success(t("settings.addAccount.clientSaved"));
  };

  const openHelp = () => {
    if (!tileId) return;
    const url = HELP_URLS[tileId];
    if (url) void openExternal(url);
  };

  const stepIndex = step === "provider" ? 0 : step === "details" ? 2 : 1;

  const footer = (() => {
    if (step === "provider") {
      return (
        <>
          <Button appearance="subtle" onClick={close}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" disabled={!tile} onClick={goToAuth}>
            {t("common.next")}
          </Button>
        </>
      );
    }
    if (step === "email") {
      return (
        <>
          <Button appearance="subtle" onClick={() => setStep("provider")} disabled={discovering}>
            {t("common.previous")}
          </Button>
          <Button appearance="primary" loading={discovering} onClick={() => void discover()}>
            {t("common.next")}
          </Button>
        </>
      );
    }
    if (step === "oauth") {
      return (
        <>
          <Button appearance="subtle" onClick={() => setStep("provider")} disabled={oauthBusy}>
            {t("common.previous")}
          </Button>
          {oauthBusy ? (
            <Button appearance="subtle" onClick={cancelOauth}>
              {t("settings.addAccount.cancelLogin")}
            </Button>
          ) : (
            <Button
              appearance="primary"
              disabled={missingClient}
              onClick={() => void startOauth()}
            >
              {t("settings.addAccount.signIn")}
            </Button>
          )}
        </>
      );
    }
    return (
      <>
        <Button
          appearance="subtle"
          onClick={() => setStep(form.authKind === "oauth" ? "oauth" : "email")}
          disabled={createAccount.isPending}
        >
          {t("common.previous")}
        </Button>
        <Button
          appearance="subtle"
          onClick={() => void testConnection()}
          loading={testBusy}
          disabled={form.authKind === "oauth" || createAccount.isPending}
        >
          {t("settings.addAccount.test")}
        </Button>
        <Button appearance="primary" loading={createAccount.isPending} onClick={() => void submit()}>
          {t("settings.addAccount.create")}
        </Button>
      </>
    );
  })();

  return (
    <Dialog
      open={open}
      onClose={close}
      title={t("settings.addAccount.title")}
      width={680}
      dismissible={!oauthBusy && !createAccount.isPending}
      // Losing half-filled wizard input to a stray click outside the dialog is
      // worse than requiring an explicit Cancel / close.
      closeOnBackdrop={false}
      closeOnEscape={!oauthBusy && !createAccount.isPending}
      footer={footer}
    >
        <div className={styles.wizardBody}>
          <div className={styles.wizardSteps} aria-hidden="true">
            {[t("settings.addAccount.stepProvider"), t("settings.addAccount.stepAuth"), t("settings.addAccount.stepOptions")].map(
              (label, index) => (
                <span
                  key={label}
                  className={`${styles.wizardStep} ${index <= stepIndex ? styles.wizardStepActive : ""}`}
                >
                  <span className={styles.wizardStepDot}>{index + 1}</span>
                  {label}
                </span>
              ),
            )}
          </div>

          {step === "provider" ? (
            <div className={styles.rowStack}>
              {localAccountOnly ? (
                <p className={styles.hint}>{t("settings.addAccount.firstAccountHint")}</p>
              ) : null}
              <div className={styles.optionTileGrid}>
                {TILES.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    className={`${styles.optionTile} ${tileId === item.id ? styles.optionTileSelected : ""}`}
                    aria-pressed={tileId === item.id}
                    onClick={() => chooseTile(item)}
                    onDoubleClick={() => {
                      chooseTile(item);
                      setOauthHint(form.email);
                      setStep(item.authKind === "oauth" ? "oauth" : "email");
                    }}
                  >
                    <span className={styles.optionTileIcon} aria-hidden="true">
                      {item.icon}
                    </span>
                    <span className={styles.optionTileText}>
                      <span className={styles.optionTileTitle}>{t(item.titleKey)}</span>
                      <span className={styles.optionTileDesc}>{t(item.descKey)}</span>
                    </span>
                  </button>
                ))}
              </div>
            </div>
          ) : null}

          {step === "email" ? (
            <div className={styles.rowStack}>
              <p className={styles.hint}>{t("settings.addAccount.emailHint")}</p>
              <Input
                label={t("settings.addAccount.email")}
                type="email"
                value={form.email}
                autoFocus
                autoComplete="email"
                spellCheck={false}
                placeholder="name@example.com"
                error={emailError ?? undefined}
                onChange={(event) => {
                  setEmailError(null);
                  setForm((current) => ({
                    ...current,
                    email: event.target.value,
                    displayName:
                      current.displayName === defaultDisplayName(current.email)
                        ? ""
                        : current.displayName,
                  }));
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void discover();
                }}
              />
              {discovering ? (
                <div className={styles.spinnerRow}>
                  <UiSpinner size="small" />
                  <span>{t("settings.addAccount.discovering")}</span>
                </div>
              ) : null}
              <p className={styles.hint}>{t("settings.addAccount.discoveryNote")}</p>
            </div>
          ) : null}

          {step === "oauth" && tile?.oauthProvider ? (
            <div className={styles.rowStack}>
              <UiBanner tone="info" title={t("settings.addAccount.oauthTitle")}>
                {t(`settings.addAccount.oauthBody.${tile.oauthProvider}`)}
              </UiBanner>
              <Input
                label={t("settings.addAccount.emailOptional")}
                type="email"
                value={oauthHint}
                placeholder="name@example.com"
                hint={t("settings.addAccount.oauthHint")}
                disabled={oauthBusy}
                onChange={(event) => setOauthHint(event.target.value)}
              />
              {missingClient ? (
                <div className={styles.rowStack}>
                  <UiBanner tone="warning" title={t("settings.addAccount.missingClientTitle")}>
                    {t("settings.addAccount.missingClientBody")}
                  </UiBanner>
                  <div className={styles.actions}>
                    <Button
                      size="small"
                      appearance="subtle"
                      onClick={() =>
                        void openExternal(
                          tile.oauthProvider === "google" ? GOOGLE_CREDENTIALS_URL : MICROSOFT_ENTRA_URL,
                        )
                      }
                    >
                      {t("settings.addAccount.openDocs")}
                    </Button>
                  </div>
                  {tile.oauthProvider === "google" ? (
                    <>
                      <Input
                        label={t("settings.advanced.googleClientId")}
                        value={googleClientId}
                        spellCheck={false}
                        onChange={(event) => setGoogleClientId(event.target.value)}
                      />
                      <Input
                        label={t("settings.advanced.googleClientSecret")}
                        value={googleClientSecret}
                        type="password"
                        spellCheck={false}
                        autoComplete="off"
                        placeholder={
                          storedGoogleSecret
                            ? t("settings.advanced.secretStoredPlaceholder")
                            : t("settings.advanced.clientSecretPlaceholder")
                        }
                        onChange={(event) => setGoogleClientSecret(event.target.value)}
                      />
                    </>
                  ) : (
                    <Input
                      label={t("settings.advanced.microsoftClientId")}
                      value={microsoftClientId}
                      spellCheck={false}
                      onChange={(event) => setMicrosoftClientId(event.target.value)}
                    />
                  )}
                  <div className={styles.actions}>
                    <Button appearance="primary" size="small" onClick={saveClientIds}>
                      {t("common.save")}
                    </Button>
                  </div>
                </div>
              ) : null}
              {oauthBusy ? (
                <div className={styles.spinnerRow}>
                  <UiSpinner size="small" />
                  <span>{t("settings.addAccount.waitingForBrowser")}</span>
                </div>
              ) : null}
              {oauthError ? (
                <UiBanner tone="error" title={t("settings.addAccount.loginFailed")}>
                  {oauthError}
                </UiBanner>
              ) : null}
            </div>
          ) : null}

          {step === "details" ? (
            <div className={styles.rowStack}>
              {discoverError ? (
                <UiBanner tone="warning" title={t("settings.addAccount.discoverFailedTitle")}>
                  {t("settings.addAccount.discoverFailedBody")} {discoverError}
                </UiBanner>
              ) : null}
              {discovery?.note ? (
                <UiBanner tone="info" title={t("settings.addAccount.providerNote")}>
                  <div>{translateMaybeKey(discovery.note)}</div>
                  {HELP_URLS[tileId ?? "imap"] ? (
                    <div className={styles.actions}>
                      <Button size="small" appearance="subtle" onClick={openHelp}>
                        {t("settings.addAccount.openHelp")}
                      </Button>
                    </div>
                  ) : null}
                </UiBanner>
              ) : null}

              <Input
                label={t("settings.addAccount.displayName")}
                value={form.displayName}
                autoFocus
                error={
                  fieldErrors.displayName
                    ? t("settings.addAccount.fieldRequired")
                    : undefined
                }
                onChange={(event) => patchForm({ displayName: event.target.value })}
              />
              <Input
                label={t("settings.addAccount.email")}
                type="email"
                value={form.email}
                spellCheck={false}
                disabled={form.authKind === "oauth"}
                error={
                  fieldErrors.email
                    ? fieldErrors.email === "invalidEmail"
                      ? t("common.invalidEmail")
                      : t("settings.addAccount.fieldRequired")
                    : undefined
                }
                onChange={(event) => patchForm({ email: event.target.value })}
              />

              {form.authKind === "password" ? (
                <Input
                  label={isDav ? t("settings.addAccount.davPassword") : t("settings.addAccount.password")}
                  type="password"
                  value={form.password}
                  autoComplete="current-password"
                  error={
                    fieldErrors.password
                      ? t("settings.addAccount.passwordRequired")
                      : undefined
                  }
                  hint={t("settings.addAccount.passwordHint")}
                  onChange={(event) => patchForm({ password: event.target.value })}
                />
              ) : (
                <UiBanner tone="success" title={t("settings.addAccount.oauthConnected")}>
                  {t("settings.addAccount.oauthConnectedBody", {
                    provider: tile?.oauthProvider === "google" ? "Google" : "Microsoft",
                  })}
                </UiBanner>
              )}

              {!isDav ? (
                <Select
                  label={t("settings.addAccount.syncWindow")}
                  value={String(form.syncWindowDays)}
                  onChange={(value) => patchForm({ syncWindowDays: Number(value) })}
                  options={SYNC_WINDOW_DAYS.map((days) => ({
                    value: String(days),
                    label:
                      days === 0
                        ? t("settings.account.syncEverything")
                        : t("settings.account.syncDays", { count: days }),
                  }))}
                />
              ) : null}

              <div className={styles.rowStack}>
                <div className={styles.sectionTitle}>{t("settings.addAccount.modules")}</div>
                <Switch
                  checked={form.mailEnabled}
                  disabled={isDav}
                  onChange={(checked) => patchForm({ mailEnabled: checked })}
                  label={t("settings.addAccount.moduleMail")}
                />
                <Switch
                  checked={form.calendarEnabled}
                  onChange={(checked) => patchForm({ calendarEnabled: checked })}
                  label={t("settings.addAccount.moduleCalendar")}
                />
                <Switch
                  checked={form.contactsEnabled}
                  onChange={(checked) => patchForm({ contactsEnabled: checked })}
                  label={t("settings.addAccount.moduleContacts")}
                />
                <Switch
                  checked={form.tasksEnabled}
                  onChange={(checked) => patchForm({ tasksEnabled: checked })}
                  label={t("settings.addAccount.moduleTasks")}
                />
              </div>

              <button
                  type="button"
                  className={styles.advancedToggle}
                  onClick={() => setAdvancedOpen((value) => !value)}
                  aria-expanded={advancedOpen}
                >
                  <span>{t("settings.addAccount.advanced")}</span>
                  {advancedOpen ? <ChevronUp24Regular /> : <ChevronDown24Regular />}
                </button>

              {advancedOpen ? (
                <div className={styles.advancedBody}>
                  {!isDav && tileId === "imap" ? (
                    <Select
                      label={t("settings.addAccount.protocol")}
                      value={form.protocol}
                      onChange={(value) => patchForm({ protocol: value as AccountProtocol })}
                      options={[
                        { value: "imap", label: "IMAP" },
                        { value: "pop3", label: "POP3" },
                      ]}
                    />
                  ) : null}
                  {isDav ? null : form.protocol === "pop3" ? (
                    <>
                      <ServerFields
                        label="POP3"
                        draft={form.pop3}
                        onChange={(draft) => patchForm({ pop3: draft })}
                      />
                      <ServerFields
                        label="SMTP"
                        draft={form.smtp}
                        onChange={(draft) => patchForm({ smtp: draft })}
                      />
                    </>
                  ) : (
                    <>
                      <ServerFields
                        label="IMAP"
                        draft={form.imap}
                        onChange={(draft) => patchForm({ imap: draft })}
                      />
                      <ServerFields
                        label="SMTP"
                        draft={form.smtp}
                        onChange={(draft) => patchForm({ smtp: draft })}
                      />
                    </>
                  )}
                  <Input
                    label="CalDAV"
                    value={form.caldavUrl}
                    placeholder="https://example.com/caldav/"
                    spellCheck={false}
                    error={
                      fieldErrors.caldavUrl ? t("settings.addAccount.invalidUrl") : undefined
                    }
                    onChange={(event) => patchForm({ caldavUrl: event.target.value })}
                  />
                  <Input
                    label="CardDAV"
                    value={form.carddavUrl}
                    placeholder="https://example.com/carddav/"
                    spellCheck={false}
                    error={
                      fieldErrors.carddavUrl ? t("settings.addAccount.invalidUrl") : undefined
                    }
                    onChange={(event) => patchForm({ carddavUrl: event.target.value })}
                  />
                </div>
              ) : null}

              {testResult ? <TestResults result={testResult} /> : null}
              {formError ? (
                <UiBanner tone="error" title={t("settings.addAccount.cannotContinue")}>
                  {formError}
                </UiBanner>
              ) : null}
            </div>
          ) : null}
        </div>
    </Dialog>
  );
}

function TestResults({ result }: { result: ConnectionTestResult }) {
  const t = useT();
  const rows: { label: string; value: boolean | null }[] = [
    { label: "IMAP", value: result.imapOk },
    { label: "SMTP", value: result.smtpOk },
    { label: "POP3", value: result.pop3Ok },
    { label: "CalDAV / CardDAV", value: result.davOk },
  ];
  const visible = rows.filter((row) => row.value !== null);
  return (
    <div className={styles.testResults}>
      <div className={styles.sectionTitle}>{t("settings.addAccount.testResults")}</div>
      {visible.length === 0 ? (
        <div className={styles.hint}>{t("settings.addAccount.testNoResult")}</div>
      ) : null}
      {visible.map((row) => (
        <div key={row.label} className={styles.testRow}>
          <span className={row.value ? styles.testOk : styles.testFail} aria-hidden="true">
            {row.value ? <CheckmarkCircle16Regular /> : <ErrorCircle16Regular />}
          </span>
          <span>{row.label}</span>
          <span className={row.value ? styles.testOk : styles.testFail}>
            {row.value ? t("settings.addAccount.testOk") : t("settings.addAccount.testFailedShort")}
          </span>
        </div>
      ))}
      {result.error ? <div className={styles.errorText}>{result.error}</div> : null}
    </div>
  );
}
