/** Reading experience: remote images, conversations and cache. */
import { Input, Select, Switch } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, StackRow } from "../components/SettingsUI";

/** Reading behaviour and cache limits; embedded by the Mail page. */
export function ReadingContent() {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <>
      <Section title={t("settings.reading.behavior")}>
        <Card>
          <SettingRow label={t("settings.reading.remoteImages")} description={t("settings.reading.remoteImagesDesc")}>
            <Select
              ariaLabel={t("settings.reading.remoteImages")}
              value={settings.mail.remoteImages}
              onChange={(value) =>
                update({ mail: { remoteImages: value as typeof settings.mail.remoteImages } })
              }
              options={[
                { value: "ask", label: t("settings.reading.remoteAsk") },
                { value: "trustedOnly", label: t("settings.reading.remoteTrusted") },
                { value: "always", label: t("settings.reading.remoteAlways") },
              ]}
            />
          </SettingRow>
          <SettingRow label={t("settings.reading.conversation")} description={t("settings.reading.conversationDesc")}>
            <Switch
              checked={settings.mail.conversationView}
              onChange={(checked) => update({ mail: { conversationView: checked } })}
              aria-label={t("settings.reading.conversationToggle")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.reading.performance")} hint={t("settings.reading.performanceHint")}>
        <Card>
          <StackRow>
            <Input
              type="number"
              min={50}
              max={8192}
              step={50}
              value={String(settings.mail.bodyCacheLimitMb)}
              label={t("settings.reading.cacheLimit")}
              hint={t("settings.reading.cacheLimitHint")}
              onChange={(event) =>
                update({
                  mail: { bodyCacheLimitMb: clamp(Number(event.target.value), 50, 8192) },
                })
              }
            />
          </StackRow>
          <StackRow>
            <Input
              type="number"
              min={1}
              max={1440}
              value={String(settings.mail.otherFoldersPollMinutes)}
              label={t("settings.reading.pollMinutes")}
              hint={t("settings.reading.pollMinutesHint")}
              onChange={(event) =>
                update({
                  mail: { otherFoldersPollMinutes: clamp(Number(event.target.value), 1, 1440) },
                })
              }
            />
          </StackRow>
        </Card>
      </Section>
    </>
  );
}

function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
}
