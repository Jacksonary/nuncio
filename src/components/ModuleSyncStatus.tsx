/**
 * Per-module sync indicator.
 *
 * Sync health is a property of one module, not of the app: a 429 from a to-do
 * provider says nothing about mail. The shell owns device connectivity
 * (`navigator.onLine`); this component answers only "is THIS module healthy?".
 *
 * The headline is always a semantic i18n string. Backend messages are raw and
 * often English, so they stay one click away in the expandable detail instead
 * of being shouted at a localized UI.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Copy16Regular } from "@fluentui/react-icons";
import { sync as syncApi } from "@/ipc/api";
import type { SyncModule, SyncState, SyncStatus } from "@/ipc/types";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { queryKeys } from "@/lib/queryKeys";
import { Popover } from "./Popover";
import styles from "./ModuleSyncStatus.module.css";

export interface SyncPresentation {
  kind: "idle" | "syncing" | "retrying" | "error" | "needsReauth";
  /** i18n key, already under the "sync." namespace. */
  labelKey: string | null;
  /** Raw backend message for the expandable section. */
  detail: string | null;
}

type AttentionKind = "needsReauth" | "error" | "retrying";

const ATTENTION_KEYS: Record<AttentionKind, string> = {
  needsReauth: "sync.needsReauth",
  error: "sync.error",
  retrying: "sync.retrying",
};

function firstMessage(entries: SyncStatus[]): string | null {
  for (const entry of entries) {
    const message = entry.message?.trim();
    if (message) return message;
  }
  return null;
}

function attention(kind: AttentionKind, entries: SyncStatus[]): SyncPresentation {
  return {
    kind,
    labelKey: ATTENTION_KEYS[kind],
    detail: firstMessage(entries),
  };
}

/**
 * Aggregate every account's status for ONE module, optionally narrowed to the
 * accounts the current view actually covers.
 *
 * Precedence puts the states that need attention above routine syncing so a
 * transient sync on one account cannot hide a broken one on another.
 *
 * `syncing` is deliberately NOT rendered: the module's own refresh button
 * already spins during a sync, and a second spinner plus a "syncing…" caption
 * beside it says the same thing twice.
 */
export function presentationFor(
  statuses: SyncStatus[],
  module: SyncModule,
  accountIds?: readonly string[],
): SyncPresentation {
  const scope = accountIds === undefined ? null : new Set(accountIds);
  const forModule = statuses.filter(
    (status) => status.module === module && (scope === null || scope.has(status.accountId)),
  );
  const inState = (state: SyncState) => forModule.filter((status) => status.state === state);

  for (const kind of ["needsReauth", "error", "retrying"] as const) {
    const matching = inState(kind);
    if (matching.length > 0) return attention(kind, matching);
  }

  // Read the state rather than rendering it: the refresh button shows the
  // in-flight sync, so this only needs to suppress the attention states while
  // a sync is running.
  if (inState("syncing").length > 0) {
    return { kind: "syncing", labelKey: null, detail: null };
  }

  // No entry at all means the module has never synced. That is not a problem
  // worth a badge, so it stays silent.
  return { kind: "idle", labelKey: null, detail: null };
}

/**
 * Raw entries for one module. Account scoping happens in `presentationFor`
 * (which owns the rules) rather than here, so this hook keeps one cache read
 * and one memo regardless of how many accounts a view covers.
 */
function useModuleStatuses(module: SyncModule): SyncStatus[] {
  const query = useQuery({
    queryKey: queryKeys.syncStatus(),
    queryFn: syncApi.status,
    staleTime: 15_000,
  });
  const data = query.data;
  return useMemo(
    () => (data ?? []).filter((status) => status.module === module),
    [data, module],
  );
}

function SyncDetail({ detail }: { detail: string | null }) {
  const t = useT();
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );

  const copy = async () => {
    if (!detail) return;
    try {
      await navigator.clipboard.writeText(detail);
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard access can be denied; the text remains selectable.
    }
  };

  return (
    <div className={styles.detail}>
      <div className={styles.detailHeader}>
        <span className={styles.detailTitle}>{t("sync.detailTitle")}</span>
        {detail ? (
          <button type="button" className={styles.copy} onClick={() => void copy()}>
            <Copy16Regular aria-hidden="true" />
            <span>{copied ? t("sync.copied") : t("sync.copyDetail")}</span>
          </button>
        ) : null}
      </div>
      {detail ? (
        <pre className={styles.detailText}>{detail}</pre>
      ) : (
        <p className={styles.noDetail}>{t("sync.noDetail")}</p>
      )}
    </div>
  );
}

export interface ModuleSyncStatusProps {
  module: SyncModule;
  /**
   * Accounts the current view covers. Omit for a module-wide answer (calendar,
   * contacts, tasks); mail passes its view's accounts so one account's failure
   * does not surface while looking at another.
   */
  accountIds?: readonly string[];
  className?: string;
}

/** Compact indicator for a module header: dot + label, expandable raw detail. */
export function ModuleSyncStatus({ module, accountIds, className }: ModuleSyncStatusProps) {
  const t = useT();
  const statuses = useModuleStatuses(module);
  const presentation = presentationFor(statuses, module, accountIds);
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);

  // Nothing to say while idle, and the refresh button already spins during a
  // sync — rendering a second spinner and a caption beside it is noise.
  if (presentation.kind === "idle" || presentation.kind === "syncing") return null;

  const tone = presentation.kind === "retrying" ? styles.warning : styles.danger;
  const label = presentation.labelKey ? t(presentation.labelKey) : "";

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className={cn(styles.status, styles.interactive, tone, className)}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span className={styles.dot} aria-hidden="true" />
        <span className={styles.label}>{label}</span>
      </button>
      <Popover
        open={open}
        onClose={() => setOpen(false)}
        anchor={triggerRef.current}
        placement="bottom-end"
        role="dialog"
        ariaLabel={t("sync.detailTitle")}
        className={styles.popover}
      >
        <SyncDetail detail={presentation.detail} />
      </Popover>
    </>
  );
}

export interface ModuleSyncDotProps {
  module: SyncModule;
}

/** Tiny status dot for the module rail icon: renders null when healthy. */
export function ModuleSyncDot({ module }: ModuleSyncDotProps) {
  const t = useT();
  const statuses = useModuleStatuses(module);
  const presentation = presentationFor(statuses, module);

  if (presentation.kind === "idle" || presentation.kind === "syncing") return null;

  const label = presentation.labelKey ? t(presentation.labelKey) : "";
  return (
    <span
      className={cn(
        styles.railDot,
        presentation.kind === "retrying" ? styles.railDotWarning : styles.railDotDanger,
      )}
      role="status"
      aria-label={label}
    />
  );
}
