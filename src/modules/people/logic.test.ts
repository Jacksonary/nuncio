import { describe, expect, it } from "vitest";
import type { Contact, ContactSummary } from "@/ipc/types";
import {
  addressLines,
  addressMapUrl,
  buildPeopleRows,
  contactRecipient,
  daysInMonth,
  draftFromContact,
  draftToContact,
  emptyDraft,
  formatBirthday,
  formatContactName,
  formatSummaryName,
  groupNameFor,
  isReadOnlyBook,
  isValidWebsite,
  joinBirthday,
  nameSortKey,
  parseBirthday,
  partitionWritable,
  primaryField,
  safeExternalUrl,
  sectionLetter,
  supportsPinyinCollation,
  toggleId,
  validateDraft,
  websiteHref,
} from "@/modules/people/logic";

function summary(id: string, displayName: string): ContactSummary {
  return {
    id,
    accountId: "local",
    addressBookId: "book",
    displayName,
    primaryEmail: `${id}@example.com`,
    primaryPhone: null,
    company: "",
    photo: null,
    isFavorite: false,
  };
}

function baseContact(overrides: Partial<Contact> = {}): Contact {
  return {
    id: "c1",
    addressBookId: "book",
    accountId: "local",
    displayName: "",
    givenName: "",
    familyName: "",
    middleName: "",
    nickname: "",
    company: "",
    jobTitle: "",
    department: "",
    emails: [],
    phones: [],
    addresses: [],
    websites: [],
    birthday: null,
    notes: "",
    photo: null,
    isFavorite: false,
    groupIds: [],
    updatedAt: null,
    ...overrides,
  };
}

describe("alphabetical sectioning", () => {
  it("uses the first Latin letter", () => {
    expect(sectionLetter("Alice Smith")).toBe("A");
    expect(sectionLetter("  zoe")).toBe("Z");
  });

  it("groups numbers and symbols under #", () => {
    expect(sectionLetter("123 Corp")).toBe("#");
    expect(sectionLetter("+81 friend")).toBe("#");
  });

  it("folds accented Latin letters onto their base letter", () => {
    expect(sectionLetter("Émile Zola")).toBe("E");
    expect(sectionLetter("Øyvind")).toBe("#");
  });

  it("uses pinyin initials for Chinese names when ICU supports it", () => {
    if (!supportsPinyinCollation()) {
      expect(sectionLetter("张伟")).toBe("中");
      return;
    }
    expect(sectionLetter("张伟")).toBe("Z");
    expect(sectionLetter("李雷")).toBe("L");
    expect(sectionLetter("王芳")).toBe("W");
    expect(sectionLetter("陈晨")).toBe("C");
  });

  it("uses the last name for the section when sorting by last name", () => {
    expect(sectionLetter("John Smith", "firstName")).toBe("J");
    expect(sectionLetter("John Smith", "lastName")).toBe("S");
  });

  it("builds ordered, counted rows with headers", () => {
    const rows = buildPeopleRows(
      [
        summary("1", "Zoe"),
        summary("2", "Alice"),
        summary("3", "123"),
        summary("4", "Bob"),
        summary("5", "李雷"),
      ],
      { sortBy: "firstName", displayOrder: "firstLast", locale: "zh-CN" },
    );
    const headers = rows.filter((row) => row.kind === "header");
    expect(headers.map((row) => (row.kind === "header" ? row.label : ""))).toEqual([
      "A",
      "B",
      "L",
      "Z",
      "#",
    ]);
    expect(headers.find((row) => row.kind === "header" && row.label === "Z")?.kind).toBe(
      "header",
    );
    expect(rows.filter((row) => row.kind === "contact")).toHaveLength(5);
    const contactRows = rows.filter(
      (row): row is Extract<typeof row, { kind: "contact" }> => row.kind === "contact",
    );
    expect(contactRows.map((row) => row.index)).toEqual([0, 1, 2, 3, 4]);
  });

  it("sorts by the last name once the full contact set is loaded", () => {
    const rows = buildPeopleRows(
      [
        summary("1", "John Smith"),
        summary("2", "Zoe Adams"),
        summary("3", "Alice Brown"),
      ],
      { sortBy: "lastName", displayOrder: "firstLast", locale: "en-US" },
    );
    const names = rows
      .filter(
        (row): row is Extract<typeof row, { kind: "contact" }> =>
          row.kind === "contact",
      )
      .map((row) => row.contact.displayName);
    expect(names).toEqual(["Zoe Adams", "Alice Brown", "John Smith"]);
    const headers = rows
      .filter((row) => row.kind === "header")
      .map((row) => (row.kind === "header" ? row.label : ""));
    expect(headers).toEqual(["A", "B", "S"]);
  });
});

describe("name formatting", () => {
  it("composes names in both display orders", () => {
    const contact = baseContact({
      givenName: "Ada",
      familyName: "Lovelace",
      displayName: "",
    });
    expect(formatContactName(contact, "firstLast")).toBe("Ada Lovelace");
    expect(formatContactName(contact, "lastFirst")).toBe("Lovelace Ada");
  });

  it("honors a custom display name but ignores an auto-composed one", () => {
    const custom = baseContact({
      givenName: "Ada",
      familyName: "Lovelace",
      displayName: "Ada L.",
    });
    expect(formatContactName(custom, "firstLast")).toBe("Ada L.");
    const auto = baseContact({
      givenName: "Ada",
      familyName: "Lovelace",
      displayName: "Lovelace Ada",
    });
    expect(formatContactName(auto, "firstLast")).toBe("Ada Lovelace");
  });

  it("joins CJK name parts without a space", () => {
    const contact = baseContact({ givenName: "伟", familyName: "张" });
    expect(formatContactName(contact, "firstLast")).toBe("张伟");
  });

  it("falls back to display name, company, then e-mail", () => {
    expect(formatContactName(baseContact({ displayName: "Team" }))).toBe("Team");
    expect(formatContactName(baseContact({ company: "Contoso" }))).toBe("Contoso");
    expect(
      formatContactName(
        baseContact({
          emails: [{ type: "work", value: "a@example.com", isPrimary: true }],
        }),
      ),
    ).toBe("a@example.com");
  });

  it("reorders summary names for last-first display", () => {
    expect(formatSummaryName("John Smith", "firstLast")).toBe("John Smith");
    expect(formatSummaryName("John Smith", "lastFirst")).toBe("Smith John");
    expect(formatSummaryName("张伟", "lastFirst")).toBe("张伟");
  });

  it("builds last-name sort keys", () => {
    expect(nameSortKey("John Smith", "lastName")).toBe("Smith John");
    expect(nameSortKey("John Smith", "firstName")).toBe("John Smith");
    expect(nameSortKey("张伟", "lastName")).toBe("张伟");
  });
});

describe("birthdays", () => {
  it("parses full and year-less birthdays", () => {
    expect(parseBirthday("1990-05-03")).toEqual({ year: 1990, month: 5, day: 3 });
    expect(parseBirthday("--05-03")).toEqual({ year: null, month: 5, day: 3 });
    expect(parseBirthday("1990-13-03")).toBeNull();
    expect(parseBirthday("1990-02-30")).toBeNull();
    expect(parseBirthday("")).toBeNull();
  });

  it("knows month lengths and leap years", () => {
    expect(daysInMonth(2024, 2)).toBe(29);
    expect(daysInMonth(2023, 2)).toBe(28);
    expect(daysInMonth(null, 2)).toBe(29);
  });

  it("joins validated birthday parts", () => {
    expect(joinBirthday("1990", "5", "3")).toBe("1990-05-03");
    expect(joinBirthday("", "5", "3")).toBe("--05-03");
    expect(joinBirthday("1990", "2", "30")).toBeNull();
    expect(joinBirthday("90", "5", "3")).toBeNull();
    expect(joinBirthday("1990", "", "3")).toBeNull();
  });

  it("formats birthdays with and without a year", () => {
    expect(formatBirthday("1990-05-03", "en-US")).toContain("1990");
    expect(formatBirthday("--05-03", "en-US")).not.toContain("1990");
    expect(formatBirthday(null, "en-US")).toBe("");
  });
});

describe("contact draft validation", () => {
  it("requires a name or an e-mail", () => {
    const draft = emptyDraft({ id: "book", accountId: "local" });
    expect(validateDraft(draft).valid).toBe(false);
    expect(validateDraft({ ...draft, givenName: "Ada" }).valid).toBe(true);
    expect(
      validateDraft({
        ...draft,
        emails: [{ type: "home", value: "ada@example.com", isPrimary: true }],
      }).valid,
    ).toBe(true);
  });

  it("reports invalid e-mail rows only when they have text", () => {
    const draft = emptyDraft({ id: "book", accountId: "local" });
    const result = validateDraft({
      ...draft,
      givenName: "Ada",
      emails: [
        { type: "home", value: "not-an-email", isPrimary: true },
        { type: "work", value: "", isPrimary: false },
      ],
    });
    expect(result.invalidEmailIndexes).toEqual([0]);
    expect(result.valid).toBe(false);
  });

  it("validates websites and birthdays", () => {
    const draft = {
      ...emptyDraft({ id: "book", accountId: "local" }),
      givenName: "Ada",
      websites: ["ok.example.com", "not a url"],
      birthday: "1990-02-30",
    };
    const result = validateDraft(draft);
    expect(result.invalidWebsiteIndexes).toEqual([1]);
    expect(result.birthdayInvalid).toBe(true);
    expect(isValidWebsite("https://example.com/path?q=1")).toBe(true);
    expect(isValidWebsite("")).toBe(true);
  });
});

describe("draft conversion", () => {
  it("trims values, drops empty rows and assigns a primary e-mail", () => {
    const draft = {
      ...emptyDraft({ id: "book", accountId: "local" }),
      givenName: " Ada ",
      familyName: " Lovelace ",
      emails: [
        { type: "home" as const, value: "  ", isPrimary: true },
        { type: "work" as const, value: " ada@example.com ", isPrimary: false },
      ],
      phones: [{ type: "mobile" as const, value: " 123 ", isPrimary: false }],
      websites: [" example.com ", ""],
      addresses: [
        { type: "home" as const, street: " ", city: "", region: "", postalCode: "", country: "" },
        {
          type: "work" as const,
          street: "1 Main St",
          city: "Springfield",
          region: "",
          postalCode: "",
          country: "US",
        },
      ],
    };
    const contact = draftToContact(draft);
    expect(contact.displayName).toBe("Ada Lovelace");
    expect(contact.emails).toEqual([
      { type: "work", value: "ada@example.com", isPrimary: true },
    ]);
    expect(contact.phones).toEqual([{ type: "mobile", value: "123", isPrimary: true }]);
    expect(contact.websites).toEqual(["example.com"]);
    expect(contact.addresses).toHaveLength(1);
    expect(contact.addresses[0]?.street).toBe("1 Main St");
  });

  it("falls back to the primary e-mail when no name is given", () => {
    const draft = {
      ...emptyDraft({ id: "book", accountId: "local" }),
      emails: [{ type: "home" as const, value: "only@example.com", isPrimary: true }],
    };
    expect(draftToContact(draft).displayName).toBe("only@example.com");
  });

  it("uses a nickname when that is the only name", () => {
    const draft = {
      ...emptyDraft({ id: "book", accountId: "local" }),
      nickname: "Ace",
    };
    const contact = draftToContact(draft);
    expect(contact.displayName).toBe("Ace");
    expect(validateDraft(draft).valid).toBe(true);
  });

  it("round-trips a contact through a draft", () => {
    const contact = baseContact({
      id: "c9",
      displayName: "Ada Lovelace",
      givenName: "Ada",
      familyName: "Lovelace",
      emails: [{ type: "work", value: "ada@example.com", isPrimary: true }],
      groupIds: ["g1"],
      isFavorite: true,
      updatedAt: "2026-01-01T00:00:00Z",
    });
    const draft = draftFromContact(contact);
    // An auto-composed name is reset so edits to the name fields win.
    expect(draft.displayName).toBe("");
    expect(draftToContact(draft)).toEqual(contact);
  });

  it("keeps an explicit display name in the draft", () => {
    const contact = baseContact({
      givenName: "Ada",
      familyName: "Lovelace",
      displayName: "The Countess",
    });
    expect(draftFromContact(contact).displayName).toBe("The Countess");
  });
});

describe("detail helpers", () => {
  it("formats addresses and map links", () => {
    const address = {
      type: "work" as const,
      street: "1 Main St",
      city: "Springfield",
      region: "IL",
      postalCode: "62701",
      country: "US",
    };
    expect(addressLines(address)).toEqual(["1 Main St", "Springfield, IL", "62701 US"]);
    expect(addressMapUrl(address)).toContain("bing.com/maps");
    expect(addressMapUrl({ ...address, street: "", city: "", region: "", postalCode: "", country: "" })).toBeNull();
  });

  it("picks primary fields and builds recipients", () => {
    const fields = [
      { type: "home" as const, value: "", isPrimary: true },
      { type: "work" as const, value: "work@example.com", isPrimary: false },
      { type: "other" as const, value: "other@example.com", isPrimary: false },
    ];
    expect(primaryField(fields)?.value).toBe("work@example.com");
    const recipient = contactRecipient(
      baseContact({ displayName: "Ada", emails: fields }),
      "firstLast",
    );
    expect(recipient).toEqual({ name: "Ada", email: "work@example.com" });
    expect(contactRecipient(baseContact())).toBeNull();
  });

  it("resolves group names and toggles ids", () => {
    expect(groupNameFor(["g1", "missing"], [{ id: "g1", name: "Team" }])).toEqual(["Team"]);
    expect(toggleId(["a"], "b")).toEqual(["a", "b"]);
    expect(toggleId(["a", "b"], "a")).toEqual(["b"]);
  });
});

describe("external links", () => {
  it("normalizes bare domains to https", () => {
    expect(websiteHref("example.com")).toBe("https://example.com");
    expect(websiteHref("  www.example.com/path?q=1  ")).toBe(
      "https://www.example.com/path?q=1",
    );
    expect(websiteHref("//example.com/team")).toBe("https://example.com/team");
    expect(websiteHref("https://example.com/path?a=b")).toBe("https://example.com/path?a=b");
    expect(websiteHref("HTTP://Example.com")).toBe("http://Example.com");
    expect(websiteHref("example.com:8080/wiki")).toBe("https://example.com:8080/wiki");
    expect(websiteHref("localhost:3000")).toBe("https://localhost:3000");
  });

  it("rejects non-http(s) schemes from imported vCards", () => {
    for (const value of [
      "file:///etc/passwd",
      "file:///C:/Windows/System32/calc.exe",
      "file:123",
      "javascript:alert(1)",
      "vbscript:msgbox(1)",
      "data:text/html;base64,PHNjcmlwdD4=",
      "ms-settings:privacy",
      "custom-app://open?x=1",
      "tel:+15550100",
      "mailto:ada@example.com",
      "not a url",
      "",
    ]) {
      expect(websiteHref(value), value).toBe("");
    }
  });

  it("allows only http(s) and mailto in the OS opener", () => {
    expect(safeExternalUrl("https://example.com")).toBe("https://example.com");
    expect(safeExternalUrl("example.com")).toBe("https://example.com");
    expect(safeExternalUrl("mailto:ada@example.com", { allowMailto: true })).toBe(
      "mailto:ada@example.com",
    );
    expect(safeExternalUrl("mailto:ada@example.com")).toBeNull();
    expect(safeExternalUrl("file:///etc/passwd")).toBeNull();
    expect(safeExternalUrl("javascript:alert(1)")).toBeNull();
    expect(safeExternalUrl("custom-app://open")).toBeNull();
    expect(safeExternalUrl("  ")).toBeNull();
    expect(safeExternalUrl(null)).toBeNull();
  });
});

describe("read-only address books", () => {
  const books = [
    { id: "writable", isReadOnly: false },
    { id: "readonly", isReadOnly: true },
  ];

  it("detects read-only address books by id", () => {
    expect(isReadOnlyBook(books, "readonly")).toBe(true);
    expect(isReadOnlyBook(books, "writable")).toBe(false);
    expect(isReadOnlyBook(books, "missing")).toBe(false);
    expect(isReadOnlyBook(books, null)).toBe(false);
  });

  it("splits a bulk selection into writable and read-only contacts", () => {
    const items = [
      { id: "a", addressBookId: "writable" },
      { id: "b", addressBookId: "readonly" },
      { id: "c", addressBookId: "writable" },
    ];
    expect(partitionWritable(items, books, ["a", "b", "c"])).toEqual({
      writable: ["a", "c"],
      readOnly: ["b"],
    });
    expect(partitionWritable(items, books, [])).toEqual({ writable: [], readOnly: [] });
  });

  it("treats ids that are no longer listed as non-writable", () => {
    const items = [{ id: "a", addressBookId: "writable" }];
    expect(partitionWritable(items, books, ["a", "gone"])).toEqual({
      writable: ["a"],
      readOnly: ["gone"],
    });
  });
});
