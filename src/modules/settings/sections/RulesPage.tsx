/** Mail rules: list, enable, editor, delete and "run now" on a folder. */
import { useEffect, useMemo, useState } from "react";
import {
  Add24Regular,
  Delete24Regular,
  Edit24Regular,
  Play24Regular,
} from "@fluentui/react-icons";
import {
  Button,
  Dialog,
  IconButton,
  Input,
  RadioGroup,
  Select,
  Switch,
  confirm,
  toast,
} from "@/components";
import { useT } from "@/lib/i18n";
import type { Account, Folder, MailRule, RuleActionKind, RuleField, RuleOp } from "@/ipc/types";
import {
  useAccounts,
  useDeleteRule,
  useFolderCollection,
  useRules,
  useRunRule,
  useUpsertRule,
} from "../lib/hooks";
import { describeError } from "../lib/errors";
import {
  RULE_ACTION_KINDS,
  RULE_FIELDS,
  RULE_OPS_BY_FIELD,
  createAction,
  createCondition,
  createRule,
  serializeRule,
  validateRule,
  withActionKind,
  withField,
} from "../lib/ruleModel";
import { Card, Section, SettingsPage, SettingRow, StackRow, Banner } from "../components/SettingsUI";
import styles from "../settings.module.css";

export function RulesPage({ onBack }: { onBack: () => void }) {
  const t = useT();
  const rulesQuery = useRules();
  const accountsQuery = useAccounts();
  const upsert = useUpsertRule();
  const remove = useDeleteRule();
  const accountIds = useMemo(
    () => (accountsQuery.data ?? []).map((account) => account.id),
    [accountsQuery.data],
  );
  const { folders: allFolders } = useFolderCollection(accountIds);
  const [draft, setDraft] = useState<{ rule: MailRule; isNew: boolean } | null>(null);
  const [runTarget, setRunTarget] = useState<MailRule | null>(null);

  const rules = useMemo(() => {
    const list = [...(rulesQuery.data ?? [])];
    list.sort((a, b) => a.sortOrder - b.sortOrder || a.name.localeCompare(b.name));
    return list;
  }, [rulesQuery.data]);

  const toggleRule = (rule: MailRule, enabled: boolean) => {
    upsert.mutate(
      { ...rule, enabled },
      {
        onError: (error) => toast.error(t("settings.rules.toggleFailed"), describeError(error, t)),
      },
    );
  };

  const onDelete = async (rule: MailRule) => {
    const ok = await confirm({
      title: t("settings.rules.deleteTitle", { name: rule.name }),
      message: t("settings.rules.deleteMessage"),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await remove.mutateAsync(rule.id);
      toast.success(t("settings.rules.deleted"));
    } catch (error) {
      toast.error(t("settings.rules.deleteFailed"), describeError(error, t));
    }
  };

  if (draft) {
    return (
      <RuleEditor
        rule={draft.rule}
        isNew={draft.isNew}
        accounts={accountsQuery.data ?? []}
        folders={allFolders}
        onCancel={() => setDraft(null)}
        onSaved={() => setDraft(null)}
        onDeleted={() => setDraft(null)}
      />
    );
  }

  return (
    <SettingsPage
      title={t("settings.rules.title")}
      subtitle={t("settings.rules.subtitle")}
      onBack={onBack}
      actions={
        <Button
          appearance="primary"
          size="small"
          icon={<Add24Regular />}
          onClick={() => setDraft({ rule: createRule(), isNew: true })}
        >
          {t("settings.rules.new")}
        </Button>
      }
    >
      {rules.length > 0 ? (
        <div className={styles.ruleList}>
          {rules.map((rule) => {
            const account = rule.accountId
              ? accountsQuery.data?.find((item) => item.id === rule.accountId)
              : null;
            return (
              <div key={rule.id} className={styles.ruleRow}>
                <div className={styles.ruleMain}>
                  <div className={styles.ruleName}>{rule.name || t("common.untitled")}</div>
                  <div className={styles.ruleSummary}>
                    {account ? `${account.displayName} · ` : `${t("settings.rules.allAccounts")} · `}
                    {t("settings.rules.summary", {
                      conditions: rule.conditions.length,
                      actions: rule.actions.length,
                    })}
                    {rule.stopProcessing ? ` · ${t("settings.rules.stopProcessingShort")}` : ""}
                  </div>
                </div>
                <div className={styles.ruleSide}>
                  <Switch
                    checked={rule.enabled}
                    onChange={(checked) => toggleRule(rule, checked)}
                    aria-label={t("settings.rules.enableToggle", { name: rule.name })}
                  />
                  <Button
                    size="small"
                    appearance="subtle"
                    icon={<Play24Regular />}
                    disabled={!rule.enabled}
                    onClick={() => setRunTarget(rule)}
                  >
                    {t("settings.rules.run")}
                  </Button>
                  <IconButton
                    icon={<Edit24Regular />}
                    label={t("common.edit")}
                    size="small"
                    appearance="subtle"
                    onClick={() => setDraft({ rule, isNew: false })}
                  />
                  <IconButton
                    icon={<Delete24Regular />}
                    label={t("common.delete")}
                    size="small"
                    appearance="subtle"
                    onClick={() => void onDelete(rule)}
                  />
                </div>
              </div>
            );
          })}
        </div>
      ) : (
        <Card>
          <div className={styles.emptyHint}>
            {rulesQuery.isLoading ? t("common.loading") : t("settings.rules.empty")}
          </div>
        </Card>
      )}

      <Banner tone="info">{t("settings.rules.info")}</Banner>

      <RunRuleDialog
        rule={runTarget}
        accounts={accountsQuery.data ?? []}
        folders={allFolders}
        onClose={() => setRunTarget(null)}
      />
    </SettingsPage>
  );
}

interface RuleEditorProps {
  rule: MailRule;
  isNew: boolean;
  accounts: Account[];
  folders: Folder[];
  onCancel: () => void;
  onSaved: () => void;
  onDeleted: () => void;
}

function RuleEditor({ rule, isNew, accounts, folders, onCancel, onSaved, onDeleted }: RuleEditorProps) {
  const t = useT();
  const [value, setValue] = useState<MailRule>(rule);
  const [issues, setIssues] = useState<string[]>([]);
  const upsert = useUpsertRule();
  const remove = useDeleteRule();

  const accountFolders = value.accountId
    ? folders.filter((folder) => folder.accountId === value.accountId)
    : folders;
  const folderOptions = buildFolderOptions(accountFolders, accounts);

  const patch = (next: Partial<MailRule>) => setValue((current) => ({ ...current, ...next }));

  const patchCondition = (index: number, next: Partial<MailRule["conditions"][number]>) => {
    setValue((current) => ({
      ...current,
      conditions: current.conditions.map((condition, i) =>
        i === index ? { ...condition, ...next } : condition,
      ),
    }));
  };

  const patchAction = (index: number, next: Partial<MailRule["actions"][number]>) => {
    setValue((current) => ({
      ...current,
      actions: current.actions.map((action, i) => (i === index ? { ...action, ...next } : action)),
    }));
  };

  const save = async () => {
    // Validate the rows the user sees (serialization drops incomplete rows,
    // which would otherwise hide "choose a folder" style errors).
    const nextIssues = validateRule(value);
    if (nextIssues.length > 0) {
      setIssues(nextIssues);
      return;
    }
    setIssues([]);
    const normalized = serializeRule(value);
    try {
      await upsert.mutateAsync(normalized);
      toast.success(t("settings.rules.saved", { name: normalized.name }));
      onSaved();
    } catch (error) {
      toast.error(t("settings.rules.saveFailed"), describeError(error, t));
    }
  };

  const onDelete = async () => {
    const ok = await confirm({
      title: t("settings.rules.deleteTitle", { name: value.name }),
      message: t("settings.rules.deleteMessage"),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await remove.mutateAsync(value.id);
      toast.success(t("settings.rules.deleted"));
      onDeleted();
    } catch (error) {
      toast.error(t("settings.rules.deleteFailed"), describeError(error, t));
    }
  };

  return (
    <SettingsPage
      title={isNew ? t("settings.rules.newTitle") : t("settings.rules.editTitle")}
      subtitle={t("settings.rules.editorSubtitle")}
      onBack={onCancel}
    >
      <Card>
        <StackRow>
          <Input
            label={t("settings.rules.name")}
            value={value.name}
            autoFocus
            onChange={(event) => patch({ name: event.target.value })}
          />
        </StackRow>
        <StackRow>
          <Select
            label={t("settings.rules.accountScope")}
            value={value.accountId ?? "all"}
            onChange={(next) => {
              const accountId = next === "all" ? null : next;
              const actions = value.actions.map((action) => {
                if (action.kind !== "move" || !action.folderId) return action;
                const folder = folders.find((item) => item.id === action.folderId);
                if (folder && (!accountId || folder.accountId === accountId)) return action;
                return { ...action, folderId: null };
              });
              patch({ accountId, actions });
            }}
            options={[
              { value: "all", label: t("settings.rules.allAccounts") },
              ...accounts.map((account) => ({
                value: account.id,
                label: account.displayName || account.email,
              })),
            ]}
          />
        </StackRow>
        <StackRow>
          <RadioGroup
            label={t("settings.rules.match")}
            value={value.matchAll ? "all" : "any"}
            onChange={(next) => patch({ matchAll: next === "all" })}
            orientation="horizontal"
            options={[
              { value: "all", label: t("settings.rules.matchAll") },
              { value: "any", label: t("settings.rules.matchAny") },
            ]}
          />
        </StackRow>
        <SettingRow label={t("settings.rules.stopProcessing")} description={t("settings.rules.stopProcessingDesc")}>
          <Switch
            checked={value.stopProcessing}
            onChange={(checked) => patch({ stopProcessing: checked })}
            aria-label={t("settings.rules.stopProcessing")}
          />
        </SettingRow>
      </Card>

      <Section
        title={t("settings.rules.conditions")}
        actions={
          <Button
            size="small"
            appearance="subtle"
            icon={<Add24Regular />}
            onClick={() => patch({ conditions: [...value.conditions, createCondition()] })}
          >
            {t("settings.rules.addCondition")}
          </Button>
        }
      >
        <Card className={styles.cardPad}>
          <div className={styles.rowStack}>
            {value.conditions.map((condition, index) => (
              <div key={`${condition.field}-${index}`} className={styles.conditionRow}>
                <div className={styles.grow}>
                  <Select
                    ariaLabel={t("settings.rules.field")}
                    value={condition.field}
                    onChange={(next) =>
                      patchCondition(index, withField(condition, next as RuleField))
                    }
                    options={RULE_FIELDS.map((field) => ({
                      value: field,
                      label: t(`settings.rules.field.${field}`),
                    }))}
                  />
                </div>
                <div className={styles.op}>
                  <Select
                    ariaLabel={t("settings.rules.operator")}
                    value={condition.op}
                    onChange={(next) => patchCondition(index, { op: next as RuleOp })}
                    options={RULE_OPS_BY_FIELD[condition.field].map((op) => ({
                      value: op,
                      label: t(`settings.rules.op.${op}`),
                    }))}
                    disabled={condition.field === "hasAttachment"}
                  />
                </div>
                {condition.field !== "hasAttachment" ? (
                  <Input
                    containerClassName={styles.valueInput}
                    aria-label={t("settings.rules.value")}
                    value={condition.value}
                    placeholder={t("settings.rules.valuePlaceholder")}
                    onChange={(event) => patchCondition(index, { value: event.target.value })}
                  />
                ) : null}
                <span className={styles.remove}>
                  <IconButton
                    icon={<Delete24Regular />}
                    label={t("settings.rules.removeCondition")}
                    size="small"
                    appearance="subtle"
                    onClick={() =>
                      patch({
                        conditions: value.conditions.filter((_item, i) => i !== index),
                      })
                    }
                  />
                </span>
              </div>
            ))}
            {value.conditions.length === 0 ? (
              <p className={styles.hint}>{t("settings.rules.noConditions")}</p>
            ) : null}
          </div>
        </Card>
      </Section>

      <Section
        title={t("settings.rules.actions")}
        actions={
          <Button
            size="small"
            appearance="subtle"
            icon={<Add24Regular />}
            onClick={() => patch({ actions: [...value.actions, createAction("markRead")] })}
          >
            {t("settings.rules.addAction")}
          </Button>
        }
      >
        <Card className={styles.cardPad}>
          <div className={styles.rowStack}>
            {value.actions.map((action, index) => (
              <div key={`${action.kind}-${index}`} className={styles.actionRow}>
                <div className={styles.grow}>
                  <Select
                    ariaLabel={t("settings.rules.action")}
                    value={action.kind}
                    onChange={(next) =>
                      patchAction(index, withActionKind(action, next as RuleActionKind))
                    }
                    options={RULE_ACTION_KINDS.map((kind) => ({
                      value: kind,
                      label: t(`settings.rules.action.${kind}`),
                    }))}
                  />
                </div>
                {action.kind === "move" ? (
                  <div className={styles.grow}>
                    <Select
                      ariaLabel={t("settings.rules.targetFolder")}
                      value={action.folderId}
                      placeholder={t("settings.rules.targetFolder")}
                      onChange={(next) => patchAction(index, { folderId: next })}
                      options={folderOptions}
                    />
                  </div>
                ) : null}
                <span className={styles.remove}>
                  <IconButton
                    icon={<Delete24Regular />}
                    label={t("settings.rules.removeAction")}
                    size="small"
                    appearance="subtle"
                    onClick={() =>
                      patch({ actions: value.actions.filter((_item, i) => i !== index) })
                    }
                  />
                </span>
              </div>
            ))}
            {value.actions.length === 0 ? (
              <p className={styles.hint}>{t("settings.rules.noActions")}</p>
            ) : null}
          </div>
        </Card>
      </Section>

      {issues.length > 0 ? (
        <Banner tone="error" title={t("settings.rules.fixIssues")}>
          <ul style={{ margin: 0, paddingLeft: 18 }}>
            {issues.map((issue) => (
              <li key={issue}>{t(`settings.rules.issue.${issue}`)}</li>
            ))}
          </ul>
        </Banner>
      ) : null}

      <div className={styles.actionsBetween}>
        <div>
          {!isNew ? (
            <Button appearance="danger" icon={<Delete24Regular />} onClick={() => void onDelete()}>
              {t("common.delete")}
            </Button>
          ) : null}
        </div>
        <div className={styles.actions}>
          <Button appearance="subtle" onClick={onCancel}>
            {t("common.cancel")}
          </Button>
          <Button appearance="primary" loading={upsert.isPending} onClick={() => void save()}>
            {t("common.save")}
          </Button>
        </div>
      </div>
    </SettingsPage>
  );
}

interface RunRuleDialogProps {
  rule: MailRule | null;
  accounts: Account[];
  folders: Folder[];
  onClose: () => void;
}

function RunRuleDialog({ rule, accounts, folders, onClose }: RunRuleDialogProps) {
  const t = useT();
  const run = useRunRule();
  const [folderId, setFolderId] = useState<string | null>(null);

  useEffect(() => {
    setFolderId(null);
  }, [rule?.id]);

  const candidates = rule
    ? folders.filter((folder) => !rule.accountId || folder.accountId === rule.accountId)
    : [];
  const options = buildFolderOptions(candidates, accounts);

  const runNow = async () => {
    if (!rule || !folderId) return;
    try {
      const count = await run.mutateAsync({ id: rule.id, folderId });
      toast.success(t("settings.rules.runDone", { count }));
      onClose();
    } catch (error) {
      toast.error(t("settings.rules.runFailed"), describeError(error, t));
    }
  };

  return (
    <Dialog
      open={rule !== null}
      onClose={onClose}
      title={t("settings.rules.runTitle")}
      footer={
        <>
          <Button appearance="subtle" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            disabled={!folderId}
            loading={run.isPending}
            onClick={() => void runNow()}
          >
            {t("settings.rules.run")}
          </Button>
        </>
      }
    >
      <p className={styles.hint}>{t("settings.rules.runHint", { name: rule?.name ?? "" })}</p>
      <Select
        label={t("settings.rules.targetFolder")}
        value={folderId}
        onChange={setFolderId}
        options={options}
        placeholder={t("settings.rules.targetFolder")}
      />
      {options.length === 0 ? <p className={styles.hint}>{t("settings.rules.noFolders")}</p> : null}
    </Dialog>
  );
}

function buildFolderOptions(folders: Folder[], accounts: Account[]) {
  const byId = new Map(folders.map((folder) => [folder.id, folder]));
  const accountNames = new Map(accounts.map((account) => [account.id, account.displayName || account.email]));
  const pathFor = (folder: Folder): string => {
    const parts: string[] = [folder.name];
    let current = folder.parentId ? byId.get(folder.parentId) : undefined;
    let guard = 0;
    while (current && guard < 12) {
      parts.unshift(current.name);
      current = current.parentId ? byId.get(current.parentId) : undefined;
      guard += 1;
    }
    return parts.join(" / ");
  };
  const sorted = [...folders].sort((a, b) => {
    const accountA = accountNames.get(a.accountId) ?? "";
    const accountB = accountNames.get(b.accountId) ?? "";
    return accountA.localeCompare(accountB) || pathFor(a).localeCompare(pathFor(b));
  });
  return sorted.map((folder) => {
    const account = accountNames.get(folder.accountId);
    return {
      value: folder.id,
      label: account ? `${account} — ${pathFor(folder)}` : pathFor(folder),
      icon: undefined,
    };
  });
}
