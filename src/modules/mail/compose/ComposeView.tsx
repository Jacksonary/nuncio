/**
 * ComposeView — the complete message composer (inline pane and secondary
 * window). It owns the draft state, autosave loop, Tiptap editor, ribbon,
 * recipients, attachments and send/discard flows.
 */
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
} from "react";
import { EditorContent, useEditor } from "@tiptap/react";
import type { Editor } from "@tiptap/core";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowMaximize16Regular,
  Attach16Regular,
  Dismiss16Regular,
  Send16Regular,
} from "@fluentui/react-icons";
import {
  Button,
  EmptyState,
  IconButton,
  Spinner,
  confirm,
  toast,
} from "@/components";
import {
  accounts as accountsApi,
  app,
  compose,
  identities as identitiesApi,
  isIpcError,
  signatures as signaturesApi,
} from "@/ipc/api";
import type {
  Account,
  Address,
  Draft,
  DraftAttachment,
  Identity,
  Signature,
} from "@/ipc/types";
import { cn } from "@/lib/cn";
import { forgetUnsaved, markSaved, markUnsaved } from "@/lib/pendingWrites";
import { formatTime } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { queryKeys, queryKeyPrefixes } from "@/lib/queryKeys";
import { useShellStore } from "@/lib/store";
import { useAppSettings } from "@/lib/useSettings";
import { AttachmentsBar } from "./AttachmentsBar";
import { ComposeHeader } from "./ComposeHeader";
import { ComposeRibbon } from "./ComposeRibbon";
import { createComposeExtensions } from "./extensions";
import { LinkDialog } from "./LinkDialog";
import {
  ATTACHMENT_SIZE_LIMIT,
  allRecipients,
  attachmentTotal,
  escapeAttribute,
  escapeHtml,
  finalizeHtml,
  findSignatureForIdentity,
  invalidRecipients,
  mentionsAttachment,
  mergeAttachments,
  moveRecipient,
  plainTextFromHtml,
  prepareEditorHtml,
  recipientCount,
  type RecipientField as RecipientFieldName,
} from "./logic";
import { createBlankDraft, useDraftQuery } from "./queries";
import {
  applySignatureToEditor,
  findSignatureBlocks,
  sanitizeSignatureBlocksHtml,
  signatureBlockHtml,
} from "./signature";
import { useComposeStore } from "./store";
import styles from "./ComposeView.module.css";
import "./i18n";

const AUTOSAVE_DELAY = 3000;

export interface ComposeViewProps {
  /** Draft id for the `#/compose/<id>` route. */
  draftId?: string | null;
  /** Already-loaded draft (inline composer, reply, mailto…). */
  initialDraft?: Draft | null;
  mode: "inline" | "window";
  onClose: () => void;
  onSent?: () => void;
}

type SavePhase = "idle" | "dirty" | "saving" | "saved" | "error";

function errorMessage(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function normalizeDraft(draft: Draft): Draft {
  return {
    ...draft,
    from: draft.from ?? { name: null, email: "" },
    to: draft.to ?? [],
    cc: draft.cc ?? [],
    bcc: draft.bcc ?? [],
    subject: draft.subject ?? "",
    html: draft.html ?? "",
    attachments: draft.attachments ?? [],
    references: draft.references ?? [],
  };
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      if (typeof reader.result === "string") resolve(reader.result);
      else reject(new Error("Unexpected file reader result"));
    };
    reader.onerror = () => reject(reader.error ?? new Error("Could not read file"));
    reader.readAsDataURL(file);
  });
}

function removeInlineImage(editor: Editor | null, contentId: string): void {
  if (!editor) return;
  const positions: number[] = [];
  editor.state.doc.descendants((node, position) => {
    if (node.type.name === "image" && node.attrs.contentId === contentId) {
      positions.push(position);
    }
  });
  if (positions.length === 0) return;
  let transaction = editor.state.tr;
  for (let index = positions.length - 1; index >= 0; index -= 1) {
    transaction = transaction.delete(positions[index], positions[index] + 1);
  }
  editor.view.dispatch(transaction);
}

/**
 * Push a saved draft into the inline store only when it still is the draft
 * that was opened in this composer. Without the guard a late save from a
 * previously opened (unsaved) composer could overwrite a newer one.
 */
function syncStore(expected: Draft, next: Draft): boolean {
  const active = useComposeStore.getState().active;
  if (!active || active !== expected) return false;
  useComposeStore.getState().update(next, expected);
  // Keep the shell's inline-compose selection in sync so the mail module's
  // "pop out" action knows the saved draft id.
  useShellStore
    .getState()
    .openInlineCompose({ draftId: next.id, accountId: next.accountId });
  return true;
}

/* ─────────────────────────── Outer component ─────────────────────── */

type FlowKind = "sending" | "closing";

interface FlowRecord {
  kind: FlowKind;
  token: number;
  settled: Promise<void>;
  settle: () => void;
}

export function ComposeView({
  draftId = null,
  initialDraft = null,
  mode,
  onClose,
  onSent,
}: ComposeViewProps) {
  const t = useT();
  const active = useComposeStore((state) => state.active);
  const revision = useComposeStore((state) => state.revision);
  const draftQuery = useDraftQuery(draftId);
  const [created, setCreated] = useState<Draft | null>(null);
  const [createError, setCreateError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);

  const storeDraft = mode === "inline" ? active : null;
  const provided = initialDraft ?? storeDraft;
  const shouldCreate = !provided && !draftId && mode === "window";

  useEffect(() => {
    if (!shouldCreate) return;
    let cancelled = false;
    setCreateError(null);
    createBlankDraft()
      .then((draft) => {
        if (cancelled) return;
        setCreated(draft);
        useComposeStore.getState().open(draft);
      })
      .catch((error: unknown) => {
        if (!cancelled) setCreateError(error);
      });
    return () => {
      cancelled = true;
    };
  }, [shouldCreate, attempt]);

  if (mode === "inline" && !provided) return null;

  const source = provided ?? draftQuery.data ?? created;
  if (source) {
    const key = initialDraft
      ? `initial:${initialDraft.id ?? initialDraft.updatedAt ?? "draft"}`
      : storeDraft
        ? `inline:${revision}`
        : draftId
          ? `draft:${draftId}`
          : `new:${revision}`;
    return (
      <Composer
        key={key}
        initialDraft={normalizeDraft(source)}
        mode={mode}
        onClose={onClose}
        onSent={onSent}
      />
    );
  }

  const error = createError ?? draftQuery.error;
  if (error) {
    return (
      <div className={styles.stateScreen}>
        <EmptyState
          title={t("compose.error.loadFailed")}
          description={errorMessage(error)}
          actions={
            <>
              <Button
                appearance="primary"
                onClick={() => {
                  if (shouldCreate) setAttempt((value) => value + 1);
                  else void draftQuery.refetch();
                }}
              >
                {t("compose.retry")}
              </Button>
              <Button appearance="subtle" onClick={onClose}>
                {t("compose.close")}
              </Button>
            </>
          }
        />
      </div>
    );
  }

  return (
    <div className={styles.stateScreen}>
      <Spinner size="medium" label={t("compose.loading")} />
    </div>
  );
}

/* ───────────────────────────── The composer ──────────────────────── */

interface ComposerProps {
  initialDraft: Draft;
  mode: "inline" | "window";
  onClose: () => void;
  onSent?: () => void;
}

function Composer({ initialDraft, mode, onClose, onSent }: ComposerProps) {
  const t = useT();
  const queryClient = useQueryClient();
  const settings = useAppSettings();
  const fontFamily = settings.mail.defaultFontFamily;
  const fontSize = settings.mail.defaultFontSize;

  const accountsQuery = useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
  });
  const identitiesQuery = useQuery({
    queryKey: queryKeys.identities(null),
    queryFn: () => identitiesApi.list(null),
  });
  const signaturesQuery = useQuery({
    queryKey: queryKeys.signatures(),
    queryFn: signaturesApi.list,
  });

  const accounts: Account[] = accountsQuery.data ?? [];
  const identities: Identity[] = identitiesQuery.data ?? [];
  const signatures: Signature[] = signaturesQuery.data ?? [];

  const [draft, setDraftState] = useState<Draft>(initialDraft);
  const draftRef = useRef<Draft>(initialDraft);
  const lastSyncedRef = useRef<Draft>(initialDraft);
  const fontRef = useRef({ fontFamily, fontSize });
  fontRef.current = { fontFamily, fontSize };
  const [savePhase, setSavePhase] = useState<SavePhase>("idle");
  const [savedAt, setSavedAt] = useState<Date | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [showCc, setShowCc] = useState(initialDraft.cc.length > 0);
  const [showBcc, setShowBcc] = useState(initialDraft.bcc.length > 0);
  const [spellcheck, setSpellcheck] = useState(true);
  const [revision, setRevision] = useState(0);
  const [linkOpen, setLinkOpen] = useState(false);
  const [dragOver, setDragOver] = useState(false);

  const dirtyRef = useRef(false);
  /** True once the component is gone: React updates and toasts are pointless. */
  const unmountedRef = useRef(false);
  const savingRef = useRef(false);
  const pendingSave = useRef(false);
  /** In-flight save, so a close can wait for it instead of racing it. */
  const inFlightRef = useRef<Promise<Draft | null> | null>(null);
  /** Bumped on every edit, so a save can tell whether it is still current. */
  const revisionRef = useRef(0);
  /** Synchronous guard so a double click cannot enqueue the same message twice. */
  const sendingRef = useRef(false);
  /** True while a close handshake is in flight: the whole composer is frozen. */
  const [closing, setClosing] = useState(false);
  /**
   * The composer runs at most one teardown-relevant flow at a time. `sending`
   * and `closing` both end with the UI disappearing, and letting them overlap is
   * what produced the draft-recreated-after-send and destroyed-while-editing
   * races.
   *
   * The record carries its *own* settle closure rather than a shared ref: a
   * shared ref cannot tell whose promise it holds, so claiming a new flow
   * silently stranded whoever was awaiting the old one — which is how "wait for
   * the running send" turned into "wait for yourself".
   */
  const flowRef = useRef<FlowRecord | null>(null);
  /** Source of flow tokens, so `endFlow` can tell an owner from a usurper. */
  const flowSeqRef = useRef(0);
  /**
   * Claim the composer for one teardown flow.
   *
   * The returned record carries its own `settle`, so the flow that owns it can
   * always complete its promise even after another flow has taken the shared
   * slot. Clearing the slot and completing the promise are separate concerns:
   * only the current owner may clear, but *every* flow must complete its own, or
   * whoever is awaiting it hangs forever.
   */
  const beginFlow = useCallback((kind: FlowKind): FlowRecord => {
    const token = ++flowSeqRef.current;
    let settle: () => void = () => undefined;
    const settled = new Promise<void>((resolve) => {
      settle = resolve;
    });
    const record: FlowRecord = { kind, token, settled, settle };
    flowRef.current = record;
    return record;
  }, []);

  /**
   * Finish a flow: always complete its own promise, release the shared slot only
   * if this flow still owns it.
   *
   * The previous version returned early on a token mismatch and skipped the
   * settle — so a cancelled close left the send it was awaiting blocked forever,
   * which is the same hang as waiting on itself, just relocated.
   */
  const endFlow = useCallback(
    (flow: FlowRecord | null) => {
      if (!flow) return;
      if (flowRef.current?.token === flow.token) flowRef.current = null;
      flow.settle();
    },
    [],
  );
  /** Identifies the live close handshake; a cancelled one must not re-freeze. */
  const closeSeqRef = useRef(0);
  const closeCancelledRef = useRef(false);
  /** Mirrors `closing` for callbacks that must not act during a close. */
  const closingRef = useRef(false);
  /**
   * True while either teardown flow owns the draft.
   *
   * This reads the two *source* refs instead of mirroring the `locked` state
   * variable: `setSending(true)` / `setClosing(true)` only schedule a render and
   * their mirroring effect runs afterwards, so a handler firing in that gap
   * would still see `false` and accept an edit that is about to be discarded.
   * `sendingRef` and `closingRef` are written synchronously at the claim, long
   * before any `await`.
   */
  const isLocked = useCallback(
    () => closingRef.current || sendingRef.current,
    [],
  );
  /** Stable handle for the editor extension, which captures it at creation. */
  const isLockedRef = useRef(isLocked);
  isLockedRef.current = isLocked;
  /** True while a programmatic editor change must not count as an edit. */
  const suppressDirtyRef = useRef(false);
  const sentRef = useRef(false);
  const discardingRef = useRef(false);
  const timerRef = useRef<number | null>(null);
  const editorRef = useRef<Editor | null>(null);
  const imageInputRef = useRef<HTMLInputElement | null>(null);
  const signatureAppliedRef = useRef(false);
  const markDirtyRef = useRef<() => void>(() => undefined);
  const saveNowRef = useRef<() => Promise<Draft | null>>(async () => null);
  const errorMessageRef = useRef<string | null>(null);
  const insertImagesRef = useRef<(files: File[]) => void>(() => undefined);

  const initialHtml = useMemo(
    () =>
      prepareEditorHtml(
        sanitizeSignatureBlocksHtml(initialDraft.html),
        initialDraft.attachments,
      ),
    [initialDraft.html, initialDraft.attachments],
  );

  const extensions = useMemo(
    () =>
      createComposeExtensions({
        placeholder: t("compose.bodyPlaceholder"),
        // Read through a ref: the editor is created once, but the lock toggles
        // with each send/close. The callback is what makes the freeze effective
        // for programmatic commands, not just for typing.
        isLocked: () => isLockedRef.current(),
      }),
    // The editor is created once; changing the placeholder mid-session would
    // require recreating the editor and losing the cursor.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const editor = useEditor({
    extensions,
    content: initialHtml,
    editorProps: {
      attributes: {
        class: styles.prose,
        spellcheck: "true",
        "aria-label": t("compose.bodyPlaceholder"),
      },
      handlePaste: (_view, event) => {
        const files = Array.from(event.clipboardData?.files ?? []).filter((file) =>
          file.type.startsWith("image/"),
        );
        if (files.length === 0) return false;
        insertImagesRef.current(files);
        return true;
      },
    },
    onUpdate: () => {
      markDirtyRef.current();
      setRevision((value) => value + 1);
    },
    onCreate: ({ editor: createdEditor }) => {
      editorRef.current = createdEditor;
      createdEditor.view.dom.spellcheck = true;
    },
  });

  useEffect(() => {
    editorRef.current = editor;
  }, [editor]);

  // Freeze/unfreeze the editor whenever the close handshake toggles. The header
  // and ribbon read the same flag through `composerLocked` below.
  const locked = closing || sending;

  /**
   * Freeze or unfreeze the editor *now*.
   *
   * `setEditable` is what actually stops ProseMirror from accepting a
   * transaction, and until this call the body is still live: `sendingRef` being
   * true only makes our own callbacks no-ops, it does not stop the browser from
   * dispatching input into the contenteditable. Calling it synchronously at the
   * claim closes the gap between "the send payload is committed" and "the
   * editor is frozen" that the effect below can only close one render later.
   */
  const applyEditable = useCallback((frozen: boolean) => {
    const active = editorRef.current;
    if (!active || active.isDestroyed) return;
    // `setEditable` fires an editor update, which would mark the draft dirty
    // again right after we saved it. Silence the dirty marking around it.
    suppressDirtyRef.current = true;
    active.setEditable(!frozen);
    suppressDirtyRef.current = false;
  }, []);

  useEffect(() => {
    applyEditable(locked);
  }, [locked, applyEditable]);

  useEffect(() => {
    useComposeStore.getState().setCancelCloseHandler(() => {
      // Invalidate the in-flight flush and unlock everything it froze.
      closeSeqRef.current += 1;
      closeCancelledRef.current = true;
      closingRef.current = false;
      // Release the flow the flush registered. Without this, a close that was
      // cancelled *before* its flush reached `finally` leaves `flowRef` pointing
      // at a "closing" record, and every later send is refused for the rest of
      // the composer's life. `endFlow` is token-checked, so a send that took
      // over is untouched.
      const current = flowRef.current;
      if (current?.kind === "closing") endFlow(current);
      // Unfreeze the body here too, not just through the `locked` effect: a
      // cancel can arrive while a send still owns the composer, and waiting for
      // the render would leave the user unable to type in a window that is
      // already back on screen.
      applyEditable(sendingRef.current);
      setClosing(false);
    });
    return () => useComposeStore.getState().setCancelCloseHandler(null);
  }, [applyEditable, endFlow]);

  useEffect(() => {
    if (editor) editor.view.dom.spellcheck = spellcheck;
  }, [editor, spellcheck]);

  const buildDraft = useCallback((): Draft => {
    const current = draftRef.current;
    let html = current.html;
    const activeEditor = editorRef.current;
    if (activeEditor && !activeEditor.isDestroyed) {
      html = finalizeHtml(activeEditor.getHTML(), {
        fontFamily,
        fontSize,
      });
    }
    return { ...current, html };
  }, [fontFamily, fontSize]);

  const markDirty = useCallback(() => {
    if (sentRef.current || discardingRef.current) return;
    // Programmatic editor updates (freezing, restoring) are not user edits.
    if (suppressDirtyRef.current) return;
    revisionRef.current += 1;
    dirtyRef.current = true;
    setSavePhase("dirty");
    setSaveError(null);
    if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => {
      timerRef.current = null;
      void saveNowRef.current();
    }, AUTOSAVE_DELAY);
  }, []);
  markDirtyRef.current = markDirty;

  const clearAutosave = () => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };

  const saveNow = useCallback(async (): Promise<Draft | null> => {
    if (sentRef.current || discardingRef.current) return null;
    // A send owns the draft from the moment it is claimed: it converts the draft
    // into an outbox row and deletes the local copy. A save started in that
    // window lands afterwards and recreates the draft — locally and, via the
    // Drafts APPEND, on the server. Autosave, retry-save and the unmount save
    // all go through here, so this single check covers every entry.
    if (sendingRef.current) return null;
    if (savingRef.current) {
      // Join the running save rather than reporting a bogus success.
      return inFlightRef.current ?? null;
    }
    clearAutosave();
    savingRef.current = true;
    setSavePhase("saving");
    const snapshotRevision = revisionRef.current;
    const toSave = buildDraft();
    // Recorded synchronously, before the write starts: the close-to-tray
    // handshake reads this snapshot instead of asking a component that may
    // already be unmounted.
    markUnsaved(toSave);
    const run = (async (): Promise<Draft | null> => {
    try {
      const saved = await compose.save(toSave);
      // No-op if the user typed again while this write ran.
      markSaved(toSave);
      const merged: Draft = {
        ...draftRef.current,
        id: saved.id,
        updatedAt: saved.updatedAt,
        remoteDraftMessageId: saved.remoteDraftMessageId,
      };
      draftRef.current = merged;
      setDraftState(merged);
      if (syncStore(lastSyncedRef.current, merged)) {
        lastSyncedRef.current = merged;
      }
      // Only clear the dirty flag if nothing was typed while this save ran;
      // otherwise the editor would look saved while the newest text is not.
      if (revisionRef.current === snapshotRevision) {
        dirtyRef.current = false;
      }
      setSavedAt(new Date());
      setSavePhase("saved");
      setSaveError(null);
      if (mode === "window" && saved.id) {
        try {
          const route = `#/compose/${saved.id}`;
          if (window.location.hash !== route) {
            window.history.replaceState(null, "", route);
          }
        } catch {
          // Non-fatal: the draft is still saved.
        }
      }
      return merged;
    } catch (error) {
      console.error("[compose] autosave failed", error);
      setSavePhase("error");
      setSaveError(errorMessage(error));
      errorMessageRef.current = errorMessage(error);
      return null;
    } finally {
      savingRef.current = false;
      inFlightRef.current = null;
      if (pendingSave.current && !sentRef.current && !discardingRef.current) {
        pendingSave.current = false;
        markDirtyRef.current();
      }
    }
    })();
    inFlightRef.current = run;
    return run;
  }, [buildDraft, mode]);
  saveNowRef.current = saveNow;

  // Expose "flush" so the shell can save before the webview is destroyed
  // (close-to-tray), where React unmount never runs. It reports whether the
  // newest content actually reached storage: the caller must not acknowledge a
  // close otherwise, or the window is destroyed with unsaved text.
  const flushDrafts = useCallback(async (): Promise<boolean> => {
    // Capture the send that is already running *before* claiming anything. A
    // running send owns the draft — `compose.send` converts it into an outbox
    // row and deletes the local copy, so a save started meanwhile would recreate
    // it. Read the promise into a local: after the claim below, a new flow
    // record exists and reading the ref again would await this very flush.
    const runningSend = flowRef.current?.kind === "sending"
      ? flowRef.current.settled
      : null;
    // Claim the close before the first await. Doing it afterwards leaves a
    // window where the close is cancelled (or the backend gives up) while this
    // flush is still waiting on the send; when it resumes it would claim a fresh
    // token and freeze a composer nobody is going to unfreeze.
    const closeToken = ++closeSeqRef.current;
    // A fresh attempt starts clean: the previous close may have been cancelled,
    // and that must not block this one until the composer remounts.
    closeCancelledRef.current = false;
    const flow = beginFlow("closing");
    const stillWanted = () => closeSeqRef.current === closeToken;
    try {
      if (runningSend) {
        await runningSend.catch(() => undefined);
      }
      // Re-check after every await: the close may have been abandoned while a
      // send was finishing, and this flush must then neither save nor freeze.
      if (!stillWanted() || closeCancelledRef.current) return false;
      if (sentRef.current) return true;
      for (let attempt = 0; attempt < 5 && dirtyRef.current && !sentRef.current; attempt += 1) {
        const saved = await saveNowRef.current();
        if (!saved) return false;
        if (!stillWanted() || closeCancelledRef.current) return false;
      }
      if (dirtyRef.current && !sentRef.current) return false;
      if (!stillWanted() || closeCancelledRef.current) return false;
      // Freeze the whole composer before acknowledging; the backend destroys the
      // webview as soon as the ack arrives, so nothing may be editable in
      // between. The lock is lifted only by an explicit "close cancelled"
      // signal. Set the ref before the state: `setClosing` schedules a render,
      // and the ack below may reach the backend first. The ref is what callbacks
      // check.
      closingRef.current = true;
      applyEditable(true);
      setClosing(true);
      return true;
    } finally {
      // Release only if this close still owns the flow. An abandoned flush must
      // not clear a send that legitimately took over while it was awaiting.
      endFlow(flow);
    }
  }, [applyEditable, beginFlow, endFlow]);

  useEffect(() => {
    useComposeStore.getState().setFlushHandler(flushDrafts);
    return () => {
      if (useComposeStore.getState().flushHandler === flushDrafts) {
        useComposeStore.getState().setFlushHandler(null);
      }
    };
  }, [flushDrafts]);

  const updateDraft = useCallback((patch: Partial<Draft>) => {
    if (isLocked() || sentRef.current || discardingRef.current) return;
    const next = { ...draftRef.current, ...patch };
    draftRef.current = next;
    setDraftState(next);
    markDirtyRef.current();
  }, []);

  const addAttachments = useCallback((added: DraftAttachment[]) => {
    if (added.length === 0) return;
    // A file picker or upload started before the freeze can finish after it; the
    // draft is about to be destroyed, so dropping the result is the safe choice.
    if (isLocked() || sentRef.current || discardingRef.current) return;
    const next = {
      ...draftRef.current,
      attachments: mergeAttachments(draftRef.current.attachments, added),
    };
    draftRef.current = next;
    setDraftState(next);
    markDirtyRef.current();
  }, []);

  const addAttachmentPaths = useCallback(
    async (paths: string[]) => {
      if (paths.length === 0) return;
      try {
        const added = await compose.addAttachments(paths);
        addAttachments(added);
      } catch (error) {
        console.error("[compose] add attachments failed", error);
        toast.error(t("compose.error.attachmentFailed"), errorMessage(error));
      }
    },
    [addAttachments, t],
  );

  const insertImages = useCallback(
    async (files: File[]) => {
      for (const file of files) {
        if (!file.type.startsWith("image/")) continue;
        try {
          const dataUrl = await readFileAsDataUrl(file);
          const separator = dataUrl.indexOf(",");
          const base64 = separator >= 0 ? dataUrl.slice(separator + 1) : "";
          const attachment = await compose.addInlineImage(
            file.name || "image.png",
            file.type || "image/png",
            base64,
          );
          // The upload can finish after a close handshake froze the composer.
          // `addAttachments` refuses on its own, but returning from it does not
          // stop this caller, so the body insert must be skipped explicitly.
          if (isLocked() || sentRef.current || discardingRef.current) continue;
          addAttachments([attachment]);
          const contentId = attachment.contentId ?? attachment.id;
          editorRef.current
            ?.chain()
            .focus()
            .insertContent({
              type: "image",
              attrs: { src: dataUrl, alt: file.name, contentId },
            })
            .run();
        } catch (error) {
          console.error("[compose] insert image failed", error);
          toast.error(t("compose.insert.imageFailed"), errorMessage(error));
        }
      }
    },
    [addAttachments, t],
  );
  insertImagesRef.current = insertImages;

  const pickFiles = useCallback(async () => {
    try {
      const selection = await openFileDialog({
        multiple: true,
        directory: false,
        title: t("compose.attachmentPickerTitle"),
      });
      if (!selection) return;
      const paths = Array.isArray(selection) ? selection : [selection];
      await addAttachmentPaths(paths);
    } catch (error) {
      console.warn("[compose] file dialog failed", error);
      toast.info(t("compose.dropFilesDesktop"));
    }
  }, [addAttachmentPaths, t]);

  const pickImage = useCallback(() => {
    imageInputRef.current?.click();
  }, []);

  const handleImageInput = useCallback(
    (event: React.ChangeEvent<HTMLInputElement>) => {
      const files = Array.from(event.target.files ?? []);
      event.target.value = "";
      if (files.length > 0) void insertImages(files);
    },
    [insertImages],
  );

  /* Tauri native file drops (paths only; the asset protocol is disabled). */
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        switch (event.payload.type) {
          case "enter":
          case "over":
            setDragOver(true);
            break;
          case "drop":
            setDragOver(false);
            void addAttachmentPaths(event.payload.paths);
            break;
          default:
            setDragOver(false);
        }
      })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch((error: unknown) => {
        console.warn("[compose] drag-drop listener failed", error);
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [addAttachmentPaths]);

  const handleDomDrop = useCallback(
    (event: ReactDragEvent<HTMLDivElement>) => {
      if (isTauri()) return;
      const files = Array.from(event.dataTransfer.files ?? []);
      if (files.length === 0) return;
      event.preventDefault();
      setDragOver(false);
      const images = files.filter((file) => file.type.startsWith("image/"));
      if (images.length > 0) void insertImages(images);
      if (images.length < files.length) toast.info(t("compose.dropFilesDesktop"));
    },
    [insertImages, t],
  );

  const suggestions = useCallback(async (query: string): Promise<Address[]> => {
    const value = query.trim();
    if (!value) return [];
    try {
      return await compose.suggestRecipients(value, 8);
    } catch (error) {
      console.warn("[compose] recipient suggestions failed", error);
      return [];
    }
  }, []);

  const moveRecipientBetween = useCallback(
    (address: Address, from: RecipientFieldName, to: RecipientFieldName) => {
      const current = draftRef.current;
      const next = moveRecipient(
        { to: current.to, cc: current.cc, bcc: current.bcc },
        from,
        to,
        address,
      );
      if (to === "cc") setShowCc(true);
      if (to === "bcc") setShowBcc(true);
      updateDraft(next);
    },
    [updateDraft],
  );

  /* Ribbon "insert signature": put the block at the cursor. */
  const insertSignature = useCallback((signature: Signature) => {
    // A popover menu lives outside the inert subtree, so it can still fire after
    // the composer was frozen for a close handshake.
    if (isLocked() || sentRef.current) return;
    const html = signatureBlockHtml(signature);
    if (!html) return;
    editorRef.current?.chain().focus().insertContent(html).run();
    markDirtyRef.current();
  }, []);

  /**
   * From identity changed: replace the existing `.nuncio-signature` block in
   * place (or insert one above the quote / at the end when missing), using the
   * identity's own signature and falling back to the account default for the
   * new/reply mode.
   */
  const syncSignatureForIdentity = useCallback(
    (identity: Identity | null) => {
      // Reached from the portalled identity menu, which stays interactive after
      // the composer is frozen for a close.
      if (isLocked() || sentRef.current || discardingRef.current) return;
      const currentEditor = editorRef.current;
      if (!currentEditor || currentEditor.isDestroyed) return;
      // Never touch the body while the signature list is still loading or
      // failed: removing the block would lose the user's signature.
      if (signaturesQuery.isLoading || signaturesQuery.isError) return;
      if (signatures.length === 0) return;
      const signature = findSignatureForIdentity(
        identity,
        draftRef.current.accountId,
        signatures,
        Boolean(draftRef.current.replyMode),
      );
      if (applySignatureToEditor(currentEditor, signature)) {
        markDirtyRef.current();
      }
    },
    [signatures, signaturesQuery.isError, signaturesQuery.isLoading],
  );

  /* Insert the default signature once, only when the body is still empty. */
  useEffect(() => {
    if (signatureAppliedRef.current) return;
    if (!editor || signaturesQuery.isLoading || identitiesQuery.isLoading) return;
    // The queries may resolve *after* a close or a send claimed the draft. This
    // effect would then insert into the body of a composer that is about to be
    // destroyed, marking it dirty again after the flush already reported success.
    if (isLocked() || sentRef.current) return;
    signatureAppliedRef.current = true;
    // The backend already inserted a signature into new/reply drafts; never
    // add a second one (an empty signature block still counts as present).
    if (findSignatureBlocks(editor.state.doc).length > 0) return;
    if (!editor.isEmpty) return;
    const identity =
      identities.find((item) => item.id === draftRef.current.identityId) ?? null;
    const signature = findSignatureForIdentity(
      identity,
      draftRef.current.accountId,
      signatures,
      Boolean(draftRef.current.replyMode),
    );
    const html = signature ? signatureBlockHtml(signature) : "";
    if (html) {
      editor.chain().focus("end").insertContent(html).run();
    }
  }, [
    editor,
    identities,
    identitiesQuery.isLoading,
    signatures,
    signaturesQuery.isLoading,
  ]);

  const removeAttachment = useCallback((attachment: DraftAttachment) => {
    // The attachments bar sits outside the inert wrapper; a focused delete
    // button still works after the composer locked.
    if (isLocked() || sentRef.current || discardingRef.current) return;
    const next = {
      ...draftRef.current,
      attachments: draftRef.current.attachments.filter(
        (item) => item.id !== attachment.id,
      ),
    };
    draftRef.current = next;
    setDraftState(next);
    if (attachment.contentId) {
      removeInlineImage(editorRef.current, attachment.contentId);
    }
    markDirtyRef.current();
  }, []);

  const openLinkDialog = useCallback(() => setLinkOpen(true), []);

  const linkSelection = useMemo(() => {
    void revision;
    const activeEditor = editor;
    if (!activeEditor) return { href: "", text: "", hasSelection: false };
    const { from, to } = activeEditor.state.selection;
    const href = (activeEditor.getAttributes("link").href as string | undefined) ?? "";
    const text =
      from === to ? "" : activeEditor.state.doc.textBetween(from, to, " ");
    return { href, text, hasSelection: from !== to };
  }, [editor, revision]);

  const applyLink = useCallback((url: string, text: string) => {
    // The link dialog is portalled to <body>, so it is outside the inert
    // subtree and can still submit after a close handshake froze the composer.
    if (isLocked() || sentRef.current || discardingRef.current) return;
    const activeEditor = editorRef.current;
    if (!activeEditor) return;
    const { from, to } = activeEditor.state.selection;
    if (from === to) {
      const label = text || url;
      activeEditor
        .chain()
        .focus()
        .insertContent(
          `<a href="${escapeAttribute(url)}">${escapeHtml(label)}</a>`,
        )
        .run();
    } else {
      activeEditor.chain().focus().extendMarkRange("link").setLink({ href: url }).run();
    }
    markDirtyRef.current();
  }, []);

  const removeLink = useCallback(() => {
    if (isLocked() || sentRef.current || discardingRef.current) return;
    editorRef.current?.chain().focus().extendMarkRange("link").unsetLink().run();
    markDirtyRef.current();
  }, []);

  const close = useCallback(async () => {
    // Loop until nothing is dirty: `saveNow` may return the result of a save
    // that started before the newest keystrokes, and closing on that would
    // commit stale text while the visible edit is still unsaved.
    while (dirtyRef.current && !sentRef.current) {
      const saved = await saveNowRef.current();
      if (!saved) {
        toast.error(t("compose.error.saveFailed"), errorMessageRef.current ?? undefined);
        return;
      }
    }
    // Saved, so nothing is outstanding for *this* draft. Passing the snapshot
    // keeps another composer's leftover protection in place — the slot is
    // module-wide.
    forgetUnsaved(draftRef.current);
    // The draft now belongs to the composer window that just opened. Keeping this
    // window's snapshot would let a later close-to-tray flush write stale text
    // over what the user is typing there — the same row, two writers.
    forgetUnsaved(draftRef.current);
    useComposeStore.getState().close();
    useShellStore.getState().closeInlineCompose();
    onClose();
  }, [onClose, t]);

  const discard = useCallback(async () => {
    const ok = await confirm({
      title: t("compose.discardTitle"),
      message: t("compose.discardMessage"),
      confirmLabel: t("compose.discard"),
      cancelLabel: t("compose.dialog.cancel"),
      danger: true,
    });
    if (!ok) return;
    // The dialog is modal to the user but not to the app: a close handshake or a
    // send can claim the draft while it is open. Discarding afterwards would
    // delete a draft the send is converting into an outbox row.
    if (isLocked() || sentRef.current || discardingRef.current) return;
    discardingRef.current = true;
    clearAutosave();
    const draftId = draftRef.current.id;
    if (draftId) {
      try {
        await compose.discard(draftId);
      } catch (error) {
        console.error("[compose] discard failed", error);
        discardingRef.current = false;
        toast.error(t("compose.error.discardFailed"), errorMessage(error));
        return;
      }
    }
    // The draft is gone on purpose. Without this the snapshot would survive in
    // the module-level slot, and a close-to-tray flush would write it back —
    // resurrecting a draft the user explicitly threw away.
    forgetUnsaved(draftRef.current);
    useComposeStore.getState().close();
    useShellStore.getState().closeInlineCompose();
    onClose();
  }, [onClose, t]);

  const popOut = useCallback(async () => {
    const saved = await saveNowRef.current();
    if (!saved?.id) {
      toast.error(t("compose.error.popOutNeedsSave"));
      return;
    }
    try {
      await app.openWindow(
        `/compose/${saved.id}`,
        saved.subject.trim() || t("compose.newMessageTitle"),
      );
    } catch (error) {
      console.warn("[compose] pop-out failed", error);
      toast.error(t("compose.popOutFailed"), errorMessage(error));
      return;
    }
    useComposeStore.getState().close();
    useShellStore.getState().closeInlineCompose();
    onClose();
  }, [onClose, t]);

  const send = useCallback(async () => {
    if (sentRef.current) return;
    const current = buildDraft();
    if (recipientCount(current) === 0) {
      toast.error(t("compose.error.noRecipients"));
      return;
    }
    const invalid = invalidRecipients(allRecipients(current));
    if (invalid.length > 0) {
      toast.error(t("compose.error.invalidRecipients", { emails: invalid.join(", ") }));
      return;
    }
    if (!current.subject.trim()) {
      const ok = await confirm({
        title: t("compose.confirm.noSubjectTitle"),
        message: t("compose.confirm.noSubjectMessage"),
        confirmLabel: t("compose.sendAnyway"),
        cancelLabel: t("compose.dialog.cancel"),
      });
      if (!ok) return;
    }
    const bodyText = editorRef.current
      ? editorRef.current.getText()
      : plainTextFromHtml(current.html);
    if (current.attachments.length === 0 && mentionsAttachment(bodyText)) {
      const ok = await confirm({
        title: t("compose.confirm.attachmentTitle"),
        message: t("compose.confirm.attachmentMessage"),
        confirmLabel: t("compose.sendAnyway"),
        cancelLabel: t("compose.dialog.cancel"),
      });
      if (!ok) return;
    }

    // Claim the send synchronously: `await` below yields, and without this a
    // second click (or Enter) would enqueue the same message twice.
    //
    // A close that started first owns the draft: `flushDrafts` registered
    // "closing" before its first await, so sending now would race the save it is
    // performing — and the send deletes the draft that save is writing back.
    if (sentRef.current || sendingRef.current) return;
    if (closingRef.current || flowRef.current?.kind === "closing") return;
    sendingRef.current = true;
    // Announce the send to the shared flow *before* any await, so a close that
    // starts meanwhile waits for this send instead of saving the draft back.
    const flow = beginFlow("sending");
    clearAutosave();
    // Freeze the composer right after the claim, not after the autosave wait:
    // `sendingRef` already makes every handler a no-op, and this makes the same
    // thing true for the DOM (`inert`) and the visible overlay, so the window in
    // which a stray keystroke could be dropped is only a single task.
    setSending(true);
    // Freeze the body synchronously. Everything below awaits, and the payload is
    // built from `buildDraft()` at the call — an input that arrives before the
    // `locked` effect runs would change `draftRef` (or, worse for the body, only
    // the ProseMirror document) and be silently dropped when the send succeeds
    // and the composer closes.
    applyEditable(true);
    // Capture the payload *now*, while the editor still exists and nothing has
    // been awaited. Building it after the autosave wait would read a destroyed
    // editor (switching modules unmounts the composer mid-send) and silently
    // fall back to the last saved HTML, dropping the words typed since.
    const payload = buildDraft();
    // The whole "wait for the old autosave, then persist the payload" chain is
    // registered *now*, synchronously at the claim. Registering only the inner
    // save would be too late: the close handshake snapshots the registry while
    // this chain is still waiting on `inFlightRef`, misses the save that is
    // about to start, and acknowledges a close whose only copy of the text is
    // the payload held in this closure.
    const write = (async () => {
      // Wait for any in-flight autosave before sending: it writes the same row
      // and a save that lands afterwards would recreate the draft the send
      // deleted.
      try {
        await inFlightRef.current;
      } catch {
        // A failed autosave is irrelevant here; `payload` is authoritative.
      }
      // The id must come from the draft as it exists *now*, not from the capture
      // above: an autosave that was running when `payload` was built may have
      // just inserted the row and written its id into `draftRef`. Saving the
      // captured copy would then insert a second draft — and the send only
      // deletes the one whose id it was handed, leaving the other behind.
      const currentId = draftRef.current.id ?? payload.id;
      return compose.save({ ...payload, id: currentId });
    })();
    // The payload is on screen but not yet on disk; the close handshake may be
    // asked to run before this settles.
    markUnsaved(payload);
    const persisted = write;
    try {
      // Persist the exact payload *before* handing it to the outbox. Once this
      // succeeds the draft on disk is the message being sent, so a failure at any
      // later point needs no recovery: the user's text is already stored, and the
      // composer may be unmounted by then without anything being lost.
      const staged = await persisted;
      // The payload is on disk now, so nothing is outstanding — clear the slot
      // before the send. Leaving the captured copy there would make a later
      // close-time flush write it again with its original `id: null` and create
      // a second draft alongside the row this save just produced.
      markSaved(payload);
      const outgoing: Draft = { ...payload, id: staged.id };
      draftRef.current = {
        ...draftRef.current,
        id: staged.id,
        updatedAt: staged.updatedAt,
        remoteDraftMessageId: staged.remoteDraftMessageId,
      };
      dirtyRef.current = false;
      await compose.send(outgoing);
      // Only after the backend accepted the message: before that a concurrent
      // close must still be allowed to save the draft.
      sentRef.current = true;
      toast.info(t("compose.sending"));
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.messages });
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.unifiedCounts });
      void queryClient.invalidateQueries({ queryKey: queryKeyPrefixes.folders });
      // The outbox owns the message now; there is no draft left to protect.
      forgetUnsaved(payload);
      endFlow(flow);
      useComposeStore.getState().close();
      useShellStore.getState().closeInlineCompose();
      onSent?.();
      onClose();
    } catch (error) {
      console.error("[compose] send failed", error);
      sentRef.current = false;
      sendingRef.current = false;
      endFlow(flow);
      // Nothing to restore: the payload was persisted before the send, so the
      // draft on disk already holds the newest text even if the composer is gone.
      // Only the UI state needs undoing, and only if the component still exists.
      if (!unmountedRef.current) {
        setSending(false);
        applyEditable(false);
        toast.error(t("compose.sendFailed"), errorMessage(error));
      }
    }
  }, [applyEditable, beginFlow, buildDraft, endFlow, onClose, onSent, queryClient, t]);
  const sendRef = useRef(send);
  sendRef.current = send;

  /* Window title mirrors the subject. */
  useEffect(() => {
    if (mode !== "window") return;
    const title = draft.subject.trim() || t("compose.newMessageTitle");
    document.title = `${title} — ${t("shell.appName")}`;
    if (isTauri()) {
      getCurrentWindow()
        .setTitle(`${title} — Nuncio`)
        .catch((error: unknown) => {
          console.warn("[compose] could not set window title", error);
        });
    }
  }, [draft.subject, mode, t]);

  /* Ctrl/Cmd+Enter sends; Ctrl/Cmd+K opens the link dialog. */
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey)) return;
      if (event.key === "Enter") {
        event.preventDefault();
        event.stopPropagation();
        void sendRef.current();
      } else if (event.key.toLowerCase() === "k") {
        event.preventDefault();
        event.stopPropagation();
        setLinkOpen(true);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  /* Save pending changes before a secondary compose window is closed. */
  useEffect(() => {
    if (mode !== "window" || !isTauri()) return;
    let disposed = false;
    let allowClose = false;
    let unlisten: (() => void) | undefined;
    getCurrentWindow()
      .onCloseRequested(async (event) => {
        if (allowClose || sentRef.current || discardingRef.current) return;
        if (!dirtyRef.current) return;
        event.preventDefault();
        // Same rule as the in-app close: keep saving until nothing is dirty, so
        // an older in-flight save cannot be mistaken for the newest content.
        while (dirtyRef.current && !sentRef.current) {
          const saved = await saveNowRef.current();
          if (!saved) {
            toast.error(t("compose.error.saveFailed"), errorMessageRef.current ?? undefined);
            return;
          }
        }
        allowClose = true;
        try {
          await getCurrentWindow().close();
        } catch (error) {
          console.warn("[compose] window close failed", error);
        }
      })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch((error: unknown) => {
        console.warn("[compose] close-request listener failed", error);
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [mode]);

  /* Flush pending edits when the composer unmounts (best effort). */
  useEffect(
    () => () => {
      unmountedRef.current = true;
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
      if (!dirtyRef.current || sentRef.current || discardingRef.current) return;
      // A send owns the draft: it converts it into an outbox row and deletes the
      // local copy. This unmount path calls `compose.save` directly instead of
      // `saveNow` (that one already refuses), so without this check a page switch
      // during a send would recreate the draft after the send cleaned it up. If
      // the editor fell back here the send still has the payload it built.
      if (sendingRef.current) return;
      const current = draftRef.current;
      const { fontFamily: family, fontSize: size } = fontRef.current;
      let html = current.html;
      const activeEditor = editorRef.current;
      try {
        if (activeEditor && !activeEditor.isDestroyed) {
          html = finalizeHtml(activeEditor.getHTML(), {
            fontFamily: family,
            fontSize: size,
          });
        }
      } catch {
        // Keep the last saved HTML.
      }
      const expected = lastSyncedRef.current;
      const finalDraft = { ...current, html };
      // This is the last chance to get the text to disk, and the component is
      // already gone — so record the snapshot outside React first.
      markUnsaved(finalDraft);
      void compose
        .save(finalDraft)
        .then((saved) => {
          markSaved(finalDraft);
          if (syncStore(expected, saved)) lastSyncedRef.current = saved;
        })
        .catch((error: unknown) => {
          console.warn("[compose] final save failed", error);
        });
    },
    [],
  );

  const saveStatusText = (() => {
    switch (savePhase) {
      case "saving":
        return t("compose.status.saving");
      case "saved":
        return savedAt
          ? t("compose.status.saved", { time: formatTime(savedAt) })
          : t("compose.status.saved", { time: "" });
      case "dirty":
        return t("compose.status.dirty");
      case "error":
        return saveError ?? t("compose.status.error");
      default:
        return dirtyRef.current
          ? t("compose.status.dirty")
          : t("compose.status.unsaved");
    }
  })();

  const attachmentSize = attachmentTotal(draft.attachments);
  const title = draft.subject.trim() || t("compose.newMessage");

  return (
    <div
      className={cn(
        styles.compose,
        mode === "inline" ? styles.inline : styles.windowMode,
        dragOver && styles.dragOver,
      )}
      onDragOver={(event) => {
        if (!isTauri() && event.dataTransfer.types.includes("Files")) {
          event.preventDefault();
          setDragOver(true);
        }
      }}
      onDragLeave={(event) => {
        const next = event.relatedTarget;
        if (next instanceof Node && event.currentTarget.contains(next)) return;
        setDragOver(false);
      }}
      onDrop={handleDomDrop}
    >
      {mode === "inline" ? (
        <div className={styles.srOnly} aria-live="polite">
          {title}
        </div>
      ) : null}

      {locked ? (
        // Non-interactive shield: the webview is about to be destroyed, so no
        // control may accept input between the ack and that teardown.
        <div className={styles.closingShield} aria-hidden="true" />
      ) : null}

      <div className={styles.composeContent} {...(locked ? { inert: "" } : {})}>
      <ComposeHeader
        draft={draft}
        accounts={accounts}
        identities={identities}
        identitiesLoading={identitiesQuery.isLoading}
        showCc={showCc}
        showBcc={showBcc}
        onToggleCc={() => setShowCc((value) => !value)}
        onToggleBcc={() => setShowBcc((value) => !value)}
        onPatch={updateDraft}
        onIdentitySelected={syncSignatureForIdentity}
        onMoveRecipient={moveRecipientBetween}
        suggestions={suggestions}
        autoFocusTo={mode === "window" && draft.to.length === 0}
      />

      <ComposeRibbon
        editor={locked ? null : editor}
        revision={revision}
        defaultFontFamily={fontFamily}
        defaultFontSize={fontSize}
        signatures={signatures}
        signaturesLoading={signaturesQuery.isLoading}
        importance={draft.importance}
        requestReadReceipt={draft.requestReadReceipt}
        spellcheck={spellcheck}
        onImportanceChange={(importance) => updateDraft({ importance })}
        onRequestReadReceiptChange={(value) => updateDraft({ requestReadReceipt: value })}
        onSpellcheckChange={setSpellcheck}
        onPickFiles={() => void pickFiles()}
        onPickImage={pickImage}
        onOpenLink={openLinkDialog}
        onInsertSignature={insertSignature}
      />

      <div className={styles.editorWrap}>
        <EditorContent
          editor={editor}
          className={styles.editorContent}
          style={{ fontFamily, fontSize: `${fontSize}px` }}
        />
        {dragOver ? (
          <div className={styles.dropOverlay}>
            <Attach16Regular />
            <span>{t("compose.attach.dropHint")}</span>
          </div>
        ) : null}
      </div>
      </div>

      {draft.attachments.length > 0 ? (
        <AttachmentsBar
          attachments={draft.attachments}
          onRemove={removeAttachment}
          compact={mode === "inline"}
        />
      ) : null}

      <div className={styles.statusBar}>
        <span
          className={cn(
            styles.saveStatus,
            savePhase === "error" && styles.saveStatusError,
          )}
          role="status"
          aria-live="polite"
          title={saveError ?? undefined}
        >
          {savePhase === "saving" ? <Spinner size="tiny" /> : null}
          {saveStatusText}
        </span>
        {savePhase === "error" ? (
          <Button size="small" appearance="transparent" onClick={() => void saveNow()}>
            {t("compose.status.retry")}
          </Button>
        ) : null}
        {attachmentSize > ATTACHMENT_SIZE_LIMIT ? (
          <span className={styles.statusWarning}>
            {t("compose.attach.tooLarge", {
              size: `${Math.round(attachmentSize / (1024 * 1024))} MB`,
            })}
          </span>
        ) : null}
        <span className={styles.spacer} />
        {mode === "inline" ? (
          <>
            <IconButton
              icon={<ArrowMaximize16Regular />}
              label={t("compose.popOut")}
              size="small"
              onClick={() => void popOut()}
            />
            <IconButton
              icon={<Dismiss16Regular />}
              label={t("compose.close")}
              size="small"
              onClick={() => void close()}
            />
          </>
        ) : null}
        <IconButton
          icon={<Attach16Regular />}
          label={t("compose.attach.add")}
          size="small"
          onClick={() => void pickFiles()}
        />
        <Button
          size="small"
          appearance="transparent"
          onClick={() => void discard()}
          disabled={sending}
        >
          {t("compose.discard")}
        </Button>
        <Button
          appearance="primary"
          size="small"
          icon={<Send16Regular />}
          loading={sending}
          onClick={() => void send()}
        >
          {t("compose.send")}
        </Button>
      </div>

      <input
        ref={imageInputRef}
        type="file"
        accept="image/*"
        multiple
        hidden
        onChange={handleImageInput}
      />

      <LinkDialog
        open={linkOpen}
        initialUrl={linkSelection.href}
        initialText={linkSelection.text}
        hasSelection={linkSelection.hasSelection}
        onClose={() => setLinkOpen(false)}
        onSubmit={applyLink}
        onRemove={removeLink}
      />
    </div>
  );
}
