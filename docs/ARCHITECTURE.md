# Nuncio Architecture & Engineering Rules

Read together with `docs/PLAN.md` (product scope) and the IPC contract
`src/ipc/types.ts` + `src/ipc/api.ts` (single source of truth for data shapes and
command names). Every coding agent MUST follow this document.

## 1. Repository layout & ownership

```
nuncio/
├─ src/                         React 18 + TS frontend (Vite)
│  ├─ ipc/{types.ts,api.ts}     CONTRACT (architect-owned, do not edit without approval)
│  ├─ main.tsx  App.tsx         entry + hash router                         [F-shell]
│  ├─ app/                      shell: layout, module switcher, settings flyout host,
│  │                            first-run, toasts, event bridge, hotkeys      [F-shell]
│  ├─ components/               Fluent-style primitives                      [F-shell]
│  ├─ styles/                   tokens.css, global.css                       [F-shell]
│  ├─ lib/                      queryClient, keys, date/format utils, i18n    [F-shell]
│  ├─ i18n/{common,shell,mail,calendar,people,todo,settings}.ts  per-module dictionaries
│  └─ modules/
│     ├─ mail/                  folder pane, list, reader, composer, windows [F-mail]
│     ├─ calendar/                                                         [F-calendar]
│     ├─ people/                                                           [F-people]
│     ├─ todo/                                                             [F-todo]
│     └─ settings/              settings panel, accounts, add-account wizard  [F-settings]
├─ src-tauri/
│  ├─ src/                      app crate: commands, wiring, tray, windows   [R-app]
│  └─ crates/
│     ├─ core/   nuncio-core    models, errors, Db+migrations, repos, traits [R-core]
│     ├─ mime/   nuncio-mime    MIME parse/build, sanitize, quoting, threading [R-mime]
│     ├─ ical/   nuncio-ical    iCalendar/vCard, RRULE expansion, iTIP        [R-ical]
│     ├─ auth/   nuncio-auth    OAuth PKCE, secrets, credentials, discovery  [R-auth]
│     ├─ mail/   nuncio-mail    IMAP/SMTP/POP3, sync engine, op queue, rules [R-mail]
│     └─ pim/    nuncio-pim     Google/Graph/DAV calendar, contacts, tasks   [R-pim]
└─ docs/
```

An agent only edits files in the area it owns. If it needs something from another
area that does not exist, it documents the need in its final report instead of
editing the other area (the integration stage resolves it).

## 2. Rust crate dependency graph

```
core  <──  mime  <──┐
core  <──  ical  <──┼── mail
core  <──  auth     │
core  <──  ical  <──┴── pim
app  ──> core, mime, ical, auth, mail, pim, tauri
```

* Only the app crate depends on `tauri`. Domain crates talk to the outside world
  through traits defined in `nuncio-core` (see §4) so they compile and test
  without Tauri.
* All third-party versions come from `[workspace.dependencies]` in
  `src-tauri/Cargo.toml` (use `dep = { workspace = true }`). To add a crate,
  add it to that crate's own `Cargo.toml` with an explicit version only if it is
  not in the workspace list, and mention it in the final report.
* Toolchain: rustc 1.97 (do not use crates requiring newer). `mail-parser` is
  pinned to 0.11 for this reason.
* TLS: rustls with the **ring** provider. The app installs it once at startup:
  `rustls::crypto::ring::default_provider().install_default()`. Tests that need
  TLS must do the same (ignore the "already installed" error).

## 3. Serialization rules (Rust ⇄ TS)

* Every type in `src/ipc/types.ts` has an identically named Rust type in
  `nuncio_core::models` with `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]`
  and `#[serde(rename_all = "camelCase")]`.
* String unions → Rust enums with `#[serde(rename_all = "camelCase")]`
  (e.g. `"needsReauth"` ↔ `AccountState::NeedsReauth`). Exceptions where the
  literal is not camelCase of the variant must use explicit `#[serde(rename)]`
  (e.g. `Language::ZhCn` ↔ `"zh-CN"`, Weekday `"MO"`…).
* Tagged unions (`MailScope`, `MessageAction`) → `#[serde(tag = "kind", rename_all = "camelCase")]`.
* `T | null` → `Option<T>` (serialized as `null`, never skipped). Optional patch
  fields (`AccountPatch`, `SettingsPatch`) use `Option<T>` with
  `#[serde(default)]`; for `T | null` inside a patch use `Option<Option<T>>`
  with a helper deserializer so "absent" ≠ "null".
* Timestamps: `chrono::DateTime<Utc>` serialized as RFC3339 with `Z`.
  Dates `NaiveDate` as `YYYY-MM-DD`. Event start/end are `String`.
* Ids: `String` (UUID v4 via `uuid::Uuid::new_v4().to_string()`).

## 4. nuncio-core public API (shape every crate relies on)

```rust
pub mod models;      // all IPC types
pub mod error;       // AppError, Result
pub mod db;          // Db, migrations
pub mod repo;        // typed repositories per table group
pub mod traits;      // EventSink, CredentialProvider, Notifier, MailSender
pub mod context;     // CoreContext
pub mod paths;       // Paths
pub mod settings;    // SettingsService (load/merge/save AppSettings)
pub mod events;      // event name constants + typed emit helpers
pub mod sync_status; // SyncStatusHub
pub mod text;        // CJK bigram tokenizer for FTS, snippet helpers

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {           // serializes as {"code": "...", "message": "..."}
    NotFound(String), Auth(String), Network(String), Protocol(String),
    InvalidInput(String), Conflict(String), Io(String), Db(String),
    Cancelled, Unsupported(String), Internal(String),
}

pub trait EventSink: Send + Sync {            // implemented by app via AppHandle::emit
    fn emit(&self, event: &str, payload: serde_json::Value);
}

pub enum TokenResource { Mail, GoogleApis, MicrosoftGraph }
pub enum MailAuth { Password { username: String, password: String },
                    XOAuth2 { username: String, access_token: String } }

#[async_trait]
pub trait CredentialProvider: Send + Sync {   // implemented by nuncio-auth
    async fn mail_auth(&self, account: &Account) -> Result<MailAuth>;
    async fn api_token(&self, account_id: &str, resource: TokenResource) -> Result<String>;
    async fn dav_auth(&self, account: &Account) -> Result<(String, String)>; // basic user/pass
    /// Server rejected the credential: refresh/mark needsReauth.
    async fn report_auth_failure(&self, account_id: &str, resource: TokenResource);
}

pub trait Notifier: Send + Sync {             // implemented by app (OS notifications + badge)
    fn new_mail(&self, account: &Account, items: &[MessageSummary]);
    fn reminder(&self, title: &str, body: &str, target: NavigateTarget);
    fn unread_badge(&self, count: u32);
}

#[async_trait]
pub trait MailSender: Send + Sync {           // implemented by nuncio-mail, used by pim for iTIP
    async fn send_raw(&self, account_id: &str, rfc822: Vec<u8>, save_to_sent: bool) -> Result<()>;
}

#[derive(Clone)]
pub struct CoreContext {
    pub db: Db,
    pub paths: Paths,
    pub events: Arc<dyn EventSink>,
    pub creds: Arc<dyn CredentialProvider>,
    pub notifier: Arc<dyn Notifier>,
    pub settings: SettingsService,
    pub sync_status: SyncStatusHub,
}
```

The R-core agent may refine signatures but must keep these names and roles.

### 4.1 Database

* SQLite via `rusqlite` (bundled), pool via `r2d2_sqlite`. Pragmas on every
  connection: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
  `busy_timeout=5000`, `cache_size=-8000` (8 MB cap → keeps RAM low).
* `Db::read(|conn| ...)` / `Db::write(|conn| ...)` run closures on a blocking
  thread (`tokio::task::spawn_blocking`); writes are serialized by a mutex.
* Migrations: `Db::migrate(namespace, &[(version, sql)])` with a
  `schema_migrations(namespace, version)` table. Core owns namespace `core` and
  creates ALL tables listed in PLAN.md §5.2 up front (plus `fts_mail` FTS5 with
  the CJK-bigram column, `recipient_history`, `trusted_sender`, `undo_log`,
  `draft`, `outbox`). Domain crates that need extra tables register their own
  namespace (`mail`, `pim`, `auth`) — they never edit core migrations.
* Every entity table has `id TEXT PRIMARY KEY`, `account_id` FK with
  `ON DELETE CASCADE` where applicable, and `updated_at`.
* Local-only built-in account: id `"local"`, provider `local`, created by core
  migration; owns local calendar "日历", address book "联系人", task list "任务".

### 4.2 Files

`Paths` (rooted at Tauri `app_local_data_dir`): `db`, `mime/<account>/<2-char>/<hash>.eml`,
`attachments/<message-id>/`, `staging/<draft-id>/`, `logs/`, `tmp/`.

## 5. Domain services (one per crate) — map 1:1 to commands

Each domain crate exposes a cloneable service struct whose `pub async fn`s map
one-to-one to commands in `api.ts`, named after the command (without the area
prefix when obvious) and taking/returning contract types:

| Service | Commands it implements |
|---|---|
| `nuncio_core::settings::SettingsService` | settings_get, settings_update |
| `nuncio_core::repo` (plain DB) | identity_*, signature_*, rule_list/upsert/delete, account persistence |
| `nuncio_auth::AuthService` | auth_oauth_login, auth_oauth_cancel, account_discover, credential storage; implements `CredentialProvider` |
| `nuncio_mail::MailService` | account_test (mail part), folder_*, message_*, attachment_*, compose_*, rule_run_now, mail sync, op queue, outbox, undo; implements `MailSender` |
| `nuncio_pim::PimService` | account_test (dav part), calendar_*, event_*, invite_respond, addressbook_list, contact_*, group_*, tasklist_*, task_*, message_create_task, reminders loop |
| app crate | account_list/create/update/update_password/reauth/delete/reorder (orchestrates auth+core+mail+pim), sync_now, sync_status, app_* , windows, tray, notifications, deep links, backup |

Each service has `start(&self)` (spawn background tasks for enabled accounts),
`on_account_added/changed/removed(&self, id)` and `shutdown(&self)`.

## 6. Mail engine rules (R-mail)

Implements PLAN.md §6.1 exactly: ≤2 IMAP connections per account (IDLE on INBOX
+ worker), metadata-first initial sync newest-first in batches of 200 within the
sync window, CONDSTORE `CHANGEDSINCE` incremental with UID-diff fallback,
UIDVALIDITY reset, IDLE re-issued every ≤25 min, other folders polled every
`otherFoldersPollMinutes`, sleep/wake + network recovery, persistent op queue
(`pending_op`) with batching, exponential backoff, permanent-failure rollback and
`op://failed`, MOVE fallback COPY+STORE+EXPUNGE, Gmail X-GM-* (labels/threads/
msgid dedupe, `[Gmail]/All Mail` hidden), outbox with retry, Sent APPEND unless
`autoSavesSent`, Drafts APPEND/replace, body prefetch for newest unread, LRU MIME
cache trimming to `bodyCacheLimitMb`, local rules on new mail, new-mail
notifications via `Notifier`, unread badge updates, server SEARCH fallback,
POP3 accounts (download to local Inbox, optional leave-on-server).

## 7. Security rules

* All e-mail HTML goes through `nuncio_mime::sanitize` (ammonia allowlist):
  no script/iframe/object/embed/form/input/meta-refresh/base, no `on*` attributes,
  no `javascript:`/`vbscript:`/`data:` (except `data:image/*`) URLs, CSS
  `expression()`/`url()` to remote hosts removed unless remote allowed.
* The HTML document returned in `MessageDetail.html` embeds
  `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src <cid-protocol> data: [https: http: if allowed]; style-src 'unsafe-inline'; font-src data:">`.
* `cid:` → `{proto}cid/{messageId}/{urlencoded-content-id}` where `{proto}` is
  `http://nuncio.localhost/` on Windows and `nuncio://localhost/` elsewhere; the
  app crate serves this scheme (`register_asynchronous_uri_scheme_protocol("nuncio", ...)`)
  and only serves attachments of messages that exist in the DB.
* The frontend renders it with `<iframe sandbox="allow-popups allow-popups-to-escape-sandbox" srcdoc=...>`
  (NO allow-scripts, NO allow-same-origin). Link clicks are intercepted via a
  `<base target="_blank">` + popup → frontend opens with `@tauri-apps/plugin-opener`
  after showing the real URL.
* Secrets only in `SecretStore` (keyring; AES-GCM file fallback). Never log
  passwords/tokens. Logs go to `Paths.logs` only.
* Attachments saved on Windows get a `:Zone.Identifier` ADS (`[ZoneTransfer]\r\nZoneId=3`).

## 8. Frontend rules

* React 18 + TypeScript strict. State: TanStack Query for backend data,
  zustand for UI state. No other UI frameworks. Icons: `@fluentui/react-icons`
  (import individual icons only). Dates: `date-fns` + `Intl`.
* Styling: CSS Modules (`*.module.css`) per component + global CSS variables
  from `src/styles/tokens.css` (Fluent 2 inspired: `--bg`, `--bg-layer`,
  `--surface`, `--surface-hover`, `--text`, `--text-secondary`, `--border`,
  `--accent`, `--accent-hover`, `--accent-text`, `--danger`, `--radius-s/m/l`,
  `--shadow-flyout`, `--font-ui`, spacing `--sp-1..8`). Light/dark via
  `[data-theme="dark"]` on `<html>`. Window background transparent so Mica shows
  through; panes use translucent layers.
* Only talk to the backend through `src/ipc/api.ts` (never call `invoke` directly).
* Query keys (in `src/lib/queryKeys.ts`, owned by F-shell): `["accounts"]`,
  `["folders", accountId]`, `["unifiedCounts"]`, `["messages", query]`,
  `["message", id]`, `["thread", accountId, threadId]`, `["calendars"]`,
  `["events", from, to, calendarIds]`, `["event", id, instanceStart]`,
  `["addressBooks"]`, `["contacts", query]`, `["contact", id]`, `["groups", abId]`,
  `["taskLists"]`, `["tasks", query]`, `["task", id]`, `["smartCounts", today]`,
  `["settings"]`, `["syncStatus"]`, `["identities", accountId]`, `["signatures"]`, `["rules"]`.
  The event bridge (F-shell) invalidates them on backend events.
* i18n: `t("mail.reply")` from `src/lib/i18n.ts`; each module owns
  `src/i18n/<module>.ts` exporting `{ "zh-CN": {...}, "en-US": {...} }` with
  keys prefixed by the module name. zh-CN is the primary language; every key
  must exist in both.
* Module contract (owned by F-shell, `src/app/moduleTypes.ts`):
  each `src/modules/<m>/index.tsx` default-exports
  `{ id: AppModule, labelKey, Icon, Sidebar: FC, Main: FC, onNew?: () => void, hotkeys? }`.
  Mail additionally exports `MessageWindow` and `ComposeWindow` from
  `src/modules/mail/windows.tsx`; settings exports `SettingsPanel`,
  `AddAccountDialog`, `AccountsSection` from `src/modules/settings/index.tsx`.
* Secondary windows use routes `#/message/<id>`, `#/compose/<draftId>` and
  `#/compose/new?accountId=<id>` rendered without the shell chrome.
* Keyboard shortcuts per PLAN.md §9 (Cmd on macOS). Everything keyboard
  reachable, visible focus rings, `prefers-reduced-motion` respected.
* No mock data in production code paths.

## 9. Quality bar for every agent

* Complete, working implementations — no `todo!()`, `unimplemented!()`, empty
  handlers, "TODO: implement" stubs or placeholder UIs in the delivered area.
* Rust: `cargo check -p <crate>` clean (no errors; warnings minimized),
  `cargo test -p <crate>` green with meaningful unit tests for parsing,
  sanitizing, recurrence, sync-state logic, SQL repos (in-memory DB).
  Use a private target dir to avoid lock contention:
  `CARGO_TARGET_DIR=src-tauri/target/wf-<crate>`.
* Frontend: `npx tsc --noEmit` has no errors in the owned files.
* Log with `tracing` (`info` for lifecycle, `debug` for protocol detail,
  never secrets).
* Code comments in English; user-facing strings via i18n (zh-CN + en-US).
