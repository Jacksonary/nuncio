import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";

import { Logo } from "@/app/Logo";

describe("Logo", () => {
  it("uses the optical master for each shipped small size", () => {
    for (const size of [16, 18, 20, 24, 32]) {
      const html = renderToStaticMarkup(<Logo size={size} />);
      expect(html).toContain(`viewBox="0 0 ${size} ${size}"`);
      expect(html).toContain(`width="${size}"`);
    }
  });

  it("falls back to the 64-unit master above 32px", () => {
    const html = renderToStaticMarkup(<Logo size={64} />);
    expect(html).toContain('viewBox="0 0 64 64"');
  });

  it("rounds an unshipped size up to the next optical master", () => {
    const html = renderToStaticMarkup(<Logo size={19} />);
    expect(html).toContain('viewBox="0 0 20 20"');
  });

  it("stays decorative without a title and labelled with one", () => {
    expect(renderToStaticMarkup(<Logo size={20} />)).toContain("aria-hidden");
    const titled = renderToStaticMarkup(<Logo size={20} title="Nuncio" />);
    expect(titled).toContain('role="img"');
    expect(titled).toContain('aria-label="Nuncio"');
  });
});
