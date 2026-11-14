/**
 * Account helpers shared by the shell and (optionally) settings sections.
 *
 * Core always seeds a built-in account with id/provider `"local"` that owns the
 * local calendar, address book and task list (see ARCHITECTURE.md §4.1), so
 * `account_list` is never empty on a fresh install. "Configured account"
 * therefore means an account other than that built-in one.
 */
import type { Account } from "@/ipc/types";

/** True for the built-in local-only account (no server, no credentials). */
export function isLocalAccount(account: Account): boolean {
  return account.provider === "local";
}

/**
 * True when at least one user-configured (mail / DAV) account exists.
 * Used by first-run onboarding and by "no accounts yet" empty states.
 */
export function hasConnectedAccount(
  accounts: readonly Account[] | null | undefined,
): boolean {
  return accounts?.some((account) => !isLocalAccount(account)) ?? false;
}

/**
 * First-run onboarding is due until the user configures a real account or
 * explicitly chooses to use only the local calendar/contacts/to-dos.
 * Callers must have loaded the account list before relying on the result.
 */
export function shouldShowFirstRun(
  accounts: readonly Account[] | null | undefined,
  skipped: boolean,
): boolean {
  return !skipped && !hasConnectedAccount(accounts);
}
