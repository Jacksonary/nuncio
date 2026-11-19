/**
 * Shared data hooks for the mail module. Everything goes through the IPC
 * contract and the shared query keys so the shell's event bridge invalidates
 * the right caches.
 */
import { useMemo } from "react";
import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { accounts as accountsApi, folders as foldersApi, sync as syncApi } from "@/ipc/api";
import type {
  Account,
  Folder,
  FolderRole,
  SyncStatus,
  UnifiedCounts,
} from "@/ipc/types";
import { useAppSettings } from "@/lib/useSettings";
import { queryKeys } from "@/lib/queryKeys";
import {
  ROLE_ORDER,
  buildFolderTree,
  displayFolderTree,
  flattenFolderTree,
  type FolderNode,
} from "./logic";

export function useAccountsQuery() {
  return useQuery({
    queryKey: queryKeys.accounts(),
    queryFn: accountsApi.list,
    staleTime: 60_000,
  });
}

export function useMailAccounts(): Account[] {
  const query = useAccountsQuery();
  return useMemo(
    () =>
      (query.data ?? [])
        .filter((account) => account.mailEnabled)
        .sort((a, b) => a.sortOrder - b.sortOrder),
    [query.data],
  );
}

export interface AllFolders {
  folders: Folder[];
  byAccount: Map<string, Folder[]>;
  byId: Map<string, Folder>;
  trees: Map<string, FolderNode[]>;
  isLoading: boolean;
  isError: boolean;
}

/** Folders of every account, grouped and shaped into trees. */
export function useAllFolders(accounts: Account[]): AllFolders {
  const results = useQueries({
    queries: accounts.map((account) => ({
      queryKey: queryKeys.folders(account.id),
      queryFn: () => foldersApi.list(account.id),
      staleTime: 30_000,
    })),
  });

  const dataSignature = results
    .map((result) => JSON.stringify(result.data ?? null))
    .join("\u0000");
  const loading = results.some((result) => result.isLoading);
  const failed = results.some((result) => result.isError);

  return useMemo(() => {
    const byAccount = new Map<string, Folder[]>();
    const byId = new Map<string, Folder>();
    const trees = new Map<string, FolderNode[]>();
    accounts.forEach((account, index) => {
      const list = results[index]?.data ?? [];
      const sorted = [...list].sort((a, b) => {
        const roleDelta = (ROLE_ORDER[a.role] ?? 99) - (ROLE_ORDER[b.role] ?? 99);
        if (roleDelta !== 0) return roleDelta;
        return a.sortOrder - b.sortOrder;
      });
      byAccount.set(account.id, sorted);
      for (const folder of sorted) byId.set(folder.id, folder);
      trees.set(account.id, displayFolderTree(buildFolderTree(sorted)));
    });
    return {
      folders: Array.from(byId.values()),
      byAccount,
      byId,
      trees,
      isLoading: loading,
      isError: failed,
    };
    // `results` gets a fresh array identity on every render; the signature only
    // changes when folder data actually changed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [accounts, dataSignature, loading, failed]);
}

export function useUnifiedCounts() {
  return useQuery<UnifiedCounts>({
    queryKey: queryKeys.unifiedCounts(),
    queryFn: foldersApi.unifiedCounts,
    staleTime: 20_000,
  });
}

export function useSyncStatusQuery() {
  return useQuery<SyncStatus[]>({
    queryKey: queryKeys.syncStatus(),
    queryFn: syncApi.status,
    staleTime: 15_000,
  });
}

/** True while mail is syncing for any of the given accounts. */
export function useMailSyncing(accountIds: string[]): boolean {
  const query = useSyncStatusQuery();
  return (query.data ?? []).some(
    (status) =>
      status.module === "mail" &&
      status.state === "syncing" &&
      (accountIds.length === 0 || accountIds.includes(status.accountId)),
  );
}

export function useMailSettings() {
  return useAppSettings().mail;
}

/** Accounts that participate in the unified ("Linked") inbox. */
export function useUnifiedAccounts(): Account[] {
  const accounts = useMailAccounts();
  return useMemo(
    () =>
      accounts.filter(
        (account) =>
          // Turning mail off leaves `includeInUnified` set, so membership must
          // test both flags. Without the explicit `mailEnabled` check the hook
          // silently depends on `useMailAccounts` prefetching; a mail-disabled
          // account would then keep contributing folders and messages to the
          // linked views.
          account.mailEnabled && account.includeInUnified,
      ),
    [accounts],
  );
}

export interface FolderChoice {
  folder: Folder;
  account: Account;
  depth: number;
}

/** Flat folder list (per account tree) used by move-to menus. */
export function useFolderChoices(accounts: Account[], folders: AllFolders): FolderChoice[] {
  return useMemo(() => {
    const choices: FolderChoice[] = [];
    for (const account of accounts) {
      const tree = folders.trees.get(account.id) ?? [];
      for (const entry of flattenFolderTree(tree)) {
        choices.push({ folder: entry.folder, account, depth: entry.depth });
      }
    }
    return choices;
  }, [accounts, folders]);
}

/** Resolve the name to show for a special-use folder. */
export function folderRoleKey(role: FolderRole): string {
  return `mail.folder.${role}`;
}

/** Invalidate everything a local message mutation can affect. */
export function useInvalidateMail(): () => void {
  const client = useQueryClient();
  return () => {
    void client.invalidateQueries({ queryKey: ["messages"] });
    void client.invalidateQueries({ queryKey: ["unifiedCounts"] });
    void client.invalidateQueries({ queryKey: ["folders"] });
    void client.invalidateQueries({ queryKey: ["message"] });
  };
}
