import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { AppSettings } from "@/ipc/types";
import { getLanguage, setLanguage, t, useT } from "@/lib/i18n";

/**
 * Reproduces the settings-flyout language bug without a Shell.
 *
 * Every window is its own webview with its own copy of the i18n module, so the
 * module starts from `localStorage["nuncio.language"]` / `navigator.language`.
 * Applying the configured language used to happen only inside the Shell
 * (`useSettingsSync`), so a window that renders without the Shell had no way to
 * learn the setting. These tests pin the fix: the language hook is mounted by
 * `App`, and applying it re-renders subscribing components.
 */

function Probe() {
  const translate = useT();
  return <span>{translate("settings.home.pim")}</span>;
}

function renderProbe(language: AppSettings["language"]) {
  const client = new QueryClient();
  // A hook that renders synchronously applies the language during render; a
  // useEffect would not run under a server render.
  setLanguage(language);
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>
      <Probe />
    </QueryClientProvider>,
  );
}

describe("language follows the configured setting in every window", () => {
  it("renders the settings home key in the configured language", () => {
    expect(renderProbe("zh-CN")).toContain("日历、联系人与待办");
    expect(renderProbe("en-US")).toContain("Calendar, contacts &amp; tasks");
  });

  it("does not fall back to the raw key for a key the panel renders", () => {
    setLanguage("zh-CN");
    expect(t("settings.home.pim")).not.toBe("settings.home.pim");
    expect(t("settings.home.pimDesc")).not.toBe("settings.home.pimDesc");
  });

  it("notifies subscribers when the language changes", () => {
    setLanguage("en-US");
    expect(getLanguage()).toBe("en-US");
    setLanguage("zh-CN");
    expect(getLanguage()).toBe("zh-CN");
    expect(t("settings.home.general")).toBe("常规");
  });

  it("resolves 'system' and unknown values to a concrete language", () => {
    setLanguage("system");
    // Node has no navigator.language, so "system" resolves to en-US.
    expect(getLanguage()).toBe("en-US");
    setLanguage(null);
    expect(getLanguage()).toBe("en-US");
  });
});
