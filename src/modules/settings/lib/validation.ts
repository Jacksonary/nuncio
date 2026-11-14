/**
 * Pure validation + serialization for the add-account wizard.
 *
 * Kept free of React and Tauri so it can be unit tested and reused by the
 * account settings page. Error values are stable codes; the UI maps them to
 * localized strings.
 */
import { isValidEmail } from "@/lib/format";
import type {
  AuthKind,
  NewAccountInput,
  OAuthProvider,
  ProviderKind,
  Security,
  ServerConfig,
} from "@/ipc/types";

export interface ServerDraft {
  host: string;
  port: string;
  security: Security;
  /** Empty = use the account e-mail as the login name. */
  username: string;
}

export type AccountProtocol = "imap" | "pop3";

export interface AccountFormState {
  provider: ProviderKind;
  authKind: AuthKind;
  oauthProvider: OAuthProvider | null;
  email: string;
  displayName: string;
  password: string;
  oauthSessionId: string | null;
  protocol: AccountProtocol;
  imap: ServerDraft;
  smtp: ServerDraft;
  pop3: ServerDraft;
  caldavUrl: string;
  carddavUrl: string;
  syncWindowDays: number;
  mailEnabled: boolean;
  calendarEnabled: boolean;
  contactsEnabled: boolean;
  tasksEnabled: boolean;
}

export type AccountFormField =
  | "email"
  | "displayName"
  | "password"
  | "oauthSessionId"
  | "imapHost"
  | "imapPort"
  | "smtpHost"
  | "smtpPort"
  | "pop3Host"
  | "pop3Port"
  | "caldavUrl"
  | "carddavUrl"
  | "syncWindowDays";

export type AccountFormErrorCode =
  | "required"
  | "invalidEmail"
  | "passwordRequired"
  | "oauthRequired"
  | "invalidPort"
  | "invalidUrl"
  | "invalidSyncWindow";

export type AccountFormErrors = Partial<Record<AccountFormField, AccountFormErrorCode>>;

/** Download windows offered by the UI, in days (0 = everything). */
export const SYNC_WINDOW_DAYS = [0, 3, 7, 14, 30, 90] as const;

export const SECURITY_OPTIONS: Security[] = ["tls", "starttls", "none"];

export function emptyServerDraft(defaults?: Partial<ServerDraft>): ServerDraft {
  return {
    host: defaults?.host ?? "",
    port: defaults?.port ?? "",
    security: defaults?.security ?? "tls",
    username: defaults?.username ?? "",
  };
}

export function serverDraftFromConfig(
  config: ServerConfig | null,
  fallbackSecurity: Security = "tls",
): ServerDraft {
  if (!config) return emptyServerDraft({ security: fallbackSecurity });
  return {
    host: config.host,
    port: String(config.port),
    security: config.security,
    username: config.username.trim(),
  };
}

export function isValidPort(value: string): boolean {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return false;
  const port = Number(trimmed);
  return Number.isInteger(port) && port >= 1 && port <= 65535;
}

export function isValidHttpUrl(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed) return false;
  try {
    const url = new URL(trimmed);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

export function serverFromDraft(draft: ServerDraft, email: string): ServerConfig {
  return {
    host: draft.host.trim(),
    port: Number(draft.port.trim()),
    security: draft.security,
    username: draft.username.trim() || email.trim(),
  };
}

export function defaultDisplayName(email: string): string {
  const local = email.split("@")[0] ?? "";
  return local.replace(/[._-]+/g, " ").trim();
}

export function createAccountForm(partial?: Partial<AccountFormState>): AccountFormState {
  return {
    provider: "imap",
    authKind: "password",
    oauthProvider: null,
    email: "",
    displayName: "",
    password: "",
    oauthSessionId: null,
    protocol: "imap",
    imap: emptyServerDraft({ port: "993", security: "tls" }),
    smtp: emptyServerDraft({ port: "587", security: "starttls" }),
    pop3: emptyServerDraft({ port: "995", security: "tls" }),
    caldavUrl: "",
    carddavUrl: "",
    syncWindowDays: 30,
    mailEnabled: true,
    calendarEnabled: true,
    contactsEnabled: true,
    tasksEnabled: true,
    ...partial,
  };
}

export function validateAccountForm(form: AccountFormState): AccountFormErrors {
  const errors: AccountFormErrors = {};

  if (!form.email.trim()) errors.email = "required";
  else if (!isValidEmail(form.email.trim())) errors.email = "invalidEmail";

  if (!form.displayName.trim()) errors.displayName = "required";

  if (form.authKind === "password") {
    if (!form.password) errors.password = "passwordRequired";
  } else if (!form.oauthSessionId) {
    errors.oauthSessionId = "oauthRequired";
  }

  // OAuth accounts use the provider's built-in server presets; servers are
  // only validated when the user entered them manually (password auth).
  if (form.mailEnabled && form.authKind === "password") {
    if (form.protocol === "pop3") {
      if (!form.pop3.host.trim()) errors.pop3Host = "required";
      if (!isValidPort(form.pop3.port)) errors.pop3Port = "invalidPort";
    } else {
      if (!form.imap.host.trim()) errors.imapHost = "required";
      if (!isValidPort(form.imap.port)) errors.imapPort = "invalidPort";
    }
    // SMTP is required whenever mail is enabled; POP3 accounts still send via SMTP.
    if (!form.smtp.host.trim()) errors.smtpHost = "required";
    if (!isValidPort(form.smtp.port)) errors.smtpPort = "invalidPort";
  }

  if (form.caldavUrl.trim() && !isValidHttpUrl(form.caldavUrl)) {
    errors.caldavUrl = "invalidUrl";
  }
  if (form.carddavUrl.trim() && !isValidHttpUrl(form.carddavUrl)) {
    errors.carddavUrl = "invalidUrl";
  }

  if (!(SYNC_WINDOW_DAYS as readonly number[]).includes(form.syncWindowDays)) {
    errors.syncWindowDays = "invalidSyncWindow";
  }

  return errors;
}

export function buildNewAccountInput(form: AccountFormState): NewAccountInput {
  const email = form.email.trim();
  const displayName = form.displayName.trim() || defaultDisplayName(email);
  const mailEnabled = form.mailEnabled;
  const imap =
    mailEnabled && form.protocol === "imap" && form.imap.host.trim()
      ? serverFromDraft(form.imap, email)
      : null;
  const smtp = mailEnabled && form.smtp.host.trim() ? serverFromDraft(form.smtp, email) : null;
  const pop3 =
    mailEnabled && form.protocol === "pop3" && form.pop3.host.trim()
      ? serverFromDraft(form.pop3, email)
      : null;
  return {
    provider: form.provider,
    email,
    displayName,
    authKind: form.authKind,
    password: form.authKind === "password" ? form.password : null,
    oauthSessionId: form.authKind === "oauth" ? form.oauthSessionId : null,
    imap,
    smtp,
    pop3,
    caldavUrl: form.caldavUrl.trim() || null,
    carddavUrl: form.carddavUrl.trim() || null,
    syncWindowDays: form.syncWindowDays,
    mailEnabled,
    calendarEnabled: form.calendarEnabled,
    contactsEnabled: form.contactsEnabled,
    tasksEnabled: form.tasksEnabled,
  };
}

/** Turn an `IpcError` field path (e.g. "imap.port") into a form error field. */
export function errorFieldFromPath(path: string): AccountFormField | null {
  const normalized = path.replace(/^account\./, "");
  switch (normalized) {
    case "email":
    case "displayName":
    case "password":
    case "oauthSessionId":
    case "caldavUrl":
    case "carddavUrl":
    case "syncWindowDays":
      return normalized;
    case "imap.host":
      return "imapHost";
    case "imap.port":
      return "imapPort";
    case "smtp.host":
      return "smtpHost";
    case "smtp.port":
      return "smtpPort";
    case "pop3.host":
      return "pop3Host";
    case "pop3.port":
      return "pop3Port";
    default:
      return null;
  }
}
