/**
 * Reusable pieces of the mail reader: sender contact card, iTIP invitation
 * card, attachment strip and the raw-header dialog.
 */
import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  Calendar16Regular,
  CalendarClock24Regular,
  Code16Regular,
  Document16Regular,
  DocumentPdf16Regular,
  DocumentText16Regular,
  ArrowDownload16Regular,
  Image16Regular,
  Location24Regular,
  Mail16Regular,
  People16Regular,
  PersonAdd16Regular,
  Send16Regular,
  Shield24Regular,
  Warning16Regular,
} from "@fluentui/react-icons";
import {
  Avatar,
  Badge,
  Button,
  Card,
  Dialog,
  IconButton,
  Popover,
  Spinner,
  confirm,
  toast,
} from "@/components";
import {
  attachments as attachmentsApi,
  contacts as contactsApi,
  events as eventsApi,
  isIpcError,
  messages as messagesApi,
} from "@/ipc/api";
import type {
  Address,
  Attachment,
  AttendeeStatus,
  Contact,
  InvitePreview,
  MessageDetail,
} from "@/ipc/types";
import { formatAddressList, formatDate, formatDateTime, formatFileSize, formatTime, isSameDay } from "@/lib/format";
import { t, useT } from "@/lib/i18n";
import { queryKeys } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useMailCompose } from "./composeIntegration";
import { outboxStatusKey, parseOutboxStatus } from "./outbox";
import { useMailSettings } from "./queries";
import styles from "./mail.module.css";

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/* ─────────────────────────── Contact card ─────────────────────────── */

function secondaryAddress(address: Address): string {
  return address.name?.trim() || address.email;
}

function defaultContact(address: Address, accountId: string, addressBookId: string): Contact {
  const local = address.email.split("@")[0]?.replace(/[._-]+/g, " ").trim() ?? "";
  const displayName = address.name?.trim() || local || address.email;
  return {
    id: "",
    addressBookId,
    accountId,
    displayName,
    givenName: "",
    familyName: "",
    middleName: "",
    nickname: "",
    company: "",
    jobTitle: "",
    department: "",
    emails: [{ type: "work", value: address.email, isPrimary: true }],
    phones: [],
    addresses: [],
    websites: [],
    birthday: null,
    notes: "",
    photo: null,
    isFavorite: false,
    groupIds: [],
    updatedAt: null,
  };
}

export interface SenderContactProps {
  address: Address;
  /** Optional avatar size used in the header line. */
  size?: number;
}

/** Sender name with a hover/focus contact card (and add-to-contacts flow). */
export function SenderContact({ address, size = 22 }: SenderContactProps) {
  const tt = useT();
  const compose = useMailCompose();
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const timer = useRef<number | null>(null);
  const anchorRef = useRef<HTMLSpanElement | null>(null);
  const [saving, setSaving] = useState(false);

  const contactQuery = useQuery({
    queryKey: ["contact", "by-email", address.email.toLowerCase()],
    queryFn: () => contactsApi.findByEmail(address.email),
    enabled: open,
    staleTime: 60_000,
  });
  const booksQuery = useQuery({
    queryKey: queryKeys.addressBooks(),
    queryFn: contactsApi.addressBooks,
    enabled: open,
    staleTime: 120_000,
  });

  useEffect(() => {
    return () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    };
  }, []);

  const show = () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setOpen(true), 220);
  };
  const hide = () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setOpen(false), 220);
  };

  const contact = contactQuery.data ?? null;
  const book =
    (booksQuery.data ?? []).find((entry) => entry.isDefault) ?? (booksQuery.data ?? [])[0] ?? null;

  const addContact = async () => {
    if (!book) {
      toast.error(tt("mail.contact.noAddressBook"));
      return;
    }
    setSaving(true);
    try {
      const created = await contactsApi.create(defaultContact(address, book.accountId, book.id));
      queryClient.setQueryData(["contact", "by-email", address.email.toLowerCase()], created);
      void queryClient.invalidateQueries({ queryKey: ["contacts"] });
      toast.success(tt("mail.contact.added"));
      setOpen(false);
    } catch (error) {
      toast.error(tt("mail.contact.addFailed"), errorText(error));
    } finally {
      setSaving(false);
    }
  };

  return (
    <span
      ref={anchorRef}
      className={styles.senderContact}
      onMouseEnter={show}
      onMouseLeave={hide}
    >
      <button
        type="button"
        className={styles.senderName}
        onClick={() => setOpen((value) => !value)}
        aria-haspopup="dialog"
        aria-expanded={open}
      >
        {secondaryAddress(address)}
      </button>
      <Popover
        open={open}
        onClose={() => setOpen(false)}
        anchor={anchorRef.current}
        placement="bottom-start"
        minWidth={280}
        maxHeight={360}
        role="dialog"
        ariaLabel={tt("mail.contact.cardTitle")}
        className={styles.contactPopover}
      >
        <div
          onMouseEnter={() => {
            if (timer.current !== null) window.clearTimeout(timer.current);
          }}
          onMouseLeave={hide}
        >
          <div className={styles.contactHeader}>
            <Avatar
              name={contact?.displayName ?? address.name}
              email={address.email}
              src={contact?.photo}
              size={40}
            />
            <div className={styles.contactIdentity}>
              <span className={styles.contactName}>
                {contact?.displayName || address.name || address.email}
              </span>
              <span className={styles.contactEmail}>{address.email}</span>
              {contact?.company || contact?.jobTitle ? (
                <span className={styles.contactMeta}>
                  {[contact?.jobTitle, contact?.company].filter(Boolean).join(" · ")}
                </span>
              ) : null}
            </div>
          </div>
          {contactQuery.isLoading ? (
            <div className={styles.contactLoading}>
              <Spinner size="tiny" />
            </div>
          ) : contact ? (
            <div className={styles.contactBody}>
              {contact.phones.length > 0 ? (
                <div className={styles.contactLine}>
                  {contact.phones.map((phone) => (
                    <span key={`${phone.type}-${phone.value}`}>{phone.value}</span>
                  ))}
                </div>
              ) : null}
              {contact.emails.length > 0 ? (
                <div className={styles.contactLine}>
                  {contact.emails.map((email) => (
                    <button
                      key={`${email.type}-${email.value}`}
                      type="button"
                      className={styles.contactLink}
                      onClick={() => {
                        void compose.fromMailto(`mailto:${encodeURIComponent(email.value)}`);
                        setOpen(false);
                      }}
                    >
                      <Mail16Regular aria-hidden="true" /> {email.value}
                    </button>
                  ))}
                </div>
              ) : null}
              <div className={styles.contactActions}>
                <Button
                  size="small"
                  appearance="subtle"
                  icon={<People16Regular />}
                  onClick={() => {
                    const store = useShellStore.getState();
                    store.setActiveModule("people");
                    store.setSelection("people", { contactId: contact.id });
                    setOpen(false);
                  }}
                >
                  {tt("mail.contact.openInPeople")}
                </Button>
              </div>
            </div>
          ) : (
            <div className={styles.contactBody}>
              <div className={styles.contactHint}>{tt("mail.contact.notInContacts")}</div>
              <Button
                size="small"
                appearance="primary"
                icon={<PersonAdd16Regular />}
                loading={saving}
                disabled={!book}
                onClick={() => void addContact()}
              >
                {tt("mail.contact.add")}
              </Button>
            </div>
          )}
        </div>
      </Popover>
    </span>
  );
}

/* ────────────────────────── Invite card ────────────────────────── */

function attendeeLabel(status: AttendeeStatus): string {
  switch (status) {
    case "accepted":
      return t("mail.invite.accepted");
    case "declined":
      return t("mail.invite.declined");
    case "tentative":
      return t("mail.invite.tentative");
    case "delegated":
      return t("mail.invite.delegated");
    default:
      return t("mail.invite.needsAction");
  }
}

function responseAppearance(status: AttendeeStatus | null): "success" | "danger" | "warning" | "neutral" {
  if (status === "accepted") return "success";
  if (status === "declined") return "danger";
  if (status === "tentative") return "warning";
  return "neutral";
}

export interface InviteCardProps {
  messageId: string;
  invite: InvitePreview;
}

export function InviteCard({ messageId, invite }: InviteCardProps) {
  const tt = useT();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<AttendeeStatus | null>(null);
  const event = invite.event;

  const when =
    event.allDay
      ? `${formatDate(event.start)}${event.end && event.end !== event.start ? ` – ${formatDate(event.end)}` : ""}`
      : (() => {
          const start = new Date(event.start);
          const end = new Date(event.end);
          if (Number.isNaN(start.getTime())) return event.start;
          if (Number.isNaN(end.getTime())) return formatDateTime(event.start);
          if (isSameDay(start, end)) {
            return `${formatDateTime(event.start)} – ${formatTime(end)}`;
          }
          return `${formatDateTime(event.start)} – ${formatDateTime(event.end)}`;
        })();

  const respond = async (response: AttendeeStatus) => {
    setBusy(response);
    try {
      await eventsApi.respondToInvite(messageId, response, null);
      toast.success(tt("mail.invite.responded"));
      void queryClient.invalidateQueries({ queryKey: ["message"] });
      void queryClient.invalidateQueries({ queryKey: ["events"] });
    } catch (error) {
      toast.error(tt("mail.invite.respondFailed"), errorText(error));
    } finally {
      setBusy(null);
    }
  };

  return (
    <Card className={styles.inviteCard} flat>
      <div className={styles.inviteHeader}>
        <span className={styles.inviteIcon} aria-hidden="true">
          <CalendarClock24Regular />
        </span>
        <div className={styles.inviteTitles}>
          <div className={styles.inviteTitle}>{event.title || tt("mail.invite.untitled")}</div>
          <div className={styles.inviteMeta}>
            <span className={styles.inviteMetaItem}>
              <Calendar16Regular aria-hidden="true" /> {when}
            </span>
            {event.location ? (
              <span className={styles.inviteMetaItem}>
                <Location24Regular aria-hidden="true" /> {event.location}
              </span>
            ) : null}
          </div>
        </div>
        {event.status === "cancelled" ? (
          <Badge appearance="danger">{tt("mail.invite.cancelled")}</Badge>
        ) : invite.myResponse ? (
          <Badge appearance={responseAppearance(invite.myResponse)}>
            {attendeeLabel(invite.myResponse)}
          </Badge>
        ) : null}
      </div>

      {event.organizer ? (
        <div className={styles.inviteRow}>
          <span className={styles.inviteLabel}>{tt("mail.invite.organizer")}</span>
          <span>{formatAddressList([event.organizer], ", ")}</span>
        </div>
      ) : null}
      {event.attendees.length > 0 ? (
        <div className={styles.inviteRow}>
          <span className={styles.inviteLabel}>
            {tt("mail.invite.attendees", { count: event.attendees.length })}
          </span>
          <span className={styles.inviteAttendees}>
            {event.attendees.slice(0, 6).map((attendee) => (
              <span key={attendee.email} className={styles.inviteAttendee}>
                <Avatar name={attendee.name} email={attendee.email} size={18} />
                {attendee.name || attendee.email}
              </span>
            ))}
            {event.attendees.length > 6 ? (
              <span className={styles.inviteMore}>
                +{event.attendees.length - 6}
              </span>
            ) : null}
          </span>
        </div>
      ) : null}

      {invite.canRespond && event.status !== "cancelled" ? (
        <div className={styles.inviteActions}>
          <Button
            size="small"
            appearance="primary"
            loading={busy === "accepted"}
            disabled={busy !== null}
            onClick={() => void respond("accepted")}
          >
            {tt("mail.invite.accept")}
          </Button>
          <Button
            size="small"
            appearance="subtle"
            loading={busy === "tentative"}
            disabled={busy !== null}
            onClick={() => void respond("tentative")}
          >
            {tt("mail.invite.tentativeAction")}
          </Button>
          <Button
            size="small"
            appearance="subtle"
            loading={busy === "declined"}
            disabled={busy !== null}
            onClick={() => void respond("declined")}
          >
            {tt("mail.invite.decline")}
          </Button>
        </div>
      ) : null}
    </Card>
  );
}

/* ──────────────────────── Attachments ──────────────────────── */

function attachmentIcon(attachment: Attachment) {
  const mime = attachment.mimeType.toLowerCase();
  const name = attachment.filename.toLowerCase();
  if (mime.startsWith("image/") || /\.(png|jpe?g|gif|webp|bmp|svg)$/.test(name)) {
    return <Image16Regular />;
  }
  if (mime === "application/pdf" || name.endsWith(".pdf")) return <DocumentPdf16Regular />;
  if (mime.startsWith("text/") || /\.(txt|md|csv|log)$/.test(name)) return <DocumentText16Regular />;
  if (mime.startsWith("text/calendar") || name.endsWith(".ics")) return <Calendar16Regular />;
  if (/\.(json|xml|html?|js|ts|css|rs|py)$/.test(name)) return <Code16Regular />;
  return <Document16Regular />;
}

export interface AttachmentStripProps {
  messageId: string;
  attachments: Attachment[];
}

export function AttachmentStrip({ messageId, attachments }: AttachmentStripProps) {
  const tt = useT();

  const openAttachment = async (attachment: Attachment) => {
    try {
      const dangerous = await attachmentsApi.isDangerous(attachment.filename);
      if (dangerous) {
        const ok = await confirm({
          title: tt("mail.attachment.dangerTitle"),
          message: tt("mail.attachment.dangerMessage", { name: attachment.filename }),
          confirmLabel: tt("mail.attachment.openAnyway"),
          danger: true,
        });
        if (!ok) return;
      }
      await attachmentsApi.open(attachment.id);
    } catch (error) {
      toast.error(tt("mail.attachment.openFailed"), errorText(error));
    }
  };

  const saveAttachment = async (attachment: Attachment) => {
    try {
      const path = await saveDialog({ defaultPath: attachment.filename });
      if (!path) return;
      await attachmentsApi.save(attachment.id, path);
      toast.success(tt("mail.attachment.saved", { name: attachment.filename }));
    } catch (error) {
      toast.error(tt("mail.attachment.saveFailed"), errorText(error));
    }
  };

  const saveAll = async () => {
    try {
      const directory = await openDialog({
        directory: true,
        multiple: false,
        title: tt("mail.attachment.saveAllTitle"),
      });
      if (typeof directory !== "string") return;
      await attachmentsApi.saveAll(messageId, directory);
      toast.success(tt("mail.attachment.savedAll", { count: attachments.length }));
    } catch (error) {
      toast.error(tt("mail.attachment.saveFailed"), errorText(error));
    }
  };

  if (attachments.length === 0) return null;

  return (
    <div className={styles.attachmentStrip} aria-label={tt("mail.attachment.title")}>
      <div className={styles.attachmentHeader}>
        <span className={styles.attachmentTitle}>
          {tt("mail.attachment.count", { count: attachments.length })}
        </span>
        <Button
          size="small"
          appearance="transparent"
          icon={<ArrowDownload16Regular />}
          onClick={() => void saveAll()}
        >
          {tt("mail.attachment.saveAll")}
        </Button>
      </div>
      <div className={styles.attachmentList}>
        {attachments.map((attachment) => (
          <div key={attachment.id} className={styles.attachmentChip}>
            <button
              type="button"
              className={styles.attachmentMain}
              onClick={() => void openAttachment(attachment)}
              title={attachment.filename}
            >
              <span className={styles.attachmentIcon} aria-hidden="true">
                {attachmentIcon(attachment)}
              </span>
              <span className={styles.attachmentName}>{attachment.filename}</span>
              <span className={styles.attachmentSize}>{formatFileSize(attachment.size)}</span>
            </button>
            <IconButton
              icon={<ArrowDownload16Regular />}
              label={tt("mail.attachment.save")}
              size="small"
              appearance="transparent"
              onClick={() => void saveAttachment(attachment)}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

/* ────────────────────── Remote content bar ────────────────────── */

export interface RemoteContentBarProps {
  detail: MessageDetail;
  onAllow: () => void;
}

export function RemoteContentBar({ detail, onAllow }: RemoteContentBarProps) {
  const tt = useT();
  const queryClient = useQueryClient();
  const mailSettings = useMailSettings();
  const [busy, setBusy] = useState(false);
  const sender = detail.summary.from?.email ?? "";
  const domain = sender.includes("@") ? `@${sender.split("@")[1]}` : "";
  const policy = mailSettings.remoteImages;

  const trust = async (value: string) => {
    setBusy(true);
    try {
      await messagesTrust(value);
      toast.success(tt("mail.remote.trusted"));
      void queryClient.invalidateQueries({ queryKey: ["message"] });
    } catch (error) {
      toast.error(tt("mail.remote.trustFailed"), errorText(error));
    } finally {
      setBusy(false);
    }
  };

  // "always" loads everything at the backend; there is nothing to ask.
  if (policy === "always") return null;

  return (
    <div className={styles.remoteBar} role="note">
      <span className={styles.remoteIcon} aria-hidden="true">
        <Shield24Regular style={{ fontSize: 16 }} />
      </span>
      <span className={styles.remoteText}>
        {policy === "trustedOnly"
          ? tt("mail.remote.blockedTrustedOnly")
          : tt("mail.remote.blocked")}
      </span>
      <span className={styles.remoteSpacer} />
      {policy === "ask" ? (
        <Button size="small" appearance="subtle" disabled={busy} onClick={onAllow}>
          {tt("mail.remote.showOnce")}
        </Button>
      ) : null}
      {sender ? (
        <Button size="small" appearance="subtle" disabled={busy} onClick={() => void trust(sender)}>
          {tt("mail.remote.trustSender")}
        </Button>
      ) : null}
      {domain ? (
        <Button size="small" appearance="subtle" disabled={busy} onClick={() => void trust(domain)}>
          {tt("mail.remote.trustDomain", { domain })}
        </Button>
      ) : null}
    </div>
  );
}

/* ───────────────────────── Outbox status notice ───────────────────────── */

export interface OutboxNoticeProps {
  /** Projected outbox preview carrying the queue state. */
  preview: string;
}

/** Read-only banner shown above a queued/failed outbox message. */
export function OutboxNotice({ preview }: OutboxNoticeProps) {
  const tt = useT();
  const status = parseOutboxStatus(preview);
  const label = tt(outboxStatusKey(status.state));
  return (
    <div
      className={`${styles.outboxNotice}${status.state === "failed" ? ` ${styles.outboxNoticeFailed}` : ""}`}
      role="note"
    >
      <span className={styles.remoteIcon} aria-hidden="true">
        {status.state === "failed" ? (
          <Warning16Regular />
        ) : (
          <Send16Regular />
        )}
      </span>
      <span className={styles.outboxNoticeText}>
        <span className={styles.outboxNoticeLabel}>{label}</span>
        {status.error ? <span className={styles.outboxNoticeError}>{status.error}</span> : null}
        <span className={styles.outboxNoticeHint}>{tt("mail.outbox.readOnly")}</span>
      </span>
    </div>
  );
}

async function messagesTrust(value: string): Promise<void> {
  await messagesApi.trustSender(value);
}

/* ───────────────────────── Raw headers ───────────────────────── */

export interface RawHeadersDialogProps {
  open: boolean;
  onClose: () => void;
  headers: string;
}

export function RawHeadersDialog({ open, onClose, headers }: RawHeadersDialogProps) {
  const tt = useT();
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={tt("mail.rawHeaders.title")}
      width={720}
    >
      <pre className={styles.rawHeaders}>{headers}</pre>
    </Dialog>
  );
}
