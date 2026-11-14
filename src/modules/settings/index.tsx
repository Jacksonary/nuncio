/**
 * Settings module public entry points, consumed by the shell:
 *  - SettingsPanel: content of the right-side settings flyout
 *  - AddAccountDialog: first-run / add-account wizard
 *  - AccountsSection: account management page
 */
import { startAutoUpdateCheck } from "./lib/autoUpdate";

export { SettingsPanel } from "./SettingsPanel";
export { AddAccountDialog } from "./AddAccountDialog";
export { AccountsSection } from "./components/AccountsSection";

// The shell imports this entry module once at startup. Starting the scheduler
// here makes the "check for updates automatically" setting take effect even
// though the settings flyout itself is only mounted on demand. The scheduler
// runs in the main window only, checks ~30 s after start and every 24 hours,
// and is a no-op outside Tauri.
startAutoUpdateCheck();
