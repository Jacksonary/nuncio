import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { IconButton } from "./IconButton";

const render = (props: Partial<Parameters<typeof IconButton>[0]> = {}) =>
  renderToStaticMarkup(<IconButton icon={<svg data-icon />} label="Sync" {...props} />);

describe("IconButton busy", () => {
  it("is a plain, clickable button by default", () => {
    const html = render();
    expect(html).not.toContain("aria-busy");
    expect(html).not.toContain("disabled");
  });

  it("marks an in-flight action without dimming the icon", () => {
    const html = render({ busy: true });
    expect(html).toContain('aria-busy="true"');
    // `disabled` would fade the icon to 40% and hide the motion.
    expect(html).not.toContain("disabled");
    // The icon itself is unchanged: same element, not swapped for a spinner.
    expect(html).toContain("data-icon");
  });

  it("still honours a real disabled state", () => {
    expect(render({ disabled: true })).toContain("disabled");
  });
});
