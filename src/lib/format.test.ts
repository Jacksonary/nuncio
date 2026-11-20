import { describe, expect, it } from "vitest";
import {
  formatAddress,
  formatAddressFull,
  formatFileSize,
  formatListDate,
  formatRelativeTime,
  formatTime,
  formatTimeOfDay,
  getInitials,
  isSameDay,
  isValidEmail,
  parseAddressInput,
  parseIsoDate,
  parseLooseTime,
  parseTimeOfDay,
  toIsoDate,
} from "@/lib/format";

const NOW = new Date(2026, 10, 11, 15, 30);
const OPTIONS = { now: NOW, locale: "en-US" };

function localIso(year: number, month: number, day: number, hours = 10, minutes = 0): string {
  return new Date(year, month, day, hours, minutes).toISOString();
}

describe("formatListDate", () => {
  it("shows the time for today", () => {
    expect(formatListDate(localIso(2026, 10, 11, 9, 5), OPTIONS)).toBe("09:05");
  });

  it("shows the weekday for other days in the same week", () => {
    expect(formatListDate(localIso(2026, 10, 9), OPTIONS)).toBe("Mon");
    expect(formatListDate(localIso(2026, 10, 12), OPTIONS)).toBe("Thu");
  });

  it("shows the date for earlier this year", () => {
    expect(formatListDate(localIso(2026, 10, 2), OPTIONS)).toBe("Nov 2");
  });

  it("includes the year for other years", () => {
    expect(formatListDate(localIso(2025, 10, 11), OPTIONS)).toBe("Nov 11, 2025");
  });

  it("supports zh-CN", () => {
    expect(formatListDate(localIso(2026, 10, 2), { ...OPTIONS, locale: "zh-CN" })).toBe(
      "11月2日",
    );
  });

  it("returns the raw value for invalid input", () => {
    expect(formatListDate("not-a-date", OPTIONS)).toBe("not-a-date");
  });
});

describe("formatTime", () => {
  it("uses a 24-hour clock", () => {
    expect(formatTime(localIso(2026, 10, 11, 14, 5), { locale: "en-US" })).toBe("14:05");
  });
});

describe("formatRelativeTime", () => {
  it("labels recent timestamps", () => {
    expect(formatRelativeTime(NOW, { now: NOW, locale: "en-US" })).toBe("Just now");
    expect(
      formatRelativeTime(new Date(NOW.getTime() - 5 * 60_000), {
        now: NOW,
        locale: "en-US",
      }),
    ).toBe("5 min ago");
    expect(
      formatRelativeTime(new Date(NOW.getTime() - 5 * 60_000), {
        now: NOW,
        locale: "zh-CN",
      }),
    ).toBe("5 分钟前");
  });
});

describe("formatFileSize", () => {
  it("formats bytes, kilobytes and megabytes", () => {
    expect(formatFileSize(0)).toBe("0 B");
    expect(formatFileSize(512)).toBe("512 B");
    expect(formatFileSize(1024)).toBe("1 KB");
    expect(formatFileSize(1536)).toBe("1.5 KB");
    expect(formatFileSize(12 * 1024 * 1024)).toBe("12 MB");
  });
});

describe("addresses", () => {
  it("formats display names and full addresses", () => {
    const address = { name: "Ada Lovelace", email: "ada@example.com" };
    expect(formatAddress(address)).toBe("Ada Lovelace");
    expect(formatAddressFull(address)).toBe("Ada Lovelace <ada@example.com>");
    expect(formatAddress({ name: null, email: "x@y.z" })).toBe("x@y.z");
  });

  it("parses pasted recipient strings", () => {
    expect(parseAddressInput("Ada <ada@example.com>, bob@example.com;")).toEqual([
      { name: "Ada", email: "ada@example.com" },
      { name: null, email: "bob@example.com" },
    ]);
  });

  it("validates e-mail addresses", () => {
    expect(isValidEmail("ada@example.com")).toBe(true);
    expect(isValidEmail("not-an-email")).toBe(false);
  });
});

describe("initials", () => {
  it("handles latin names, CJK names and bare e-mails", () => {
    expect(getInitials("Ada Lovelace")).toBe("AL");
    expect(getInitials("张三")).toBe("张三");
    expect(getInitials(null, "john.doe@example.com")).toBe("JD");
    expect(getInitials("", "")).toBe("?");
  });
});

describe("date helpers", () => {
  it("round-trips ISO dates", () => {
    const date = parseIsoDate("2026-11-12");
    expect(date).not.toBeNull();
    expect(toIsoDate(date as Date)).toBe("2026-11-12");
    expect(parseIsoDate("nonsense")).toBeNull();
  });

  it("compares local days", () => {
    expect(isSameDay(new Date(2026, 10, 11, 1), new Date(2026, 10, 11, 23))).toBe(true);
    expect(isSameDay(new Date(2026, 10, 11), new Date(2026, 10, 12))).toBe(false);
  });

  it("parses and formats times of day", () => {
    expect(parseTimeOfDay("09:05")).toBe(545);
    expect(parseTimeOfDay("24:00")).toBeNull();
    expect(formatTimeOfDay(545)).toBe("09:05");
    expect(formatTimeOfDay(1440 + 30)).toBe("00:30");
  });
});

describe("parseLooseTime", () => {
  it("accepts the shorthands people type", () => {
    expect(parseLooseTime("09:05")).toBe("09:05");
    expect(parseLooseTime("9:05")).toBe("09:05");
    expect(parseLooseTime("9：05")).toBe("09:05");
    expect(parseLooseTime("905")).toBe("09:05");
    expect(parseLooseTime("0905")).toBe("09:05");
    expect(parseLooseTime("9")).toBe("09:00");
    expect(parseLooseTime("17:15")).toBe("17:15");
  });

  it("rejects text that is not a time of day", () => {
    expect(parseLooseTime("")).toBeNull();
    expect(parseLooseTime("24:00")).toBeNull();
    expect(parseLooseTime("12:60")).toBeNull();
    expect(parseLooseTime("noon")).toBeNull();
    expect(parseLooseTime("12345")).toBeNull();
  });
});
