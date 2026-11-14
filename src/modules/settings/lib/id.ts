/**
 * Local id generation for entities the frontend creates before the backend
 * confirms them (identities, signatures). The backend accepts these ids, and
 * falls back to generating its own when an empty id is supplied.
 */
export function newId(): string {
  try {
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
  } catch {
    // crypto.randomUUID can be missing in insecure contexts; fall through.
  }
  const random = Math.random().toString(36).slice(2, 10);
  return `local-${Date.now().toString(36)}-${random}`;
}
