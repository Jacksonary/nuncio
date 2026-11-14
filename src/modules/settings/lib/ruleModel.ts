/**
 * Mail-rule editor model: pure helpers so the list/editor can stay small and
 * the serialization rules are unit tested.
 *
 * Serialization rules:
 *  - `name` is trimmed.
 *  - Conditions with an empty value are dropped (except `hasAttachment`, whose
 *    operator is always `isTrue` and whose value is always empty).
 *  - `hasAttachment` is normalized to `{ field, op: "isTrue", value: "" }`.
 *  - `move` actions without a target folder are dropped; other actions are
 *    kept as-is. Duplicate actions are removed.
 *  - `sortOrder` is always an integer.
 */
import type {
  MailRule,
  RuleAction,
  RuleActionKind,
  RuleCondition,
  RuleField,
  RuleOp,
} from "@/ipc/types";
import { newId } from "./id";

export const RULE_FIELDS: RuleField[] = [
  "from",
  "to",
  "cc",
  "anyRecipient",
  "subject",
  "body",
  "hasAttachment",
];

export const RULE_OPS_BY_FIELD: Record<RuleField, RuleOp[]> = {
  from: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  to: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  cc: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  anyRecipient: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  subject: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  body: ["contains", "notContains", "equals", "startsWith", "endsWith", "matchesRegex"],
  hasAttachment: ["isTrue"],
};

export const RULE_ACTION_KINDS: RuleActionKind[] = [
  "move",
  "markRead",
  "flag",
  "delete",
  "markJunk",
  "archive",
  "notify",
  "skipNotify",
];

/** Actions that take no parameters (everything except move). */
export const SIMPLE_ACTIONS: RuleActionKind[] = RULE_ACTION_KINDS.filter(
  (kind) => kind !== "move",
);

export function createCondition(field: RuleField = "from"): RuleCondition {
  return {
    field,
    op: RULE_OPS_BY_FIELD[field][0],
    value: "",
  };
}

export function createAction(kind: RuleActionKind = "move"): RuleAction {
  return { kind, folderId: null };
}

export function createRule(partial: Partial<MailRule> = {}): MailRule {
  return {
    id: partial.id ?? newId(),
    accountId: partial.accountId ?? null,
    name: partial.name ?? "",
    enabled: partial.enabled ?? true,
    matchAll: partial.matchAll ?? true,
    conditions: partial.conditions ?? [createCondition()],
    actions: partial.actions ?? [createAction("move")],
    stopProcessing: partial.stopProcessing ?? false,
    sortOrder: partial.sortOrder ?? 0,
  };
}

/** When the field changes, reset the operator to one that is valid for it. */
export function withField(condition: RuleCondition, field: RuleField): RuleCondition {
  if (field === "hasAttachment") {
    return { field, op: "isTrue", value: "" };
  }
  const ops = RULE_OPS_BY_FIELD[field];
  return {
    field,
    op: ops.includes(condition.op) ? condition.op : ops[0],
    value: condition.value,
  };
}

export function withActionKind(action: RuleAction, kind: RuleActionKind): RuleAction {
  return { kind, folderId: kind === "move" ? action.folderId : null };
}

function conditionKey(condition: RuleCondition): string {
  return `${condition.field}\u0000${condition.op}\u0000${condition.value}`;
}

function actionKey(action: RuleAction): string {
  return `${action.kind}\u0000${action.folderId ?? ""}`;
}

/** Normalize a rule for persistence. Returns a new object; input is untouched. */
export function serializeRule(rule: MailRule): MailRule {
  const conditions: RuleCondition[] = [];
  const seenConditions = new Set<string>();
  for (const condition of rule.conditions) {
    const isAttachment = condition.field === "hasAttachment";
    const value = isAttachment ? "" : condition.value.trim();
    if (!isAttachment && !value) continue;
    const next: RuleCondition = {
      field: condition.field,
      op: isAttachment ? "isTrue" : condition.op,
      value,
    };
    const key = conditionKey(next);
    if (seenConditions.has(key)) continue;
    seenConditions.add(key);
    conditions.push(next);
  }

  const actions: RuleAction[] = [];
  const seenActions = new Set<string>();
  for (const action of rule.actions) {
    const next: RuleAction = {
      kind: action.kind,
      folderId: action.kind === "move" ? (action.folderId ?? null) : null,
    };
    if (next.kind === "move" && !next.folderId) continue;
    const key = actionKey(next);
    if (seenActions.has(key)) continue;
    seenActions.add(key);
    actions.push(next);
  }

  return {
    id: rule.id,
    accountId: rule.accountId,
    name: rule.name.trim(),
    enabled: rule.enabled,
    matchAll: rule.matchAll,
    conditions,
    actions,
    stopProcessing: rule.stopProcessing,
    sortOrder: Number.isFinite(rule.sortOrder) ? Math.trunc(rule.sortOrder) : 0,
  };
}

export type RuleIssue =
  | "nameRequired"
  | "conditionRequired"
  | "conditionValueRequired"
  | "actionRequired"
  | "actionFolderRequired";

/** Validation codes for the editor UI; serialization drops incomplete rows. */
export function validateRule(rule: MailRule): RuleIssue[] {
  const issues: RuleIssue[] = [];
  if (!rule.name.trim()) issues.push("nameRequired");
  if (rule.conditions.length === 0) issues.push("conditionRequired");
  if (
    rule.conditions.some(
      (condition) => condition.field !== "hasAttachment" && !condition.value.trim(),
    )
  ) {
    issues.push("conditionValueRequired");
  }
  if (rule.actions.length === 0) {
    issues.push("actionRequired");
  } else if (rule.actions.some((action) => action.kind === "move" && !action.folderId)) {
    issues.push("actionFolderRequired");
  }
  return issues;
}

/** True when a persisted rule can be run against a folder. */
export function isRuleRunnable(rule: MailRule): boolean {
  const normalized = serializeRule(rule);
  return normalized.conditions.length > 0 && normalized.actions.length > 0;
}
