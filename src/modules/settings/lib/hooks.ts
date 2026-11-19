/**
 * TanStack Query hooks for every backend command the settings UI needs.
 * All writes invalidate the exact query-key prefixes owned by F-shell.
 */
import { useMemo } from "react";
import {
  useMutation,
  useQueries,
  useQuery,
  useQueryClient,
  type UseMutationResult,
} from "@tanstack/react-query";
import { accounts, app, folders, identities, rules, signatures, sync } from "@/ipc/api";
import type {
  Account,
  AccountPatch,
  Folder,
  Identity,
  MailRule,
  Signature,
  StorageUsage,
} from "@/ipc/types";
import {
  accountScopedPrefixes,
  invalidateAccountScopedData,
  queryKeys,
  queryKeyPrefixes,
} from "@/lib/queryKeys";

const APP_INFO_KEY = ["appInfo"] as const;
const STORAGE_USAGE_KEY = ["storageUsage"] as const;

export function useAccounts() {
  return useQuery({ queryKey: queryKeys.accounts(), queryFn: accounts.list });
}

export function useAppInfo() {
  return useQuery({
    queryKey: APP_INFO_KEY,
    queryFn: app.info,
    staleTime: 5 * 60_000,
  });
}

export function useStorageUsage(enabled = true) {
  return useQuery({
    queryKey: STORAGE_USAGE_KEY,
    queryFn: () => app.storageUsage(),
    enabled,
    staleTime: 30_000,
  });
}

function useAccountInvalidation() {
  const client = useQueryClient();
  return () => {
    void client.invalidateQueries({ queryKey: queryKeyPrefixes.accounts });
    void client.invalidateQueries({ queryKey: queryKeyPrefixes.syncStatus });
  };
}

export function useCreateAccount(): UseMutationResult<
  Account,
  unknown,
  Parameters<typeof accounts.create>[0]
> {
  const invalidate = useAccountInvalidation();
  return useMutation({
    mutationFn: (input: Parameters<typeof accounts.create>[0]) => accounts.create(input),
    onSuccess: invalidate,
  });
}

export interface UpdateAccountVariables {
  id: string;
  patch: AccountPatch;
}

/**
 * Recompute the OS unread badge. There is no dedicated command: the badge is
 * derived from `include_in_unified` inside the mail sync's notification step
 * (`sync::notify::update_badge`), so ask for a sync of the affected account.
 * Failures are cosmetic — the next scheduled sync corrects the badge.
 */
function refreshUnreadBadge(accountId: string): void {
  sync.now(accountId, "mail").catch((error: unknown) => {
    console.warn("[settings] badge refresh sync failed", error);
  });
}

export function useUpdateAccount(): UseMutationResult<Account, unknown, UpdateAccountVariables> {
  const client = useQueryClient();
  const invalidate = useAccountInvalidation();
  return useMutation({
    mutationFn: ({ id, patch }: UpdateAccountVariables) => accounts.update(id, patch),
    onSuccess: (account, variables) => {
      client.setQueryData<Account[]>(queryKeys.accounts(), (list) =>
        list ? list.map((item) => (item.id === account.id ? account : item)) : list,
      );
      // Linked-inbox membership feeds the unified list, every derived count and
      // the OS badge; refreshing only the account row leaves all three stale.
      if (variables.patch.includeInUnified !== undefined) {
        invalidateAccountScopedData(client);
        refreshUnreadBadge(variables.id);
      }
    },
    onSettled: invalidate,
  });
}

/** Prefixes dropped on account delete, exported for the parity test. */
export const deleteAccountPrefixes = accountScopedPrefixes;

export function useDeleteAccount(): UseMutationResult<void, unknown, string> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => accounts.delete(id),
    onSuccess: () => {
      // The shared list covers everything the deleted account owned, including
      // calendar events, contacts and tasks that used to stay on screen.
      invalidateAccountScopedData(client);
    },
  });
}

export function useReorderAccounts(): UseMutationResult<void, unknown, string[]> {
  const client = useQueryClient();
  const invalidate = useAccountInvalidation();
  return useMutation({
    mutationFn: (ids: string[]) => accounts.reorder(ids),
    onMutate: async (ids) => {
      await client.cancelQueries({ queryKey: queryKeys.accounts() });
      const previous = client.getQueryData<Account[]>(queryKeys.accounts());
      if (previous) {
        const byId = new Map(previous.map((account) => [account.id, account]));
        const next = ids
          .map((id) => byId.get(id))
          .filter((account): account is Account => Boolean(account));
        client.setQueryData<Account[]>(queryKeys.accounts(), next);
      }
      return { previous };
    },
    onError: (_error, _ids, context) => {
      if (context?.previous) client.setQueryData(queryKeys.accounts(), context.previous);
    },
    onSettled: invalidate,
  });
}

export function useUpdateAccountPassword(): UseMutationResult<
  void,
  unknown,
  { id: string; password: string }
> {
  const invalidate = useAccountInvalidation();
  return useMutation({
    mutationFn: ({ id, password }: { id: string; password: string }) =>
      accounts.updatePassword(id, password),
    onSuccess: invalidate,
  });
}

export function useDavPasswordStatus(accountId: string, enabled = true) {
  return useQuery({
    queryKey: ["davPasswordStatus", accountId] as const,
    queryFn: () => accounts.davPasswordStatus(accountId),
    enabled,
  });
}

export function useSetDavPassword(): UseMutationResult<
  void,
  unknown,
  { id: string; service: "calendar" | "contacts"; password: string | null }
> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      service,
      password,
    }: {
      id: string;
      service: "calendar" | "contacts";
      password: string | null;
    }) => accounts.setDavPassword(id, service, password),
    onSuccess: (_result, { id }) => {
      void client.invalidateQueries({ queryKey: ["davPasswordStatus", id] });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.syncStatus });
    },
  });
}

export function useReauthAccount(): UseMutationResult<Account, unknown, string> {
  const client = useQueryClient();
  const invalidate = useAccountInvalidation();
  return useMutation({
    mutationFn: (id: string) => accounts.reauth(id),
    onSuccess: (account) => {
      client.setQueryData<Account[]>(queryKeys.accounts(), (list) =>
        list ? list.map((item) => (item.id === account.id ? account : item)) : list,
      );
    },
    onSettled: invalidate,
  });
}

/* ─────────────────────────── Signatures ─────────────────────────── */

export function useSignatures() {
  return useQuery({ queryKey: queryKeys.signatures(), queryFn: signatures.list });
}

export function useUpsertSignature(): UseMutationResult<Signature, unknown, Signature> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (signature: Signature) => signatures.upsert(signature),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.signatures });
    },
  });
}

export function useDeleteSignature(): UseMutationResult<void, unknown, string> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => signatures.delete(id),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.signatures });
    },
  });
}

/* ─────────────────────────── Identities ─────────────────────────── */

export function useIdentities(accountId: string | null) {
  return useQuery({
    queryKey: queryKeys.identities(accountId),
    queryFn: () => identities.list(accountId),
    enabled: accountId !== null,
  });
}

export function useUpsertIdentity(): UseMutationResult<Identity, unknown, Identity> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (identity: Identity) => identities.upsert(identity),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.identities });
    },
  });
}

export function useDeleteIdentity(): UseMutationResult<void, unknown, string> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => identities.delete(id),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.identities });
    },
  });
}

/* ───────────────────────────── Folders ──────────────────────────── */

export function useFolders(accountId: string | null) {
  return useQuery({
    queryKey: queryKeys.folders(accountId ?? ""),
    queryFn: () => folders.list(accountId as string),
    enabled: accountId !== null,
    staleTime: 10_000,
  });
}

/**
 * Folders of several accounts, flattened in account order. Used by the rule
 * folder picker and the "run now" dialog.
 */
export function useFolderCollection(accountIds: string[]) {
  const unique = useMemo(
    () => Array.from(new Set(accountIds.filter(Boolean))).sort(),
    [accountIds],
  );
  const results = useQueries({
    queries: unique.map((id) => ({
      queryKey: queryKeys.folders(id),
      queryFn: () => folders.list(id),
      staleTime: 10_000,
    })),
  });
  const data = useMemo(
    () => results.flatMap((result) => result.data ?? []),
    [results],
  );
  const isLoading = results.some((result) => result.isLoading);
  return { folders: data, isLoading };
}

export function useSetFolderSync(): UseMutationResult<
  void,
  unknown,
  { id: string; enabled: boolean; accountId: string }
> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean; accountId: string }) =>
      folders.setSyncEnabled(id, enabled),
    onSuccess: (_data, variables) => {
      void client.invalidateQueries({ queryKey: queryKeys.folders(variables.accountId) });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.unifiedCounts });
    },
  });
}

/* ────────────────────────────── Rules ───────────────────────────── */

export function useRules() {
  return useQuery({ queryKey: queryKeys.rules(), queryFn: rules.list });
}

export function useUpsertRule(): UseMutationResult<MailRule, unknown, MailRule> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (rule: MailRule) => rules.upsert(rule),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.rules });
    },
  });
}

export function useDeleteRule(): UseMutationResult<void, unknown, string> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => rules.delete(id),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.rules });
    },
  });
}

export function useRunRule(): UseMutationResult<
  number,
  unknown,
  { id: string; folderId: string }
> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, folderId }: { id: string; folderId: string }) =>
      rules.runNow(id, folderId),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.messages });
      void client.invalidateQueries({ queryKey: queryKeyPrefixes.unifiedCounts });
    },
  });
}

/** Invalidate user data after a backup import (the DB was replaced). */
export function useInvalidateAllData() {
  const client = useQueryClient();
  return () => {
    void client.invalidateQueries();
  };
}

export type { Folder, StorageUsage };
