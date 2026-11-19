import { describe, expect, it } from "vitest";
import { payloadCarriesDraft, pruneDetachedFields } from "./SettingsPanel";

describe("payloadCarriesDraft", () => {
  it("matches the typed value inside a nested settings patch", () => {
    const variables = { advanced: { googleClientId: "abc.apps.googleusercontent.com" } };
    expect(payloadCarriesDraft(variables, " abc.apps.googleusercontent.com ")).toBe(true);
  });

  it("matches values inside arrays and account server patches", () => {
    const variables = {
      id: "acct-1",
      patch: { imap: { host: "imap.example.com", port: 993 } },
    };
    expect(payloadCarriesDraft(variables, "imap.example.com")).toBe(true);
  });

  it("does not treat an unrelated write as saving the draft", () => {
    const variables = { id: "acct-1", patch: { mailEnabled: false } };
    expect(payloadCarriesDraft(variables, "imap.example.com")).toBe(false);
  });

  it("treats an empty draft as nothing to restore", () => {
    expect(payloadCarriesDraft({ patch: {} }, "")).toBe(true);
  });

  it("survives payloads without objects", () => {
    expect(payloadCarriesDraft(undefined, "typed")).toBe(false);
    expect(payloadCarriesDraft("typed", "typed")).toBe(true);
  });
});

describe("pruneDetachedFields", () => {
  it("drops fields that left the document and reports it", () => {
    const attached = { isConnected: true };
    const gone = { isConnected: false };
    const fields = new Set([attached, gone]);
    expect(pruneDetachedFields(fields)).toBe(true);
    expect([...fields]).toEqual([attached]);
  });

  it("leaves attached fields alone and reports no change", () => {
    const fields = new Set([{ isConnected: true }]);
    expect(pruneDetachedFields(fields)).toBe(false);
    expect(fields.size).toBe(1);
  });

  it("empties the set when every field is gone, so the guard can clear", () => {
    const fields = new Set([{ isConnected: false }, { isConnected: false }]);
    expect(pruneDetachedFields(fields)).toBe(true);
    expect(fields.size).toBe(0);
  });
});
