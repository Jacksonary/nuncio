/**
 * Windows/Linux caption controls for the custom title bar.
 *
 * The native title bar is removed on those platforms (decorations = false), so
 * the shell renders its own minimize / maximize-restore / close buttons with
 * the Windows 11 caption metrics (46x32, red close hover). macOS keeps its
 * native traffic lights, therefore this component renders nothing there.
 *
 * Maximize state is kept in sync through `onResized` because the window can
 * also be maximized by double-clicking the drag region, snapping or a
 * keyboard shortcut.
 */
import { useEffect, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  Dismiss16Regular,
  Square16Regular,
  SquareMultiple16Regular,
  Subtract16Regular,
} from "@fluentui/react-icons";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { isMac } from "@/lib/platform";
import styles from "./WindowControls.module.css";

export interface WindowControlsProps {
  className?: string;
}

export function WindowControls({ className }: WindowControlsProps) {
  const t = useT();
  const [maximized, setMaximized] = useState(false);
  const tauri = isTauri();
  const mac = isMac();

  useEffect(() => {
    if (!tauri || mac) return;
    const window = getCurrentWindow();
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    const sync = () => {
      window
        .isMaximized()
        .then((value) => {
          if (!disposed) setMaximized(value);
        })
        .catch((error: unknown) => {
          console.warn("[window] isMaximized failed", error);
        });
    };

    void sync();
    window
      .onResized(() => sync())
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((error: unknown) => {
        console.warn("[window] onResized failed", error);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [tauri, mac]);

  if (!tauri || mac) return null;

  const run = (action: () => Promise<void>, label: string) => {
    action().catch((error: unknown) => {
      console.warn(`[window] ${label} failed`, error);
    });
  };

  return (
    <div className={cn(styles.controls, className)}>
      <button
        type="button"
        className={styles.button}
        title={t("common.minimizeWindow")}
        aria-label={t("common.minimizeWindow")}
        onClick={() => run(() => getCurrentWindow().minimize(), "minimize")}
      >
        <Subtract16Regular />
      </button>
      <button
        type="button"
        className={styles.button}
        title={maximized ? t("common.restoreWindow") : t("common.maximizeWindow")}
        aria-label={maximized ? t("common.restoreWindow") : t("common.maximizeWindow")}
        onClick={() => run(() => getCurrentWindow().toggleMaximize(), "toggleMaximize")}
      >
        {maximized ? <SquareMultiple16Regular /> : <Square16Regular />}
      </button>
      <button
        type="button"
        className={cn(styles.button, styles.close)}
        title={t("common.closeWindow")}
        aria-label={t("common.closeWindow")}
        onClick={() => run(() => getCurrentWindow().close(), "close")}
      >
        <Dismiss16Regular />
      </button>
    </div>
  );
}
