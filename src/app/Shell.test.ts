import { describe, expect, it } from "vitest";
import type { AppModule, SyncModule } from "@/ipc/types";
import { syncTargetFor } from "./Shell";

describe("syncTargetFor", () => {
  it.each<[AppModule, SyncModule]>([
    ["mail", "mail"],
    ["calendar", "calendar"],
    ["people", "contacts"],
    ["todo", "tasks"],
  ])("maps the %s module to the %s sync module", (module, expected) => {
    expect(syncTargetFor(module)).toBe(expected);
  });
});
