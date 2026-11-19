import { describe, expect, it } from "vitest";
import { isInnermostSurfaceAt, isInsideSurface, type OpenSurface } from "./Popover";

const target = { name: "target" };

function containing(hit: unknown) {
  return { contains: (candidate: unknown) => candidate === hit };
}

const never = { contains: () => false };

describe("popover containment", () => {
  it("treats the surface itself as inside", () => {
    expect(isInsideSurface(target, containing(target), [never])).toBe(true);
  });

  it("treats a descendant portal as inside", () => {
    expect(isInsideSurface(target, never, [containing(target)])).toBe(true);
  });

  it("is outside when neither the surface nor a descendant contains it", () => {
    expect(isInsideSurface(target, containing({}), [never])).toBe(false);
    expect(isInsideSurface(target, null, [])).toBe(false);
  });
});

describe("escape ownership", () => {
  const outer = Symbol("outer");
  const inner = Symbol("inner");

  it("gives escape to the deepest surface containing the target", () => {
    const surfaces: OpenSurface[] = [
      { token: outer, depth: 0, element: never },
      { token: inner, depth: 1, element: containing(target) },
    ];
    expect(isInnermostSurfaceAt(surfaces, inner, target)).toBe(true);
    expect(isInnermostSurfaceAt(surfaces, outer, target)).toBe(false);
  });

  it("keeps escape with the ancestor when focus is outside the nested portal", () => {
    const surfaces: OpenSurface[] = [
      { token: outer, depth: 0, element: containing(target) },
      { token: inner, depth: 1, element: never },
    ];
    expect(isInnermostSurfaceAt(surfaces, outer, target)).toBe(true);
  });

  it("ignores surfaces that are not open", () => {
    expect(isInnermostSurfaceAt([], outer, target)).toBe(false);
  });
});
