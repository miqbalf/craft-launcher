//! Live editing: a cloud file whose app can join a live session opens live when the server
//! offers it, instead of through the check-out.
//!
//! The launcher asks the server's Craft Office app for a join ticket with the account's app
//! password, writes it to a session file (`<data>/live/<random>.json`, mode 0600) and starts the
//! app with `--live-session <that file>` and a working copy of the file, which the app uses as its
//! first content and as an offline fallback. The app talks to the relay itself; the relay and the
//! server save the file. So a live file is never locked and never uploaded by the launcher: that
//! would fight the web editors. The ticket is short-lived, so the launcher writes a fresh one to
//! the same file (atomically) before it runs out, and removes the file once the app quits.
//!
//! When the server offers no ticket (no Craft Office app, no access, not this kind of file), or
//! its relay can't be reached, the check-out runs as before and the user is told live editing
//! isn't available. The ticket never reaches a log, an event or an error message.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{CloudAccount, LIVE_UNAVAILABLE, SyncStatus};
use craft_nextcloud::Account as DavAccount;
use craft_nextcloud::dav::Files;
use craft_nextcloud::http::{Http, Method, Request};
use craft_suite_account::TokenStore;
use serde::{Deserialize, Serialize};

use super::CloudService;
use super::checkout::{self, Book};

/// The session file format this launcher writes.
pub const SESSION_VERSION: u32 = 1;
/// The ticket endpoint of the server's Craft Office app, under the server's base URL.
pub const TICKET_PATH: &str = "/ocs/v2.php/apps/craft_office/api/v1/ticket";
/// Largest ticket answer read: it is a few hundred bytes.
const MAX_TICKET_ANSWER: usize = 64 * 1024;
/// Longest ticket accepted. The relay checks it; this only keeps a hostile answer small.
const MAX_TICKET: usize = 8 * 1024;
/// The shortest wait before a ticket is renewed, whatever lifetime the server gives.
const MIN_REFRESH: Duration = Duration::from_secs(5);
/// The first wait after a failed renewal, doubled after each further failure up to
/// [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(15);
const MAX_RETRY: Duration = Duration::from_secs(2 * 60);
/// How long the relay is given to accept a connection before live editing is called unreachable.
const RELAY_PROBE: Duration = Duration::from_secs(3);

/// An app started for a live session. The launcher polls it to learn when the session ends.
pub trait LiveProcess: Send {
    /// Whether the app has quit (or can no longer be followed).
    fn exited(&mut self) -> bool;
}

impl LiveProcess for std::process::Child {
    fn exited(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }
}

/// A join ticket, as the server's Craft Office app issued it. No `Debug`: the ticket is a secret.
pub(super) struct Ticket {
    ticket: String,
    relay_url: String,
    room: String,
    read_only: bool,
    display_name: String,
    expires_in: u64,
    /// The server has a worker saving the room right now (an edit ticket says false when not).
    saver: bool,
    /// When it was asked for: its lifetime counts from (at the latest) then.
    asked_at: jiff::Timestamp,
}

/// Why there is no ticket.
#[derive(Debug)]
pub(super) enum TicketError {
    /// The server answered with this status: no Craft Office app (404), no access (403), no
    /// relay configured or busy (503), or a sign-in it refused (401).
    Status(u16),
    Transport(String),
    /// The answer isn't a ticket this launcher can use.
    Malformed(&'static str),
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "the server answered HTTP {status}"),
            Self::Transport(why) => write!(f, "couldn't reach the server ({why})"),
            Self::Malformed(why) => write!(f, "the server's answer can't be used ({why})"),
        }
    }
}

/// The fields of the ticket answer this launcher reads (`ocs.data`); others are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketAnswer {
    ticket: String,
    relay_url: String,
    room: String,
    #[serde(default)]
    permissions: String,
    #[serde(default)]
    display_name: String,
    expires_in: u64,
    #[serde(default = "yes")]
    saver: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct OcsAnswer {
    ocs: OcsBody,
}

#[derive(Deserialize)]
struct OcsBody {
    data: TicketAnswer,
}

/// Asks `server` for a ticket to edit `file_id` live, as `login` with the app password
/// `password` (Basic auth, as every Nextcloud API call from the launcher).
pub(super) fn request_ticket<H: Http>(http: &H, server: &str, login: &str, password: &str, file_id: u64) -> Result<Ticket, TicketError> {
    let url = format!("{}{TICKET_PATH}", server.trim_end_matches('/'));
    let body = serde_json::json!({ "fileId": file_id }).to_string();
    let req = Request::new(Method::Post, url)
        .header("authorization", basic(login, password))
        .header("ocs-apirequest", "true")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(body.into_bytes())
        .max_body(MAX_TICKET_ANSWER);
    let resp = http.send(req).map_err(|e| TicketError::Transport(e.to_string()))?;
    if !(200..300).contains(&resp.status) {
        return Err(TicketError::Status(resp.status));
    }
    if resp.body.len() > MAX_TICKET_ANSWER {
        return Err(TicketError::Malformed("too large"));
    }
    let answer: OcsAnswer = serde_json::from_slice(&resp.body).map_err(|_| TicketError::Malformed("not a ticket"))?;
    let data = answer.ocs.data;
    if data.ticket.is_empty() || data.ticket.len() > MAX_TICKET || data.ticket.chars().any(char::is_control) {
        return Err(TicketError::Malformed("no usable ticket"));
    }
    if data.room.is_empty() || data.room.len() > 1024 {
        return Err(TicketError::Malformed("no room"));
    }
    // The server may give the relay as a path on itself (`/craft`): it is made absolute here,
    // so the app always gets a `wss://` address.
    let Some(relay_url) = resolve_relay(&data.relay_url, server) else {
        return Err(TicketError::Malformed("the relay address isn't wss://"));
    };
    if data.expires_in == 0 {
        return Err(TicketError::Malformed("an expired ticket"));
    }
    Ok(Ticket {
        ticket: data.ticket,
        relay_url,
        room: data.room,
        read_only: data.permissions == "read",
        display_name: data.display_name,
        expires_in: data.expires_in,
        saver: data.saver,
        asked_at: jiff::Timestamp::UNIX_EPOCH,
    })
}

/// The value of a Basic `Authorization` header.
fn basic(login: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{login}:{password}").as_bytes()))
}

/// Standard base64 with padding (RFC 4648 §4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The relay's WebSocket address from the ticket's `relayUrl`: a `wss://` (or, on this computer,
/// `ws://`) URL as given; an `https://`/`http://` URL with its scheme turned into `wss://`/`ws://`;
/// a path (`/craft`) on `server`. `None` when the result isn't an address [`relay_address`] takes.
pub fn resolve_relay(relay: &str, server: &str) -> Option<String> {
    let relay = relay.trim();
    let url = if relay.starts_with("wss://") || relay.starts_with("ws://") {
        relay.to_string()
    } else if let Some(rest) = relay.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = relay.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if relay.starts_with('/') && !relay.starts_with("//") {
        // A path is relative to the server's origin, as a browser resolves it.
        let (scheme, rest) = if let Some(r) = server.strip_prefix("https://") { ("wss", r) } else { ("ws", server.strip_prefix("http://")?) };
        let origin = rest.split('/').next().unwrap_or("");
        format!("{scheme}://{origin}{relay}")
    } else {
        return None;
    };
    relay_address(&url).map(|_| url)
}

/// The host and port a relay URL points at: `wss://` anywhere, `ws://` only on this computer
/// (tests). `None` for anything else.
pub fn relay_address(url: &str) -> Option<(String, u16)> {
    let (rest, default_port, loopback_only) =
        if let Some(r) = url.strip_prefix("wss://") { (r, 443, false) } else { (url.strip_prefix("ws://")?, 80, true) };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains(['@', ' ']) {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => default_port,
                None => return None,
            };
            (host.to_string(), port)
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), default_port),
        },
    };
    if host.is_empty() {
        return None;
    }
    if loopback_only && !matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "::1" | "localhost") {
        return None;
    }
    Some((host, port))
}

/// Whether the relay at `url` accepts a connection within a few seconds. Only the TCP handshake
/// is tried; the app makes the real connection.
pub fn relay_reachable(url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Some((host, port)) = relay_address(url) else { return false };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else { return false };
    addrs.into_iter().any(|addr| TcpStream::connect_timeout(&addr, RELAY_PROBE).is_ok())
}

/// The session file, as the app reads it. Field names are the hand-off contract
/// (`livepaste/DJ-brief.md` §2). No `Debug`: it holds the ticket.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionRecord<'a> {
    v: u32,
    server: &'a str,
    file_id: u64,
    path: &'a str,
    ticket: &'a str,
    relay_url: &'a str,
    room: &'a str,
    /// RFC 3339, UTC.
    expires_at: String,
    read_only: bool,
    display_name: &'a str,
    /// Extra to the contract (apps ignore unknown fields): whether the server is saving the room
    /// right now, as the web editor's "Changes are not being saved right now" note reads it.
    saver: bool,
}

/// A file open in its app for live editing. Kept with the open files, in memory only: its app
/// can't be followed across a launcher restart.
pub(super) struct LiveSession {
    pub account: String,
    /// The server path it was opened as: the key commands and events use.
    pub path: String,
    app: String,
    file_id: u64,
    server: String,
    /// `<data>/live/<id>.json`.
    session_file: PathBuf,
    /// `<data>/live/<id>/`, holding the working copy under the file's real name.
    folder: PathBuf,
    local: PathBuf,
    /// The working copy as downloaded: a copy still like this is removed when the session ends.
    downloaded: String,
    read_only: bool,
    /// Asked for read-only by the user: a renewed edit ticket doesn't change that.
    forced_read_only: bool,
    saver: bool,
    refresh_at: Instant,
    backoff: Duration,
    /// The last renewal failed (the user was told once).
    failing: bool,
    process: Box<dyn LiveProcess>,
}

/// How a live open ended.
pub(super) enum LiveOpen {
    /// The app is editing the file live.
    Opened,
    /// No live session for this file: the check-out runs instead. Logged, never shown.
    Unavailable(String),
    /// Something went wrong after the ticket (the download, the session file, the app launch):
    /// already told, and nothing is left behind.
    Failed,
}

/// Where a live session's files go.
struct Slot {
    session_file: PathBuf,
    folder: PathBuf,
    local: PathBuf,
}

/// A free, random slot under `root`, for the server file named `name`.
fn new_slot(root: &Path, name: &str) -> Result<Slot, String> {
    if name.is_empty() || name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err("That file's name can't be used on this computer.".to_string());
    }
    for _ in 0..8 {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| format!("no randomness for a session name ({e})"))?;
        let id = crate::http::hex(&bytes);
        let session_file = root.join(format!("{id}.json"));
        let folder = root.join(&id);
        if std::fs::symlink_metadata(&session_file).is_ok() || std::fs::symlink_metadata(&folder).is_ok() {
            continue;
        }
        let local = folder.join(name);
        return Ok(Slot { session_file, folder, local });
    }
    Err("no free name for a live session".to_string())
}

/// Creates `dir` readable by this user only (on Unix).
fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("couldn't protect {} ({e})", dir.display()))?;
    }
    Ok(())
}

/// The status line of a running live session.
fn live_detail(app_name: &str, read_only: bool, saver: bool) -> String {
    let mut detail = if read_only {
        format!("Open live in {app_name}, read-only: you see others' changes as they make them.")
    } else {
        format!("Open live in {app_name}: changes are saved to the cloud as you type, and others' changes show as they make them.")
    };
    if !read_only && !saver {
        detail.push_str(" The server isn't saving changes right now; they're kept in the live session until it can.");
    }
    detail.push_str(" The session ends when you close the app.");
    detail
}

/// The display name of an app id, for messages.
fn app_name(app: &str) -> &str {
    match app {
        "wordcraft" => "WordCraft",
        "gridcraft" => "GridCraft",
        "deckcraft" => "DeckCraft",
        "pdfcraft" => "PdfCraft",
        other => other,
    }
}

impl Book {
    /// The index of `account`'s live session of `path`.
    pub fn live_index(&self, account: &str, path: &str) -> Option<usize> {
        self.live.iter().position(|l| l.account == account && l.path == path)
    }
}

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    /// A ticket for `file_id` as the connected account.
    fn ticket(&self, file_id: u64) -> Result<Ticket, TicketError> {
        let Some((server, login, password)) = self.session_credentials() else { return Err(TicketError::Status(401)) };
        let asked_at = (self.wall)();
        request_ticket(&self.http(), &server, &login, password.expose(), file_id).map(|t| Ticket { asked_at, ..t })
    }

    /// The server's id of the file at `path`: from the last listings, else from a listing of its
    /// folder.
    fn file_id_for(&self, dav: &DavAccount, path: &str) -> Option<u64> {
        if let Some(id) = self.listed_file_id(path) {
            return Some(id);
        }
        let folder = super::sync::parent(path);
        match Files::new(dav).list(&self.http(), folder) {
            Ok(entries) => {
                let entries: Vec<_> = entries.into_iter().map(super::entry).collect();
                self.note_writable(&entries);
            }
            Err(e) => log::info!("couldn't list a cloud folder for a file id ({e})"),
        }
        self.listed_file_id(path)
    }

    /// Tries to open `path` live in `app`. The book is held by the caller, so no other action
    /// on the file runs meanwhile.
    pub(super) fn open_live(&self, book: &mut Book, account: &CloudAccount, dav: &DavAccount, path: &str, app: &str, read_only: bool) -> LiveOpen {
        let Some(file_id) = self.file_id_for(dav, path) else { return LiveOpen::Unavailable("the server gave no file id".to_string()) };
        // 503 is also a room that is busy for a moment: asked once more after a short wait.
        let ticket = match self.ticket(file_id) {
            Err(TicketError::Status(503)) => {
                std::thread::sleep(self.poll_every);
                self.ticket(file_id)
            }
            other => other,
        };
        let ticket = match ticket {
            Ok(t) => t,
            // The app password was refused: the user signs in again, rather than the check-out
            // meeting the same refusal.
            Err(TicketError::Status(401)) => {
                self.error(artcraft_launcher_core::cloud::SIGN_IN_EXPIRED);
                return LiveOpen::Failed;
            }
            Err(e) => return LiveOpen::Unavailable(format!("no live ticket: {e}")),
        };
        if !(self.relay_probe)(&ticket.relay_url) {
            return LiveOpen::Unavailable("the relay can't be reached".to_string());
        }
        let root = self.paths.live_root();
        let slot = match private_dir(&root).and_then(|()| new_slot(&root, super::sync::file_name(path))) {
            Ok(s) => s,
            Err(e) => {
                self.closed(path, SyncStatus::ReadOnly, Some(format!("Couldn't prepare the live session ({e}).")));
                return LiveOpen::Failed;
            }
        };
        self.status(path, SyncStatus::Opening, None);
        let cleanup = |slot: &Slot| {
            let _ = std::fs::remove_file(&slot.session_file);
            let _ = std::fs::remove_dir_all(&slot.folder);
        };
        let (bytes, _etag) = match Files::new(dav).get(&self.http(), path, super::transport::MAX_FILE) {
            Ok(got) => got,
            Err(e) => {
                cleanup(&slot);
                self.closed(path, SyncStatus::ReadOnly, Some(super::sync::open_message(&e)));
                return LiveOpen::Failed;
            }
        };
        let written = private_dir(&slot.folder).and_then(|()| crate::paths::write_atomic(&slot.local, &bytes));
        if let Err(e) = written {
            cleanup(&slot);
            self.closed(path, SyncStatus::ReadOnly, Some(format!("Couldn't save the file on this computer ({e}).")));
            return LiveOpen::Failed;
        }
        let read_only = read_only || ticket.read_only;
        if let Err(e) = write_session(&slot.session_file, &account.server, file_id, path, &ticket, read_only) {
            cleanup(&slot);
            self.closed(path, SyncStatus::ReadOnly, Some(format!("Couldn't prepare the live session ({e}).")));
            return LiveOpen::Failed;
        }
        let process = match (self.live_launcher)(app, &slot.session_file, &slot.local) {
            Ok(p) => p,
            Err(e) => {
                cleanup(&slot);
                self.closed(path, SyncStatus::ReadOnly, Some(super::sync::sentence(&e)));
                return LiveOpen::Failed;
            }
        };
        let now = self.now();
        book.live.push(LiveSession {
            account: account.id.clone(),
            path: path.to_string(),
            app: app.to_string(),
            file_id,
            server: account.server.clone(),
            session_file: slot.session_file,
            folder: slot.folder,
            local: slot.local,
            downloaded: checkout::digest(&bytes),
            read_only,
            forced_read_only: read_only && !ticket.read_only,
            saver: ticket.saver,
            refresh_at: now + refresh_after(ticket.expires_in),
            backoff: Duration::ZERO,
            failing: false,
            process,
        });
        self.remember(&account.id, path, app);
        self.status(path, SyncStatus::Live, Some(live_detail(app_name(app), read_only, ticket.saver)));
        LiveOpen::Opened
    }

    /// Says a live file is still open live (opening it again doesn't start a second session).
    pub(super) fn still_live(&self, book: &Book, i: usize) {
        let l = &book.live[i];
        self.status(&l.path, SyncStatus::Live, Some(live_detail(app_name(&l.app), l.read_only, l.saver)));
    }

    /// One pass over the live sessions of `account`: ends those whose app quit, renews tickets
    /// that are due.
    pub(super) fn tick_live(&self, book: &mut Book, account: &str) {
        let now = self.now();
        let mut i = 0;
        while i < book.live.len() {
            if book.live[i].process.exited() {
                let ended = book.live.remove(i);
                self.end_live(ended);
                continue;
            }
            if book.live[i].account == account && now >= book.live[i].refresh_at {
                self.renew(&mut book.live[i], now);
            }
            i += 1;
        }
    }

    /// Writes a fresh ticket into the session file before the old one runs out. A failure keeps
    /// the old file and tries again with backoff: the app keeps editing over the connection it
    /// has, and only a reconnect needs the new ticket.
    fn renew(&self, l: &mut LiveSession, now: Instant) {
        let result = self.ticket(l.file_id).and_then(|t| {
            let read_only = l.forced_read_only || t.read_only;
            write_session(&l.session_file, &l.server, l.file_id, &l.path, &t, read_only)
                .map_err(|_| TicketError::Malformed("the session file couldn't be written"))
                .map(|()| (t, read_only))
        });
        match result {
            Ok((ticket, read_only)) => {
                l.refresh_at = now + refresh_after(ticket.expires_in);
                l.backoff = Duration::ZERO;
                let changed = l.failing || l.read_only != read_only || l.saver != ticket.saver;
                l.failing = false;
                l.read_only = read_only;
                l.saver = ticket.saver;
                if changed {
                    let detail = live_detail(app_name(&l.app), read_only, ticket.saver);
                    self.status(&l.path, SyncStatus::Live, Some(detail));
                }
            }
            Err(e) => {
                l.backoff = if l.backoff.is_zero() { FIRST_RETRY } else { (l.backoff * 2).min(MAX_RETRY) };
                l.refresh_at = now + l.backoff;
                log::info!("couldn't renew a live session ticket ({e}); retrying in {:?}", l.backoff);
                if !l.failing {
                    l.failing = true;
                    self.status(
                        &l.path,
                        SyncStatus::Live,
                        Some(format!(
                            "Couldn't renew the live session ({e}). The app keeps editing; if its connection drops, it reconnects once the launcher renews the session."
                        )),
                    );
                }
            }
        }
    }

    /// The app of a live session quit: its session file goes, and its working copy too unless
    /// it changed (then it is kept, as every copy the cloud may not have).
    fn end_live(&self, l: LiveSession) {
        if let Err(e) = std::fs::remove_file(&l.session_file)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("couldn't remove a live session file ({e}); it is at {}", l.session_file.display());
        }
        let changed = std::fs::read(&l.local).ok().is_some_and(|b| checkout::digest(&b) != l.downloaded);
        let mut detail = "The live session ended when the app closed; the cloud has every change made in it.".to_string();
        if changed {
            match self.keep(&l.account, &l.path, &l.local, "live session copy") {
                Ok(at) => detail.push_str(&format!(" The app also saved a copy on this computer; it is kept at {}.", at.display())),
                Err(e) => {
                    log::warn!("couldn't keep a live session's local copy ({e}); it stays at {}", l.local.display());
                    detail.push_str(&format!(" The app also saved a copy on this computer; it is at {}.", l.local.display()));
                    return self.closed(&l.path, SyncStatus::Live, Some(detail));
                }
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&l.folder)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("couldn't remove a live session folder ({e})");
        }
        self.closed(&l.path, SyncStatus::Live, Some(detail));
    }

    /// At start: the live sessions of an earlier run can't be followed any more. Their session
    /// files (tickets) are removed; their working copies are moved to the kept folder, never
    /// deleted, in case an app saved one while offline.
    pub(super) fn clear_stale_live(&self) {
        let root = self.paths.live_root();
        let Ok(dir) = std::fs::read_dir(&root) else { return };
        for entry in dir.flatten() {
            let at = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&at) else { continue };
            if meta.is_file() && at.extension().is_some_and(|e| e == "json") {
                if let Err(e) = std::fs::remove_file(&at) {
                    log::warn!("couldn't remove an old live session file ({e})");
                }
            } else if meta.is_dir() {
                let empty = std::fs::read_dir(&at).map(|mut d| d.next().is_none()).unwrap_or(false);
                let kept = self.paths.cloud_root().join("kept").join("live").join(entry.file_name());
                let moved = if empty {
                    std::fs::remove_dir(&at).map_err(|e| e.to_string())
                } else {
                    kept.parent()
                        .map_or(Ok(()), |p| std::fs::create_dir_all(p).map_err(|e| e.to_string()))
                        .and_then(|()| std::fs::rename(&at, &kept).map_err(|e| e.to_string()))
                };
                if let Err(e) = moved {
                    log::warn!("couldn't tidy an old live session folder ({e}); it is at {}", at.display());
                }
            }
        }
    }
}

/// When a ticket that lasts `expires_in` seconds is renewed: at four fifths of its life (480 s
/// of 600, as the web editor does), early enough that a reconnect never meets an expired one.
fn refresh_after(expires_in: u64) -> Duration {
    Duration::from_secs(expires_in.saturating_mul(4) / 5).max(MIN_REFRESH)
}

/// Writes the session file atomically (a private temporary file renamed into place, mode 0600),
/// so the app never reads half of it.
fn write_session(file: &Path, server: &str, file_id: u64, path: &str, t: &Ticket, read_only: bool) -> Result<(), String> {
    let now = t.asked_at;
    let lasts = i64::try_from(t.expires_in).unwrap_or(i64::MAX);
    let expires_at = now.checked_add(jiff::SignedDuration::from_secs(lasts)).unwrap_or(now);
    let record = SessionRecord {
        v: SESSION_VERSION,
        server,
        file_id,
        path,
        ticket: &t.ticket,
        relay_url: &t.relay_url,
        room: &t.room,
        expires_at: expires_at.to_string(),
        read_only,
        display_name: &t.display_name,
        saver: t.saver,
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
    crate::paths::write_atomic(file, &bytes)
}

/// The note a live-capable file that opened through the check-out carries; `why` goes to the log.
pub(super) fn unavailable_note(why: &str) -> String {
    log::info!("live editing isn't available for a cloud file ({why}); opening it through the check-out");
    LIVE_UNAVAILABLE.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (plain, coded) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(plain.as_bytes()), coded, "{plain}");
        }
    }

    #[test]
    fn only_secure_relays_are_accepted_except_on_this_computer() {
        assert_eq!(relay_address("wss://relay.example.com/live"), Some(("relay.example.com".to_string(), 443)));
        assert_eq!(relay_address("wss://relay.example.com:8443"), Some(("relay.example.com".to_string(), 8443)));
        assert_eq!(relay_address("ws://127.0.0.1:9001/x"), Some(("127.0.0.1".to_string(), 9001)));
        assert_eq!(relay_address("wss://[::1]:9"), Some(("::1".to_string(), 9)));
        assert_eq!(resolve_relay("/craft", "https://cloud.example.com/nextcloud").as_deref(), Some("wss://cloud.example.com/craft"));
        assert_eq!(resolve_relay("https://relay.example.com/x", "https://cloud.example.com").as_deref(), Some("wss://relay.example.com/x"));
        assert_eq!(resolve_relay("/craft", "http://127.0.0.1:8080").as_deref(), Some("ws://127.0.0.1:8080/craft"));
        for bad in ["http://relay.example.com", "//evil.example.com/x", "craft", "ftp://x"] {
            assert!(resolve_relay(bad, "https://cloud.example.com").is_none(), "{bad}");
        }
        for bad in
            ["ws://relay.example.com", "https://relay.example.com", "wss://", "wss://user@host", "wss://host:notaport", "ws://127.0.0.1.evil.com"]
        {
            assert!(relay_address(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_ticket_is_renewed_well_before_it_runs_out() {
        assert!(refresh_after(600) < Duration::from_secs(600));
        assert!(refresh_after(0) >= MIN_REFRESH, "never a busy loop");
    }

    #[test]
    fn a_slot_never_leaves_the_live_folder() {
        let root = Path::new("/data/live");
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(new_slot(root, bad).is_err(), "{bad:?}");
        }
        let slot = new_slot(root, "report.docx").unwrap();
        assert!(slot.local.starts_with(root) && slot.session_file.starts_with(root));
        assert_eq!(slot.local.file_name().and_then(|n| n.to_str()), Some("report.docx"), "the app's title shows the real name");
    }
}
