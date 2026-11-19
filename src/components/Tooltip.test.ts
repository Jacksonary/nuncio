import { describe, expect, it } from "vitest";
import { isKeyboardFocus } from "./Tooltip";

describe("isKeyboardFocus", () => {
  it("is false when focus was moved by script after a mouse interaction", () => {
    expect(isKeyboardFocus({ matches: () => false })).toBe(false);
  });

  it("is true for a keyboard focus", () => {
    expect(isKeyboardFocus({ matches: (selector: string) => selector === ":focus-visible" })).toBe(
      true,
    );
  });

  it("keeps showing the tooltip when :focus-visible is unsupported", () => {
    expect(
      isKeyboardFocus({
        matches: () => {
          throw new SyntaxError("unsupported selector");
        },
      }),
    ).toBe(true);
  });

  it("keeps showing it for a target that cannot answer", () => {
    expect(isKeyboardFocus(null)).toBe(true);
  });
});
