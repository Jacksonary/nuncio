/**
 * Settings query + optimistic updates + theme/accent/language application.
 *
 * `useSettingsSync()` is mounted once by the shell and applies accent and
 * language. `useThemeSync()` is mounted by `App` so that every window — the
 * shell plus the message / compose windows — follows the theme.
 */
import { useEffect, useRef } from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
  type UseMutationResult,
} from "@tanstack/react-query";
import { app, settings } from "@/ipc/api";
import type { AppSettings, SettingsPatch, ThemeMode } from "@/ipc/types";
import { queryKeys } from "@/lib/queryKeys";
import { setLanguage } from "@/lib/i18n";
import { setPlatform } from "@/lib/platform";
import {
  applyAccent,
  applyTheme,
  resolveNativeSystemTheme,
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
 * Theme part of the settings sync: applies the palette, mirrors the mode into
 * the native window and, for "system", keeps following the OS.
 *
 * Mounted once at the app root (see `App`), not only by the Shell: secondary
 * message/compose windows have their own webview and would otherwise only get
 * the theme from the one-shot `bootstrapTheme()` cache read, so a settings
 * change or an OS flip while they are open would leave them stale.
 *
 * `mode === undefined` means "no authoritative value yet" (settings still
 * loading), so the cached bootstrap theme is left alone.
 */
export function useThemeSync(mode: ThemeMode | undefined): void {
  // Bumped on every apply so a slower, older async chain cannot overwrite a
  // newer result (e.g. the user flips system -> dark while un-pinning runs).
  const generation = useRef(0);

  useEffect(() => {
    if (mode === undefined) return;

    // "Find the real OS theme, then repaint": we must not read matchMedia
    // while the webview is still pinned to a forced scheme, or we compute the
    // theme from a stale value and get one wrong repaint.
    if (mode === "system") {
      const ticket = ++generation.current;
      void syncNativeTheme("system")
        .then(() => resolveNativeSystemTheme())
        .then((resolved) => {
          // A newer apply superseded this one; drop the result.
          if (ticket !== generation.current) return;
          // The native value wins over matchMedia because the webview may
          // still report the scheme it was pinned to. No timer or artificial
          // delay is needed: `setTheme` resolving means the un-pin is done.
          applyTheme("system", resolved);
        });
      return;
    }

    // Fixed light/dark: no un-pinning involved, so the synchronous apply is
    // already correct and the native call can follow in the background.
    generation.current += 1;
    applyTheme(mode);
    void syncNativeTheme(mode);
  }, [mode]);

  useEffect(() => {
    if (mode !== "system") return;
    return watchSystemTheme((resolved) => {
      // The native event payload is the new OS theme. The matchMedia fallback
      // passes undefined, which makes applyTheme resolve it from the webview.
      applyTheme("system", resolved);
    });
  }, [mode]);
}

/**
 * Apply the accent color, language and platform of the current settings.
 * Theme lives in `useThemeSync` so it can be mounted by every window.
 * Mount once (Shell). Returns the query for loading/error handling.
 */
export function useSettingsSync() {
  const query = useSettingsQuery();
  const data = query.data;

  useEffect(() => {
    // Before the first successful load, keep the bootstrap values cached from
    // the previous run instead of overwriting them with defaults.
    if (!data) return;
    applyAccent(data.accentColor);
    setLanguage(data.language);
  }, [data]);

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
 * update helpers. `useSettingsSync` (mounted by the shell) applies accent and
 * language; `useThemeSync` (mounted by `App`) applies the theme in every
 * window.
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
