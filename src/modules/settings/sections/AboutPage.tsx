/**
 * About Nuncio: update controls plus version, platform, paths, links, licence
 * and privacy statement.
 *
 * This is a top-level settings entry (it used to be a sub-page reached from the
 * bottom of General), so the update section that also lived in General now lives
 * here instead of being duplicated.
 */
import { Copy24Regular, Open24Regular } from "@fluentui/react-icons";
import { Button, IconButton, Spinner, toast } from "@/components";
import { useT } from "@/lib/i18n";
import { app } from "@/ipc/api";
import { copyText, isDesktop, openExternal } from "../lib/desktop";
import { describeError } from "../lib/errors";
import { useAppInfo } from "../lib/hooks";
import { Banner, Card, Section, SettingsPage } from "../components/SettingsUI";
import { UpdateSection } from "./UpdateSection";
import styles from "../settings.module.css";

const PLATFORM_LABEL: Record<string, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
};

/** Official project links, rendered as buttons and opened in the OS browser. */
const ABOUT_LINKS: ReadonlyArray<{ labelKey: string; url: string }> = [
  { labelKey: "settings.about.homepage", url: "https://jacksonary.github.io/nuncio/" },
  { labelKey: "settings.about.privacyPolicy", url: "https://jacksonary.github.io/nuncio/privacy.html" },
  { labelKey: "settings.about.github", url: "https://github.com/Jacksonary/nuncio" },
  { labelKey: "settings.about.gitee", url: "https://gitee.com/weiguoliu/nuncio" },
  { labelKey: "settings.about.feedback", url: "https://github.com/Jacksonary/nuncio/issues" },
];

export function AboutPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const info = useAppInfo();

  const copy = async (value: string, label: string) => {
    const ok = await copyText(value);
    if (ok) toast.success(t("settings.about.copied", { label }));
    else toast.error(t("settings.about.copyFailed"));
  };

  const openLogs = async () => {
    if (!isDesktop()) {
      toast.info(t("settings.data.desktopOnly"));
      return;
    }
    try {
      await app.openLogDir();
    } catch (error) {
      toast.error(t("settings.data.openLogsFailed"), describeError(error, t));
    }
  };

  const openLink = async (url: string) => {
    // The list above is static, but validate defensively so nothing other than
    // http(s) can ever reach the OS handler.
    let target: URL;
    try {
      target = new URL(url);
    } catch {
      return;
    }
    if (target.protocol !== "http:" && target.protocol !== "https:") return;
    try {
      await openExternal(target.toString());
    } catch (error) {
      toast.error(t("settings.about.openLinkFailed"), describeError(error, t));
    }
  };

  return (
    <SettingsPage title={t("settings.home.about")} subtitle={t("settings.home.aboutDesc")} onBack={onBack}>
      <UpdateSection />

      <Section>
        <Card className={styles.cardPad}>
          {info.isLoading ? (
            <div className={styles.spinnerRow}>
              <Spinner size="small" />
              <span>{t("common.loading")}</span>
            </div>
          ) : info.data ? (
            <div className={styles.aboutGrid}>
              <span className={styles.aboutLabel}>{t("settings.about.appName")}</span>
              <span className={styles.aboutValue}>Nuncio</span>
              <span className={styles.aboutLabel}>{t("settings.about.version")}</span>
              <span className={styles.aboutValue}>{info.data.version}</span>
              <span className={styles.aboutLabel}>{t("settings.about.platform")}</span>
              <span className={styles.aboutValue}>
                {PLATFORM_LABEL[info.data.platform] ?? info.data.platform}
              </span>
              <span className={styles.aboutLabel}>{t("settings.about.dataDir")}</span>
              <span className={styles.aboutValue}>
                <span className={styles.mono}>{info.data.dataDir}</span>{" "}
                <IconButton
                  icon={<Copy24Regular />}
                  label={t("settings.about.copyPath")}
                  size="small"
                  tooltip={false}
                  onClick={() => void copy(info.data!.dataDir, t("settings.about.dataDir"))}
                />
              </span>
              <span className={styles.aboutLabel}>{t("settings.about.logDir")}</span>
              <span className={styles.aboutValue}>
                <span className={styles.mono}>{info.data.logDir}</span>{" "}
                <IconButton
                  icon={<Copy24Regular />}
                  label={t("settings.about.copyPath")}
                  size="small"
                  tooltip={false}
                  onClick={() => void copy(info.data!.logDir, t("settings.about.logDir"))}
                />
                <IconButton
                  icon={<Open24Regular />}
                  label={t("settings.data.openLogs")}
                  size="small"
                  tooltip={false}
                  onClick={() => void openLogs()}
                />
              </span>
              <span className={styles.aboutLabel}>{t("settings.about.license")}</span>
              <span className={styles.aboutValue}>{t("settings.about.licenseValue")}</span>
            </div>
          ) : (
            <Banner tone="warning" title={t("settings.about.infoUnavailable")}>
              {describeError(info.error, t)}
            </Banner>
          )}
        </Card>
      </Section>

      <Section title={t("settings.about.linksTitle")}>
        <Card className={styles.cardPad}>
          <div className={styles.aboutLinks}>
            {ABOUT_LINKS.map((link) => (
              <Button
                key={link.url}
                appearance="transparent"
                size="small"
                icon={<Open24Regular />}
                title={link.url}
                className={styles.aboutLinkButton}
                onClick={() => void openLink(link.url)}
              >
                {t(link.labelKey)}
              </Button>
            ))}
          </div>
        </Card>
      </Section>

      <Section title={t("settings.about.privacyTitle")}>
        <Banner tone="success" title={t("settings.about.privacyHeadline")}>
          {t("settings.about.privacyBody")}
        </Banner>
      </Section>

      <Section title={t("settings.about.acknowledgementTitle")}>
        <Card className={styles.cardPad}>
          <p>{t("settings.about.acknowledgement")}</p>
          <p className={styles.hint}>{t("settings.about.acknowledgementBody")}</p>
        </Card>
      </Section>

      <Section title={t("settings.about.opensourceTitle")}>
        <Card className={styles.cardPad}>
          <p className={styles.hint}>{t("settings.about.opensourceBody")}</p>
        </Card>
      </Section>
    </SettingsPage>
  );
}
