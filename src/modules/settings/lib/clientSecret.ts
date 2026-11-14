/**
 * Google OAuth client-secret masking semantics.
 *
 * The secret lives only in the OS keyring. `settings_get` never returns it:
 * a stored secret is reported as the mask below and a missing one as `null`.
 * When writing, the mask means "unchanged", an empty string clears it and any
 * other value replaces it.
 */
export const CLIENT_SECRET_MASK = "********";

/** True when the backend reported that a secret is stored. */
export function hasStoredClientSecret(value: string | null | undefined): boolean {
  return value === CLIENT_SECRET_MASK;
}

/**
 * Value to put into a settings patch for the secret input.
 *
 * An empty draft means the user did not type anything, so the field must be
 * omitted from the patch (leaving the stored value untouched). The mask is
 * never sent back: an unchanged secret does not need a round trip.
 */
export function clientSecretPatch(draft: string): string | undefined {
  const value = draft.trim();
  if (value.length === 0 || value === CLIENT_SECRET_MASK) return undefined;
  return value;
}
