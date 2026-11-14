import { describe, expect, it } from "vitest";
import type { Account } from "@/ipc/types";
import { hasConnectedAccount, isLocalAccount, shouldShowFirstRun } from "@/lib/accounts";

function account(provider: Account["provider"], id = provider): Account {
  return {
    id,
    provider,
    email: "",
    displayName: provider,
    color: "#5B5FC7",
    authKind: "password",
    oauthProvider: null,
    imap: null,
    smtp: null,
    pop3: null,
    caldavUrl: null,
    carddavUrl: null,
    syncWindowDays: 0,
    mailEnabled: provider !== "local",
    calendarEnabled: true,
    contactsEnabled: true,
    tasksEnabled: true,
    notificationsEnabled: true,
    includeInUnified: provider !== "local",
    sortOrder: 0,
    state: "ok",
    stateMessage: null,
    capabilities: {
      idle: false,
      condstore: false,
      qresync: false,
      move: false,
      uidplus: false,
      gmailExtensions: false,
      specialUse: false,
      serverSearch: false,
      autoSavesSent: false,
      calendar: false,
      contacts: false,
      tasks: false,
      taskImportance: false,
      taskReminders: false,
      taskSteps: false,
      taskRecurrence: false,
    },
    createdAt: "2026-01-01T00:00:00Z",
  };
}

describe("account helpers", () => {
  const local = account("local");

  it("recognises the built-in local account", () => {
    expect(isLocalAccount(local)).toBe(true);
    expect(isLocalAccount(account("imap"))).toBe(false);
  });

  it("treats a fresh install (local only) as unconfigured", () => {
    expect(hasConnectedAccount([local])).toBe(false);
    expect(hasConnectedAccount([])).toBe(false);
    expect(hasConnectedAccount(null)).toBe(false);
    expect(hasConnectedAccount(undefined)).toBe(false);
  });

  it("detects server-backed accounts next to the local one", () => {
    expect(hasConnectedAccount([local, account("gmail")])).toBe(true);
  });

  it("shows first-run onboarding until a real account exists or it is skipped", () => {
    expect(shouldShowFirstRun([local], false)).toBe(true);
    expect(shouldShowFirstRun([], false)).toBe(true);
    expect(shouldShowFirstRun([local], true)).toBe(false);
    expect(shouldShowFirstRun([local, account("outlook")], false)).toBe(false);
  });
});
