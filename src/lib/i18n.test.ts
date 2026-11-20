import { beforeEach, describe, expect, it } from "vitest";
import { dictionaries } from "@/i18n";
import {
  getLanguage,
  resolveLanguage,
  setLanguage,
  t,
  translateMaybeKey,
} from "@/lib/i18n";

/**
 * Every `src/i18n/<module>.ts` dictionary file with its source text, read
 * through Vite so no Node built-ins (and no `@types/node`) are needed.
 */
const DICTIONARY_SOURCES = import.meta.glob("../i18n/*.ts", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string> | undefined;

function moduleSources(): Array<[string, string]> {
  return Object.entries(DICTIONARY_SOURCES ?? {})
    .filter(([path]) => !path.endsWith("/index.ts"))
    .map(([path, source]): [string, string] => [path.split("/").pop() ?? path, source])
    .sort(([a], [b]) => a.localeCompare(b));
}

/**
 * Source text of one language block, from its `"zh-CN": {` line to the matching
 * closing `},`. Dictionary files are a flat object of string literals, so the
 * first `},` at the same indent ends the block.
 */
function languageBlock(source: string, language: "zh-CN" | "en-US"): string | null {
  const start = source.search(new RegExp(`^\\s*"${language}":\\s*\\{`, "m"));
  if (start < 0) return null;
  const open = source.indexOf("{", start);
  const end = source.indexOf("\n  },", open);
  return source.slice(open, end < 0 ? undefined : end);
}

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

  /**
   * A duplicated key inside one language object is legal JavaScript: the later
   * entry silently wins, so the earlier translation disappears without any
   * error. The merge that produced this bug left entries shadowed that way, so
   * the dictionaries are scanned as source text rather than as objects (the
   * object would already have collapsed the duplicate).
   */
  it("declares every key at most once per language block", () => {
    const duplicated: string[] = [];
    for (const [file, source] of moduleSources()) {
      for (const language of ["zh-CN", "en-US"] as const) {
        const block = languageBlock(source, language);
        if (block === null) continue;
        const seen = new Map<string, number>();
        for (const match of block.matchAll(/^\s*"([^"]+)"\s*:/gm)) {
          const key = match[1];
          const line = block.slice(0, match.index).split("\n").length;
          const previous = seen.get(key);
          if (previous !== undefined) {
            duplicated.push(`${file} ${language} ${key} (lines ${previous} and ${line})`);
          } else {
            seen.set(key, line);
          }
        }
      }
    }
    expect(duplicated).toEqual([]);
  });

  it("contains no empty translations", () => {
    for (const language of ["zh-CN", "en-US"] as const) {
      for (const [key, value] of Object.entries(dictionaries[language])) {
        expect(value, `${language} ${key}`).not.toBe("");
      }
    }
  });
});
