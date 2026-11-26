#![allow(dead_code)]
//! In-process scripted IMAP server used by the mail engine integration tests.
//!
//! It implements the subset of IMAP4rev1 the engine exercises: CAPABILITY,
//! LOGIN, LIST, SELECT/EXAMINE, UID SEARCH, UID FETCH (with literals and
//! CHANGEDSINCE), UID STORE, UID COPY/MOVE/EXPUNGE, APPEND, CREATE/DELETE/
//! RENAME/SUBSCRIBE and IDLE. Responses are valid enough for async-imap's
//! parser; no attempt is made to be a general-purpose server.

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nuncio_core::models::Address;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct MockMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internaldate: String,
    pub raw: Vec<u8>,
    pub modseq: u64,
    pub gm_msgid: Option<u64>,
    pub gm_thrid: Option<u64>,
    pub gm_labels: Vec<String>,
}

#[derive(Debug)]
pub struct MockFolder {
    pub name: String,
    pub attributes: Vec<String>,
    pub delimiter: String,
    pub uidvalidity: u32,
    pub next_uid: u32,
    pub next_modseq: u64,
    pub messages: Vec<MockMessage>,
}

/// A server-assigned arrival time. The executor correlates an unacknowledged
/// APPEND with a copy through this value, so it must be near the client clock.
fn now_internaldate() -> String {
    chrono::Utc::now()
        .format("%d-%b-%Y %H:%M:%S +0000")
        .to_string()
}

impl MockFolder {
    fn new(name: &str, attributes: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            attributes: attributes.iter().map(|value| value.to_string()).collect(),
            delimiter: "/".into(),
            uidvalidity: 1,
            next_uid: 1,
            next_modseq: 1,
            messages: Vec::new(),
        }
    }
}

pub struct MockState {
    pub username: String,
    pub password: String,
    pub capabilities: Vec<String>,
    pub folders: Mutex<Vec<MockFolder>>,
    /// Accepted TCP connections (used by liveness tests).
    pub connections: AtomicUsize,
    /// When set, a connection that stays idle (no command) for this long is
    /// silently closed, mimicking NetEase 163.
    idle_timeout: Mutex<Option<Duration>>,
    /// When set, content SEARCH keys (TEXT/SUBJECT/FROM/...) answer `OK` with
    /// zero hits while structural keys and FETCH keep working (NetEase 163).
    content_search_empty: AtomicBool,
    /// Omit the `UIDNEXT` response code from SELECT/EXAMINE (NetEase 163).
    omit_uidnext: AtomicBool,
    /// Answer `UID SEARCH` with a tagged `NO`/`BAD` while sending no untagged
    /// results. Models a server whose refusal looks like an empty set to the
    /// typed `async-imap` helper.
    search_fail: Mutex<Option<String>>,
    /// Answer every `UID STORE` with `NO` (models a server that refuses the
    /// flag update while still accepting the connection).
    fail_store: AtomicBool,
    /// Answer every `UID EXPUNGE`/`EXPUNGE` with `NO` (models a server that
    /// acknowledges the flag update but refuses the removal).
    fail_expunge: AtomicBool,
    /// Include a UIDPLUS `APPENDUID` response code on APPEND.
    append_uid_plus: AtomicBool,
    /// Include a UIDPLUS `COPYUID` response code on UID MOVE/COPY.
    copy_uid_plus: AtomicBool,
    /// Store the APPEND and then drop the connection without sending the
    /// tagged completion (models a lost receipt after the server committed).
    append_drop_completion: AtomicBool,
    /// Apply `UID MOVE` and then drop the connection without sending the
    /// tagged completion. Models a move whose receipt was lost even though the
    /// server committed it.
    move_drop_completion: AtomicBool,
    /// Apply `UID COPY` and then drop the connection without sending the
    /// tagged completion. Models a copy whose receipt was lost even though the
    /// server committed it.
    copy_drop_completion: AtomicBool,
    /// Apply `UID STORE` and then drop the connection without sending the
    /// tagged completion. Models a flag update whose receipt was lost even
    /// though the server committed it.
    store_drop_completion: AtomicBool,
    /// Read the APPEND literal (so it is provably on the wire) and answer a
    /// tagged `NO` without storing it. Models a server that explicitly refuses
    /// an upload.
    fail_append: AtomicBool,
    /// Folder names whose `DELETE` answers `NO` (used to model servers that
    /// refuse to remove a mailbox and leave it selected).
    fail_deletes: Mutex<Vec<String>>,
    /// Every command received, as `"COMMAND args"` in arrival order.
    commands: Mutex<Vec<String>>,
    /// Commands received per accepted connection (index = accept order). Lets
    /// a timeout test prove that a poisoned connection received nothing after
    /// the command that hung.
    connection_commands: Mutex<Vec<Vec<String>>>,
    /// Connections whose handler is still open (not closed by either side).
    open_connections: AtomicUsize,
    /// Command names that should be received but never acknowledged, one
    /// entry consumed per occurrence. Models a server that is half-dead.
    hang_next: Mutex<Vec<String>>,
    /// When set, an APPEND blocks until a permit is available. Lets a test
    /// pause the executor in the middle of an in-flight upload.
    append_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Called synchronously while handling a successful expunge. Tests use it
    /// to mutate the staged draft between validation and upload.
    on_expunge: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl MockState {
    /// Silently close connections that have been idle for `timeout`.
    pub fn set_idle_timeout(&self, timeout: Option<Duration>) {
        *self.idle_timeout.lock().expect("mock idle timeout lock") = timeout;
    }

    /// Make content SEARCH keys return no hits.
    pub fn set_content_search_empty(&self, value: bool) {
        self.content_search_empty.store(value, Ordering::SeqCst);
    }

    /// Make SELECT/EXAMINE leave out `UIDNEXT`, as NetEase 163 does.
    pub fn set_omit_uidnext(&self, value: bool) {
        self.omit_uidnext.store(value, Ordering::SeqCst);
    }

    /// Answer `UID SEARCH` with the given tagged status (`"NO"` / `"BAD"`)
    /// and no untagged result.
    pub fn set_fail_search(&self, status: Option<&str>) {
        *self.search_fail.lock().expect("mock search fail lock") =
            status.map(|value| value.to_string());
    }

    /// Make every `UID STORE` answer `NO` without changing flags.
    pub fn set_fail_store(&self, value: bool) {
        self.fail_store.store(value, Ordering::SeqCst);
    }

    /// Make every `EXPUNGE`/`UID EXPUNGE` answer `NO` without removing mail.
    pub fn set_fail_expunge(&self, value: bool) {
        self.fail_expunge.store(value, Ordering::SeqCst);
    }

    /// Pause every APPEND until a permit is added to `gate`.
    pub fn set_append_gate(&self, gate: Arc<tokio::sync::Semaphore>) {
        *self.append_gate.lock().expect("mock append gate lock") = Some(gate);
    }

    /// Run `hook` while a successful EXPUNGE/UID EXPUNGE is being handled.
    pub fn set_on_expunge<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.on_expunge.lock().expect("mock expunge hook lock") = Some(Arc::new(hook));
    }

    /// Emit `APPENDUID <uidvalidity> <uid>` on APPEND, as UIDPLUS servers do.
    pub fn set_append_uid_plus(&self, value: bool) {
        self.append_uid_plus.store(value, Ordering::SeqCst);
    }

    /// Emit `COPYUID <uidvalidity> <source set> <destination set>` on
    /// UID MOVE/COPY, as UIDPLUS servers do.
    pub fn set_copy_uid_plus(&self, value: bool) {
        self.copy_uid_plus.store(value, Ordering::SeqCst);
    }

    /// Store the APPEND, then close the connection before the tagged
    /// completion. The client sees a network failure but the message exists.
    pub fn set_append_drop_completion(&self, value: bool) {
        self.append_drop_completion.store(value, Ordering::SeqCst);
    }

    /// Apply `UID MOVE`/`UID COPY`, then close the connection before the
    /// tagged completion. The copy is committed, but the client cannot know.
    pub fn set_move_drop_completion(&self, value: bool) {
        self.move_drop_completion.store(value, Ordering::SeqCst);
    }

    /// Apply `UID COPY`, then close the connection before the tagged
    /// completion. The copy is committed, but the client cannot know.
    pub fn set_copy_drop_completion(&self, value: bool) {
        self.copy_drop_completion.store(value, Ordering::SeqCst);
    }

    /// Apply `UID STORE`, then close the connection before the tagged
    /// completion. The flags are committed, but the client cannot know.
    pub fn set_store_drop_completion(&self, value: bool) {
        self.store_drop_completion.store(value, Ordering::SeqCst);
    }

    /// Accept the APPEND literal but answer `NO`, storing nothing.
    pub fn set_fail_append(&self, value: bool) {
        self.fail_append.store(value, Ordering::SeqCst);
    }

    /// Make `DELETE` of `folder` answer `NO`, leaving the mailbox in place.
    pub fn fail_delete(&self, folder: &str) {
        self.fail_deletes
            .lock()
            .expect("mock fail deletes lock")
            .push(folder.to_string());
    }

    /// Commands received so far, as `"COMMAND args"`.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("mock commands lock").clone()
    }

    /// Commands received on one accepted connection (accept order).
    pub fn commands_on_connection(&self, index: usize) -> Vec<String> {
        self.connection_commands
            .lock()
            .expect("mock connection commands lock")
            .get(index)
            .cloned()
            .unwrap_or_default()
    }

    /// Number of commands received on each accepted connection.
    pub fn connection_command_counts(&self) -> Vec<usize> {
        self.connection_commands
            .lock()
            .expect("mock connection commands lock")
            .iter()
            .map(Vec::len)
            .collect()
    }

    /// Accept the next occurrence of `command` (case-insensitive) and never
    /// answer it. The command is still recorded, so a test can prove it
    /// reached the server.
    pub fn hang_next_command(&self, command: &str) {
        self.hang_next
            .lock()
            .expect("mock hang lock")
            .push(command.to_ascii_uppercase());
    }

    fn take_hang(&self, command: &str) -> bool {
        let mut hangs = self.hang_next.lock().expect("mock hang lock");
        match hangs
            .iter()
            .position(|item| item.eq_ignore_ascii_case(command))
        {
            Some(index) => {
                hangs.remove(index);
                true
            }
            None => false,
        }
    }

    fn idle_timeout(&self) -> Option<Duration> {
        *self.idle_timeout.lock().expect("mock idle timeout lock")
    }

    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Connections whose handler has not returned (still open).
    pub fn open_connection_count(&self) -> usize {
        self.open_connections.load(Ordering::SeqCst)
    }

    pub fn add_message(&self, folder: &str, raw: &[u8]) -> u32 {
        let mut folders = self.folders.lock().expect("mock folders lock");
        let folder = folders
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .expect("mock folder exists");
        let uid = folder.next_uid;
        folder.next_uid += 1;
        let modseq = folder.next_modseq;
        folder.next_modseq += 1;
        folder.messages.push(MockMessage {
            uid,
            flags: Vec::new(),
            internaldate: now_internaldate(),
            raw: raw.to_vec(),
            modseq,
            gm_msgid: None,
            gm_thrid: None,
            gm_labels: Vec::new(),
        });
        uid
    }

    /// Add a message carrying Gmail extension metadata.
    pub fn add_gmail_message(
        &self,
        folder: &str,
        raw: &[u8],
        gm_msgid: u64,
        gm_thrid: u64,
        labels: &[&str],
    ) -> u32 {
        let uid = self.add_message(folder, raw);
        let mut folders = self.folders.lock().expect("mock folders lock");
        let folder = folders
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .expect("mock folder exists");
        if let Some(message) = folder.messages.iter_mut().find(|item| item.uid == uid) {
            message.gm_msgid = Some(gm_msgid);
            message.gm_thrid = Some(gm_thrid);
            message.gm_labels = labels.iter().map(|value| value.to_string()).collect();
        }
        uid
    }

    pub fn set_flags(&self, folder: &str, uid: u32, flags: &[&str]) {
        let mut folders = self.folders.lock().expect("mock folders lock");
        let folder = folders
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .expect("mock folder exists");
        let modseq = folder.next_modseq;
        folder.next_modseq += 1;
        if let Some(message) = folder.messages.iter_mut().find(|item| item.uid == uid) {
            message.flags = flags.iter().map(|value| value.to_string()).collect();
            message.modseq = modseq;
        }
    }

    pub fn remove_message(&self, folder: &str, uid: u32) {
        let mut folders = self.folders.lock().expect("mock folders lock");
        let folder = folders
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .expect("mock folder exists");
        folder.messages.retain(|message| message.uid != uid);
    }

    pub fn message_flags(&self, folder: &str, uid: u32) -> Vec<String> {
        let folders = self.folders.lock().expect("mock folders lock");
        folders
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .and_then(|folder| folder.messages.iter().find(|message| message.uid == uid))
            .map(|message| message.flags.clone())
            .unwrap_or_default()
    }

    pub fn bump_uidvalidity(&self, folder: &str, value: u32) {
        let mut folders = self.folders.lock().expect("mock folders lock");
        let folder = folders
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .expect("mock folder exists");
        folder.uidvalidity = value;
    }

    pub fn message_count(&self, folder: &str) -> usize {
        let folders = self.folders.lock().expect("mock folders lock");
        folders
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .map(|folder| folder.messages.len())
            .unwrap_or(0)
    }

    /// Raw bytes of every message in a folder, in UID order.
    pub fn message_bodies(&self, folder: &str) -> Vec<Vec<u8>> {
        let folders = self.folders.lock().expect("mock folders lock");
        folders
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(folder))
            .map(|item| {
                item.messages
                    .iter()
                    .map(|message| message.raw.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub struct MockImap {
    pub addr: std::net::SocketAddr,
    pub state: Arc<MockState>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for MockImap {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl MockImap {
    pub async fn start(capabilities: &[&str]) -> Self {
        Self::start_with_folders(capabilities, &[("INBOX", &[r"\Inbox"][..])]).await
    }

    pub async fn start_with_folders(capabilities: &[&str], folders: &[(&str, &[&str])]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock IMAP");
        let addr = listener.local_addr().expect("mock address");
        let mut caps = vec!["IMAP4rev1".to_string()];
        caps.extend(capabilities.iter().map(|value| value.to_string()));
        let state = Arc::new(MockState {
            username: "test@example.com".into(),
            password: "secret".into(),
            capabilities: caps,
            connections: AtomicUsize::new(0),
            idle_timeout: Mutex::new(None),
            content_search_empty: AtomicBool::new(false),
            omit_uidnext: AtomicBool::new(false),
            search_fail: Mutex::new(None),
            fail_store: AtomicBool::new(false),
            fail_expunge: AtomicBool::new(false),
            append_uid_plus: AtomicBool::new(false),
            copy_uid_plus: AtomicBool::new(false),
            append_drop_completion: AtomicBool::new(false),
            move_drop_completion: AtomicBool::new(false),
            copy_drop_completion: AtomicBool::new(false),
            store_drop_completion: AtomicBool::new(false),
            fail_append: AtomicBool::new(false),
            fail_deletes: Mutex::new(Vec::new()),
            append_gate: Mutex::new(None),
            on_expunge: Mutex::new(None),
            commands: Mutex::new(Vec::new()),
            connection_commands: Mutex::new(Vec::new()),
            open_connections: AtomicUsize::new(0),
            hang_next: Mutex::new(Vec::new()),
            folders: Mutex::new(
                folders
                    .iter()
                    .map(|(name, attributes)| MockFolder::new(name, attributes))
                    .collect(),
            ),
        });
        let cancel = CancellationToken::new();
        let accept_state = state.clone();
        let accept_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let index =
                                    accept_state.connections.fetch_add(1, Ordering::SeqCst);
                                accept_state.open_connections.fetch_add(1, Ordering::SeqCst);
                                {
                                    let mut per = accept_state
                                        .connection_commands
                                        .lock()
                                        .expect("mock connection commands lock");
                                    while per.len() <= index {
                                        per.push(Vec::new());
                                    }
                                }
                                let state = accept_state.clone();
                                tokio::spawn(async move {
                                    let _ = handle_connection(stream, state.clone(), index).await;
                                    state.open_connections.fetch_sub(1, Ordering::SeqCst);
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
        Self {
            addr,
            state,
            cancel,
            task,
        }
    }
}

struct Session {
    selected: Option<String>,
}

async fn handle_connection(
    stream: TcpStream,
    state: Arc<MockState>,
    connection_index: usize,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    write_str(&mut reader, "* OK Nuncio mock IMAP ready\r\n").await?;
    let mut session = Session { selected: None };
    loop {
        // Some providers (NetEase 163 among them) silently drop a socket that
        // has been idle for a while. The optional timeout reproduces that.
        let line = match state.idle_timeout() {
            Some(timeout) => match tokio::time::timeout(timeout, read_line(&mut reader)).await {
                Ok(result) => result?,
                Err(_) => return Ok(()),
            },
            None => read_line(&mut reader).await?,
        };
        let Some(line) = line else {
            return Ok(());
        };
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            continue;
        }
        if !handle_command(&mut reader, &state, &mut session, &line, connection_index).await? {
            return Ok(());
        }
    }
}

async fn handle_command(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &mut Session,
    line: &str,
    connection_index: usize,
) -> io::Result<bool> {
    let tokens = tokenize(line);
    if tokens.len() < 2 {
        return Ok(true);
    }
    let tag = tokens[0].clone();
    let command = tokens[1].to_ascii_uppercase();
    let args = &tokens[2..];
    let recorded = format!("{command} {}", args.join(" "));
    state
        .commands
        .lock()
        .expect("mock commands lock")
        .push(recorded.clone());
    if let Some(per) = state
        .connection_commands
        .lock()
        .expect("mock connection commands lock")
        .get_mut(connection_index)
    {
        per.push(recorded);
    }

    // Test-only knob: a SEARCH whose criteria carry this marker is answered
    // slowly, for exercising the FETCH/SEARCH budget independently of the
    // interactive command budget.
    if command == "UID" && line.contains("audit-slow-search") {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Received but deliberately never acknowledged: models a server whose
    // socket is half-dead. The connection stays open (and silent) until the
    // client gives up.
    if state.take_hang(&command) {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        return Ok(false);
    }

    match command.as_str() {
        "CAPABILITY" => {
            let caps = state.capabilities.join(" ");
            write_str(
                reader,
                &format!("* CAPABILITY {caps}\r\n{tag} OK CAPABILITY completed\r\n"),
            )
            .await?;
        }
        "LOGIN" => {
            let user = args.first().cloned().unwrap_or_default();
            let pass = args.get(1).cloned().unwrap_or_default();
            if user == state.username && pass == state.password {
                write_str(reader, &format!("{tag} OK LOGIN completed\r\n")).await?;
            } else {
                write_str(
                    reader,
                    &format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n"),
                )
                .await?;
            }
        }
        "ENABLE" => {
            write_str(reader, &format!("{tag} OK ENABLE completed\r\n")).await?;
        }
        "LIST" | "LSUB" => {
            let lines: Vec<String> = {
                let folders = state.folders.lock().expect("folders lock");
                folders
                    .iter()
                    .map(|folder| {
                        let attrs = if folder.attributes.is_empty() {
                            String::new()
                        } else {
                            folder.attributes.join(" ")
                        };
                        format!(
                            "* LIST ({attrs}) \"{}\" \"{}\"\r\n",
                            folder.delimiter, folder.name
                        )
                    })
                    .collect()
            };
            for line in lines {
                write_str(reader, &line).await?;
            }
            write_str(reader, &format!("{tag} OK LIST completed\r\n")).await?;
        }
        "SELECT" | "EXAMINE" => {
            let name = args.first().cloned().unwrap_or_else(|| "INBOX".into());
            let details = {
                let folders = state.folders.lock().expect("folders lock");
                folders
                    .iter()
                    .find(|folder| folder.name.eq_ignore_ascii_case(&name))
                    .map(|folder| {
                        (
                            folder.name.clone(),
                            folder.messages.len(),
                            folder.uidvalidity,
                            folder.next_uid,
                            folder.next_modseq.saturating_sub(1),
                        )
                    })
            };
            let Some((name, exists, uidvalidity, next_uid, modseq)) = details else {
                write_str(reader, &format!("{tag} NO no such mailbox\r\n")).await?;
                return Ok(true);
            };
            let uidnext_line = if state.omit_uidnext.load(Ordering::SeqCst) {
                String::new()
            } else {
                format!("* OK [UIDNEXT {next_uid}] Predicted next UID\r\n")
            };
            write_str(
                reader,
                &format!(
                    "* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)\r\n\
                     * {exists} EXISTS\r\n* 0 RECENT\r\n\
                     * OK [UIDVALIDITY {uidvalidity}] UIDs valid\r\n\
                     {uidnext_line}\
                     * OK [HIGHESTMODSEQ {modseq}] Highest\r\n"
                ),
            )
            .await?;
            session.selected = Some(name);
            write_str(
                reader,
                &format!("{tag} OK [READ-WRITE] SELECT completed\r\n"),
            )
            .await?;
        }
        "NOOP" => {
            write_str(reader, &format!("{tag} OK NOOP completed\r\n")).await?;
        }
        "LOGOUT" => {
            write_str(
                reader,
                &format!("* BYE logging out\r\n{tag} OK LOGOUT completed\r\n"),
            )
            .await?;
            return Ok(false);
        }
        "IDLE" => {
            write_str(reader, "+ idling\r\n").await?;
            loop {
                let Some(line) = read_line(reader).await? else {
                    return Ok(false);
                };
                if line.trim().eq_ignore_ascii_case("DONE") {
                    write_str(reader, &format!("{tag} OK IDLE terminated\r\n")).await?;
                    break;
                }
            }
        }
        "APPEND" => {
            let Some(mailbox) = args.first().cloned() else {
                write_str(reader, &format!("{tag} BAD APPEND args\r\n")).await?;
                return Ok(true);
            };
            let Some(open) = line.rfind('{') else {
                write_str(reader, &format!("{tag} BAD APPEND literal\r\n")).await?;
                return Ok(true);
            };
            let Some(close) = line.rfind('}') else {
                write_str(reader, &format!("{tag} BAD APPEND literal\r\n")).await?;
                return Ok(true);
            };
            let len: usize = line[open + 1..close].parse().unwrap_or(0);
            // Tests can hold the APPEND here to discard/send a draft while the
            // executor is genuinely in flight.
            let gate = state
                .append_gate
                .lock()
                .expect("mock append gate lock")
                .clone();
            if let Some(gate) = gate {
                let _permit = gate.acquire().await.expect("mock append gate was closed");
            }
            write_str(reader, "+ Ready for literal data\r\n").await?;
            let mut data = vec![0u8; len];
            reader.read_exact(&mut data).await?;
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf).await;
            // Test-only knob: once the literal is on the wire, emit a tunable
            // burst of unsolicited `EXISTS` before the tagged completion. A
            // per-read timeout would keep extending; the total APPEND budget
            // must still expire.
            if data
                .windows(b"AUDIT_APPEND_CHATTER".len())
                .any(|window| window == b"AUDIT_APPEND_CHATTER")
            {
                for _ in 0..8 {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    write_str(reader, "* 1 EXISTS\r\n").await?;
                }
            }
            if state.fail_append.load(Ordering::SeqCst) {
                // The literal was received but the server refuses it: the
                // client may retry the same APPEND safely.
                write_str(reader, &format!("{tag} NO APPEND rejected\r\n")).await?;
                return Ok(true);
            }
            let flags: Vec<String> = line
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(inside, _)| inside.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
            let appended = {
                let mut folders = state.folders.lock().expect("folders lock");
                folders
                    .iter_mut()
                    .find(|folder| folder.name.eq_ignore_ascii_case(&mailbox))
                    .map(|folder| {
                        let uid = folder.next_uid;
                        folder.next_uid += 1;
                        let modseq = folder.next_modseq;
                        folder.next_modseq += 1;
                        folder.messages.push(MockMessage {
                            uid,
                            flags,
                            internaldate: now_internaldate(),
                            raw: data,
                            modseq,
                            gm_msgid: None,
                            gm_thrid: None,
                            gm_labels: Vec::new(),
                        });
                        (folder.uidvalidity, uid)
                    })
            };
            if state.append_drop_completion.load(Ordering::SeqCst) {
                // The message is committed, but the client never learns it.
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "mock APPEND completion lost",
                ));
            }
            let code = appended
                .filter(|_| state.append_uid_plus.load(Ordering::SeqCst))
                .map(|(validity, uid)| format!(" [APPENDUID {validity} {uid}]"))
                .unwrap_or_default();
            write_str(reader, &format!("{tag} OK{code} APPEND completed\r\n")).await?;
        }
        "CREATE" => {
            let name = args.first().cloned().unwrap_or_default();
            {
                let mut folders = state.folders.lock().expect("folders lock");
                if !folders
                    .iter()
                    .any(|folder| folder.name.eq_ignore_ascii_case(&name))
                {
                    folders.push(MockFolder::new(&name, &[]));
                }
            }
            write_str(reader, &format!("{tag} OK CREATE completed\r\n")).await?;
        }
        "DELETE" => {
            let name = args.first().cloned().unwrap_or_default();
            let fails = state
                .fail_deletes
                .lock()
                .expect("fail deletes lock")
                .iter()
                .any(|folder| folder.eq_ignore_ascii_case(&name));
            if fails {
                write_str(reader, &format!("{tag} NO DELETE failed\r\n")).await?;
            } else {
                state
                    .folders
                    .lock()
                    .expect("folders lock")
                    .retain(|folder| !folder.name.eq_ignore_ascii_case(&name));
                write_str(reader, &format!("{tag} OK DELETE completed\r\n")).await?;
            }
        }
        "RENAME" => {
            let from = args.first().cloned().unwrap_or_default();
            let to = args.get(1).cloned().unwrap_or_default();
            {
                let mut folders = state.folders.lock().expect("folders lock");
                if let Some(folder) = folders
                    .iter_mut()
                    .find(|folder| folder.name.eq_ignore_ascii_case(&from))
                {
                    folder.name = to;
                }
            }
            write_str(reader, &format!("{tag} OK RENAME completed\r\n")).await?;
        }
        "SUBSCRIBE" | "UNSUBSCRIBE" => {
            write_str(reader, &format!("{tag} OK completed\r\n")).await?;
        }
        "UID" => {
            let sub = args
                .first()
                .map(|value| value.to_ascii_uppercase())
                .unwrap_or_default();
            let rest = &args[1..];
            match sub.as_str() {
                "SEARCH" => handle_search(reader, state, session, &tag, rest).await?,
                "FETCH" => handle_fetch(reader, state, session, &tag, rest).await?,
                "STORE" => handle_store(reader, state, session, &tag, rest).await?,
                "COPY" => handle_copy(reader, state, session, &tag, rest, false).await?,
                "MOVE" => handle_copy(reader, state, session, &tag, rest, true).await?,
                "EXPUNGE" => {
                    let spec = rest.first().cloned().unwrap_or_else(|| "1:*".into());
                    expunge(reader, state, session, &tag, Some(&spec)).await?;
                }
                _ => write_str(reader, &format!("{tag} BAD unknown UID command\r\n")).await?,
            }
        }
        "EXPUNGE" => expunge(reader, state, session, &tag, None).await?,
        _ => {
            write_str(
                reader,
                &format!("{tag} BAD unsupported command {command}\r\n"),
            )
            .await?;
        }
    }
    Ok(true)
}

async fn handle_search(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &Session,
    tag: &str,
    args: &[String],
) -> io::Result<()> {
    let Some(folder_name) = session.selected.clone() else {
        write_str(reader, &format!("{tag} BAD no mailbox selected\r\n")).await?;
        return Ok(());
    };
    let search_fail = state
        .search_fail
        .lock()
        .expect("mock search fail lock")
        .clone();
    if let Some(status) = search_fail {
        // No untagged `SEARCH` line: a parser that ignores the tagged status
        // would report an empty result here.
        write_str(reader, &format!("{tag} {status} SEARCH failed\r\n")).await?;
        return Ok(());
    }
    let criteria = args.join(" ").to_ascii_uppercase();
    let content_key = ["TEXT", "SUBJECT", "FROM", "TO", "CC", "BODY", "HEADER"]
        .iter()
        .any(|key| criteria.split_whitespace().any(|token| token == *key));
    if content_key && state.content_search_empty.load(Ordering::SeqCst) {
        write_str(reader, "* SEARCH\r\n").await?;
        write_str(reader, &format!("{tag} OK SEARCH completed\r\n")).await?;
        return Ok(());
    }
    let uids: Vec<String> = {
        let folders = state.folders.lock().expect("folders lock");
        let folder = folders
            .iter()
            .find(|folder| folder.name.eq_ignore_ascii_case(&folder_name))
            .expect("selected folder");
        let start = criteria
            .strip_prefix("UID ")
            .and_then(|value| value.split(':').next())
            .and_then(|value| value.parse::<u32>().ok());
        // `UID lo:hi` (bounded range); `hi` may be `*`.
        let end = criteria
            .strip_prefix("UID ")
            .and_then(|value| value.split(':').nth(1))
            .and_then(|value| value.parse::<u32>().ok());
        folder
            .messages
            .iter()
            .filter(|message| start.map(|start| message.uid >= start).unwrap_or(true))
            .filter(|message| end.map(|end| message.uid <= end).unwrap_or(true))
            .map(|message| message.uid.to_string())
            .collect()
    };
    write_str(reader, &format!("* SEARCH {}\r\n", uids.join(" "))).await?;
    write_str(reader, &format!("{tag} OK SEARCH completed\r\n")).await?;
    Ok(())
}

async fn handle_fetch(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &Session,
    tag: &str,
    args: &[String],
) -> io::Result<()> {
    let Some(folder_name) = session.selected.clone() else {
        write_str(reader, &format!("{tag} BAD no mailbox selected\r\n")).await?;
        return Ok(());
    };
    let spec = args.first().cloned().unwrap_or_else(|| "1:*".into());
    let query = args[1..].join(" ").to_ascii_uppercase();
    let changed_since = query
        .split("CHANGEDSINCE")
        .nth(1)
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok());
    let responses: Vec<(usize, Vec<FetchChunk>)> = {
        let folders = state.folders.lock().expect("folders lock");
        let folder = folders
            .iter()
            .find(|folder| folder.name.eq_ignore_ascii_case(&folder_name))
            .expect("selected folder");
        let max_uid = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
        let wanted = parse_uid_set(&spec, max_uid);
        let mut responses = Vec::new();
        for (index, message) in folder.messages.iter().enumerate() {
            if !wanted.contains(&message.uid) {
                continue;
            }
            if let Some(modseq) = changed_since {
                if message.modseq <= modseq {
                    continue;
                }
            }
            responses.push((
                index,
                fetch_response(query.as_str(), message, index as u32 + 1),
            ));
        }
        responses
    };
    for (index, chunks) in responses {
        write_str(reader, &format!("* {} FETCH (", index + 1)).await?;
        let mut first = true;
        for chunk in chunks {
            if !first {
                write_str(reader, " ").await?;
            }
            match chunk {
                FetchChunk::Text(text) => write_str(reader, &text).await?,
                FetchChunk::Literal { key, data } => {
                    write_str(reader, &format!("{key} {{{}}}\r\n", data.len())).await?;
                    reader.get_mut().write_all(&data).await?;
                }
            }
            first = false;
        }
        write_str(reader, ")\r\n").await?;
    }
    write_str(reader, &format!("{tag} OK FETCH completed\r\n")).await?;
    Ok(())
}

enum FetchChunk {
    Text(String),
    Literal { key: String, data: Vec<u8> },
}

fn fetch_response(query: &str, message: &MockMessage, seq: u32) -> Vec<FetchChunk> {
    let mut chunks = vec![
        FetchChunk::Text(format!("UID {}", message.uid)),
        FetchChunk::Text(format!("FLAGS ({})", message.flags.join(" "))),
        FetchChunk::Text(format!("INTERNALDATE \"{}\"", message.internaldate)),
        FetchChunk::Text(format!("RFC822.SIZE {}", message.raw.len())),
        FetchChunk::Text(format!("MODSEQ ({})", message.modseq)),
    ];
    if query.contains("ENVELOPE") {
        chunks.push(FetchChunk::Text(format!("ENVELOPE {}", envelope(message))));
    }
    if query.contains("BODYSTRUCTURE") {
        chunks.push(FetchChunk::Text(format!(
            "BODYSTRUCTURE {}",
            bodystructure(message)
        )));
    }
    if query.contains("X-GM-MSGID") {
        if let Some(value) = message.gm_msgid {
            chunks.push(FetchChunk::Text(format!("X-GM-MSGID {value}")));
        }
    }
    if query.contains("X-GM-THRID") {
        if let Some(value) = message.gm_thrid {
            chunks.push(FetchChunk::Text(format!("X-GM-THRID {value}")));
        }
    }
    if query.contains("X-GM-LABELS") && !message.gm_labels.is_empty() {
        let labels = message
            .gm_labels
            .iter()
            .map(|value| quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        chunks.push(FetchChunk::Text(format!("X-GM-LABELS ({labels})")));
    }
    if let Some(section) = requested_section(query) {
        let data = section_data(&section, message);
        chunks.push(FetchChunk::Literal {
            key: format!("BODY[{section}]"),
            data,
        });
    }
    let _ = seq;
    chunks
}

fn requested_section(query: &str) -> Option<String> {
    let start = query.find("BODY.PEEK[")? + "BODY.PEEK[".len();
    let end = query[start..].find(']')? + start;
    Some(query[start..end].to_string())
}

fn section_data(section: &str, message: &MockMessage) -> Vec<u8> {
    let upper = section.to_ascii_uppercase();
    if upper.starts_with("HEADER.FIELDS") {
        if let Some(end) = message
            .raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            return message.raw[..end + 4].to_vec();
        }
        return message.raw.clone();
    }
    let parsed = nuncio_mime::parse(&message.raw).ok();
    let body = parsed
        .as_ref()
        .and_then(|parsed| parsed.text.clone())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|parsed| parsed.html.as_deref().map(nuncio_mime::html_to_text))
        })
        .unwrap_or_default();
    if upper == "TEXT" || upper == "1" || upper == "1.1" {
        return body.into_bytes();
    }
    message.raw.clone()
}

fn envelope(message: &MockMessage) -> String {
    let parsed = nuncio_mime::parse(&message.raw).ok();
    let headers = parsed.as_ref().map(|parsed| &parsed.headers);
    let date = headers
        .and_then(|headers| headers.date)
        .map(|date| quote(&date.to_rfc2822()))
        .unwrap_or_else(|| "NIL".into());
    let subject = headers
        .map(|headers| quote(&headers.subject))
        .unwrap_or_else(|| "NIL".into());
    let from = headers
        .and_then(|headers| headers.from.as_ref())
        .map(|address| address_list(std::slice::from_ref(address)))
        .unwrap_or_else(|| "NIL".into());
    let to = headers
        .map(|headers| address_list(&headers.to))
        .unwrap_or_else(|| "NIL".into());
    let cc = headers
        .map(|headers| address_list(&headers.cc))
        .unwrap_or_else(|| "NIL".into());
    let bcc = headers
        .map(|headers| address_list(&headers.bcc))
        .unwrap_or_else(|| "NIL".into());
    let reply_to = headers
        .map(|headers| {
            if headers.reply_to.is_empty() {
                "NIL".into()
            } else {
                address_list(&headers.reply_to)
            }
        })
        .unwrap_or_else(|| "NIL".into());
    let in_reply_to = headers
        .and_then(|headers| headers.in_reply_to.as_deref())
        .map(quote)
        .unwrap_or_else(|| "NIL".into());
    let message_id = headers
        .and_then(|headers| headers.message_id.as_deref())
        .map(quote)
        .unwrap_or_else(|| "NIL".into());
    format!(
        "({date} {subject} {from} {from} {reply_to} {to} {cc} {bcc} {in_reply_to} {message_id})"
    )
}

fn address_list(addresses: &[Address]) -> String {
    if addresses.is_empty() {
        return "NIL".into();
    }
    let entries: Vec<String> = addresses
        .iter()
        .map(|address| {
            let (mailbox, host) = address
                .email
                .rsplit_once('@')
                .unwrap_or((address.email.as_str(), ""));
            let name = address
                .name
                .as_deref()
                .map(quote)
                .unwrap_or_else(|| "NIL".into());
            format!("({name} NIL {} {})", quote(mailbox), quote(host))
        })
        .collect();
    format!("({})", entries.join(" "))
}

fn bodystructure(message: &MockMessage) -> String {
    let parsed = nuncio_mime::parse(&message.raw).ok();
    let body = parsed
        .as_ref()
        .and_then(|parsed| parsed.text.clone())
        .unwrap_or_default();
    let lines = body.lines().count().max(1);
    format!(
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" {} {lines})",
        body.len()
    )
}

async fn handle_store(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &Session,
    tag: &str,
    args: &[String],
) -> io::Result<()> {
    if state.fail_store.load(Ordering::SeqCst) {
        write_str(reader, &format!("{tag} NO STORE rejected\r\n")).await?;
        return Ok(());
    }
    let Some(folder_name) = session.selected.clone() else {
        write_str(reader, &format!("{tag} BAD no mailbox selected\r\n")).await?;
        return Ok(());
    };
    let spec = args.first().cloned().unwrap_or_default();
    let operation = args.join(" ");
    let silent = operation.to_ascii_uppercase().contains(".SILENT");
    let add = operation.to_ascii_uppercase().contains("+FLAGS");
    let remove = operation.to_ascii_uppercase().contains("-FLAGS");
    let flags: Vec<String> = operation
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inside, _)| inside.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let updates: Vec<(usize, u32, Vec<String>)> = {
        let mut folders = state.folders.lock().expect("folders lock");
        let folder = folders
            .iter_mut()
            .find(|folder| folder.name.eq_ignore_ascii_case(&folder_name))
            .expect("selected folder");
        let max_uid = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
        let wanted = parse_uid_set(&spec, max_uid);
        let modseq = folder.next_modseq;
        folder.next_modseq += 1;
        let mut updates = Vec::new();
        for (index, message) in folder.messages.iter_mut().enumerate() {
            if !wanted.contains(&message.uid) {
                continue;
            }
            if add {
                for flag in &flags {
                    if !message
                        .flags
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case(flag))
                    {
                        message.flags.push(flag.clone());
                    }
                }
            } else if remove {
                message
                    .flags
                    .retain(|value| !flags.iter().any(|flag| flag.eq_ignore_ascii_case(value)));
            } else {
                message.flags = flags.clone();
            }
            message.modseq = modseq;
            updates.push((index, message.uid, message.flags.clone()));
        }
        updates
    };
    if state.store_drop_completion.load(Ordering::SeqCst) {
        // The flags are committed, but the client never learns it. A retry
        // may be skipped after the writer moves on; the executor must keep
        // the take-off `Unknown` in that case.
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "mock STORE completion lost",
        ));
    }
    if !silent {
        for (index, uid, flags) in updates {
            write_str(
                reader,
                &format!(
                    "* {} FETCH (UID {} FLAGS ({}))\r\n",
                    index + 1,
                    uid,
                    flags.join(" ")
                ),
            )
            .await?;
        }
    }
    write_str(reader, &format!("{tag} OK STORE completed\r\n")).await?;
    Ok(())
}

async fn handle_copy(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &Session,
    tag: &str,
    args: &[String],
    remove_source: bool,
) -> io::Result<()> {
    let Some(folder_name) = session.selected.clone() else {
        write_str(reader, &format!("{tag} BAD no mailbox selected\r\n")).await?;
        return Ok(());
    };
    let spec = args.first().cloned().unwrap_or_default();
    let target = args.get(1).cloned().unwrap_or_default();
    let mut copyuid: Option<(u32, Vec<i64>, Vec<i64>)> = None;
    let target_exists = {
        let folders = state.folders.lock().expect("folders lock");
        folders
            .iter()
            .any(|folder| folder.name.eq_ignore_ascii_case(&target))
    };
    if !target_exists {
        write_str(reader, &format!("{tag} NO no such mailbox\r\n")).await?;
        return Ok(());
    }
    {
        let mut folders = state.folders.lock().expect("folders lock");
        let source_index = folders
            .iter()
            .position(|folder| folder.name.eq_ignore_ascii_case(&folder_name))
            .expect("selected folder");
        let target_index = folders
            .iter()
            .position(|folder| folder.name.eq_ignore_ascii_case(&target))
            .expect("target folder");
        if source_index != target_index {
            let max_uid = folders[source_index]
                .messages
                .iter()
                .map(|m| m.uid)
                .max()
                .unwrap_or(0);
            let wanted = parse_uid_set(&spec, max_uid);
            let messages: Vec<MockMessage> = folders[source_index]
                .messages
                .iter()
                .filter(|message| wanted.contains(&message.uid))
                .cloned()
                .collect();
            let mut sources = Vec::new();
            let mut destinations = Vec::new();
            for mut message in messages {
                let target = &mut folders[target_index];
                sources.push(message.uid as i64);
                destinations.push(target.next_uid as i64);
                message.uid = target.next_uid;
                target.next_uid += 1;
                message.modseq = target.next_modseq;
                target.next_modseq += 1;
                target.messages.push(message);
            }
            // COPYUID reports the *destination* mailbox's UIDVALIDITY, not
            // the source's (RFC 4315 §3).
            copyuid = Some((folders[target_index].uidvalidity, sources, destinations));
            if remove_source {
                let uids: HashSet<u32> = folders[source_index]
                    .messages
                    .iter()
                    .filter(|message| wanted.contains(&message.uid))
                    .map(|message| message.uid)
                    .collect();
                folders[source_index]
                    .messages
                    .retain(|message| !uids.contains(&message.uid));
            }
        }
    }
    if remove_source && state.move_drop_completion.load(Ordering::SeqCst) {
        // The move is committed, but the client never learns it.
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "mock MOVE completion lost",
        ));
    }
    if !remove_source && state.copy_drop_completion.load(Ordering::SeqCst) {
        // The copy is committed, but the client never learns it.
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "mock COPY completion lost",
        ));
    }
    if state.copy_uid_plus.load(Ordering::SeqCst) {
        if let Some((uidvalidity, sources, destinations)) = copyuid {
            if !sources.is_empty() && sources.len() == destinations.len() {
                let source_set = sources
                    .iter()
                    .map(|uid| uid.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let destination_set = destinations
                    .iter()
                    .map(|uid| uid.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                write_str(
                    reader,
                    &format!(
                        "{tag} OK [COPYUID {uidvalidity} {source_set} {destination_set}] completed\r\n"
                    ),
                )
                .await?;
                return Ok(());
            }
        }
    }
    write_str(reader, &format!("{tag} OK completed\r\n")).await?;
    Ok(())
}

async fn expunge(
    reader: &mut BufReader<TcpStream>,
    state: &Arc<MockState>,
    session: &Session,
    tag: &str,
    spec: Option<&str>,
) -> io::Result<()> {
    if state.fail_expunge.load(Ordering::SeqCst) {
        write_str(reader, &format!("{tag} NO EXPUNGE rejected\r\n")).await?;
        return Ok(());
    }
    let Some(folder_name) = session.selected.clone() else {
        write_str(reader, &format!("{tag} BAD no mailbox selected\r\n")).await?;
        return Ok(());
    };
    let removed: Vec<usize> = {
        let mut folders = state.folders.lock().expect("folders lock");
        let folder = folders
            .iter_mut()
            .find(|folder| folder.name.eq_ignore_ascii_case(&folder_name))
            .expect("selected folder");
        let max_uid = folder.messages.iter().map(|m| m.uid).max().unwrap_or(0);
        let wanted = spec.map(|spec| parse_uid_set(spec, max_uid));
        let mut removed = Vec::new();
        for (index, message) in folder.messages.iter().enumerate() {
            if message
                .flags
                .iter()
                .any(|flag| flag.eq_ignore_ascii_case("\\Deleted"))
                && wanted
                    .as_ref()
                    .map(|wanted| wanted.contains(&message.uid))
                    .unwrap_or(true)
            {
                removed.push(index + 1);
            }
        }
        folder.messages.retain(|message| {
            !(message
                .flags
                .iter()
                .any(|flag| flag.eq_ignore_ascii_case("\\Deleted"))
                && wanted
                    .as_ref()
                    .map(|wanted| wanted.contains(&message.uid))
                    .unwrap_or(true))
        });
        removed
    };
    for seq in removed.into_iter().rev() {
        write_str(reader, &format!("* {seq} EXPUNGE\r\n")).await?;
    }
    // Tests that need to change the staged draft between the executor's
    // validation read and its upload hook in here.
    let hook = state
        .on_expunge
        .lock()
        .expect("mock expunge hook lock")
        .clone();
    if let Some(hook) = hook {
        hook();
    }
    write_str(reader, &format!("{tag} OK EXPUNGE completed\r\n")).await?;
    Ok(())
}

fn parse_uid_set(spec: &str, max_uid: u32) -> HashSet<u32> {
    let mut set = HashSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once(':') {
            let start = parse_uid_bound(start, max_uid);
            let end = parse_uid_bound(end, max_uid);
            let (low, high) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            for uid in low..=high {
                set.insert(uid);
            }
        } else {
            set.insert(parse_uid_bound(part, max_uid));
        }
    }
    set
}

fn parse_uid_bound(value: &str, max_uid: u32) -> u32 {
    if value.trim() == "*" {
        max_uid.max(1)
    } else {
        value.trim().parse().unwrap_or(1)
    }
}

fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn write_str(reader: &mut BufReader<TcpStream>, value: &str) -> io::Result<()> {
    reader.get_mut().write_all(value.as_bytes()).await?;
    reader.get_mut().flush().await
}

async fn read_line(reader: &mut BufReader<TcpStream>) -> io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}
