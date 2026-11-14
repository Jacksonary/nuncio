/**
 * Helpers for the virtual Outbox folder.
 *
 * Queued/failed messages are projected by the backend as `MessageSummary`
 * values whose id is `outbox:<outboxId>` (they are not real `message` rows).
 * Commands that operate on the queue itself (`outbox_retry`, `outbox_cancel`)
 * take the bare `<outboxId>`, so the prefix has to be stripped before calling
 * them. The projection also carries the queue state in `preview` (see
 * `nuncio-mail`'s outbox service), which these helpers parse for the list.
 */

export const OUTBOX_MESSAGE_PREFIX = "outbox:";

export type OutboxDisplayState = "pending" | "sending" | "failed" | "sent";

/** True when `id` is a synthetic outbox message id (`outbox:<id>`). */
export function isOutboxMessageId(id: string | null | undefined): boolean {
  return (
    typeof id === "string" &&
    id.startsWith(OUTBOX_MESSAGE_PREFIX) &&
    id.length > OUTBOX_MESSAGE_PREFIX.length
  );
}

/** Bare outbox row id for IPC calls; null for normal message ids. */
export function outboxItemId(messageId: string): string | null {
  return isOutboxMessageId(messageId)
    ? messageId.slice(OUTBOX_MESSAGE_PREFIX.length)
    : null;
}

export interface OutboxStatus {
  state: OutboxDisplayState;
  /** Delivery error reported by the backend, when the item failed. */
  error: string | null;
}

/**
 * Parse the queue state from the projected preview. The backend prefixes the
 * line with a stable English marker ("Failed: …", "Sending to …", …); anything
 * unrecognised is treated as pending so the row still shows a sensible state.
 */
export function parseOutboxStatus(preview: string | null | undefined): OutboxStatus {
  const text = (preview ?? "").trim();
  if (text === "") return { state: "pending", error: null };

  const failed = /^Failed:\s*(.*)$/i.exec(text);
  if (failed) {
    const error = (failed[1] ?? "").trim();
    return { state: "failed", error: error === "" ? null : error };
  }
  if (/^Sending to\b/i.test(text)) return { state: "sending", error: null };
  if (/^Sent to\b/i.test(text)) return { state: "sent", error: null };
  return { state: "pending", error: null };
}

/** i18n key for an outbox state label. */
export function outboxStatusKey(state: OutboxDisplayState): string {
  return `mail.outbox.state.${state}`;
}
