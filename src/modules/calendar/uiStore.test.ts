import { describe, expect, it } from "vitest";
import { mergeSeedDraft, type NewEventSeed } from "./uiStore";

const start = new Date("2026-11-18T09:00:00.000Z");
const end = new Date("2026-11-18T09:30:00.000Z");

describe("mergeSeedDraft", () => {
  it("carries the typed title and chosen calendar into the full editor", () => {
    const seed: NewEventSeed = { start, end, allDay: false, calendarId: "original" };
    expect(mergeSeedDraft(seed, { title: "Lunch", calendarId: "work" })).toEqual({
      start,
      end,
      allDay: false,
      calendarId: "work",
      title: "Lunch",
    });
  });

  it("keeps the original calendar when the quick form had none", () => {
    const seed: NewEventSeed = { start, end, allDay: true, calendarId: "original" };
    expect(mergeSeedDraft(seed, { title: "", calendarId: null }).calendarId).toBe("original");
  });

  it("does not mutate the seed", () => {
    const seed: NewEventSeed = { start, end, allDay: false, calendarId: "original" };
    mergeSeedDraft(seed, { title: "Lunch", calendarId: "work" });
    expect(seed.title).toBeUndefined();
    expect(seed.calendarId).toBe("original");
  });
});
