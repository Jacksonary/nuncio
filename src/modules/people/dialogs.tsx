/**
 * Small dialogs owned by the People module: create/rename group and .vcf
 * import. Import needs a file picker (Tauri dialog) because the IPC contract
 * has no "read file" command — the backend imports by path.
 */
import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  Button,
  Dialog,
  EmptyState,
  Input,
  Select,
  Spinner,
  toast,
} from "@/components";
import { contacts, isIpcError } from "@/ipc/api";
import type { AddressBook, ContactGroup } from "@/ipc/types";
import { useT } from "@/lib/i18n";
import { queryKeyPrefixes } from "@/lib/queryKeys";
import { useAddressBooksQuery } from "./queries";
import { peopleErrorText, writableAddressBooks } from "./actions";
import styles from "./People.module.css";

/* ───────────────────────────── Group name ──────────────────────────── */

export interface GroupNameDialogProps {
  open: boolean;
  mode: "create" | "rename";
  /** Address book for create mode. */
  addressBookId: string | null;
  /** Group for rename mode. */
  group: ContactGroup | null;
  onClose: () => void;
  /** Called after a successful create/rename. */
  onSaved?: (group: ContactGroup) => void;
}

export function GroupNameDialog({
  open,
  mode,
  addressBookId,
  group,
  onClose,
  onSaved,
}: GroupNameDialogProps) {
  const translate = useT();
  const client = useQueryClient();
  const books = useAddressBooksQuery().data ?? [];
  const targetBook =
    mode === "create"
      ? books.find((book) => book.id === addressBookId)
      : group
        ? books.find((book) => book.id === group.addressBookId)
        : undefined;
  const readOnly = targetBook?.isReadOnly ?? false;
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setName(mode === "rename" ? group?.name ?? "" : "");
    setError(null);
    setBusy(false);
  }, [open, mode, group]);

  const canSubmit = name.trim().length > 0 && !busy && !readOnly;

  const submit = async () => {
    if (readOnly) {
      setError(translate("people.detail.readOnlyHint"));
      return;
    }
    if (!canSubmit) {
      setError(translate("people.group.nameRequired"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      let saved: ContactGroup;
      if (mode === "create") {
        if (!addressBookId) {
          setError(translate("people.editor.noAddressBooks"));
          setBusy(false);
          return;
        }
        saved = await contacts.createGroup(addressBookId, name.trim());
        toast.success(translate("people.sidebar.groupCreated", { name: saved.name }));
      } else {
        if (!group) {
          setBusy(false);
          return;
        }
        saved = await contacts.renameGroup(group.id, name.trim());
        toast.success(translate("people.sidebar.groupRenamed"));
      }
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.groups });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.contacts });
      onSaved?.(saved);
      onClose();
    } catch (submitError) {
      setError(peopleErrorText(submitError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={
        mode === "create"
          ? translate("people.group.createTitle")
          : translate("people.group.renameTitle")
      }
      width={420}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" onClick={onClose} disabled={busy}>
            {translate("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            onClick={() => void submit()}
            loading={busy}
            disabled={!canSubmit}
          >
            {mode === "create"
              ? translate("people.group.create")
              : translate("people.group.rename")}
          </Button>
        </>
      }
    >
      <div className={styles.dialogBody}>
        {readOnly ? (
          <p className={styles.warningText} role="status">
            {translate("people.detail.readOnlyHint")}
          </p>
        ) : null}
        <Input
          autoFocus
          label={translate("people.group.nameLabel")}
          placeholder={translate("people.group.namePlaceholder")}
          value={name}
          maxLength={120}
          error={error ?? undefined}
          disabled={busy}
          onChange={(event) => {
            setName(event.target.value);
            setError(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              void submit();
            }
          }}
        />

      </div>
    </Dialog>
  );
}

/* ─────────────────────────────── Import ────────────────────────────── */

export interface VcfImportDialogProps {
  open: boolean;
  /** Preselected address book (e.g. from a book's context menu). */
  initialAddressBookId: string | null;
  onClose: () => void;
}

export function VcfImportDialog({
  open,
  initialAddressBookId,
  onClose,
}: VcfImportDialogProps) {
  const translate = useT();
  const client = useQueryClient();
  const booksQuery = useAddressBooksQuery();
  const books = useMemo(
    () => writableAddressBooks(booksQuery.data ?? []),
    [booksQuery.data],
  );
  const [addressBookId, setAddressBookId] = useState<string | null>(null);
  const [path, setPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setPath(null);
    setError(null);
    setBusy(false);
    setAddressBookId(null);
  }, [open, initialAddressBookId]);

  // Pick a destination as soon as the address books are available; never
  // override a book the user has already chosen.
  useEffect(() => {
    if (!open || addressBookId !== null || books.length === 0) return;
    const preferred =
      (initialAddressBookId
        ? books.find((book) => book.id === initialAddressBookId)
        : undefined) ??
      books.find((book) => book.isDefault) ??
      books[0] ??
      null;
    setAddressBookId(preferred?.id ?? null);
  }, [addressBookId, books, initialAddressBookId, open]);

  const fileName = path
    ? path.split(/[\\/]/u).pop() ?? path
    : null;

  const chooseFile = async () => {
    if (!isTauri()) {
      setError(translate("people.import.notAvailable"));
      return;
    }
    try {
      const selected = await openFileDialog({
        multiple: false,
        directory: false,
        filters: [{ name: translate("people.import.title"), extensions: ["vcf", "vcard"] }],
      });
      if (typeof selected === "string") {
        setPath(selected);
        setError(null);
      }
    } catch (chooseError) {
      setError(peopleErrorText(chooseError));
    }
  };

  const submit = async () => {
    if (!path) {
      setError(translate("people.import.noFile"));
      return;
    }
    if (!addressBookId) {
      setError(translate("people.import.noWritableBooks"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const count = await contacts.importVcf(path, addressBookId);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.contacts });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.groups });
      toast.success(translate("people.import.done", { count }));
      onClose();
    } catch (submitError) {
      const message = isIpcError(submitError) ? submitError.message : peopleErrorText(submitError);
      setError(translate("people.import.failed", { message }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={translate("people.import.title")}
      width={480}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" onClick={onClose} disabled={busy}>
            {translate("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            onClick={() => void submit()}
            loading={busy}
            disabled={!path || !addressBookId}
          >
            {busy ? translate("people.import.importing") : translate("people.import.action")}
          </Button>
        </>
      }
    >
      <div className={styles.dialogBody}>
        {booksQuery.isLoading ? (
          <Spinner label={translate("common.loading")} />
        ) : books.length === 0 ? (
          <EmptyState
            compact
            title={translate("people.import.noWritableBooks")}
            description={translate("people.sidebar.noAddressBooks")}
          />
        ) : (
          <>
            <Select
              label={translate("people.import.chooseBook")}
              value={addressBookId}
              onChange={(value) => setAddressBookId(value)}
              options={books.map((book: AddressBook) => ({
                value: book.id,
                label: book.name,
              }))}
              disabled={busy}
            />
            <div className={styles.dialogBody}>
              <Button
                appearance="outline"
                onClick={() => void chooseFile()}
                disabled={busy}
              >
                {translate("people.import.chooseFile")}
              </Button>
              {fileName ? (
                <p className={styles.dialogHint}>{fileName}</p>
              ) : (
                <p className={styles.dialogHint}>{translate("people.import.fileHint")}</p>
              )}
              {error ? <p className={styles.errorText}>{error}</p> : null}
            </div>
          </>
        )}
      </div>
    </Dialog>
  );
}

export default VcfImportDialog;
