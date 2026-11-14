/**
 * Right pane: full contact details with a large avatar, primary actions
 * (compose / meeting), copy helpers, addresses with a map link, favorites,
 * edit and delete.
 */
import { useCallback } from "react";
import {
  Building16Regular,
  CalendarLtr16Regular,
  Call16Regular,
  Copy16Regular,
  Delete16Regular,
  Edit16Regular,
  Globe16Regular,
  Info16Regular,
  Location16Regular,
  Mail16Regular,
  Map16Regular,
  Note16Regular,
  Open16Regular,
  People20Regular,
  Star24Filled,
  Star24Regular,
  Warning16Regular,
} from "@fluentui/react-icons";
import {
  Avatar,
  Badge,
  Button,
  EmptyState,
  IconButton,
  Spinner,
  toast,
} from "@/components";
import type { ContactField, PostalAddress } from "@/ipc/types";
import { formatRelativeTime } from "@/lib/format";
import { getLocale, useT } from "@/lib/i18n";
import { useAppSettings } from "@/lib/useSettings";
import { useShellStore } from "@/lib/store";
import {
  addressLines,
  addressMapUrl,
  formatBirthday,
  formatContactName,
  groupNameFor,
  primaryEmail,
  websiteHref,
} from "./logic";
import {
  composeToContact,
  composeToEmail,
  deleteContactWithConfirm,
  peopleErrorText,
  scheduleMeetingWithContact,
  setContactFavorite,
} from "./actions";
import { useAddressBooksQuery, useAllGroupsQuery, useContactQuery } from "./queries";
import { copyText, openExternalUrl } from "./links";
import { usePeopleUiStore } from "./uiStore";
import styles from "./People.module.css";

function typeLabel(
  t: (key: string, vars?: Record<string, string | number>) => string,
  field: ContactField | PostalAddress,
): string {
  const key = `people.editor.fieldType.${field.type}`;
  const translated = t(key);
  return translated === key ? field.type : translated;
}

export function ContactDetail() {
  const t = useT();
  const settings = useAppSettings();
  const contactId = useShellStore((state) => state.selection.people.contactId);
  const openEdit = usePeopleUiStore((state) => state.openEdit);
  const query = useContactQuery(contactId);
  const booksQuery = useAddressBooksQuery();
  const books = booksQuery.data ?? [];
  const groups = useAllGroupsQuery().data ?? [];
  const contact = query.data;

  const copy = useCallback(
    async (value: string) => {
      const copied = await copyText(value);
      if (copied) toast.success(t("people.detail.copied"), value);
      else toast.error(t("people.detail.copyFailed"), value);
    },
    [t],
  );

  if (!contactId) {
    return (
      <div className={styles.detailColumn}>
        <div className={styles.detailEmpty}>
          <EmptyState
            icon={<People20Regular />}
            title={t("people.detail.emptyTitle")}
            description={t("people.detail.emptyDescription")}
          />
        </div>
      </div>
    );
  }

  if (query.isLoading) {
    return (
      <div className={styles.detailColumn}>
        <div className={styles.detailLoading}>
          <Spinner label={t("common.loading")} />
        </div>
      </div>
    );
  }

  if (query.isError || !contact) {
    return (
      <div className={styles.detailColumn}>
        <div className={styles.detailError}>
          <EmptyState
            icon={<Warning16Regular />}
            title={t("people.detail.loadFailed")}
            description={query.error ? peopleErrorText(query.error) : undefined}
            actions={
              <Button size="small" onClick={() => void query.refetch()}>
                {t("common.retry")}
              </Button>
            }
          />
        </div>
      </div>
    );
  }

  const displayOrder = settings.contacts.displayOrder;
  const displayName = formatContactName(contact, displayOrder) || t("people.detail.noName");
  const subtitle = [contact.company, contact.jobTitle].filter(Boolean).join(" · ");
  const emails = contact.emails.filter((field) => field.value.trim());
  const phones = contact.phones.filter((field) => field.value.trim());
  const addresses = contact.addresses.filter((address) => addressLines(address).length > 0);
  const websites = contact.websites.filter((value) => value.trim());
  const groupNames = groupNameFor(contact.groupIds, groups);
  const book = books.find((candidate) => candidate.id === contact.addressBookId);
  const readOnly = book?.isReadOnly === true;
  // Until the address books have loaded the contact's book is unknown; keep
  // modifying actions disabled rather than risk touching a read-only book.
  const actionsLocked = book ? readOnly : booksQuery.isPending;
  const email = primaryEmail(contact);

  return (
    <div className={styles.detailColumn}>
      <div className={styles.detailHeader}>
        <Avatar
          className={styles.detailAvatar}
          name={displayName}
          email={email}
          src={contact.photo}
          size={88}
          ariaLabel={contact.photo ? t("people.editor.photoAlt", { name: displayName }) : undefined}
        />
        <div className={styles.detailIdentity}>
          <div className={styles.detailNameRow}>
            <h2 className={styles.detailName}>{displayName}</h2>
            {readOnly ? (
              <Badge
                className={styles.readOnlyBadge}
                appearance="neutral"
                title={t("people.detail.readOnlyHint")}
              >
                {t("people.detail.readOnlyBook")}
              </Badge>
            ) : null}
            <IconButton
              className={styles.detailStar}
              icon={contact.isFavorite ? <Star24Filled /> : <Star24Regular />}
              label={
                contact.isFavorite
                  ? t("people.detail.unfavorite")
                  : t("people.detail.favorite")
              }
              selected={contact.isFavorite}
              disabled={actionsLocked}
              tooltip={readOnly ? t("people.detail.readOnlyHint") : undefined}
              onClick={() => void setContactFavorite(contact, !contact.isFavorite)}
            />
          </div>
          {subtitle ? <div className={styles.detailSubtitle}>{subtitle}</div> : null}
          {contact.nickname.trim() ? (
            <div className={styles.detailNickname}>
              {t("people.detail.nickname", { name: contact.nickname })}
            </div>
          ) : null}
          <div className={styles.detailActions}>
            <Button
              appearance="primary"
              icon={<Mail16Regular />}
              disabled={!email}
              onClick={() => void composeToContact(contact, displayOrder)}
            >
              {t("people.detail.sendEmail")}
            </Button>
            <Button
              appearance="outline"
              icon={<CalendarLtr16Regular />}
              disabled={!email}
              onClick={() => void scheduleMeetingWithContact(contact, displayOrder)}
            >
              {t("people.detail.scheduleMeeting")}
            </Button>
            <IconButton
              icon={<Edit16Regular />}
              label={t("people.detail.edit")}
              disabled={actionsLocked}
              tooltip={readOnly ? t("people.detail.readOnlyHint") : undefined}
              onClick={() => openEdit(contact.id)}
            />
            <IconButton
              icon={<Delete16Regular />}
              label={t("people.detail.delete")}
              disabled={actionsLocked}
              tooltip={readOnly ? t("people.detail.readOnlyHint") : undefined}
              onClick={() => void deleteContactWithConfirm(contact)}
            />
          </div>
        </div>
      </div>

      <div className={styles.detailScroll}>
        <section className={styles.detailSection} aria-labelledby="people-detail-emails">
          <h3 className={styles.detailSectionTitle} id="people-detail-emails">
            {t("people.detail.section.emails")}
          </h3>
          {emails.length === 0 ? (
            <p className={styles.mutedText}>{t("people.detail.noEmails")}</p>
          ) : (
            emails.map((field, index) => (
              <div className={styles.fieldRow} key={`email-${index}`}>
                <span className={styles.fieldIcon} aria-hidden="true">
                  <Mail16Regular />
                </span>
                <div className={styles.fieldBody}>
                  <button
                    type="button"
                    className={styles.fieldValueLink}
                    title={t("people.detail.sendEmail")}
                    onClick={() =>
                      void composeToEmail(field.value.trim(), displayName, contact.accountId)
                    }
                  >
                    {field.value.trim()}
                  </button>
                  <span className={styles.fieldMeta}>
                    {typeLabel(t, field)}
                    {field.isPrimary ? (
                      <Badge appearance="neutral">{t("people.detail.primary")}</Badge>
                    ) : null}
                  </span>
                </div>
                <span className={styles.fieldActions}>
                  <IconButton
                    icon={<Copy16Regular />}
                    label={t("people.detail.copy")}
                    size="small"
                    onClick={() => void copy(field.value.trim())}
                  />
                </span>
              </div>
            ))
          )}
        </section>

        <section className={styles.detailSection} aria-labelledby="people-detail-phones">
          <h3 className={styles.detailSectionTitle} id="people-detail-phones">
            {t("people.detail.section.phones")}
          </h3>
          {phones.length === 0 ? (
            <p className={styles.mutedText}>{t("people.detail.noPhones")}</p>
          ) : (
            phones.map((field, index) => (
              <div className={styles.fieldRow} key={`phone-${index}`}>
                <span className={styles.fieldIcon} aria-hidden="true">
                  <Call16Regular />
                </span>
                <div className={styles.fieldBody}>
                  <span className={styles.fieldValue}>{field.value.trim()}</span>
                  <span className={styles.fieldMeta}>
                    {typeLabel(t, field)}
                    {field.isPrimary ? (
                      <Badge appearance="neutral">{t("people.detail.primary")}</Badge>
                    ) : null}
                  </span>
                </div>
                <span className={styles.fieldActions}>
                  <IconButton
                    icon={<Copy16Regular />}
                    label={t("people.detail.copy")}
                    size="small"
                    onClick={() => void copy(field.value.trim())}
                  />
                </span>
              </div>
            ))
          )}
        </section>

        {addresses.length > 0 ? (
          <section className={styles.detailSection} aria-labelledby="people-detail-addresses">
            <h3 className={styles.detailSectionTitle} id="people-detail-addresses">
              {t("people.detail.section.addresses")}
            </h3>
            {addresses.map((address, index) => {
              const lines = addressLines(address);
              const mapUrl = addressMapUrl(address);
              return (
                <div className={styles.fieldRow} key={`address-${index}`}>
                  <span className={styles.fieldIcon} aria-hidden="true">
                    <Location16Regular />
                  </span>
                  <div className={styles.fieldBody}>
                    <span className={styles.addressLines}>
                      {lines.map((line, lineIndex) => (
                        <span key={lineIndex}>{line}</span>
                      ))}
                    </span>
                    <span className={styles.fieldMeta}>{typeLabel(t, address)}</span>
                    {mapUrl ? (
                      <button
                        type="button"
                        className={styles.mapLink}
                        onClick={() => openExternalUrl(mapUrl)}
                      >
                        <Map16Regular aria-hidden="true" />
                        {t("people.detail.openMap")}
                      </button>
                    ) : null}
                  </div>
                  <span className={styles.fieldActions}>
                    <IconButton
                      icon={<Copy16Regular />}
                      label={t("people.detail.copy")}
                      size="small"
                      onClick={() => void copy(lines.join(", "))}
                    />
                  </span>
                </div>
              );
            })}
          </section>
        ) : null}

        {websites.length > 0 ? (
          <section className={styles.detailSection} aria-labelledby="people-detail-websites">
            <h3 className={styles.detailSectionTitle} id="people-detail-websites">
              {t("people.detail.section.websites")}
            </h3>
            {websites.map((value, index) => {
              // Only http(s) links are opened; anything else (file:, custom
              // schemes from imported vCards) stays plain text.
              const href = websiteHref(value);
              return (
                <div className={styles.fieldRow} key={`website-${index}`}>
                  <span className={styles.fieldIcon} aria-hidden="true">
                    <Globe16Regular />
                  </span>
                  <div className={styles.fieldBody}>
                    {href ? (
                      <button
                        type="button"
                        className={styles.fieldValueLink}
                        title={t("people.detail.openWebsite", { url: value })}
                        onClick={() => openExternalUrl(href)}
                      >
                        {value}
                      </button>
                    ) : (
                      <span className={styles.fieldValue}>{value}</span>
                    )}
                  </div>
                  <span className={styles.fieldActions}>
                    {href ? (
                      <IconButton
                        icon={<Open16Regular />}
                        label={t("people.detail.openWebsite", { url: value })}
                        size="small"
                        onClick={() => openExternalUrl(href)}
                      />
                    ) : null}
                  </span>
                </div>
              );
            })}
          </section>
        ) : null}

        {contact.birthday ? (
          <section className={styles.detailSection} aria-labelledby="people-detail-birthday">
            <h3 className={styles.detailSectionTitle} id="people-detail-birthday">
              {t("people.detail.section.birthday")}
            </h3>
            <div className={styles.fieldRow}>
              <span className={styles.fieldIcon} aria-hidden="true">
                <CalendarLtr16Regular />
              </span>
              <div className={styles.fieldBody}>
                <span className={styles.fieldValue}>
                  {formatBirthday(contact.birthday, getLocale())}
                </span>
              </div>
            </div>
          </section>
        ) : null}

        <section className={styles.detailSection} aria-labelledby="people-detail-groups">
          <h3 className={styles.detailSectionTitle} id="people-detail-groups">
            {t("people.detail.section.groups")}
          </h3>
          {groupNames.length === 0 ? (
            <p className={styles.mutedText}>{t("people.detail.noGroups")}</p>
          ) : (
            <div className={styles.groupsRow}>
              {groupNames.map((name) => (
                <Badge key={name} appearance="neutral">
                  {name}
                </Badge>
              ))}
            </div>
          )}
        </section>

        {contact.notes.trim() ? (
          <section className={styles.detailSection} aria-labelledby="people-detail-notes">
            <h3 className={styles.detailSectionTitle} id="people-detail-notes">
              {t("people.detail.section.notes")}
            </h3>
            <div className={styles.fieldRow}>
              <span className={styles.fieldIcon} aria-hidden="true">
                <Note16Regular />
              </span>
              <div className={styles.fieldBody}>
                <span className={styles.notesText}>{contact.notes}</span>
              </div>
            </div>
          </section>
        ) : null}

        <section className={styles.detailSection} aria-labelledby="people-detail-info">
          <h3 className={styles.detailSectionTitle} id="people-detail-info">
            {t("people.detail.section.details")}
          </h3>
          {book ? (
            <div className={styles.fieldRow}>
              <span className={styles.fieldIcon} aria-hidden="true">
                <Info16Regular />
              </span>
              <div className={styles.fieldBody}>
                <span className={styles.fieldValue}>{book.name}</span>
                <span className={styles.fieldMeta}>{t("people.detail.addressBook")}</span>
              </div>
            </div>
          ) : null}
          {contact.department.trim() ? (
            <div className={styles.fieldRow}>
              <span className={styles.fieldIcon} aria-hidden="true">
                <Building16Regular />
              </span>
              <div className={styles.fieldBody}>
                <span className={styles.fieldValue}>{contact.department}</span>
              </div>
            </div>
          ) : null}
        </section>
      </div>

      <div className={styles.detailFooter}>
        {book ? <span>{t("people.detail.addressBook")}: {book.name}</span> : null}
        {contact.updatedAt ? (
          <span>
            {t("people.detail.updated", {
              time: formatRelativeTime(contact.updatedAt),
            })}
          </span>
        ) : null}
      </div>
    </div>
  );
}

export default ContactDetail;
