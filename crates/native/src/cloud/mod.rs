//! Cloud files: sign in to a Nextcloud server, and browse, share-list and search it.
//!
//! [`CloudService`] runs on the caller's thread and blocks on the network, so the backend calls it
//! from worker threads. Every answer goes out as an [`Event`] through the `emit` function given to
//! [`CloudService::new`]. The sign-in token lives only in the OS keychain (through
//! `craft-suite-account`'s `TokenStore`); the account record on disk holds no secret and the
//! suite account list is never touched.
//!
//! Files opened from the cloud are checked out into a cache folder, opened in their Craft app and
//! watched; saves go back with `If-Match`, so a newer version on the server is never replaced
//! silently (see [`sync`]).

mod account;
mod assist;
mod checkout;
mod live;
mod sync;
pub mod transport;

pub use assist::{ASSIST_FILE_VERSION, ASSIST_TOKEN_PATH};
pub use live::{LiveProcess, SESSION_VERSION, TICKET_PATH, relay_address, relay_reachable, resolve_relay};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{CloudAccount, CloudEntry};
use artcraft_launcher_core::protocol::Event;
use craft_nextcloud::http::Http;
use craft_nextcloud::{Account as DavAccount, Error as DavError, dav};
use craft_suite_account::{Secret, TokenStore};

use crate::paths::Paths;

pub use artcraft_launcher_core::cloud::{SEARCH_PATH, SHARED_PATH};

/// How often the sign-in is checked while the user approves it in the browser.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long a sign-in waits for the browser before giving up.
const GIVE_UP_AFTER: Duration = Duration::from_secs(20 * 60);

type Emit = Box<dyn Fn(Event) + Send + Sync>;
type Opener = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Opens a working copy in a catalog app: `(app id, file)`.
type Launcher = Box<dyn Fn(&str, &Path) -> Result<(), String> + Send + Sync>;
type Clock = Box<dyn Fn() -> Instant + Send + Sync>;
type WallClock = Box<dyn Fn() -> jiff::Timestamp + Send + Sync>;
/// Starts an app for a live session: `(app id, session file, working copy)`.
type LiveLauncher = Box<dyn Fn(&str, &Path, &Path) -> Result<Box<dyn LiveProcess>, String> + Send + Sync>;
/// Whether a relay URL accepts connections.
type RelayProbe = Box<dyn Fn(&str) -> bool + Send + Sync>;
/// Shows a folder in the file manager.
type Revealer = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// A signed-in account: what the UI sees, and what the WebDAV client needs.
struct Session {
    account: CloudAccount,
    dav: DavAccount,
    /// The app password, for the calls `craft-nextcloud` doesn't make (the live ticket).
    password: Secret,
}

/// The cloud connection of this launcher.
pub struct CloudService<S: TokenStore, H: Http> {
    paths: Paths,
    tokens: S,
    http: Mutex<H>,
    emit: Emit,
    open_url: Opener,
    poll_every: Duration,
    give_up_after: Duration,
    session: Mutex<Option<Session>>,
    /// Bumped by every `connect`, `cancel_connect` and `disconnect`: a waiting sign-in that sees a
    /// number other than its own stops quietly.
    attempt: AtomicU64,
    /// The folder last listed, for the name filter that stands in when the server can't search.
    last_listed: Mutex<Option<Vec<CloudEntry>>>,
    launcher: Launcher,
    clock: Clock,
    /// The date and time names of conflict copies and backups carry.
    wall: WallClock,
    reveal: Revealer,
    /// Runs just before a file action waits for the open files (tests make an account change
    /// land in that gap).
    before_book: Box<dyn Fn() + Send + Sync>,
    /// Paths the last listings showed as not writable for this user: they open read-only.
    not_writable: Mutex<HashSet<String>>,
    /// The server's file ids of the paths the last listings showed, which live tickets name.
    file_ids: Mutex<HashMap<String, u64>>,
    live_launcher: LiveLauncher,
    relay_probe: RelayProbe,
    /// The open files. `None` until first read from disk. Held for the whole of each sync step, so
    /// an open, a poll, a Done, a resolve and a disconnect never interleave on one file.
    book: Mutex<Option<checkout::Book>>,
    /// When the account-wide grammar and AI token is renewed next.
    assist: Mutex<assist::AssistState>,
}

/// Why a cloud call produced nothing.
enum Failure {
    NotConnected,
    Dav(DavError),
}

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    pub fn new(paths: &Paths, tokens: S, http: H, emit: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self {
            paths: paths.clone(),
            tokens,
            http: Mutex::new(http),
            emit: Box::new(emit),
            open_url: Box::new(|url| crate::process::open(std::ffi::OsStr::new(url))),
            poll_every: POLL_EVERY,
            give_up_after: GIVE_UP_AFTER,
            session: Mutex::new(None),
            attempt: AtomicU64::new(0),
            last_listed: Mutex::new(None),
            launcher: Box::new(|_, _| Err("Opening from the cloud isn't supported on this system yet".to_string())),
            clock: Box::new(Instant::now),
            wall: Box::new(jiff::Timestamp::now),
            reveal: Box::new(crate::process::reveal),
            before_book: Box::new(|| {}),
            not_writable: Mutex::new(HashSet::new()),
            file_ids: Mutex::new(HashMap::new()),
            live_launcher: Box::new(|_, _, _| Err("Live editing isn't supported on this system yet".to_string())),
            relay_probe: Box::new(relay_reachable),
            book: Mutex::new(None),
            assist: Mutex::new(assist::AssistState::default()),
        }
    }

    /// Replaces the wall clock conflict and backup names are dated with (tests fix it).
    #[must_use]
    pub fn with_wall_clock(mut self, now: impl Fn() -> jiff::Timestamp + Send + Sync + 'static) -> Self {
        self.wall = Box::new(now);
        self
    }

    /// Replaces how the kept folder is shown in the file manager (tests record the call).
    #[must_use]
    pub fn with_revealer(mut self, reveal: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.reveal = Box::new(reveal);
        self
    }

    /// Runs `pause` each time a file action (open, Done, Stop tracking, a conflict choice) is about
    /// to wait for the open files, after it has checked the account. Tests use it to change the
    /// account in that gap.
    #[doc(hidden)]
    #[must_use]
    pub fn with_pause_before_book(mut self, pause: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_book = Box::new(pause);
        self
    }

    /// Replaces how a working copy is opened in its app (the backend opens installed apps; tests
    /// record the call).
    #[must_use]
    pub fn with_launcher(mut self, launch: impl Fn(&str, &Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.launcher = Box::new(launch);
        self
    }

    /// Replaces how an app is started for a live session (the backend starts installed apps
    /// with `--live-session`; tests record the call and say when the app quits).
    #[must_use]
    pub fn with_live_launcher(mut self, launch: impl Fn(&str, &Path, &Path) -> Result<Box<dyn LiveProcess>, String> + Send + Sync + 'static) -> Self {
        self.live_launcher = Box::new(launch);
        self
    }

    /// Replaces the check that a live session's relay can be reached (tests answer it).
    #[must_use]
    pub fn with_relay_probe(mut self, probe: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.relay_probe = Box::new(probe);
        self
    }

    /// Replaces the clock the watcher's waits are measured with (tests move it by hand).
    #[must_use]
    pub fn with_clock(mut self, now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        self.clock = Box::new(now);
        self
    }

    /// Replaces how the sign-in address is opened (tests must not start a browser).
    #[must_use]
    pub fn with_opener(mut self, open: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.open_url = Box::new(open);
        self
    }

    /// Replaces how often and how long a sign-in waits for the browser. `poll_every` is also the
    /// wait before a live ticket the server called busy (503) is asked for again.
    #[must_use]
    pub fn with_timing(mut self, poll_every: Duration, give_up_after: Duration) -> Self {
        self.poll_every = poll_every;
        self.give_up_after = give_up_after;
        self
    }

    fn emit(&self, event: Event) {
        (self.emit)(event);
    }

    fn http(&self) -> H {
        self.http.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Lists the folder `path`, answering with [`Event::CloudListed`].
    pub fn list(&self, path: &str) {
        let result = self.call(|files, http| files.list(http, path));
        self.answer(
            path,
            result.map(|entries| {
                let entries = sorted(entries);
                *self.last_listed.lock().unwrap_or_else(PoisonError::into_inner) = Some(entries.clone());
                entries
            }),
        );
    }

    /// Lists what others shared with the user, answering with [`SHARED_PATH`].
    pub fn shared(&self) {
        let result = self.call(|files, http| files.shared_with_me(http));
        self.answer(SHARED_PATH, result.map(sorted));
    }

    /// Searches by file name, answering with [`SEARCH_PATH`]. When the server can't search (any
    /// failure but a rejected sign-in) it filters the folder listed last by name instead.
    pub fn search(&self, query: &str) {
        let mut folder_only = false;
        let result = match self.call(|files, http| files.search(http, query)) {
            Ok(entries) => Ok(sorted(entries)),
            Err(Failure::Dav(DavError::Unauthorized)) => Err(Failure::Dav(DavError::Unauthorized)),
            Err(Failure::NotConnected) => Err(Failure::NotConnected),
            Err(Failure::Dav(e)) => {
                log::info!("cloud search failed ({e}); filtering the listed folder instead");
                folder_only = true;
                Ok(self.filter_listed(query))
            }
        };
        self.answer_with(SEARCH_PATH, result, folder_only);
    }

    /// Whether the last listings showed `path` as not writable for this user.
    fn listed_read_only(&self, path: &str) -> bool {
        self.not_writable.lock().unwrap_or_else(PoisonError::into_inner).contains(path)
    }

    /// The server's id of `path`, as the last listings gave it.
    fn listed_file_id(&self, path: &str) -> Option<u64> {
        self.file_ids.lock().unwrap_or_else(PoisonError::into_inner).get(path).copied()
    }

    /// The server, login and app password of the connected account.
    fn session_credentials(&self) -> Option<(String, String, Secret)> {
        self.account()?;
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        session.as_ref().map(|s| (s.account.server.clone(), s.account.login.clone(), s.password.clone()))
    }

    /// Remembers which of `entries` this user may not change, and their file ids.
    fn note_writable(&self, entries: &[CloudEntry]) {
        {
            let mut ids = self.file_ids.lock().unwrap_or_else(PoisonError::into_inner);
            for e in entries.iter().filter(|e| !e.is_dir) {
                match e.file_id {
                    Some(id) => ids.insert(e.path.clone(), id),
                    None => ids.remove(&e.path),
                };
            }
        }
        let mut set = self.not_writable.lock().unwrap_or_else(PoisonError::into_inner);
        for e in entries.iter().filter(|e| !e.is_dir) {
            if e.writable {
                set.remove(&e.path);
            } else {
                set.insert(e.path.clone());
            }
        }
    }

    /// Forgets everything learnt from the listings of the account that was connected.
    fn forget_listings(&self) {
        *self.last_listed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.not_writable.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.file_ids.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The lock holder the last folder listing showed for `path`: `(name, held by an app)`. The
    /// name is empty when the listing didn't say or didn't show the file.
    fn listed_lock(&self, path: &str) -> (String, bool) {
        let listed = self.last_listed.lock().unwrap_or_else(PoisonError::into_inner);
        listed
            .iter()
            .flatten()
            .find(|e| e.path == path)
            .map_or_else(|| (String::new(), false), |e| (e.locked_by.clone().unwrap_or_default(), e.lock_by_app))
    }

    fn filter_listed(&self, query: &str) -> Vec<CloudEntry> {
        let needle = query.trim().to_lowercase();
        let listed = self.last_listed.lock().unwrap_or_else(PoisonError::into_inner);
        listed.iter().flatten().filter(|e| e.name.to_lowercase().contains(&needle)).cloned().collect()
    }

    /// Runs one WebDAV call as the connected account.
    fn call<T>(&self, op: impl FnOnce(&dav::Files<'_>, &H) -> craft_nextcloud::Result<T>) -> Result<T, Failure> {
        let dav_account = self.session_dav().ok_or(Failure::NotConnected)?;
        op(&dav::Files::new(&dav_account), &self.http()).map_err(Failure::Dav)
    }

    /// Sends the outcome of a listing: always an [`Event::CloudListed`]; a rejected sign-in also
    /// an [`Event::CloudError`] that says to reconnect.
    fn answer(&self, path: &str, result: Result<Vec<CloudEntry>, Failure>) {
        self.answer_with(path, result, false);
    }

    /// [`Self::answer`], saying whether the entries come from the folder listed last only.
    fn answer_with(&self, path: &str, result: Result<Vec<CloudEntry>, Failure>, folder_only: bool) {
        match result {
            Ok(entries) => {
                self.note_writable(&entries);
                self.emit(Event::CloudListed { path: path.to_string(), entries: Ok(entries), folder_only });
            }
            Err(failure) => {
                let (message, reconnect) = explain(&failure);
                self.emit(Event::CloudListed { path: path.to_string(), entries: Err(message.clone()), folder_only });
                if reconnect {
                    self.emit(Event::CloudError(message));
                }
            }
        }
    }
}

/// A user-facing message, and whether the fix is to reconnect.
fn explain(failure: &Failure) -> (String, bool) {
    match failure {
        Failure::NotConnected => ("Connect to your cloud first.".to_string(), false),
        Failure::Dav(e) => explain_dav(e),
    }
}

/// [`explain`] for a WebDAV error.
fn explain_dav(e: &DavError) -> (String, bool) {
    match e {
        DavError::Unauthorized => (artcraft_launcher_core::cloud::SIGN_IN_EXPIRED.to_string(), true),
        DavError::NotFound => ("That folder wasn't found in your cloud.".to_string(), false),
        DavError::Transport(why) => (format!("Couldn't reach your cloud server ({why})."), false),
        DavError::Status { status, .. } => (format!("Your cloud server answered HTTP {status}."), false),
        DavError::Malformed(_) => ("Your cloud server sent an answer the launcher couldn't read.".to_string(), false),
        DavError::Conflict | DavError::Locked => ("Your cloud server refused the request.".to_string(), false),
    }
}

/// The launcher's view of an entry. A modified time the server gave in a form we can't read is 0.
fn entry(e: craft_cloud::Entry) -> CloudEntry {
    let modified = e.modified.as_deref().and_then(|m| m.parse::<jiff::Timestamp>().ok()).map_or(0, |t| t.as_second());
    CloudEntry {
        path: e.path,
        name: e.name,
        is_dir: e.kind == craft_cloud::EntryKind::Folder,
        size: e.size.unwrap_or(0),
        modified,
        etag: e.etag.unwrap_or_default(),
        shared: e.shared_by.is_some(),
        // The holder's name, else its account id; a lock the server gives no holder for is an
        // empty name, which the UI words as "Locked".
        locked_by: e.lock.as_ref().map(|l| l.name.clone().or_else(|| l.id.clone()).unwrap_or_default()),
        lock_by_app: e.lock.as_ref().is_some_and(|l| l.app),
        writable: e.writable,
        // Nextcloud gives `oc:fileid` (or a share's `item_source`) as the room; a path fallback is no id.
        file_id: e.room.parse().ok(),
    }
}

/// Folders first, then by name without regard to letter case.
fn sorted(entries: Vec<craft_cloud::Entry>) -> Vec<CloudEntry> {
    let mut out: Vec<CloudEntry> = entries.into_iter().map(entry).collect();
    out.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    out
}
