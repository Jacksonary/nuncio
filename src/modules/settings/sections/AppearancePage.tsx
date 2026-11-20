/** Personalization: theme, accent and language. */
import { ColorPicker, RadioGroup } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { Section, Card, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

/** Theme, accent and language; embedded by the General page. */
export function AppearanceContent() {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <Section title={t("settings.general.appearanceSection")}>
      <Card>
        <StackRow>
          <RadioGroup
            label={t("settings.appearance.theme")}
            value={settings.theme}
            onChange={(value) => update({ theme: value as typeof settings.theme })}
            options={[
              { value: "light", label: t("settings.appearance.themeLight") },
              { value: "dark", label: t("settings.appearance.themeDark") },
              { value: "system", label: t("settings.appearance.themeSystem") },
            ]}
          />
        </StackRow>
        <StackRow>
          <ColorPicker
            label={t("settings.appearance.accent")}
            value={settings.accentColor}
            allowNull
            onChange={(value) => update({ accentColor: value })}
          />
          <p className={styles.hint}>{t("settings.appearance.accentHint")}</p>
        </StackRow>
        <StackRow>
          <RadioGroup
            label={t("settings.appearance.language")}
            value={settings.language}
            onChange={(value) => update({ language: value as typeof settings.language })}
            options={[
              { value: "system", label: t("settings.appearance.languageSystem") },
              { value: "zh-CN", label: t("settings.appearance.languageZh") },
              { value: "en-US", label: t("settings.appearance.languageEn") },
            ]}
          />
        </StackRow>
      </Card>
    </Section>
  );
}
