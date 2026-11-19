import { describe, expect, it } from "vitest";
import { isDraftTextField, type DraftFieldTarget } from "./SidePanel";

function field(overrides: DraftFieldTarget): DraftFieldTarget {
  return { tagName: "INPUT", type: "text", ...overrides };
}

describe("isDraftTextField", () => {
  it.each(["text", "search", "email", "url", "tel", "password", ""])(
    "treats input[type=%s] as a draft field",
    (type) => {
      expect(isDraftTextField(field({ type }))).toBe(true);
    },
  );

  it("treats a textarea and editable content as draft fields", () => {
    expect(isDraftTextField(field({ tagName: "TEXTAREA", type: undefined }))).toBe(true);
    expect(isDraftTextField(field({ tagName: "DIV", isContentEditable: true }))).toBe(true);
  });

  it("ignores controls that persist on change", () => {
    for (const type of ["number", "range", "checkbox", "radio", "file", "date", "color"]) {
      expect(isDraftTextField(field({ type }))).toBe(false);
    }
    expect(isDraftTextField(field({ tagName: "SELECT", type: undefined }))).toBe(false);
  });

  it("handles a missing target", () => {
    expect(isDraftTextField(null)).toBe(false);
    expect(isDraftTextField(undefined)).toBe(false);
  });
});
