import { describe, expect, it } from "vitest";
import { composeI18n } from "./i18n";

describe("compose dictionary", () => {
  it("has the same keys in both languages", () => {
    expect(Object.keys(composeI18n["zh-CN"]).sort()).toEqual(
      Object.keys(composeI18n["en-US"]).sort(),
    );
  });

  it("prefixes every key with compose. and has no empty strings", () => {
    for (const language of ["zh-CN", "en-US"] as const) {
      for (const [key, value] of Object.entries(composeI18n[language])) {
        expect(key.startsWith("compose.")).toBe(true);
        expect(value.length).toBeGreaterThan(0);
      }
    }
  });
});
