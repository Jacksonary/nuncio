/** Advanced OAuth client ids; embedded under a disclosure on the Accounts page. */
import { useState } from "react";
import { Eye24Regular, EyeOff24Regular, Open24Regular } from "@fluentui/react-icons";
import { Button, IconButton, Input, confirm, toast } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { openExternal } from "../lib/desktop";
import { useAppInfo } from "../lib/hooks";
import {
  clientSecretPatch,
  hasStoredClientSecret,
} from "../lib/clientSecret";
import { Banner, Card, Hint, Section, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

const GOOGLE_CREDENTIALS_URL = "https://console.cloud.google.com/apis/credentials";
const MICROSOFT_ENTRA_URL = "https://entra.microsoft.com/";

export function AdvancedContent() {
  const t = useT();
  const { settings, updateAsync } = useSettings();
  const appInfo = useAppInfo();
  const [googleClientId, setGoogleClientId] = useState(settings.advanced.googleClientId ?? "");
  // The backend never returns the secret, only the mask. Keep the input empty
  // so the user explicitly types a replacement; an empty draft means
  // "unchanged" and is omitted from the patch.
  const [googleClientSecret, setGoogleClientSecret] = useState("");
  const [microsoftClientId, setMicrosoftClientId] = useState(
    settings.advanced.microsoftClientId ?? "",
  );
  const [showSecret, setShowSecret] = useState(false);
  const [saving, setSaving] = useState(false);

  const storedSecret = hasStoredClientSecret(settings.advanced.googleClientSecret);

  // The inputs are seeded once from `useState` initialisers and never re-seeded
  // from the settings cache. That cache is written optimistically, so watching it
  // would overwrite what the user is typing — and on a failed save the rollback
  // looks like an external change and wipes the retry. Closing and reopening the
  // settings flyout remounts this page, which re-reads the stored values.

  const save = async () => {
    const advanced: {
      googleClientId: string | null;
      microsoftClientId: string | null;
      googleClientSecret?: string;
    } = {
      googleClientId: googleClientId.trim() || null,
      microsoftClientId: microsoftClientId.trim() || null,
    };
    const secret = clientSecretPatch(googleClientSecret);
    if (secret !== undefined) advanced.googleClientSecret = secret;
    setSaving(true);
    try {
      // Await the write: clearing the input and reporting success before the
      // backend has stored anything loses whatever the user typed.
      await updateAsync({ advanced });
      setGoogleClientSecret("");
      toast.success(t("settings.advanced.saved"));
    } catch {
      // useUpdateSettings already reverted the optimistic value and reported
      // the failure in a toast.
    } finally {
      setSaving(false);
    }
  };

  const clearSecret = async () => {
    const ok = await confirm({
      title: t("settings.advanced.clearSecret"),
      message: `${t("settings.advanced.secretStoredHint")} ${t("settings.rules.deleteMessage")}`,
      confirmLabel: t("settings.advanced.clearSecret"),
      danger: true,
    });
    if (!ok) return;
    // "" tells the backend to remove the keyring entry.
    setSaving(true);
    try {
      await updateAsync({ advanced: { googleClientSecret: "" } });
      setGoogleClientSecret("");
      toast.success(t("settings.advanced.secretCleared"));
    } catch {
      // Reported by useUpdateSettings; the stored secret is still there.
    } finally {
      setSaving(false);
    }
  };

  return (
    <>
      <Banner tone="info" title={t("settings.advanced.whyTitle")}>
        {t("settings.advanced.why")}
      </Banner>

      <Section
        title="Google"
        hint={
          appInfo.data?.buildHasGoogleClient
            ? t("settings.advanced.buildHasClient")
            : t("settings.advanced.buildMissingClient")
        }
      >
        <Card>
          <StackRow>
            <Input
              label={t("settings.advanced.googleClientId")}
              value={googleClientId}
              placeholder={t("settings.advanced.clientIdPlaceholder")}
              spellCheck={false}
              onChange={(event) => setGoogleClientId(event.target.value)}
            />
          </StackRow>
          <StackRow>
            <Input
              label={t("settings.advanced.googleClientSecret")}
              value={googleClientSecret}
              placeholder={
                storedSecret
                  ? t("settings.advanced.secretStoredPlaceholder")
                  : t("settings.advanced.clientSecretPlaceholder")
              }
              hint={storedSecret ? t("settings.advanced.secretStoredHint") : undefined}
              type={showSecret ? "text" : "password"}
              spellCheck={false}
              autoComplete="off"
              suffix={
                <IconButton
                  icon={showSecret ? <EyeOff24Regular /> : <Eye24Regular />}
                  label={showSecret ? t("settings.advanced.hideSecret") : t("settings.advanced.showSecret")}
                  size="small"
                  onClick={() => setShowSecret((value) => !value)}
                />
              }
              onChange={(event) => setGoogleClientSecret(event.target.value)}
            />
          </StackRow>
          <StackRow>
            <div className={styles.actions}>
              <Button
                appearance="subtle"
                size="small"
                icon={<Open24Regular />}
                onClick={() => void openExternal(GOOGLE_CREDENTIALS_URL)}
              >
                {t("settings.advanced.openGoogleConsole")}
              </Button>
              {storedSecret ? (
                <Button appearance="danger" size="small" onClick={clearSecret} disabled={saving}>
                  {t("settings.advanced.clearSecret")}
                </Button>
              ) : null}
            </div>
          </StackRow>
        </Card>
      </Section>

      <Section title="Microsoft">
        <Card>
          <StackRow>
            <Input
              label={t("settings.advanced.microsoftClientId")}
              value={microsoftClientId}
              placeholder={t("settings.advanced.clientIdPlaceholder")}
              spellCheck={false}
              onChange={(event) => setMicrosoftClientId(event.target.value)}
            />
          </StackRow>
          <StackRow>
            <Button
              appearance="subtle"
              size="small"
              icon={<Open24Regular />}
              onClick={() => void openExternal(MICROSOFT_ENTRA_URL)}
            >
              {t("settings.advanced.openMicrosoftPortal")}
            </Button>
          </StackRow>
        </Card>
      </Section>

      <div className={styles.actions}>
        <Button appearance="primary" onClick={save} disabled={saving}>
          {t("common.save")}
        </Button>
      </div>

      <Hint>{t("settings.advanced.restartNote")}</Hint>
      <Hint>{t("settings.advanced.securityNote")}</Hint>
    </>
  );
}
