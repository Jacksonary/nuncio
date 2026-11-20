import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setLanguage } from "@/lib/i18n";
import { AboutPage } from "./AboutPage";
import { GeneralPage } from "./GeneralPage";

function render(page: JSX.Element): string {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  // The version/platform/paths grid only renders once `app_info` has data,
  // which never resolves in a server render, so seed it.
  client.setQueryData(["appInfo"], {
    version: "1.2.3",
    platform: "macos",
    dataDir: "/tmp/data",
    logDir: "/tmp/logs",
  });
  return renderToStaticMarkup(
    <QueryClientProvider client={client}>{page}</QueryClientProvider>,
  );
}

/**
 * Updates and About are one top-level entry now, so the update controls moved
 * out of General instead of being duplicated in both places.
 */
describe("About Nuncio page", () => {
  it("hosts the update controls", () => {
    setLanguage("zh-CN");
    const html = render(<AboutPage onBack={() => {}} />);

    expect(html).toContain("更新");
    expect(html).toContain("自动检查更新");
    expect(html).toContain("检查更新");
    expect(html).toContain("立即检查");
    expect(html).toContain("当前版本");
  });

  it("keeps the version, paths, links and licence information", () => {
    setLanguage("zh-CN");
    const html = render(<AboutPage onBack={() => {}} />);

    expect(html).toContain("版本");
    expect(html).toContain("数据目录");
    expect(html).toContain("日志目录");
    expect(html).toContain("许可");
    expect(html).toContain("致谢");
    expect(html).toContain("开源");
  });

  it("renders without raw i18n keys", () => {
    setLanguage("zh-CN");
    expect(render(<AboutPage onBack={() => {}} />)).not.toContain("settings.");
  });
});

describe("General page", () => {
  it("no longer hosts the update controls", () => {
    setLanguage("zh-CN");
    const html = render(<GeneralPage onBack={() => {}} />);

    expect(html).not.toContain("自动检查更新");
    expect(html).not.toContain("立即检查");
    expect(html).not.toContain("settings.general.updates");
  });

  it("no longer links to About", () => {
    setLanguage("zh-CN");
    const html = render(<GeneralPage onBack={() => {}} />);

    // The About entry is top-level now; General must not offer a second route.
    expect(html).not.toContain("关于");
  });

  it("keeps appearance, startup and privacy reachable", () => {
    setLanguage("zh-CN");
    const html = render(<GeneralPage onBack={() => {}} />);

    expect(html).toContain("启动");
    expect(html).toContain("开机自启");
    expect(html).toContain("关闭到托盘");
    expect(html).toContain("隐私");
    expect(html).toContain("ISPDB");
  });
});
