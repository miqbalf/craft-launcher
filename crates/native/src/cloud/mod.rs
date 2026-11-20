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
mod checkout;
mod sync;
pub mod transport;

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{CloudAccount, CloudEntry};
use artcraft_launcher_core::protocol::Event;
use craft_nextcloud::http::Http;
use craft_nextcloud::{Account as DavAccount, Error as DavError, dav};
use craft_suite_account::TokenStore;

use crate::paths::Paths;

pub use artcraft_launcher_core::cloud::{CLOSED_DETAIL, SEARCH_PATH, SHARED_PATH};

/// How often the sign-in is checked while the user approves it in the browser.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long a sign-in waits for the browser before giving up.
const GIVE_UP_AFTER: Duration = Duration::from_secs(20 * 60);

type Emit = Box<dyn Fn(Event) + Send + Sync>;
type Opener = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Opens a working copy in a catalog app: `(app id, file)`.
type Launcher = Box<dyn Fn(&str, &Path) -> Result<(), String> + Send + Sync>;
type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// A signed-in account: what the UI sees, and what the WebDAV client needs.
struct Session {
    account: CloudAccount,
    dav: DavAccount,
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
    /// The open files. `None` until first read from disk. Held for the whole of each sync step, so
    /// an open, a poll, a Done, a resolve and a disconnect never interleave on one file.
    book: Mutex<Option<checkout::Book>>,
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
            book: Mutex::new(None),
        }
    }

    /// Replaces how a working copy is opened in its app (the backend opens installed apps; tests
    /// record the call).
    #[must_use]
    pub fn with_launcher(mut self, launch: impl Fn(&str, &Path) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.launcher = Box::new(launch);
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

    /// Replaces how often and how long a sign-in waits for the browser.
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
        let result = match self.call(|files, http| files.search(http, query)) {
            Ok(entries) => Ok(sorted(entries)),
            Err(Failure::Dav(DavError::Unauthorized)) => Err(Failure::Dav(DavError::Unauthorized)),
            Err(Failure::NotConnected) => Err(Failure::NotConnected),
            Err(Failure::Dav(e)) => {
                log::info!("cloud search failed ({e}); filtering the listed folder instead");
                Ok(self.filter_listed(query))
            }
        };
        self.answer(SEARCH_PATH, result);
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
        match result {
            Ok(entries) => self.emit(Event::CloudListed { path: path.to_string(), entries: Ok(entries) }),
            Err(failure) => {
                let (message, reconnect) = explain(&failure);
                self.emit(Event::CloudListed { path: path.to_string(), entries: Err(message.clone()) });
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
    }
}

/// Folders first, then by name without regard to letter case.
fn sorted(entries: Vec<craft_cloud::Entry>) -> Vec<CloudEntry> {
    let mut out: Vec<CloudEntry> = entries.into_iter().map(entry).collect();
    out.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    out
}
