/**
 * Settings query + optimistic updates + theme/accent/language application.
 *
 * `useSettingsSync()` is mounted once by the shell. It also mirrors the native
 * window theme so mail iframes and OS chrome follow the app theme.
 */
import { useEffect } from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
  type UseMutationResult,
} from "@tanstack/react-query";
import { app, settings } from "@/ipc/api";
import type { AppSettings, SettingsPatch } from "@/ipc/types";
import { queryKeys } from "@/lib/queryKeys";
import { setLanguage } from "@/lib/i18n";
import { setPlatform } from "@/lib/platform";
import {
  applyAccent,
  applyTheme,
  syncNativeTheme,
  watchSystemTheme,
} from "@/lib/theme";

/**
 * Fallback settings used before the backend answers (and for optimistic
 * updates). MUST stay identical to `AppSettings::default()` in
 * nuncio-core/src/models.rs.
 */
export const DEFAULT_SETTINGS: AppSettings = {
  theme: "system",
  accentColor: null,
  language: "system",
  closeToTray: true,
  launchAtStartup: false,
  startMinimized: false,
  mail: {
    conversationView: true,
    previewLines: 1,
    density: "normal",
    showAvatars: true,
    markReadMode: "onOpen",
    markReadDelaySeconds: 5,
    remoteImages: "always",
    lightMessageBackground: false,
    hoverActions: ["archive", "delete", "flag"],
    swipeLeft: "delete",
    swipeRight: "archive",
    confirmDelete: true,
    autoAdvance: "next",
    defaultFontFamily: "",
    defaultFontSize: 14,
    prefetchUnreadBodies: 50,
    bodyCacheLimitMb: 500,
    otherFoldersPollMinutes: 15,
    quoteOriginal: true,
  },
  calendar: {
    defaultView: "week",
    firstDayOfWeek: 1,
    workDays: [1, 2, 3, 4, 5],
    workHoursStart: "09:00",
    workHoursEnd: "18:00",
    defaultReminderMinutes: 15,
    defaultEventDurationMinutes: 30,
    showWeekNumbers: false,
    pollMinutes: 15,
  },
  contacts: {
    sortBy: "firstName",
    displayOrder: "firstLast",
    pollMinutes: 60,
  },
  tasks: {
    showCompleted: false,
    pollMinutes: 30,
    newTasksOnTop: true,
  },
  notifications: {
    enabled: true,
    sound: true,
    showPreview: true,
    eventReminders: true,
    taskReminders: true,
  },
  privacy: {
    allowIspdbLookup: true,
  },
  updates: {
    autoCheck: true,
  },
  advanced: {
    googleClientId: null,
    googleClientSecret: null,
    microsoftClientId: null,
  },
};

/** Deep-merge a settings patch (objects merged, arrays replaced). */
export function mergeSettings(base: AppSettings, patch: SettingsPatch): AppSettings {
  const result: AppSettings = { ...base };
  const target = result as unknown as Record<string, unknown>;
  const source = patch as unknown as Record<string, unknown>;
  for (const key of Object.keys(source)) {
    const value = source[key];
    if (value === undefined) continue;
    if (Array.isArray(value)) {
      target[key] = [...value];
      continue;
    }
    if (value !== null && typeof value === "object") {
      const current = target[key];
      target[key] = {
        ...(current && typeof current === "object" ? (current as object) : {}),
        ...(value as object),
      };
      continue;
    }
    target[key] = value;
  }
  return result;
}

/** Raw settings query (undefined while loading). */
export function useSettingsQuery() {
  return useQuery({
    queryKey: queryKeys.settings(),
    queryFn: settings.get,
    staleTime: Infinity,
  });
}

/** Settings with sensible defaults, safe for every component. */
export function useAppSettings(): AppSettings {
  const query = useSettingsQuery();
  return query.data ?? DEFAULT_SETTINGS;
}

/** Optimistic settings update used by every settings control. */
export function useUpdateSettings(): UseMutationResult<
  AppSettings,
  unknown,
  SettingsPatch,
  { previous: AppSettings | undefined }
> {
  const client = useQueryClient();
  return useMutation<AppSettings, unknown, SettingsPatch, { previous: AppSettings | undefined }>({
    mutationFn: (patch) => settings.update(patch),
    onMutate: async (patch) => {
      await client.cancelQueries({ queryKey: queryKeys.settings() });
      const previous = client.getQueryData<AppSettings>(queryKeys.settings());
      const optimistic = mergeSettings(previous ?? DEFAULT_SETTINGS, patch);
      client.setQueryData(queryKeys.settings(), optimistic);
      return { previous };
    },
    onError: (_error, _patch, context) => {
      if (context?.previous) {
        client.setQueryData(queryKeys.settings(), context.previous);
      } else {
        void client.invalidateQueries({ queryKey: queryKeys.settings() });
      }
    },
    onSuccess: (data) => {
      client.setQueryData(queryKeys.settings(), data);
    },
  });
}

/**
 * Apply the current settings to the document and native window.
 * Mount once (Shell). Returns the query for loading/error handling.
 */
export function useSettingsSync() {
  const query = useSettingsQuery();
  const data = query.data;

  useEffect(() => {
    // Before the first successful load, keep the bootstrap values cached from
    // the previous run instead of overwriting them with defaults.
    if (!data) return;
    applyTheme(data.theme);
    applyAccent(data.accentColor);
    setLanguage(data.language);
    void syncNativeTheme(data.theme);
  }, [data]);

  useEffect(() => {
    if ((data?.theme ?? "system") !== "system") return;
    return watchSystemTheme(() => {
      applyTheme("system");
      void syncNativeTheme("system");
    });
  }, [data?.theme]);

  useEffect(() => {
    let cancelled = false;
    app
      .info()
      .then((info) => {
        if (!cancelled) setPlatform(info.platform);
      })
      .catch(() => {
        // app.info is unavailable outside the Tauri runtime; navigator wins.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return query;
}

export interface UseSettingsResult {
  /** Current settings with defaults filled in. */
  settings: AppSettings;
  /** Fire-and-forget optimistic update. */
  update: (patch: SettingsPatch) => void;
  /** Await the backend confirmation. */
  updateAsync: (patch: SettingsPatch) => Promise<AppSettings>;
  isSaving: boolean;
}

/**
 * Convenience hook for module settings pages: current settings plus optimistic
 * update helpers. `useSettingsSync` (mounted by the shell) applies theme,
 * accent and language whenever the cached value changes.
 */
export function useSettings(): UseSettingsResult {
  const settingsValue = useAppSettings();
  const mutation = useUpdateSettings();
  return {
    settings: settingsValue,
    update: (patch) => mutation.mutate(patch),
    updateAsync: (patch) => mutation.mutateAsync(patch),
    isSaving: mutation.isPending,
  };
}
