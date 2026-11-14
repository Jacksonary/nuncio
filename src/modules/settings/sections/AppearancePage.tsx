/** Personalization: theme, accent, language, density and compose defaults. */
import { Checkbox, ColorPicker, RadioGroup, Select, Switch } from "@/components";
import { useT } from "@/lib/i18n";
import type { HoverAction } from "@/ipc/types";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, SettingsPage, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

const HOVER_ACTIONS: HoverAction[] = ["archive", "delete", "flag", "read", "move", "none"];

const FONT_FAMILIES = [
  "Segoe UI",
  "Microsoft YaHei UI",
  "PingFang SC",
  "Arial",
  "Georgia",
  "Times New Roman",
  "Consolas",
];

export function AppearancePage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();

  const toggleHoverAction = (action: HoverAction, checked: boolean) => {
    const current = settings.mail.hoverActions;
    let next: HoverAction[];
    if (action === "none") {
      next = checked ? ["none"] : [];
    } else {
      next = checked
        ? [...current.filter((item) => item !== "none"), action]
        : current.filter((item) => item !== action);
    }
    update({ mail: { hoverActions: next } });
  };

  const fontOptions = FONT_FAMILIES.includes(settings.mail.defaultFontFamily)
    ? FONT_FAMILIES
    : [settings.mail.defaultFontFamily, ...FONT_FAMILIES];

  return (
    <SettingsPage title={t("settings.appearance.title")} subtitle={t("settings.appearance.subtitle")} onBack={onBack}>
      <Section title={t("settings.appearance.themeSection")}>
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

      <Section title={t("settings.appearance.listSection")}>
        <Card>
          <SettingRow label={t("settings.appearance.density")} description={t("settings.appearance.densityDesc")}>
            <Select
              ariaLabel={t("settings.appearance.density")}
              value={settings.mail.density}
              onChange={(value) =>
                update({ mail: { density: value as typeof settings.mail.density } })
              }
              options={[
                { value: "compact", label: t("settings.appearance.densityCompact") },
                { value: "normal", label: t("settings.appearance.densityNormal") },
                { value: "spacious", label: t("settings.appearance.densitySpacious") },
              ]}
            />
          </SettingRow>
          <SettingRow label={t("settings.appearance.previewLines")} description={t("settings.appearance.previewLinesDesc")}>
            <Select
              ariaLabel={t("settings.appearance.previewLines")}
              value={String(settings.mail.previewLines)}
              onChange={(value) => update({ mail: { previewLines: Number(value) } })}
              options={[0, 1, 2, 3].map((value) => ({
                value: String(value),
                label:
                  value === 0
                    ? t("settings.appearance.previewNone")
                    : t("settings.appearance.previewCount", { count: value }),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.appearance.avatars")} description={t("settings.appearance.avatarsDesc")}>
            <Switch
              checked={settings.mail.showAvatars}
              onChange={(checked) => update({ mail: { showAvatars: checked } })}
              aria-label={t("settings.appearance.avatarsToggle")}
            />
          </SettingRow>
          <SettingRow
            label={t("settings.appearance.messageBackground")}
            description={t("settings.appearance.messageBackgroundDesc")}
          >
            <Switch
              checked={settings.mail.lightMessageBackground}
              onChange={(checked) => update({ mail: { lightMessageBackground: checked } })}
              aria-label={t("settings.appearance.messageBackgroundToggle")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.appearance.actionsSection")} hint={t("settings.appearance.actionsHint")}>
        <Card>
          <StackRow>
            <div className={styles.rowLabel}>{t("settings.appearance.hoverActions")}</div>
            <div className={styles.chipRow}>
              {HOVER_ACTIONS.map((action) => (
                <Checkbox
                  key={action}
                  label={t(`settings.hoverAction.${action}`)}
                  checked={settings.mail.hoverActions.includes(action)}
                  onChange={(event) => toggleHoverAction(action, event.target.checked)}
                />
              ))}
            </div>
          </StackRow>
          <SettingRow label={t("settings.appearance.swipeLeft")} description={t("settings.appearance.swipeHint")}>
            <Select
              ariaLabel={t("settings.appearance.swipeLeft")}
              value={settings.mail.swipeLeft}
              onChange={(value) =>
                update({ mail: { swipeLeft: value as HoverAction } })
              }
              options={HOVER_ACTIONS.map((action) => ({
                value: action,
                label: t(`settings.hoverAction.${action}`),
              }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.appearance.swipeRight")}>
            <Select
              ariaLabel={t("settings.appearance.swipeRight")}
              value={settings.mail.swipeRight}
              onChange={(value) =>
                update({ mail: { swipeRight: value as HoverAction } })
              }
              options={HOVER_ACTIONS.map((action) => ({
                value: action,
                label: t(`settings.hoverAction.${action}`),
              }))}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.appearance.composeSection")}>
        <Card>
          <SettingRow label={t("settings.appearance.fontFamily")} wide>
            <Select
              ariaLabel={t("settings.appearance.fontFamily")}
              value={settings.mail.defaultFontFamily}
              onChange={(value) => update({ mail: { defaultFontFamily: value } })}
              options={fontOptions.map((font) => ({ value: font, label: font }))}
            />
          </SettingRow>
          <SettingRow label={t("settings.appearance.fontSize")} description={t("settings.appearance.fontSizeDesc")}>
            <Select
              ariaLabel={t("settings.appearance.fontSize")}
              value={String(settings.mail.defaultFontSize)}
              onChange={(value) => update({ mail: { defaultFontSize: Number(value) } })}
              options={[11, 12, 13, 14, 15, 16, 18, 20].map((size) => ({
                value: String(size),
                label: t("settings.appearance.fontSizeValue", { size }),
              }))}
            />
          </SettingRow>
        </Card>
      </Section>
    </SettingsPage>
  );
}
