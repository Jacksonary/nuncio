/**
 * Secondary windows: the standalone message reader (`#/message/:id`,
 * `#/message/eml?path=…`) and the compose window (`#/compose/:id`,
 * `#/compose/new?accountId=…`).
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Warning16Regular } from "@fluentui/react-icons";
import { Button, EmptyState, Spinner } from "@/components";
import type { ComposeWindowProps, MessageWindowProps } from "@/app/moduleTypes";
import { compose, isIpcError } from "@/ipc/api";
import type { Draft } from "@/ipc/types";
import { useT } from "@/lib/i18n";
import { useShellStore } from "@/lib/store";
import { ComposeView, InlineComposePane } from "./composeIntegration";
import { CreateTaskDialog } from "./CreateTaskDialog";
import { ReadingPane } from "./ReadingPane";
import { useMailTaskDialog } from "./taskDialog";
import styles from "./mail.module.css";

function errorText(error: unknown): string {
  if (isIpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function closeCurrentWindow(): void {
  if (isTauri()) {
    getCurrentWindow()
      .close()
      .catch((error: unknown) => {
        console.warn("[mail] close window failed", error);
      });
    return;
  }
  if (typeof window !== "undefined") window.close();
}

function readPrintFlag(): boolean {
  if (typeof window === "undefined") return false;
  const queryIndex = window.location.hash.indexOf("?");
  if (queryIndex < 0) return false;
  const params = new URLSearchParams(window.location.hash.slice(queryIndex + 1));
  return params.get("print") === "1";
}

export function MessageWindow({ messageId, emlPath }: MessageWindowProps) {
  const tt = useT();
  const print = useMemo(readPrintFlag, []);
  const inlineCompose = useShellStore((state) => state.inlineCompose);
  const [removed, setRemoved] = useState(false);

  if (inlineCompose) {
    return (
      <div className={styles.windowRoot}>
        <InlineComposePane />
      </div>
    );
  }

  if (removed) {
    return (
      <div className={styles.windowRoot}>
        <EmptyState
          icon={<Warning16Regular />}
          title={tt("mail.window.removedTitle")}
          description={tt("mail.window.removedHint")}
          actions={
            <Button size="small" appearance="primary" onClick={closeCurrentWindow}>
              {tt("common.close")}
            </Button>
          }
        />
      </div>
    );
  }

  return (
    <div className={styles.windowRoot}>
      <ReadingPane
        messageId={messageId}
        emlPath={emlPath}
        standalone
        print={print}
        onRemoved={() => setRemoved(true)}
      />
      <MessageWindowTaskDialog />
    </div>
  );
}

/// The standalone message window has its own module tree, so the shared dialog
/// has to be mounted here as well; otherwise "Create task" opens nothing.
function MessageWindowTaskDialog() {
  const source = useMailTaskDialog((state) => state.source);
  const close = useMailTaskDialog((state) => state.close);
  return <CreateTaskDialog open={source !== null} source={source} onClose={close} />;
}

export function ComposeWindow({ draftId, accountId }: ComposeWindowProps) {
  const tt = useT();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const startedRef = useRef(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = draftId
        ? await compose.getDraft(draftId)
        : await compose.create(accountId ?? null);
      setDraft(next);
    } catch (loadError) {
      console.error("[mail] could not open draft", loadError);
      setError(errorText(loadError));
    } finally {
      setLoading(false);
    }
  }, [accountId, draftId]);

  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    void load();
  }, [load]);

  if (loading) {
    return (
      <div className={styles.windowRoot}>
        <div className={styles.readerLoading}>
          <Spinner size="medium" />
        </div>
      </div>
    );
  }

  if (error || !draft) {
    return (
      <div className={styles.windowRoot}>
        <EmptyState
          icon={<Warning16Regular />}
          title={tt("mail.window.composeError")}
          description={error ?? tt("mail.window.noDraft")}
          actions={
            <Button
              size="small"
              appearance="primary"
              onClick={() => {
                startedRef.current = false;
                void load();
              }}
            >
              {tt("common.retry")}
            </Button>
          }
        />
      </div>
    );
  }

  return (
    <div className={styles.windowRoot}>
      <ComposeView
        draftId={draft.id ?? draftId}
        initialDraft={draft}
        mode="window"
        onClose={closeCurrentWindow}
        onSent={closeCurrentWindow}
      />
    </div>
  );
}
