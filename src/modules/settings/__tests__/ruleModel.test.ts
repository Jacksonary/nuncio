import { describe, expect, it } from "vitest";
import type { MailRule } from "@/ipc/types";
import {
  createAction,
  createCondition,
  createRule,
  isRuleRunnable,
  serializeRule,
  validateRule,
  withActionKind,
  withField,
} from "../lib/ruleModel";

function ruleWith(partial: Partial<MailRule>): MailRule {
  return createRule({ id: "rule-1", name: "Test", ...partial });
}

describe("rule serialization", () => {
  it("trims the name and drops empty conditions", () => {
    const rule = ruleWith({
      name: "  Newsletters  ",
      conditions: [
        { field: "from", op: "contains", value: "  news@  " },
        { field: "subject", op: "contains", value: "   " },
      ],
      actions: [{ kind: "markRead", folderId: null }],
    });
    const result = serializeRule(rule);
    expect(result.name).toBe("Newsletters");
    expect(result.conditions).toEqual([{ field: "from", op: "contains", value: "news@" }]);
  });

  it("normalizes hasAttachment to isTrue with an empty value", () => {
    const rule = ruleWith({
      conditions: [{ field: "hasAttachment", op: "contains", value: "ignored" }],
      actions: [{ kind: "notify", folderId: null }],
    });
    const result = serializeRule(rule);
    expect(result.conditions).toEqual([{ field: "hasAttachment", op: "isTrue", value: "" }]);
  });

  it("drops move actions without a folder and deduplicates actions", () => {
    const rule = ruleWith({
      conditions: [{ field: "from", op: "contains", value: "a" }],
      actions: [
        { kind: "move", folderId: null },
        { kind: "move", folderId: "folder-1" },
        { kind: "move", folderId: "folder-1" },
        { kind: "flag", folderId: "ignored" },
      ],
    });
    const result = serializeRule(rule);
    expect(result.actions).toEqual([
      { kind: "move", folderId: "folder-1" },
      { kind: "flag", folderId: null },
    ]);
  });

  it("does not mutate the input rule", () => {
    const condition = { field: "subject" as const, op: "contains" as const, value: "  x  " };
    const rule = ruleWith({ name: "  R  ", conditions: [condition], actions: [createAction("archive")] });
    const snapshot = JSON.parse(JSON.stringify(rule));
    serializeRule(rule);
    expect(rule).toEqual(snapshot);
    expect(condition.value).toBe("  x  ");
  });

  it("normalizes sortOrder to an integer", () => {
    const rule = ruleWith({ sortOrder: 2.7, conditions: [createCondition("from")], actions: [] });
    expect(serializeRule(rule).sortOrder).toBe(2);
    expect(serializeRule({ ...rule, sortOrder: Number.NaN }).sortOrder).toBe(0);
  });
});

describe("rule validation", () => {
  it("reports the issues needed by the editor", () => {
    const rule = ruleWith({
      name: "   ",
      conditions: [],
      actions: [],
    });
    expect(validateRule(rule)).toEqual(["nameRequired", "conditionRequired", "actionRequired"]);
  });

  it("requires condition values and move folders", () => {
    const rule = ruleWith({
      conditions: [{ field: "body", op: "contains", value: " " }],
      actions: [{ kind: "move", folderId: null }],
    });
    expect(validateRule(rule)).toEqual(["conditionValueRequired", "actionFolderRequired"]);
  });

  it("accepts a fully specified rule", () => {
    const rule = ruleWith({
      conditions: [{ field: "hasAttachment", op: "isTrue", value: "" }],
      actions: [{ kind: "move", folderId: "folder-1" }],
    });
    expect(validateRule(rule)).toEqual([]);
    expect(isRuleRunnable(rule)).toBe(true);
  });
});

describe("editor helpers", () => {
  it("resets the operator when switching fields", () => {
    const condition = { field: "from" as const, op: "matchesRegex" as const, value: "x" };
    expect(withField(condition, "hasAttachment")).toEqual({
      field: "hasAttachment",
      op: "isTrue",
      value: "",
    });
    expect(withField(condition, "subject").op).toBe("matchesRegex");
    const weird = { field: "from" as const, op: "isTrue" as const, value: "x" };
    // isTrue is not valid for "from"; the first valid operator is used.
    expect(withField(weird, "to").op).toBe("contains");
  });

  it("clears the folder when switching away from move", () => {
    const action = { kind: "move" as const, folderId: "folder-1" };
    expect(withActionKind(action, "delete")).toEqual({ kind: "delete", folderId: null });
    expect(withActionKind(action, "move")).toEqual(action);
  });

  it("creates rules with usable defaults", () => {
    const rule = createRule();
    expect(rule.enabled).toBe(true);
    expect(rule.matchAll).toBe(true);
    expect(rule.conditions).toHaveLength(1);
    expect(rule.actions).toHaveLength(1);
    expect(rule.actions[0].kind).toBe("move");
    expect(rule.id).toBeTruthy();
  });
});
