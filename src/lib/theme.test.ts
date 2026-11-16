/**
 * Regression tests for the theme sync self-lock.
 *
 * Pinning the native window to a *resolved* light/dark value (instead of
 * `null`) makes tao stop dispatching ThemeChanged and makes the webview's
 * `prefers-color-scheme` report the pinned value forever. These tests pin down
 * the two invariants that keep that from coming back:
 *
 *   1. `system` maps to `setTheme(null)`; explicit modes map to their value.
 *   2. The OS-change listener prefers the native payload over matchMedia.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

interface FakeWindow {
  setTheme: ReturnType<typeof vi.fn>;
  theme: ReturnType<typeof vi.fn>;
  onThemeChanged: ReturnType<typeof vi.fn>;
  emitTheme: (theme: "light" | "dark") => void;
  unlisten: ReturnType<typeof vi.fn>;
}

let fakeWindow: FakeWindow;

function createFakeWindow(): FakeWindow {
  let handler: ((event: { payload: "light" | "dark" }) => void) | null = null;
  const unlisten = vi.fn();
  return {
    setTheme: vi.fn(async () => undefined),
    theme: vi.fn(async () => "light"),
    onThemeChanged: vi.fn(async (next) => {
      handler = next;
      return unlisten;
    }),
    emitTheme: (th) => handler?.({ payload: th }),
    unlisten,
  };
}

/**
 * Wait for the async `onThemeChanged()` subscription to settle. Two microtask
 * ticks cover the promise chain inside `watchSystemTheme`.
 */
async function flushSubscription(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => fakeWindow,
}));

const theme = await import("@/lib/theme");

/** Minimal DOM/localStorage stubs: theme.ts only needs these two globals. */
function installDomStubs(): { dataset: Record<string, string> } {
  const dataset: Record<string, string> = {};
  const store = new Map<string, string>();
  const g = globalThis as unknown as Record<string, unknown>;
  g.document = { documentElement: { dataset } };
  g.localStorage = {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, value),
    removeItem: (key: string) => void store.delete(key),
  };
  return { dataset };
}

function installMatchMedia(matches: boolean): {
  setMatches: (next: boolean) => void;
} {
  let current = matches;
  let handler: (() => void) | null = null;
  const g = globalThis as unknown as Record<string, unknown>;
  g.window = {
    matchMedia: () => ({
      get matches() {
        return current;
      },
      addEventListener: (_type: string, next: () => void) => {
        handler = next;
      },
      removeEventListener: () => {
        handler = null;
      },
    }),
  };
  return {
    setMatches: (next: boolean) => {
      current = next;
      handler?.();
    },
  };
}

beforeEach(() => {
  fakeWindow = createFakeWindow();
  installDomStubs();
  installMatchMedia(false);
});

afterEach(() => {
  const g = globalThis as unknown as Record<string, unknown>;
  delete g.document;
  delete g.localStorage;
  delete g.window;
  vi.restoreAllMocks();
});

describe("syncNativeTheme", () => {
  it('passes null for "system" so the window keeps following the OS', async () => {
    await theme.syncNativeTheme("system");
    expect(fakeWindow.setTheme).toHaveBeenCalledTimes(1);
    expect(fakeWindow.setTheme).toHaveBeenCalledWith(null);
  });

  it("passes the concrete value for explicit modes", async () => {
    await theme.syncNativeTheme("dark");
    await theme.syncNativeTheme("light");
    expect(fakeWindow.setTheme).toHaveBeenNthCalledWith(1, "dark");
    expect(fakeWindow.setTheme).toHaveBeenNthCalledWith(2, "light");
  });

  it("never forwards a resolved value for system", async () => {
    installMatchMedia(true); // OS says dark...
    await theme.syncNativeTheme("system");
    // ...and the native call still asks the OS instead of pinning "dark".
    expect(fakeWindow.setTheme).toHaveBeenCalledTimes(1);
    expect(fakeWindow.setTheme).toHaveBeenCalledWith(null);
  });
});

describe("applyTheme", () => {
  it("uses the caller-provided resolved theme over matchMedia", () => {
    // matchMedia is stale (says light) after un-pinning; native says dark.
    const { dataset } = installDomStubs();
    theme.applyTheme("system", "dark");
    expect(dataset.theme).toBe("dark");
  });

  it("falls back to matchMedia when no resolved theme is supplied", () => {
    installMatchMedia(true);
    const { dataset } = installDomStubs();
    theme.applyTheme("system");
    expect(dataset.theme).toBe("dark");
  });
});

describe("resolveNativeSystemTheme", () => {
  it("prefers the native window theme", async () => {
    fakeWindow.theme.mockResolvedValue("dark");
    await expect(theme.resolveNativeSystemTheme()).resolves.toBe("dark");
  });

  it("falls back to matchMedia when the window reports nothing", async () => {
    fakeWindow.theme.mockResolvedValue(null);
    installMatchMedia(true);
    await expect(theme.resolveNativeSystemTheme()).resolves.toBe("dark");
  });
});

describe("watchSystemTheme", () => {
  it("reports the native event payload", async () => {
    const seen: ("light" | "dark" | undefined)[] = [];
    const stop = theme.watchSystemTheme((resolved) => seen.push(resolved));
    await flushSubscription();

    fakeWindow.emitTheme("dark");
    fakeWindow.emitTheme("light");
    expect(seen).toEqual(["dark", "light"]);

    stop();
  });

  it("does not also listen to matchMedia once the native event is available", async () => {
    // Both sources at once would race on Windows: the runtime re-applies the
    // OS scheme to the webview on ThemeChanged, so a media change can arrive
    // late still describing the previous scheme.
    const media = installMatchMedia(false);
    const seen: ("light" | "dark" | undefined)[] = [];
    const stop = theme.watchSystemTheme((resolved) => seen.push(resolved));
    await flushSubscription();

    media.setMatches(true);
    expect(seen).toEqual([]);

    stop();
  });

  it("falls back to matchMedia when the native subscription fails", async () => {
    fakeWindow.onThemeChanged.mockRejectedValue(new Error("unsupported"));
    const media = installMatchMedia(false);
    const seen: ("light" | "dark" | undefined)[] = [];
    const stop = theme.watchSystemTheme((resolved) => seen.push(resolved));
    await flushSubscription();

    media.setMatches(true);
    expect(seen).toEqual([undefined]);

    stop();
  });

  it("unsubscribes immediately when cleaned up before the async subscribe resolves", async () => {
    const stop = theme.watchSystemTheme(() => undefined);
    stop(); // unmount happens while onThemeChanged is still in flight
    await flushSubscription();
    expect(fakeWindow.unlisten).toHaveBeenCalledTimes(1);
  });

  it("stops delivering events after cleanup", async () => {
    const listener = vi.fn();
    const stop = theme.watchSystemTheme(listener);
    await flushSubscription();
    stop();
    fakeWindow.unlisten();

    fakeWindow.emitTheme("dark");
    expect(listener).not.toHaveBeenCalled();
  });
});
