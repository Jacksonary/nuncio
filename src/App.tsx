/**
 * Hash router + secondary window routes.
 *
 *   #/                         main shell
 *   #/message/:id              message window (without shell chrome)
 *   #/message/eml?path=...     external .eml viewer
 *   #/eml?path=...             alias for the external .eml viewer
 *   #/compose/:draftId         compose window
 *   #/compose/new?accountId=…  new message window
 */
import {
  Component,
  useEffect,
  useMemo,
  useState,
  type ErrorInfo,
  type ReactNode,
} from "react";
import { Button, ConfirmHost, ToastProvider } from "@/components";
import { ComposeWindow, MessageWindow } from "@/modules/mail/windows";
import { useT, t as translate } from "@/lib/i18n";
import { installSecondaryEventBridge } from "@/lib/eventBridge";
import { useThemeSync, useLanguageSync, useSettingsQuery } from "@/lib/useSettings";
import { Shell } from "@/app/Shell";
import { WindowFrame } from "@/app/WindowFrame";
import styles from "./App.module.css";

type Route =
  | { kind: "shell" }
  | { kind: "message"; id: string }
  | { kind: "eml"; path: string }
  | { kind: "compose"; draftId: string | null; accountId: string | null };

export function parseRoute(hash: string): Route {
  const raw = hash.replace(/^#/, "");
  const queryIndex = raw.indexOf("?");
  const pathPart = queryIndex >= 0 ? raw.slice(0, queryIndex) : raw;
  const params = new URLSearchParams(queryIndex >= 0 ? raw.slice(queryIndex + 1) : "");
  const segments = pathPart.split("/").filter(Boolean);

  if (segments[0] === "message") {
    if (segments[1] === "eml") {
      return { kind: "eml", path: params.get("path") ?? "" };
    }
    if (segments[1]) return { kind: "message", id: segments[1] };
    return { kind: "shell" };
  }
  if (segments[0] === "eml") {
    return { kind: "eml", path: params.get("path") ?? "" };
  }
  if (segments[0] === "compose") {
    const draftId = segments[1] && segments[1] !== "new" ? segments[1] : null;
    return { kind: "compose", draftId, accountId: params.get("accountId") };
  }
  return { kind: "shell" };
}

function useHashRoute(): Route {
  const [hash, setHash] = useState(() =>
    typeof window === "undefined" ? "" : window.location.hash,
  );
  useEffect(() => {
    const onChange = () => setHash(window.location.hash);
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  return useMemo(() => parseRoute(hash), [hash]);
}

interface ErrorBoundaryProps {
  children: ReactNode;
}

interface ErrorBoundaryState {
  error: Error | null;
}

class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error("[nuncio] render error", error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <div className={styles.error} role="alert">
          <div className={styles.title}>{translate("common.error")}</div>
          <div className={styles.detail}>{this.state.error.message}</div>
          <Button appearance="primary" onClick={() => window.location.reload()}>
            {translate("common.retry")}
          </Button>
        </div>
      );
    }
    return this.props.children;
  }
}

/**
 * Secondary windows (message / compose) need cache invalidation but not the
 * shell's toasts, navigation or pending-file handling.
 */
function SecondaryWindow({ title, children }: { title: string; children: ReactNode }) {
  useEffect(() => installSecondaryEventBridge(), []);
  return <WindowFrame title={title}>{children}</WindowFrame>;
}

function RoutedView() {
  const route = useHashRoute();
  const t = useT();

  if (route.kind === "message") {
    return (
      <SecondaryWindow title={t("shell.window.message")}>
        <MessageWindow messageId={route.id} emlPath={null} />
      </SecondaryWindow>
    );
  }
  if (route.kind === "eml") {
    return (
      <SecondaryWindow title={t("shell.window.message")}>
        <MessageWindow messageId={null} emlPath={route.path} />
      </SecondaryWindow>
    );
  }
  if (route.kind === "compose") {
    return (
      <SecondaryWindow title={t("shell.window.compose")}>
        <ComposeWindow draftId={route.draftId} accountId={route.accountId} />
      </SecondaryWindow>
    );
  }
  return <Shell />;
}

export default function App() {
  const settings = useSettingsQuery().data;
  // Theme and language sync are mounted here rather than in the Shell so they
  // also run in the message / compose windows, which render without the shell.
  // Every window is its own webview with its own theme and its own i18n module
  // instance, so each one has to follow the settings independently.
  useThemeSync(settings?.theme);
  useLanguageSync(settings?.language);

  return (
    <ErrorBoundary>
      <ToastProvider>
        <RoutedView />
        <ConfirmHost />
      </ToastProvider>
    </ErrorBoundary>
  );
}
