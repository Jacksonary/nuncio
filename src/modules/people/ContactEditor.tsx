/**
 * Contact create/edit dialog. Covers every `Contact` field, multiple typed
 * e-mails / phones / addresses / websites, year-less birthdays, group
 * membership, favorite state and a drag & drop / clipboard photo picker.
 */
import {
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import {
  Add16Regular,
  Delete16Regular,
  ImageAdd20Regular,
  Warning16Regular,
} from "@fluentui/react-icons";
import {
  Button,
  Checkbox,
  Dialog,
  EmptyState,
  IconButton,
  Input,
  Select,
  Spinner,
  TextArea,
  confirm,
  toast,
} from "@/components";
import type { AddressBook, ContactField, ContactFieldType } from "@/ipc/types";
import { getLocale, useT } from "@/lib/i18n";
import {
  CONTACT_FIELD_TYPES,
  daysInMonth,
  draftFromContact,
  draftToContact,
  emptyAddress,
  emptyDraft,
  emptyField,
  joinBirthday,
  parseBirthday,
  toggleId,
  validateDraft,
  type ContactDraft,
  type DraftValidation,
} from "./logic";
import {
  defaultWritableBook,
  peopleErrorText,
  saveContact,
  writableAddressBooks,
} from "./actions";
import { useAddressBooksQuery, useContactQuery, useGroupsQuery } from "./queries";
import {
  PHOTO_MAX_FILE_BYTES,
  imageFileFromClipboard,
  photoFromFile,
} from "./photo";
import { usePeopleUiStore, type ContactEditorRequest } from "./uiStore";
import styles from "./People.module.css";

function fieldTypeOptions(
  translate: (key: string, vars?: Record<string, string | number>) => string,
) {
  return CONTACT_FIELD_TYPES.map((type) => ({
    value: type,
    label: translate(`people.editor.fieldType.${type}`),
  }));
}

function MonthSelect({
  value,
  onChange,
  locale,
  disabled,
}: {
  value: string | null;
  onChange: (value: string) => void;
  locale: string;
  disabled?: boolean;
}) {
  const translate = useT();
  const options = useMemo(() => {
    let format: Intl.DateTimeFormat;
    try {
      format = new Intl.DateTimeFormat(locale, { month: "long" });
    } catch {
      format = new Intl.DateTimeFormat("en", { month: "long" });
    }
    return Array.from({ length: 12 }, (_, index) => ({
      value: String(index + 1),
      label: format.format(new Date(2000, index, 1, 12)),
    }));
  }, [locale]);
  return (
    <Select
      value={value}
      onChange={onChange}
      options={options}
      placeholder={translate("people.editor.birthdayMonth")}
      ariaLabel={translate("people.editor.birthdayMonth")}
      size="small"
      disabled={disabled}
    />
  );
}

function BirthdayField({
  value,
  onChange,
  error,
}: {
  value: string | null;
  onChange: (value: string | null) => void;
  error?: string;
}) {
  const translate = useT();
  const locale = getLocale();
  const parts = parseBirthday(value);
  const [yearText, setYearText] = useState(parts?.year ? String(parts.year) : "");

  // Keep the text field in sync when the value changes from outside (e.g.
  // opening the editor for another contact).
  useEffect(() => {
    setYearText(parseBirthday(value)?.year?.toString() ?? "");
  }, [value]);

  const monthValue = parts ? String(parts.month) : null;
  const dayValue = parts ? String(parts.day) : null;
  const effectiveYear = yearText.length === 4 ? yearText : "";

  const commit = useCallback(
    (year: string, month: string | null, day: string | null) => {
      if (!month) {
        onChange(null);
        return;
      }
      onChange(joinBirthday(year, month, day ?? "1"));
    },
    [onChange],
  );

  const commitYear = useCallback(
    (raw: string) => {
      const digits = raw.replace(/\D/gu, "").slice(0, 4);
      if (!digits) {
        setYearText("");
        commit("", monthValue, dayValue);
        return;
      }
      if (digits.length !== 4) {
        // Keep the text while typing; do not destroy the stored month/day.
        setYearText(digits);
        return;
      }
      setYearText(digits);
      const month = parts?.month ?? 1;
      const day = Math.min(parts?.day ?? 1, daysInMonth(Number(digits), month));
      commit(digits, monthValue ?? "1", String(day));
    },
    [commit, dayValue, monthValue, parts?.day, parts?.month],
  );

  const dayOptions = useMemo(() => {
    const count = daysInMonth(
      yearText && yearText.length === 4 ? Number(yearText) : null,
      parts?.month ?? 1,
    );
    return Array.from({ length: count }, (_, index) => ({
      value: String(index + 1),
      label: String(index + 1),
    }));
  }, [parts?.month, yearText]);

  return (
    <div className={styles.editorSection}>
      <div className={styles.editorSectionHeader}>{translate("people.editor.birthday")}</div>
      <div className={styles.birthdayRow}>
        <Input
          value={yearText}
          inputMode="numeric"
          placeholder={translate("people.editor.birthdayYear")}
          aria-label={translate("people.editor.birthdayYear")}
          maxLength={4}
          size="small"
          onChange={(event) => setYearText(event.target.value.replace(/\D/gu, "").slice(0, 4))}
          onBlur={(event) => commitYear(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") commitYear((event.target as HTMLInputElement).value);
          }}
        />
        <MonthSelect
          value={monthValue}
          locale={locale}
          onChange={(month) => commit(effectiveYear, month, dayValue ?? "1")}
        />
        <Select
          value={dayValue}
          onChange={(day) => commit(effectiveYear, monthValue ?? "1", day)}
          options={dayOptions}
          placeholder={translate("people.editor.birthdayDay")}
          ariaLabel={translate("people.editor.birthdayDay")}
          size="small"
          disabled={!parts && !monthValue && !dayValue}
        />
      </div>
      <p className={styles.birthdayHint}>{translate("people.editor.birthdayYearOptional")}</p>
      {value ? (
        <div>
          <Button size="small" appearance="transparent" onClick={() => onChange(null)}>
            {translate("common.clear")}
          </Button>
        </div>
      ) : null}
      {error ? <p className={styles.errorText}>{error}</p> : null}
    </div>
  );
}

/* ────────────────────────────── Editor ─────────────────────────────── */

function EditorDialog({ request }: { request: ContactEditorRequest }) {
  const translate = useT();
  const closeEditor = usePeopleUiStore((state) => state.closeEditor);
  const booksQuery = useAddressBooksQuery();
  const books = booksQuery.data ?? [];
  const writable = useMemo(() => writableAddressBooks(books), [books]);
  const editContactQuery = useContactQuery(
    request.mode === "edit" ? request.contactId : null,
  );

  const [draft, setDraft] = useState<ContactDraft | null>(null);
  const [saving, setSaving] = useState(false);
  const [showErrors, setShowErrors] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [photoBusy, setPhotoBusy] = useState(false);
  const [photoError, setPhotoError] = useState<string | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const photoInputRef = useRef<HTMLInputElement | null>(null);
  const baseline = useRef<string | null>(null);
  const editInitialized = useRef(false);
  const emailRadioName = useId();
  const phoneRadioName = useId();

  // Initialize the edit draft once the contact arrives.
  useEffect(() => {
    if (request.mode !== "edit") return;
    if (editInitialized.current) return;
    if (editContactQuery.data) {
      editInitialized.current = true;
      setDraft(draftFromContact(editContactQuery.data));
    }
  }, [editContactQuery.data, request.mode]);

  // Initialize a create draft when the address books finish loading.
  useEffect(() => {
    if (draft || request.mode !== "create") return;
    if (booksQuery.isLoading) return;
    const book =
      (request.addressBookId
        ? writable.find((candidate) => candidate.id === request.addressBookId)
        : undefined) ??
      defaultWritableBook(books) ??
      null;
    if (book) setDraft(emptyDraft(book));
  }, [books, booksQuery.isLoading, draft, request, writable]);

  useEffect(() => {
    if (draft && baseline.current === null) baseline.current = JSON.stringify(draft);
  }, [draft]);

  const groupsQuery = useGroupsQuery(draft?.addressBookId ?? null);
  const groups = groupsQuery.data ?? [];
  const typeOptions = useMemo(() => fieldTypeOptions(translate), [translate]);

  const dirty = Boolean(
    draft && baseline.current !== null && JSON.stringify(draft) !== baseline.current,
  );

  // Contacts living in a read-only address book can be inspected but not saved.
  const currentBook = draft
    ? books.find((book) => book.id === draft.addressBookId)
    : undefined;
  const readOnlyBook = currentBook?.isReadOnly ?? false;

  const validation: DraftValidation = useMemo(
    () =>
      draft
        ? validateDraft(draft)
        : {
            hasIdentity: false,
            invalidEmailIndexes: [],
            invalidWebsiteIndexes: [],
            birthdayInvalid: false,
            valid: false,
          },
    [draft],
  );

  const requestClose = useCallback(async () => {
    if (saving) return;
    if (dirty) {
      const discard = await confirm({
        title: translate("people.editor.dirtyTitle"),
        message: translate("people.editor.dirtyMessage"),
        confirmLabel: translate("people.editor.discard"),
        cancelLabel: translate("people.editor.keepEditing"),
        danger: true,
      });
      if (!discard) return;
    }
    closeEditor();
  }, [closeEditor, dirty, saving, translate]);

  const update = useCallback((patch: Partial<ContactDraft>) => {
    setDraft((current) => (current ? { ...current, ...patch } : current));
    setSaveError(null);
  }, []);

  const updateField = useCallback(
    (list: "emails" | "phones", index: number, patch: Partial<ContactField>) => {
      setDraft((current) => {
        if (!current) return current;
        const fields = current[list].map((field, fieldIndex) =>
          fieldIndex === index ? { ...field, ...patch } : field,
        );
        return { ...current, [list]: fields };
      });
    },
    [],
  );

  const addField = useCallback((list: "emails" | "phones", type: ContactFieldType) => {
    setDraft((current) =>
      current
        ? {
            ...current,
            [list]: [
              ...current[list],
              { ...emptyField(type), isPrimary: current[list].length === 0 },
            ],
          }
        : current,
    );
  }, []);

  const removeField = useCallback((list: "emails" | "phones", index: number) => {
    setDraft((current) => {
      if (!current) return current;
      const fields = current[list].filter((_, fieldIndex) => fieldIndex !== index);
      if (fields.length > 0 && !fields.some((field) => field.isPrimary)) {
        fields[0] = { ...fields[0], isPrimary: true };
      }
      return { ...current, [list]: fields };
    });
  }, []);

  const setPrimary = useCallback((list: "emails" | "phones", index: number) => {
    setDraft((current) =>
      current
        ? {
            ...current,
            [list]: current[list].map((field, fieldIndex) => ({
              ...field,
              isPrimary: fieldIndex === index,
            })),
          }
        : current,
    );
  }, []);

  const updateAddress = useCallback(
    (index: number, patch: Partial<ContactDraft["addresses"][number]>) => {
      setDraft((current) =>
        current
          ? {
              ...current,
              addresses: current.addresses.map((address, addressIndex) =>
                addressIndex === index ? { ...address, ...patch } : address,
              ),
            }
          : current,
      );
    },
    [],
  );

  const applyPhoto = useCallback(
    async (file: File) => {
      if (!file.type.startsWith("image/")) {
        setPhotoError(translate("people.editor.photoInvalid"));
        return;
      }
      if (file.size > PHOTO_MAX_FILE_BYTES) {
        setPhotoError(translate("people.editor.photoTooLarge"));
        return;
      }
      setPhotoBusy(true);
      setPhotoError(null);
      try {
        const dataUrl = await photoFromFile(file);
        setDraft((current) => (current ? { ...current, photo: dataUrl } : current));
      } catch {
        setPhotoError(translate("people.editor.photoFailed"));
      } finally {
        setPhotoBusy(false);
      }
    },
    [translate],
  );

  // Ctrl/⌘+V attaches an image unless the user is typing in a text field.
  useEffect(() => {
    const handlePaste = (event: ClipboardEvent) => {
      if (!event.clipboardData) return;
      const target = event.target;
      if (
        target instanceof HTMLElement &&
        target.closest('input, textarea, [contenteditable="true"]')
      ) {
        return;
      }
      const file = imageFileFromClipboard(event.clipboardData);
      if (!file) return;
      event.preventDefault();
      void applyPhoto(file);
    };
    document.addEventListener("paste", handlePaste);
    return () => document.removeEventListener("paste", handlePaste);
  }, [applyPhoto]);

  const submit = useCallback(async () => {
    if (!draft) return;
    if (readOnlyBook) {
      setSaveError(translate("people.editor.readOnlyBook"));
      return;
    }
    setShowErrors(true);
    const check = validateDraft(draft);
    if (!check.valid || !draft.addressBookId) return;
    setSaving(true);
    setSaveError(null);
    try {
      const contact = draftToContact(draft);
      const saved = await saveContact(contact, request.mode === "create");
      toast.success(translate("people.editor.saved"), saved.displayName);
      baseline.current = null;
      closeEditor();
    } catch (error) {
      setSaveError(peopleErrorText(error));
    } finally {
      setSaving(false);
    }
  }, [closeEditor, draft, readOnlyBook, request.mode, translate]);

  if (request.mode === "edit" && editContactQuery.isLoading) {
    return (
      <Dialog open onClose={() => void requestClose()} title={translate("people.editor.titleEdit")} width={760}>
        <div className={styles.editorLoading}>
          <Spinner label={translate("people.editor.loading")} />
        </div>
      </Dialog>
    );
  }

  if (
    request.mode === "edit" &&
    (editContactQuery.isError || (editContactQuery.isSuccess && !editContactQuery.data))
  ) {
    return (
      <Dialog
        open
        onClose={closeEditor}
        title={translate("people.editor.titleEdit")}
        width={560}
        footer={
          <Button appearance="subtle" onClick={closeEditor}>
            {translate("common.close")}
          </Button>
        }
      >
        <EmptyState
          compact
          icon={<Warning16Regular />}
          title={translate("people.editor.errorLoad")}
          description={
            editContactQuery.error ? peopleErrorText(editContactQuery.error) : undefined
          }
        />
      </Dialog>
    );
  }

  if (!draft) {
    return (
      <Dialog
        open
        onClose={closeEditor}
        title={translate("people.editor.titleCreate")}
        width={560}
      >
        {booksQuery.isLoading || writable.length > 0 ? (
          <div className={styles.editorLoading}>
            <Spinner label={translate("common.loading")} />
          </div>
        ) : (
          <EmptyState
            compact
            title={translate("people.editor.noAddressBooks")}
            description={translate("people.sidebar.noAddressBooks")}
          />
        )}
      </Dialog>
    );
  }

  const renderFieldRows = (list: "emails" | "phones") => {
    const radioName = list === "emails" ? emailRadioName : phoneRadioName;
    const rows = draft[list];
    return (
      <div className={styles.fieldList}>
        {rows.map((field, index) => {
          const invalidEmail =
            list === "emails" && showErrors && validation.invalidEmailIndexes.includes(index);
          return (
            <div className={styles.fieldEditorRow} key={`${list}-${index}`}>
              <Select
                value={field.type}
                onChange={(value) =>
                  updateField(list, index, { type: value as ContactFieldType })
                }
                options={typeOptions}
                ariaLabel={translate("people.editor.type")}
                size="small"
              />
              <Input
                value={field.value}
                size="small"
                type={list === "emails" ? "email" : "tel"}
                inputMode={list === "emails" ? "email" : "tel"}
                placeholder={
                  list === "emails"
                    ? translate("people.editor.emailPlaceholder")
                    : translate("people.editor.phonePlaceholder")
                }
                aria-label={
                  list === "emails"
                    ? translate("people.editor.emails")
                    : translate("people.editor.phones")
                }
                error={invalidEmail ? translate("people.editor.errorEmail") : undefined}
                onChange={(event) =>
                  updateField(list, index, { value: event.target.value })
                }
              />
              <label className={styles.primaryRadio}>
                <input
                  type="radio"
                  name={radioName}
                  checked={field.isPrimary}
                  disabled={!field.value.trim()}
                  onChange={() => setPrimary(list, index)}
                />
                {translate("people.editor.primary")}
              </label>
              <IconButton
                icon={<Delete16Regular />}
                label={translate("common.remove")}
                size="small"
                onClick={() => removeField(list, index)}
              />
            </div>
          );
        })}
        <div>
          <Button
            size="small"
            appearance="subtle"
            icon={<Add16Regular />}
            onClick={() =>
              addField(list, list === "emails" ? "home" : "mobile")
            }
          >
            {list === "emails"
              ? translate("people.editor.addEmail")
              : translate("people.editor.addPhone")}
          </Button>
        </div>
      </div>
    );
  };

  return (
    <Dialog
      open
      onClose={() => void requestClose()}
      title={
        request.mode === "create"
          ? translate("people.editor.titleCreate")
          : translate("people.editor.titleEdit")
      }
      width={760}
      dismissible={!saving}
      closeOnBackdrop={!saving}
      footer={
        <>
          <Button appearance="subtle" onClick={() => void requestClose()} disabled={saving}>
            {translate("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            onClick={() => void submit()}
            loading={saving}
            disabled={saving || !draft.addressBookId || readOnlyBook}
          >
            {saving ? translate("people.editor.saving") : translate("people.editor.save")}
          </Button>
        </>
      }
    >
      <div className={styles.editorBody}>
        {readOnlyBook ? (
          <p className={styles.warningText} role="status">
            {translate("people.editor.readOnlyBook")}
          </p>
        ) : null}
        {showErrors && !validation.hasIdentity ? (
          <p className={styles.errorText}>{translate("people.editor.errorNameOrEmail")}</p>
        ) : null}
        {saveError ? <p className={styles.errorText}>{saveError}</p> : null}

        {/* Photo */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>{translate("people.editor.photo")}</div>
          <div className={styles.photoRow}>
            <input
              ref={photoInputRef}
              type="file"
              accept="image/*"
              style={{ display: "none" }}
              onChange={(event) => {
                const file = event.target.files?.[0];
                if (file) void applyPhoto(file);
                // Allow re-selecting the same file after a failed attempt.
                event.target.value = "";
              }}
            />
            <div
              className={`${styles.photoDrop} ${dragActive ? styles.photoDropActive : ""}`}
              role="button"
              tabIndex={0}
              aria-label={translate("people.editor.photoDrop")}
              onClick={() => photoInputRef.current?.click()}
              onKeyDown={(event: ReactKeyboardEvent<HTMLDivElement>) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  photoInputRef.current?.click();
                }
              }}
              onDragOver={(event) => {
                event.preventDefault();
                setDragActive(true);
              }}
              onDragLeave={() => setDragActive(false)}
              onDrop={(event) => {
                event.preventDefault();
                setDragActive(false);
                const file = Array.from(event.dataTransfer.files).find((candidate) =>
                  candidate.type.startsWith("image/"),
                );
                if (file) void applyPhoto(file);
                else setPhotoError(translate("people.editor.photoInvalid"));
              }}
            >
              {photoBusy ? (
                <Spinner size="small" />
              ) : draft.photo ? (
                <img
                  className={styles.photoImage}
                  src={draft.photo}
                  alt={translate("people.editor.photoAlt", {
                    name:
                      draft.displayName ||
                      draft.givenName ||
                      translate("people.detail.noName"),
                  })}
                />
              ) : (
                <span className={styles.photoPlaceholder}>
                  <ImageAdd20Regular aria-hidden="true" />
                  {translate("people.editor.photoAdd")}
                </span>
              )}
            </div>
            <div className={styles.photoText}>
              <p className={styles.photoHint}>{translate("people.editor.photoHint")}</p>
              {draft.photo ? (
                <div>
                  <Button
                    size="small"
                    appearance="subtle"
                    onClick={() => update({ photo: null })}
                  >
                    {translate("people.editor.photoRemove")}
                  </Button>
                </div>
              ) : null}
              {photoError ? <p className={styles.errorText}>{photoError}</p> : null}
            </div>
          </div>
        </div>

        {/* Identity */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>
            {translate("people.editor.section.identity")}
          </div>
          <div className={styles.formGrid}>
            <Input
              label={translate("people.editor.givenName")}
              value={draft.givenName}
              autoFocus={request.mode === "create"}
              onChange={(event) => update({ givenName: event.target.value })}
            />
            <Input
              label={translate("people.editor.familyName")}
              value={draft.familyName}
              onChange={(event) => update({ familyName: event.target.value })}
            />
            <Input
              label={translate("people.editor.middleName")}
              value={draft.middleName}
              onChange={(event) => update({ middleName: event.target.value })}
            />
            <Input
              label={translate("people.editor.nickname")}
              value={draft.nickname}
              onChange={(event) => update({ nickname: event.target.value })}
            />
            <div className={styles.full}>
              <Input
                label={translate("people.editor.displayName")}
                hint={translate("people.editor.displayNameHint")}
                value={draft.displayName}
                onChange={(event) => update({ displayName: event.target.value })}
              />
            </div>
          </div>
        </div>

        {/* Organization */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>
            {translate("people.editor.section.organization")}
          </div>
          <div className={styles.formGrid}>
            <Input
              label={translate("people.editor.company")}
              value={draft.company}
              onChange={(event) => update({ company: event.target.value })}
            />
            <Input
              label={translate("people.editor.jobTitle")}
              value={draft.jobTitle}
              onChange={(event) => update({ jobTitle: event.target.value })}
            />
            <Input
              label={translate("people.editor.department")}
              value={draft.department}
              onChange={(event) => update({ department: event.target.value })}
            />
          </div>
        </div>

        {/* Contact fields */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>
            {translate("people.editor.section.contact")}
          </div>
          <div className={styles.editorSection}>
            <div className={styles.editorSectionHeader}>
              {translate("people.editor.emails")}
            </div>
            {renderFieldRows("emails")}
          </div>
          <div className={styles.editorSection}>
            <div className={styles.editorSectionHeader}>
              {translate("people.editor.phones")}
            </div>
            {renderFieldRows("phones")}
          </div>

          <div className={styles.editorSection}>
            <div className={styles.editorSectionHeader}>
              {translate("people.editor.addresses")}
            </div>
            <div className={styles.fieldList}>
              {draft.addresses.map((address, index) => (
                <div className={styles.addressEditor} key={`address-${index}`}>
                  <div className={styles.addressEditorHeader}>
                    <Select
                      value={address.type}
                      onChange={(value) =>
                        updateAddress(index, { type: value as ContactFieldType })
                      }
                      options={typeOptions}
                      ariaLabel={translate("people.editor.type")}
                      size="small"
                    />
                    <span />
                    <IconButton
                      icon={<Delete16Regular />}
                      label={translate("common.remove")}
                      size="small"
                      onClick={() =>
                        update({
                          addresses: draft.addresses.filter(
                            (_, addressIndex) => addressIndex !== index,
                          ),
                        })
                      }
                    />
                  </div>
                  <div className={styles.addressGrid}>
                    <div className={styles.full}>
                      <Input
                        label={translate("people.editor.street")}
                        value={address.street}
                        size="small"
                        onChange={(event) =>
                          updateAddress(index, { street: event.target.value })
                        }
                      />
                    </div>
                    <Input
                      label={translate("people.editor.city")}
                      value={address.city}
                      size="small"
                      onChange={(event) =>
                        updateAddress(index, { city: event.target.value })
                      }
                    />
                    <Input
                      label={translate("people.editor.region")}
                      value={address.region}
                      size="small"
                      onChange={(event) =>
                        updateAddress(index, { region: event.target.value })
                      }
                    />
                    <Input
                      label={translate("people.editor.postalCode")}
                      value={address.postalCode}
                      size="small"
                      onChange={(event) =>
                        updateAddress(index, { postalCode: event.target.value })
                      }
                    />
                    <Input
                      label={translate("people.editor.country")}
                      value={address.country}
                      size="small"
                      onChange={(event) =>
                        updateAddress(index, { country: event.target.value })
                      }
                    />
                  </div>
                </div>
              ))}
              <div>
                <Button
                  size="small"
                  appearance="subtle"
                  icon={<Add16Regular />}
                  onClick={() => update({ addresses: [...draft.addresses, emptyAddress()] })}
                >
                  {translate("people.editor.addAddress")}
                </Button>
              </div>
            </div>
          </div>

          <div className={styles.editorSection}>
            <div className={styles.editorSectionHeader}>
              {translate("people.editor.websites")}
            </div>
            <div className={styles.fieldList}>
              {draft.websites.map((website, index) => {
                const invalid =
                  showErrors && validation.invalidWebsiteIndexes.includes(index);
                return (
                  <div
                    className={`${styles.fieldEditorRow} ${styles.websiteRow}`}
                    key={`website-${index}`}
                  >
                    <Input
                      value={website}
                      size="small"
                      placeholder={translate("people.editor.websitePlaceholder")}
                      aria-label={translate("people.editor.websites")}
                      error={invalid ? translate("people.editor.errorWebsite") : undefined}
                      onChange={(event) =>
                        update({
                          websites: draft.websites.map((candidate, websiteIndex) =>
                            websiteIndex === index ? event.target.value : candidate,
                          ),
                        })
                      }
                    />
                    <IconButton
                      icon={<Delete16Regular />}
                      label={translate("common.remove")}
                      size="small"
                      onClick={() =>
                        update({
                          websites: draft.websites.filter(
                            (_, websiteIndex) => websiteIndex !== index,
                          ),
                        })
                      }
                    />
                  </div>
                );
              })}
              <div>
                <Button
                  size="small"
                  appearance="subtle"
                  icon={<Add16Regular />}
                  onClick={() => update({ websites: [...draft.websites, ""] })}
                >
                  {translate("people.editor.addWebsite")}
                </Button>
              </div>
            </div>
          </div>
        </div>

        {/* Details */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>
            {translate("people.editor.section.details")}
          </div>
          <BirthdayField
            value={draft.birthday}
            onChange={(birthday) => update({ birthday })}
            error={
              showErrors && validation.birthdayInvalid
                ? translate("people.editor.errorBirthday")
                : undefined
            }
          />
          <TextArea
            label={translate("people.editor.notes")}
            placeholder={translate("people.editor.notesPlaceholder")}
            value={draft.notes}
            rows={3}
            onChange={(event) => update({ notes: event.target.value })}
          />
        </div>

        {/* Groups and address book */}
        <div className={styles.editorSection}>
          <div className={styles.editorSectionHeader}>
            {translate("people.editor.groups")}
          </div>
          {groupsQuery.isLoading ? (
            <Spinner size="small" label={translate("common.loading")} />
          ) : groups.length === 0 ? (
            <p className={styles.mutedText}>{translate("people.editor.noGroups")}</p>
          ) : (
            <div className={styles.groupChecks}>
              {groups.map((group) => (
                <Checkbox
                  key={group.id}
                  label={group.name}
                  checked={draft.groupIds.includes(group.id)}
                  disabled={readOnlyBook}
                  onChange={() =>
                    update({ groupIds: toggleId(draft.groupIds, group.id) })
                  }
                />
              ))}
            </div>
          )}
        </div>

        <div className={styles.formGrid}>
          <div>
            {request.mode === "create" ? (
              <Select
                label={translate("people.editor.addressBook")}
                value={draft.addressBookId || null}
                onChange={(value) => {
                  const book = books.find((candidate) => candidate.id === value);
                  if (!book) return;
                  update({
                    addressBookId: book.id,
                    accountId: book.accountId,
                    groupIds: [],
                  });
                }}
                options={writable.map((book: AddressBook) => ({
                  value: book.id,
                  label: book.name,
                }))}
                disabled={writable.length === 0}
              />
            ) : (
              <Input
                label={translate("people.editor.addressBook")}
                value={currentBook?.name ?? ""}
                disabled
                readOnly
              />
            )}
          </div>
          <div>
            <Checkbox
              label={translate("people.editor.favorite")}
              checked={draft.isFavorite}
              disabled={readOnlyBook}
              onChange={(event) => update({ isFavorite: event.target.checked })}
            />
          </div>
        </div>
      </div>
    </Dialog>
  );
}

export function ContactEditor() {
  const request = usePeopleUiStore((state) => state.editor);
  if (!request) return null;
  const key =
    request.mode === "edit"
      ? `edit:${request.contactId}`
      : `create:${request.addressBookId ?? "default"}`;
  return <EditorDialog key={key} request={request} />;
}

export default ContactEditor;
