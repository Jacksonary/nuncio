import { describe, expect, it } from "vitest";
import {
  buildNewAccountInput,
  createAccountForm,
  defaultDisplayName,
  emptyServerDraft,
  errorFieldFromPath,
  isValidHttpUrl,
  isValidPort,
  serverDraftFromConfig,
  serverFromDraft,
  validateAccountForm,
  type AccountFormState,
} from "../lib/validation";

function validPasswordForm(): AccountFormState {
  return createAccountForm({
    provider: "imap",
    authKind: "password",
    email: "user@example.com",
    displayName: "Test User",
    password: "app-password",
    imap: emptyServerDraft({ host: "imap.example.com", port: "993", security: "tls" }),
    smtp: emptyServerDraft({ host: "smtp.example.com", port: "587", security: "starttls" }),
  });
}

describe("account form validation", () => {
  it("accepts a complete IMAP password form", () => {
    expect(validateAccountForm(validPasswordForm())).toEqual({});
  });

  it("requires a valid email and a display name", () => {
    const form = validPasswordForm();
    form.email = "not-an-email";
    form.displayName = "   ";
    const errors = validateAccountForm(form);
    expect(errors.email).toBe("invalidEmail");
    expect(errors.displayName).toBe("required");
  });

  it("requires a password for password accounts", () => {
    const form = validPasswordForm();
    form.password = "";
    expect(validateAccountForm(form).password).toBe("passwordRequired");
  });

  it("requires a completed OAuth session instead of a password", () => {
    const form = createAccountForm({
      authKind: "oauth",
      oauthProvider: "google",
      email: "user@gmail.com",
      displayName: "User",
      mailEnabled: true,
      oauthSessionId: null,
      imap: emptyServerDraft(),
      smtp: emptyServerDraft(),
    });
    expect(validateAccountForm(form).oauthSessionId).toBe("oauthRequired");
    form.oauthSessionId = "session-1";
    // OAuth accounts may leave servers empty; the backend uses provider presets.
    expect(validateAccountForm(form)).toEqual({});
  });

  it("validates IMAP and SMTP hosts and ports", () => {
    const form = validPasswordForm();
    form.imap.host = " ";
    form.imap.port = "99999";
    form.smtp.host = "";
    form.smtp.port = "abc";
    const errors = validateAccountForm(form);
    expect(errors.imapHost).toBe("required");
    expect(errors.imapPort).toBe("invalidPort");
    expect(errors.smtpHost).toBe("required");
    expect(errors.smtpPort).toBe("invalidPort");
  });

  it("uses POP3 fields when the POP3 protocol is selected", () => {
    const form = validPasswordForm();
    form.protocol = "pop3";
    form.pop3 = emptyServerDraft({ host: "", port: "995" });
    const errors = validateAccountForm(form);
    expect(errors.pop3Host).toBe("required");
    // IMAP is not required when POP3 is selected.
    expect(errors.imapHost).toBeUndefined();
  });

  it("does not require mail servers for a CalDAV/CardDAV-only account", () => {
    const form = createAccountForm({
      provider: "imap",
      authKind: "password",
      email: "user@example.com",
      displayName: "User",
      password: "secret",
      mailEnabled: false,
      calendarEnabled: true,
      contactsEnabled: true,
      tasksEnabled: false,
      caldavUrl: "https://example.com/caldav/",
      carddavUrl: "https://example.com/carddav/",
    });
    expect(validateAccountForm(form)).toEqual({});
  });

  it("rejects non-http(s) DAV URLs", () => {
    const form = validPasswordForm();
    form.caldavUrl = "ftp://example.com/caldav";
    form.carddavUrl = "example.com/carddav";
    const errors = validateAccountForm(form);
    expect(errors.caldavUrl).toBe("invalidUrl");
    expect(errors.carddavUrl).toBe("invalidUrl");
  });

  it("rejects sync windows outside the supported list", () => {
    const form = validPasswordForm();
    form.syncWindowDays = 11;
    expect(validateAccountForm(form).syncWindowDays).toBe("invalidSyncWindow");
  });
});

describe("account input serialization", () => {
  it("maps a password form to NewAccountInput", () => {
    const form = validPasswordForm();
    form.syncWindowDays = 7;
    form.calendarEnabled = true;
    form.contactsEnabled = false;
    form.tasksEnabled = true;
    form.caldavUrl = " https://example.com/caldav/ ";
    form.carddavUrl = "";
    const input = buildNewAccountInput(form);
    expect(input.email).toBe("user@example.com");
    expect(input.authKind).toBe("password");
    expect(input.password).toBe("app-password");
    expect(input.oauthSessionId).toBeNull();
    expect(input.imap).toEqual({
      host: "imap.example.com",
      port: 993,
      security: "tls",
      username: "user@example.com",
    });
    expect(input.smtp?.port).toBe(587);
    expect(input.pop3).toBeNull();
    expect(input.caldavUrl).toBe("https://example.com/caldav/");
    expect(input.carddavUrl).toBeNull();
    expect(input.syncWindowDays).toBe(7);
    expect(input.contactsEnabled).toBe(false);
  });

  it("omits servers for OAuth when none were entered", () => {
    const form = createAccountForm({
      authKind: "oauth",
      oauthProvider: "microsoft",
      email: "user@outlook.com",
      displayName: "User",
      oauthSessionId: "session",
      imap: emptyServerDraft(),
      smtp: emptyServerDraft(),
    });
    const input = buildNewAccountInput(form);
    expect(input.password).toBeNull();
    expect(input.oauthSessionId).toBe("session");
    expect(input.imap).toBeNull();
    expect(input.smtp).toBeNull();
  });

  it("falls back to the email local part for the display name", () => {
    const form = validPasswordForm();
    form.displayName = "  ";
    expect(defaultDisplayName("jane.doe@example.com")).toBe("jane doe");
    expect(buildNewAccountInput(form).displayName).toBe("user");
  });

  it("keeps a custom username and trims it", () => {
    const draft = serverDraftFromConfig({
      host: "mail.example.com",
      port: 143,
      security: "starttls",
      username: "  login  ",
    });
    expect(draft).toEqual({
      host: "mail.example.com",
      port: "143",
      security: "starttls",
      username: "login",
    });
    expect(serverFromDraft({ ...draft, username: "" }, "me@example.com").username).toBe(
      "me@example.com",
    );
  });

  it("validates ports and URLs", () => {
    expect(isValidPort("993")).toBe(true);
    expect(isValidPort("0")).toBe(false);
    expect(isValidPort("65536")).toBe(false);
    expect(isValidPort("99a")).toBe(false);
    expect(isValidHttpUrl("https://example.com")).toBe(true);
    expect(isValidHttpUrl("javascript:alert(1)")).toBe(false);
  });

  it("maps backend field paths to form fields", () => {
    expect(errorFieldFromPath("imap.port")).toBe("imapPort");
    expect(errorFieldFromPath("account.email")).toBe("email");
    expect(errorFieldFromPath("carddavUrl")).toBe("carddavUrl");
    expect(errorFieldFromPath("unknown")).toBeNull();
  });
});
