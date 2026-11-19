/**
 * Dictionary aggregation. Every module owns `src/i18n/<module>.ts` and exports
 * `{ "zh-CN": {...}, "en-US": {...} }`; keys must be prefixed with the module
 * name. The shell merges them in this file.
 */
import { common } from "./common";
import { shell } from "./shell";
import { mail } from "./mail";
import { calendar } from "./calendar";
import { people } from "./people";
import { todo } from "./todo";
import { settings } from "./settings";
import { sync } from "./sync";

export type UiLanguage = "zh-CN" | "en-US";

export type Dictionary = Record<string, string>;

export type DictionarySet = {
  "zh-CN": Dictionary;
  "en-US": Dictionary;
};

const all: DictionarySet[] = [
  common,
  shell,
  mail,
  calendar,
  people,
  todo,
  settings,
  sync,
] as DictionarySet[];

export const dictionaries: DictionarySet = {
  "zh-CN": Object.assign({}, ...all.map((d) => d["zh-CN"])) as Dictionary,
  "en-US": Object.assign({}, ...all.map((d) => d["en-US"])) as Dictionary,
};
