/** Reading experience: mark-as-read, remote images, conversations and cache. */
import { Input, Select, Switch } from "@/components";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/useSettings";
import { SettingRow, Section, Card, SettingsPage, StackRow } from "../components/SettingsUI";
import styles from "../settings.module.css";

export function ReadingPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const { settings, update } = useSettings();

  return (
    <SettingsPage title={t("settings.reading.title")} subtitle={t("settings.reading.subtitle")} onBack={onBack}>
      <Section title={t("settings.reading.behavior")}>
        <Card>
          <SettingRow label={t("settings.reading.markRead")} description={t("settings.reading.markReadDesc")}>
            <Select
              ariaLabel={t("settings.reading.markRead")}
              value={settings.mail.markReadMode}
              onChange={(value) =>
                update({ mail: { markReadMode: value as typeof settings.mail.markReadMode } })
              }
              options={[
                { value: "onOpen", label: t("settings.reading.markReadOnOpen") },
                { value: "afterDelay", label: t("settings.reading.markReadAfterDelay") },
                { value: "manual", label: t("settings.reading.markReadManual") },
              ]}
            />
          </SettingRow>
          {settings.mail.markReadMode === "afterDelay" ? (
            <SettingRow label={t("settings.reading.markReadDelay")} description={t("settings.reading.markReadDelayDesc")}>
              <Select
                ariaLabel={t("settings.reading.markReadDelay")}
                value={String(settings.mail.markReadDelaySeconds)}
                onChange={(value) => update({ mail: { markReadDelaySeconds: Number(value) } })}
                options={[2, 3, 5, 10, 15, 30].map((seconds) => ({
                  value: String(seconds),
                  label: t("settings.reading.seconds", { count: seconds }),
                }))}
              />
            </SettingRow>
          ) : null}
          <SettingRow label={t("settings.reading.autoAdvance")} description={t("settings.reading.autoAdvanceDesc")}>
            <Select
              ariaLabel={t("settings.reading.autoAdvance")}
              value={settings.mail.autoAdvance}
              onChange={(value) =>
                update({ mail: { autoAdvance: value as typeof settings.mail.autoAdvance } })
              }
              options={[
                { value: "next", label: t("settings.reading.autoNext") },
                { value: "previous", label: t("settings.reading.autoPrevious") },
                { value: "list", label: t("settings.reading.autoList") },
              ]}
            />
          </SettingRow>
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
          <SettingRow label={t("settings.reading.quoteOriginal")} description={t("settings.reading.quoteOriginalDesc")}>
            <Switch
              checked={settings.mail.quoteOriginal}
              onChange={(checked) => update({ mail: { quoteOriginal: checked } })}
              aria-label={t("settings.reading.quoteOriginalToggle")}
            />
          </SettingRow>
          <SettingRow label={t("settings.reading.confirmDelete")} description={t("settings.reading.confirmDeleteDesc")}>
            <Switch
              checked={settings.mail.confirmDelete}
              onChange={(checked) => update({ mail: { confirmDelete: checked } })}
              aria-label={t("settings.reading.confirmDeleteToggle")}
            />
          </SettingRow>
        </Card>
      </Section>

      <Section title={t("settings.reading.performance")} hint={t("settings.reading.performanceHint")}>
        <Card>
          <StackRow>
            <Input
              type="number"
              min={0}
              max={500}
              value={String(settings.mail.prefetchUnreadBodies)}
              label={t("settings.reading.prefetch")}
              hint={t("settings.reading.prefetchHint")}
              onChange={(event) =>
                update({
                  mail: {
                    prefetchUnreadBodies: clamp(Number(event.target.value), 0, 500),
                  },
                })
              }
            />
          </StackRow>
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
    </SettingsPage>
  );
}

function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
}
