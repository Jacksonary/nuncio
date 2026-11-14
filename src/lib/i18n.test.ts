import { beforeEach, describe, expect, it } from "vitest";
import { dictionaries } from "@/i18n";
import {
  getLanguage,
  resolveLanguage,
  setLanguage,
  t,
  translateMaybeKey,
} from "@/lib/i18n";

describe("i18n", () => {
  beforeEach(() => {
    setLanguage("en-US");
  });

  it("resolves system language from navigator", () => {
    // Node has no navigator.language => defaults to en-US.
    expect(resolveLanguage("system")).toBe("en-US");
    expect(resolveLanguage("zh-CN")).toBe("zh-CN");
    expect(resolveLanguage(null)).toBe("en-US");
  });

  it("translates strings in the active language", () => {
    expect(t("common.cancel")).toBe("Cancel");
    expect(t("shell.newMail")).toBe("New mail");
    setLanguage("zh-CN");
    expect(getLanguage()).toBe("zh-CN");
    expect(t("common.cancel")).toBe("取消");
    expect(t("shell.newMail")).toBe("新邮件");
  });

  it("interpolates variables", () => {
    setLanguage("zh-CN");
    expect(t("common.minutesAgo", { count: 5 })).toBe("5 分钟前");
    setLanguage("en-US");
    expect(t("shell.importIcs.done", { count: 3 })).toBe("Imported 3 events");
  });

  it("falls back to the key for unknown strings", () => {
    expect(t("does.not.exist")).toBe("does.not.exist");
  });

  it("translates discovery note keys but passes plain text through", () => {
    setLanguage("zh-CN");
    expect(translateMaybeKey("discovery.note.authCode")).toBe(
      dictionaries["zh-CN"]["discovery.note.authCode"],
    );
    expect(translateMaybeKey("Server: imap.example.com")).toBe("Server: imap.example.com");
    expect(translateMaybeKey("discovery.note.unknownKey")).toBe("discovery.note.unknownKey");
    expect(translateMaybeKey(null)).toBe("");
    setLanguage("en-US");
    expect(translateMaybeKey("discovery.note.guessed")).toBe(
      dictionaries["en-US"]["discovery.note.guessed"],
    );
  });

  it("keeps zh-CN and en-US keys in sync", () => {
    const zh = Object.keys(dictionaries["zh-CN"]).sort();
    const en = Object.keys(dictionaries["en-US"]).sort();
    expect(zh).toEqual(en);
    expect(zh.length).toBeGreaterThan(50);
  });

  it("contains no empty translations", () => {
    for (const language of ["zh-CN", "en-US"] as const) {
      for (const [key, value] of Object.entries(dictionaries[language])) {
        expect(value, `${language} ${key}`).not.toBe("");
      }
    }
  });
});
