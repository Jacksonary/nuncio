/** Chromeless frame for secondary windows (message, compose, .eml viewer). */
import { useEffect, type ReactNode } from "react";
import { WindowControls } from "@/components";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { isMac } from "@/lib/platform";
import { Logo } from "./Logo";
import styles from "./WindowFrame.module.css";

export interface WindowFrameProps {
  title: string;
  children: ReactNode;
}

export function WindowFrame({ title, children }: WindowFrameProps) {
  const t = useT();
  const mac = isMac();
  useEffect(() => {
    document.title = `${title} — ${t("shell.appName")}`;
  }, [title, t]);
  return (
    <div className={styles.window}>
      <header
        className={cn(styles.titleBar, mac ? styles.titleBarMac : styles.titleBarControls)}
        data-tauri-drag-region
      >
        <Logo size={18} />
        <span className={styles.title} data-tauri-drag-region>
          {title}
        </span>
        <div className={styles.spacer} data-tauri-drag-region />
        {mac ? null : <WindowControls />}
      </header>
      <div className={styles.body}>{children}</div>
    </div>
  );
}
