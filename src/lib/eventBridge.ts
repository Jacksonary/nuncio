/**
 * Backend event bridge: the single place that reacts to Rust -> JS events.
 * It invalidates the right query prefixes, routes navigation requests, handles
 * file/mailto opens and turns operation failures / reminders into toasts.
 *
 * Installed once by the Shell; secondary windows install a lightweight bridge
 * that only refreshes cached queries (see `installSecondaryEventBridge`).
 */
import { isTauri } from "@tauri-apps/api/core";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { app, calendars, compose, events, on, tasks, type EventPayloads } from "@/ipc/api";
import type { AppSettings, Draft, SyncStatus } from "@/ipc/types";
import { flushUnsavedDraft, registerComposeSave } from "@/lib/pendingWrites";
import { toast } from "@/components/Toast";
import { formatListDate, formatTime } from "@/lib/format";
import { t } from "@/lib/i18n";
import { playReminderChime } from "@/lib/sound";
import { queryClient } from "@/lib/queryClient";
import {
  accountScopedPrefixes,
  invalidateAccountScopedData,
  queryKeys,
  queryKeyPrefixes,
} from "@/lib/queryKeys";
import {
  useShellStore,
  type CalendarSelection,
  type MailSelection,
  type TodoSelection,
} from "@/lib/store";

function invalidate(prefix: readonly unknown[]): void {
  void queryClient.invalidateQueries({ queryKey: prefix });
}

function invalidateAccountData(): void {
  invalidateAccountScopedData(queryClient);
}

/**
 * Prefixes dropped for an account change, exported so the settings delete
 * mutation and this bridge are pinned to the same list by the parity test.
 */
export const accountDataPrefixes = accountScopedPrefixes;

function invalidateMailData(): void {
  invalidate(queryKeyPrefixes.folders);
  invalidate(queryKeyPrefixes.messages);
  invalidate(queryKeyPrefixes.message);
  invalidate(queryKeyPrefixes.thread);
  invalidate(queryKeyPrefixes.unifiedCounts);
}

/** A reminder start is all-day when the backend says so or sends a date-only value. */
function isAllDay(start: string | null, allDay: boolean | undefined): boolean {
  if (allDay) return true;
  return typeof start === "string" && /^\d{4}-\d{2}-\d{2}$/.test(start);
}

function reminderTimeText(payload: EventPayloads["reminder://due"]): string {
  if (!payload.start) return "";
  if (isAllDay(payload.start, payload.allDay)) return t("shell.reminder.allDay");
  return formatTime(payload.start);
}

function openRoute(route: string, title: string): void {
  app.openWindow(route, title).catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    console.warn("[eventBridge] app_open_window failed, falling back to hash route", message);
    if (typeof window !== "undefined") {
      window.location.hash = route.startsWith("/") ? `#${route}` : `#/${route}`;
    }
  });
}

/**
 * Persist a draft (when it has no id yet) and open its compose window. The
 * subject is used as the window title so new mail is recognisable in the task
 * bar; "New message" is the fallback for drafts without one.
 */
async function openComposeDraft(draft: Draft): Promise<void> {
  const saved = draft.id ? draft : await compose.save(draft);
  if (!saved.id) {
    throw new Error("compose_save returned a draft without an id");
  }
  const subject = saved.subject.trim();
  openRoute(`/compose/${saved.id}`, subject || t("shell.window.compose"));
}

/** Save a draft and open its window; errors are surfaced as a toast. */
/**
 * Save any open composer before the webview is destroyed. A compose window keeps
 * its own close handler; this covers the inline composer in the main window,
 * where unmount never runs because the whole webview goes away.
 */
async function flushComposeDrafts(seq: number): Promise<void> {
  const { useComposeStore } = await import("@/modules/mail/compose");
  const flush = useComposeStore.getState().flushHandler;
  let ok = true;
  try {
    // No composer open means there is nothing to save.
    if (flush) ok = await flush();
  } catch (error) {
    console.warn("[compose] flush before close failed", error);
    ok = false;
  }
  if (ok) {
    // The handler only covers a *mounted* composer. A send that started just
    // before the switch keeps running after the component is gone, and its draft
    // write is the only copy of the text — so write the recorded snapshot too,
    // rather than trusting that "no composer mounted" means "nothing to save".
    ok = await flushUnsavedDraft();
  }
  if (!ok) {
    // Do not acknowledge: the backend keeps the webview alive and shows the
    // window instead of destroying unsaved text.
    console.warn("[compose] draft could not be saved; close-to-tray was cancelled");
    toast.error(t("compose.error.saveFailed"));
    return;
  }
  await compose.flushDone(seq);
}

function openComposeDraftWithFeedback(draft: Draft): void {
  openComposeDraft(draft).catch((error: unknown) => {
    toast.error(
      t("shell.window.compose"),
      t("shell.composeFailed", {
        message: error instanceof Error ? error.message : String(error),
      }),
    );
  });
}

function applyNavigate(payload: EventPayloads["app://navigate"]): void {
  const store = useShellStore.getState();
  store.setActiveModule(payload.module);
  store.setShowSettings(false);

  if (payload.module === "mail") {
    const patch: Partial<MailSelection> = {};
    if (payload.folderId) patch.folderId = payload.folderId;
    if (payload.messageId) patch.messageId = payload.messageId;
    if (Object.keys(patch).length > 0) store.setSelection("mail", patch);
  } else if (payload.module === "calendar") {
    if (payload.eventId) {
      const patch: Partial<CalendarSelection> = {
        eventId: payload.eventId,
        instanceStart: null,
      };
      store.setSelection("calendar", patch);
    }
  } else if (payload.module === "todo") {
    if (payload.taskId) {
      const patch: Partial<TodoSelection> = { taskId: payload.taskId };
      store.setSelection("todo", patch);
    }
  }

  if (payload.messageId) {
    openRoute(`/message/${payload.messageId}`, t("shell.window.message"));
  }
}

function reportError(title: string, error: unknown): void {
  toast.error(title, error instanceof Error ? error.message : String(error));
}

function rememberReminder(
  payload: EventPayloads["reminder://due"],
): void {
  const timeText = reminderTimeText(payload);
  // The backend skips the system notification while the window is in the
  // foreground, so this card is the only cue — give it a sound too. Follow the
  // same switches as the notifier and stay silent when the window is in the
  // background, where the system notification already carries a sound.
  const settings = queryClient.getQueryData<AppSettings>(queryKeys.settings());
  const enabled = settings?.notifications.enabled ?? true;
  const moduleOn =
    payload.kind === "task"
      ? (settings?.notifications.taskReminders ?? true)
      : (settings?.notifications.eventReminders ?? true);
  const soundOn = settings?.notifications.sound ?? true;
  if (enabled && moduleOn && soundOn && document.hasFocus()) playReminderChime();
  if (payload.kind === "event") {
    toast.reminder(
      payload.title,
      [timeText, payload.location].filter(Boolean).join(" · ") ||
        t("shell.reminder.title"),
      {
        duration: 0,
        key: `reminder:${payload.kind}:${payload.id}`,
        actions: [
          {
            label: t("shell.reminder.open"),
            primary: true,
            onClick: () => {
              useShellStore.getState().setActiveModule("calendar");
              useShellStore.getState().setSelection("calendar", {
                eventId: payload.id,
                instanceStart: payload.instanceStart,
              });
            },
          },
          {
            label: t("shell.reminder.snooze"),
            onClick: () => {
              events
                .snoozeReminder(payload.id, payload.instanceStart ?? "", 10)
                .catch((error: unknown) => reportError(t("common.error"), error));
            },
          },
          {
            label: t("shell.reminder.dismiss"),
            onClick: () => {
              events
                .dismissReminder(payload.id, payload.instanceStart ?? "")
                .catch((error: unknown) => {
                  console.warn("[eventBridge] dismissReminder failed", error);
                });
            },
          },
        ],
      },
    );
  } else {
    toast.reminder(
      payload.title,
      timeText
        ? `${timeText} · ${t("shell.reminder.taskBody")}`
        : t("shell.reminder.taskBody"),
      {
        duration: 0,
        key: `reminder:${payload.kind}:${payload.id}`,
        actions: [
          {
            label: t("shell.reminder.open"),
            primary: true,
            onClick: () => {
              useShellStore.getState().setActiveModule("todo");
              useShellStore.getState().setSelection("todo", { taskId: payload.id });
            },
          },
          {
            label: t("shell.reminder.snooze"),
            onClick: () => {
              tasks
                .snoozeReminder(payload.id, 10)
                .catch((error: unknown) => reportError(t("common.error"), error));
            },
          },
          {
            label: t("shell.reminder.snoozeHour"),
            onClick: () => {
              tasks
                .snoozeReminder(payload.id, 60)
                .catch((error: unknown) => reportError(t("common.error"), error));
            },
          },
          {
            label: t("shell.reminder.dismiss"),
            onClick: () => {
              tasks.dismissReminder(payload.id).catch((error: unknown) => {
                console.warn("[eventBridge] task dismissReminder failed", error);
              });
            },
          },
        ],
      },
    );
  }
}

function webcalName(url: string): string {
  try {
    const parsed = new URL(url.replace(/^webcals?:/i, "https:"));
    return parsed.hostname || t("shell.calendar");
  } catch {
    return t("shell.calendar");
  }
}

async function handlePendingOpen(
  item: { kind: "eml" | "ics" | "vcf" | "mailto" | "webcal"; value: string },
): Promise<void> {
  switch (item.kind) {
    case "eml":
      openRoute(`/message/eml?path=${encodeURIComponent(item.value)}`, t("shell.window.message"));
      break;
    case "ics":
      useShellStore.getState().setPendingIcsPath(item.value);
      break;
    case "vcf":
      useShellStore.getState().setPendingVcfPath(item.value);
      break;
    case "mailto": {
      try {
        // fromMailto parses recipients/subject/body into a (possibly unsaved)
        // draft; persist it so the compose window can load it by id.
        const draft = await compose.fromMailto(item.value);
        const saved = await compose.save(draft);
        await openComposeDraft(saved);
      } catch (error) {
        toast.error(
          t("shell.window.compose"),
          t("shell.composeFailed", {
            message: error instanceof Error ? error.message : String(error),
          }),
        );
      }
      break;
    }
    case "webcal": {
      const name = webcalName(item.value);
      try {
        await calendars.subscribe(item.value, name, "#0f6cbd");
        toast.success(t("shell.webcal.subscribed", { name }));
      } catch (error) {
        toast.error(
          t("shell.window.message"),
          t("shell.webcal.failed", {
            message: error instanceof Error ? error.message : String(error),
          }),
        );
      }
      break;
    }
    default:
      break;
  }
}

/** Consume files/URLs the backend is holding for the frontend. */
export async function processPendingOpen(): Promise<void> {
  if (!isTauri()) return;
  let pending: Array<{ kind: "eml" | "ics" | "vcf" | "mailto" | "webcal"; value: string }>;
  try {
    pending = await app.takePendingOpen();
  } catch (error) {
    console.warn("[eventBridge] app_take_pending_open failed", error);
    return;
  }
  for (const item of pending) {
    try {
      await handlePendingOpen(item);
    } catch (error) {
      toast.error(
        t("common.error"),
        t("shell.openFileFailed", {
          message: error instanceof Error ? error.message : String(error),
        }),
      );
    }
  }
}

interface EventSubscriptions {
  subscribe: <K extends keyof EventPayloads>(
    name: K,
    handler: (payload: EventPayloads[K]) => void,
  ) => () => void;
  dispose: () => void;
}

/**
 * Group listeners so a window can tear every one of them down again. Listeners
 * that resolve after disposal are unregistered immediately.
 */
function createSubscriptions(): EventSubscriptions {
  const unlisteners: UnlistenFn[] = [];
  const pending: Array<Promise<unknown>> = [];
  let disposed = false;

  const subscribe = <K extends keyof EventPayloads>(
    name: K,
    handler: (payload: EventPayloads[K]) => void,
  ) => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    const promise = on(name, handler)
      .then((fn) => {
        if (disposed || cancelled) {
          fn();
        } else {
          unlisten = fn;
          unlisteners.push(fn);
        }
      })
      .catch((error: unknown) => {
        console.warn(`[eventBridge] failed to listen for ${name}`, error);
      });
    pending.push(promise);
    return () => {
      cancelled = true;
      if (unlisten) {
        unlisten();
        const index = unlisteners.indexOf(unlisten);
        if (index >= 0) unlisteners.splice(index, 1);
      }
    };
  };

  const dispose = () => {
    disposed = true;
    for (const unlisten of unlisteners.splice(0)) {
      try {
        unlisten();
      } catch {
        // Ignore unlisten failures while tearing down.
      }
    }
    void Promise.allSettled(pending);
  };

  return { subscribe, dispose };
}

let installed = false;

/**
 * Subscribe to every backend event. Returns an unlisten function.
 * Calling it twice returns a no-op (the Shell is the single owner).
 */
export function installEventBridge(): () => void {
  if (!isTauri()) return () => undefined;
  if (installed) return () => undefined;
  installed = true;

  // The retry path in `pendingWrites` must be able to write a stored payload
  // without the composer that owned it; it lives outside React by design, so it
  // gets the save call injected here rather than importing the IPC module.
  registerComposeSave((draft) => compose.save(draft));

  const { subscribe, dispose } = createSubscriptions();

  subscribe("sync://status", (status) => {
    queryClient.setQueryData<SyncStatus[]>(queryKeys.syncStatus(), (current) => {
      const list = current ?? [];
      const index = list.findIndex(
        (entry) => entry.accountId === status.accountId && entry.module === status.module,
      );
      if (index < 0) return [...list, status];
      const next = [...list];
      next[index] = status;
      return next;
    });
  });

  subscribe("accounts://changed", invalidateAccountData);

  subscribe("settings://changed", (settings: AppSettings) => {
    queryClient.setQueryData(queryKeys.settings(), settings);
  });

  subscribe("mail://changed", (payload) => {
    invalidateMailData();
    // Deleting a mail clears the task that was created from it (the prune
    // happens on the next task read), so the todo lists must refresh too.
    if (payload.kind === "deleted") {
      invalidate(queryKeyPrefixes.tasks);
      invalidate(queryKeyPrefixes.taskLists);
      invalidate(queryKeyPrefixes.smartCounts);
    }
  });

  subscribe("calendar://changed", () => {
    invalidate(queryKeyPrefixes.events);
    invalidate(queryKeyPrefixes.event);
  });

  subscribe("contacts://changed", () => {
    invalidate(queryKeyPrefixes.contacts);
    invalidate(queryKeyPrefixes.contact);
    invalidate(queryKeyPrefixes.groups);
    invalidate(queryKeyPrefixes.addressBooks);
  });

  subscribe("tasks://changed", () => {
    invalidate(queryKeyPrefixes.tasks);
    invalidate(queryKeyPrefixes.task);
    invalidate(queryKeyPrefixes.taskLists);
    invalidate(queryKeyPrefixes.smartCounts);
  });

  subscribe("op://failed", (payload) => {
    toast.error(
      t("shell.opFailed.title"),
      t("shell.opFailed.body", {
        description: payload.description,
        error: payload.error,
      }),
    );
  });

  subscribe("reminder://due", (payload) => {
    rememberReminder(payload);
  });

  subscribe("app://navigate", (payload) => {
    applyNavigate(payload);
  });

  subscribe("app://open-requested", () => {
    void processPendingOpen();
  });

  subscribe("app://compose", (payload) => {
    openComposeDraftWithFeedback(payload.draft);
  });

  // Close-to-tray destroys the webview, so pending edits must reach SQLite
  // first. The backend may still decide not to close; it then tells us so the
  // frozen composer can be unlocked.
  subscribe("app://flush-drafts", (seq) => {
    void flushComposeDrafts(seq);
  });
  subscribe("app://close-cancelled", () => {
    void (async () => {
      const { useComposeStore } = await import("@/modules/mail/compose");
      useComposeStore.getState().cancelCloseHandler?.();
    })();
  });

  // The backend may already hold a pending open from launch.
  void processPendingOpen();

  return () => {
    dispose();
    installed = false;
  };
}

let secondaryInstalled = false;

/**
 * Lightweight bridge for secondary windows (`/message/*`, `/compose/*`).
 *
 * It only keeps the cached queries of that window fresh. Toasts, navigation
 * and pending file opens stay with the main window so they are never shown
 * twice.
 */
export function installSecondaryEventBridge(): () => void {
  if (!isTauri()) return () => undefined;
  if (secondaryInstalled) return () => undefined;
  secondaryInstalled = true;

  const { subscribe, dispose } = createSubscriptions();

  subscribe("mail://changed", invalidateMailData);
  subscribe("settings://changed", (settings: AppSettings) => {
    queryClient.setQueryData(queryKeys.settings(), settings);
  });
  subscribe("accounts://changed", invalidateAccountData);

  return () => {
    dispose();
    secondaryInstalled = false;
  };
}

/** Exposed for the shell's F5 handler. */
export function refreshSyncStatus(): void {
  invalidate(queryKeyPrefixes.syncStatus);
}

/** Human helper used by the sync indicator. */
export function formatSyncTime(iso: string | null): string {
  return iso ? formatListDate(iso) : t("shell.neverSynced");
}
