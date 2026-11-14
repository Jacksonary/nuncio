/**
 * Public entry point of the mail composer.
 *
 *   ComposeView      — render the inline composer or the secondary window
 *   useComposeActions/newMessage/reply/openDraft/fromMailto — create drafts
 *   useComposeStore  — zustand store the reading pane observes
 */
export { ComposeView } from "./ComposeView";
export type { ComposeViewProps } from "./ComposeView";
export { useComposeActions, composeActions, closeCompose } from "./actions";
export type { ComposeActions } from "./actions";
export { useComposeStore } from "./store";
export type { ComposeStoreState } from "./store";
export { composeI18n, registerComposeI18n } from "./i18n";
