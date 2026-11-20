import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { dictionaries } from "@/i18n";
import { setLanguage } from "@/lib/i18n";
import { HOME_ITEMS, SettingsPanel } from "./SettingsPanel";

function renderHome(): string {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <SettingsPanel accountId={null} onClose={() => {}} />
    </QueryClientProvider>,
  );
}

/** Visible text of every home row title. */
function homeTitles(html: string): string[] {
  return Array.from(html.matchAll(/class="_homeRowTitle_[^"]*">([^<]*)</g), (m) => m[1]);
}

/**
 * Regression tests for the settings home list showing raw `settings.home.*`
 * keys: the panel's `HOME_ITEMS` referenced keys that its dictionary did not
 * (yet) contain, and `t()` falls back to the key itself, so the row rendered
 * "settings.home.pim" verbatim.
 */
describe("SettingsPanel home list", () => {
  it("renders every home row in Simplified Chinese without raw keys", () => {
    setLanguage("zh-CN");
    const html = renderHome();

    expect(html).not.toContain("settings.home.");
    expect(homeTitles(html)).toContain("日历、联系人与待办");
    expect(homeTitles(html)).toContain("关于 Nuncio");
  });

  it("renders every home row in English without raw keys", () => {
    setLanguage("en-US");
    const html = renderHome();

    expect(html).not.toContain("settings.home.");
    expect(homeTitles(html)).toContain("Calendar, contacts &amp; tasks");
    expect(homeTitles(html)).toContain("About Nuncio");
  });

  it("follows a language change made after mount", () => {
    setLanguage("en-US");
    const first = renderHome();
    expect(homeTitles(first)).toContain("Accounts");

    // Same module state, new language: a re-render must use the new dictionary.
    setLanguage("zh-CN");
    const second = renderHome();
    expect(homeTitles(second)).toContain("账户");
    expect(homeTitles(second)).not.toContain("Accounts");
  });

  it("has every home row key in both dictionaries", () => {
    const zh = Object.keys(dictionaries["zh-CN"]);
    const en = Object.keys(dictionaries["en-US"]);
    const missing = HOME_ITEMS.flatMap((item) => [item.titleKey, item.descKey]).filter(
      (key) => !zh.includes(key) || !en.includes(key),
    );
    expect(missing).toEqual([]);
  });

  it("lists About Nuncio last, after Data", () => {
    setLanguage("zh-CN");
    const titles = homeTitles(renderHome());
    expect(titles.indexOf("数据")).toBeLessThan(titles.indexOf("关于 Nuncio"));
    expect(titles[titles.length - 1]).toBe("关于 Nuncio");
  });
});
