/**
 * Hosts the .ics / .vcf import dialogs triggered by `app.takePendingOpen()`.
 * The shell sets `pendingIcsPath` / `pendingVcfPath` in the store and these
 * dialogs let the user pick the destination calendar / address book.
 */
import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Dialog, EmptyState, Select, Spinner, toast } from "@/components";
import { calendars, contacts, isIpcError } from "@/ipc/api";
import { useT } from "@/lib/i18n";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function ImportDialogs() {
  const pendingIcsPath = useShellStore((state) => state.pendingIcsPath);
  const pendingVcfPath = useShellStore((state) => state.pendingVcfPath);
  return (
    <>
      {pendingIcsPath ? <IcsImportDialog path={pendingIcsPath} /> : null}
      {pendingVcfPath ? <VcfImportDialog path={pendingVcfPath} /> : null}
    </>
  );
}

function IcsImportDialog({ path }: { path: string }) {
  const t = useT();
  const close = useShellStore((state) => state.setPendingIcsPath);
  const client = useQueryClient();
  const calendarsQuery = useQuery({
    queryKey: queryKeys.calendars(),
    queryFn: calendars.list,
    staleTime: 30_000,
  });
  const list = calendarsQuery.data ?? [];
  const [calendarId, setCalendarId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!calendarId && list.length > 0) setCalendarId(list[0].id);
  }, [calendarId, list]);

  const runImport = async () => {
    if (!calendarId) return;
    setBusy(true);
    try {
      const count = await calendars.importIcs(path, calendarId);
      toast.success(t("shell.importIcs.done", { count }));
      close(null);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.events });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.calendars });
    } catch (error) {
      toast.error(
        t("shell.importIcs.title"),
        t("shell.importIcs.failed", { message: errorMessage(error) }),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onClose={() => close(null)}
      title={t("shell.importIcs.title")}
      width={460}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={() => close(null)}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            loading={busy}
            disabled={!calendarId}
            onClick={() => void runImport()}
          >
            {t("shell.importConfirm")}
          </Button>
        </>
      }
    >
      {calendarsQuery.isLoading ? (
        <Spinner label={t("common.loading")} />
      ) : list.length === 0 ? (
        <EmptyState
          compact
          title={t("shell.importIcs.title")}
          description={t("shell.importIcs.noCalendars")}
        />
      ) : (
        <Select
          label={t("shell.importIcs.choose")}
          value={calendarId}
          onChange={setCalendarId}
          options={list.map((calendar) => ({
            value: calendar.id,
            label: calendar.name,
            icon: (
              <span
                style={{
                  display: "inline-block",
                  width: 12,
                  height: 12,
                  borderRadius: 3,
                  background: calendar.color,
                }}
              />
            ),
          }))}
          disabled={busy}
        />
      )}
    </Dialog>
  );
}

function VcfImportDialog({ path }: { path: string }) {
  const t = useT();
  const close = useShellStore((state) => state.setPendingVcfPath);
  const client = useQueryClient();
  const booksQuery = useQuery({
    queryKey: queryKeys.addressBooks(),
    queryFn: contacts.addressBooks,
    staleTime: 30_000,
  });
  const books = booksQuery.data ?? [];
  const [addressBookId, setAddressBookId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!addressBookId && books.length > 0) setAddressBookId(books[0].id);
  }, [addressBookId, books]);

  const runImport = async () => {
    if (!addressBookId) return;
    setBusy(true);
    try {
      const count = await contacts.importVcf(path, addressBookId);
      toast.success(t("shell.importVcf.done", { count }));
      close(null);
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.contacts });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.addressBooks });
    } catch (error) {
      toast.error(
        t("shell.importVcf.title"),
        t("shell.importVcf.failed", { message: errorMessage(error) }),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onClose={() => close(null)}
      title={t("shell.importVcf.title")}
      width={460}
      dismissible={!busy}
      closeOnBackdrop={!busy}
      footer={
        <>
          <Button appearance="subtle" disabled={busy} onClick={() => close(null)}>
            {t("common.cancel")}
          </Button>
          <Button
            appearance="primary"
            loading={busy}
            disabled={!addressBookId}
            onClick={() => void runImport()}
          >
            {t("shell.importConfirm")}
          </Button>
        </>
      }
    >
      {booksQuery.isLoading ? (
        <Spinner label={t("common.loading")} />
      ) : books.length === 0 ? (
        <EmptyState
          compact
          title={t("shell.importVcf.title")}
          description={t("shell.importVcf.noAddressBooks")}
        />
      ) : (
        <Select
          label={t("shell.importVcf.choose")}
          value={addressBookId}
          onChange={setAddressBookId}
          options={books.map((book) => ({ value: book.id, label: book.name }))}
          disabled={busy}
        />
      )}
    </Dialog>
  );
}
