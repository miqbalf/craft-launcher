//! Opening cloud files in their Craft app, saving them back, settling conflicts and closing them.
//!
//! Every save goes up with `If-Match: <the ETag the working copy started from>`, so a version
//! someone else saved in the meantime is never replaced: the server answers 412 and the file is
//! marked [`SyncStatus::Conflict`] until the user picks what to keep. The working copy itself is
//! only ever replaced after "Use theirs" once the app has closed it (the user's version is moved
//! to a backup first), and only removed by "Done" once the server has every change.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{CLOUD_APPS, CloudEntry, ConflictChoice, SyncStatus, app_for};
use artcraft_launcher_core::protocol::Event;
use craft_nextcloud::dav::Files;
use craft_nextcloud::http::Http;
use craft_nextcloud::{Account as DavAccount, Error as DavError};
use craft_suite_account::TokenStore;

use super::checkout::{self, Book, Checkout, HeldLock, Recent, Runtime, Synced, rt_key};
use super::{CLOSED_DETAIL, CloudService, explain_dav};

/// How long a changed file must stay unchanged before it is uploaded: apps save in steps.
const SETTLE: Duration = Duration::from_secs(2);
/// The first wait after a failed upload, doubled after each further failure up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// The lock asked for; it is refreshed at half this.
const LOCK_SECS: u32 = 30 * 60;
/// How often the backend's watcher looks at the open files.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// How many numbered conflict-copy names are tried before giving up.
const CONFLICT_NAMES: u32 = 9;

/// How an upload ended.
#[derive(PartialEq, Eq)]
enum Upload {
    Saved,
    /// The file changed while it was read: try again when it settles.
    Busy,
    Failed,
}

/// The display name of a server path.
fn file_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// The folder of a server path, with its trailing slash (`/` for a top-level file).
fn parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rfind('/').map_or("/", |i| &trimmed[..=i])
}

/// The local date and minute, as conflict and backup names carry them.
fn minute() -> String {
    jiff::Zoned::now().strftime("%Y-%m-%d %H%M").to_string()
}

/// A login as it may appear inside a file name.
fn name_safe(login: &str) -> String {
    login.chars().map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect()
}

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    fn status(&self, path: &str, status: SyncStatus, detail: Option<String>) {
        self.emit(Event::CloudFileStatus { path: path.to_string(), status, detail, forgettable: false });
    }

    /// A status Done can't get past, where stopping tracking ([`Self::forget`]) loses nothing the
    /// cloud could still get: a read-only copy with changes, or a copy that is missing.
    fn status_forgettable(&self, path: &str, status: SyncStatus, detail: String) {
        self.emit(Event::CloudFileStatus { path: path.to_string(), status, detail: Some(detail), forgettable: true });
    }

    fn error(&self, message: impl Into<String>) {
        self.emit(Event::CloudError(message.into()));
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    /// The open files, read from disk the first time.
    pub(super) fn book<'a>(&self, guard: &'a mut MutexGuard<'_, Option<Book>>) -> Result<&'a mut Book, String> {
        if guard.is_none() {
            **guard = Some(Book::load(self.paths.cloud_root().join("checkouts.json"))?);
        }
        guard.as_mut().ok_or_else(|| "the list of open cloud files isn't loaded".to_string())
    }

    pub(super) fn lock_book(&self) -> MutexGuard<'_, Option<Book>> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Saves the book; a failure is told, but the files stay watched for this session.
    fn save_book(&self, book: &Book) {
        if let Err(e) = book.save() {
            log::warn!("couldn't save the list of open cloud files: {e}");
            self.error(format!("Couldn't record the open cloud files on this computer ({e}). Keep the launcher open until they're saved."));
        }
    }

    /// The account's own cache folder. Its id is a hex handle; anything else could climb out.
    fn cache_dir(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("unexpected account id".to_string());
        }
        Ok(self.paths.cloud_root().join(id))
    }

    fn connected(&self) -> Option<(artcraft_launcher_core::cloud::CloudAccount, DavAccount)> {
        let account = self.account()?;
        let dav = self.session_dav()?;
        Some((account, dav))
    }

    fn launch(&self, app: &str, local: &Path) {
        if let Err(e) = (self.launcher)(app, local) {
            self.error(e);
        }
    }

    fn remember(&self, account: &str, path: &str, app: &str) {
        let item = Recent {
            account: account.to_string(),
            path: path.to_string(),
            name: file_name(path).to_string(),
            app: app.to_string(),
            opened: jiff::Timestamp::now().as_second(),
        };
        if let Err(e) = checkout::remember(&self.paths.cloud_root().join("recent.json"), item) {
            log::warn!("couldn't update the recent cloud files: {e}");
        }
    }

    /// Lists the files opened through the launcher for the connected account, newest first, as
    /// [`Event::CloudRecentListed`].
    pub fn recent(&self) {
        let id = self.account().map(|a| a.id);
        let list = checkout::read_recent(&self.paths.cloud_root().join("recent.json"))
            .into_iter()
            .filter(|r| Some(&r.account) == id.as_ref())
            .map(|r| CloudEntry {
                path: r.path,
                name: r.name,
                is_dir: false,
                size: 0,
                modified: r.opened,
                etag: String::new(),
                shared: false,
                locked_by: None,
                lock_by_app: false,
            })
            .collect();
        self.emit(Event::CloudRecentListed(list));
    }

    /// Opens the server file `path` in `app` (or the app its extension picks): locks it when the
    /// server can, downloads it into the cache and launches the app with it. A file already open
    /// is not downloaded again: its working copy, with any unsynced edit, is opened instead.
    pub fn open(&self, path: &str, app: Option<String>, read_only: bool) {
        let Some((account, dav)) = self.connected() else { return self.error("Connect to your cloud first.") };
        let app = match app.as_deref().or_else(|| app_for(file_name(path))) {
            Some(a) if CLOUD_APPS.contains(&a) => a.to_string(),
            Some(_) => return self.error("Opening from the cloud isn't supported for this app yet."),
            None => return self.error("No Craft app opens this kind of file yet."),
        };
        let mut guard = self.lock_book();
        let book = match self.book(&mut guard) {
            Ok(b) => b,
            Err(e) => return self.error(format!("Couldn't read the list of open cloud files ({e}).")),
        };
        if let Some(i) = book.find(&account.id, path) {
            let c = &book.list[i];
            let (key, local, app, status) = (c.path.clone(), c.local.clone(), c.app.clone(), c.status);
            self.remember(&account.id, &key, &app);
            self.status(&key, status, None);
            return self.launch(&app, &local);
        }
        let local = match self.cache_dir(&account.id).and_then(|cache| checkout::working_copy(&cache, path)) {
            Ok(l) => l,
            Err(e) => return self.error(e),
        };
        // Another open file whose name differs only in letter case would share this file on a
        // case-insensitive disk: refuse rather than move or overwrite its working copy.
        if book.owns_slot(&local) {
            return self.error("Another file with nearly the same name is open from the cloud. Choose Done on it first, then open this one.");
        }
        let http = self.http();
        let files = Files::new(&dav);
        let lock = if read_only {
            None
        } else {
            match self.take_lock(&files, &http, path) {
                Ok(lock) => lock,
                Err(()) => return,
            }
        };
        self.status(path, SyncStatus::Opening, None);
        let unlock = |why: &str| {
            if let Some(lock) = &lock
                && let Err(e) = files.unlock(&http, &lock.to_dav())
            {
                log::warn!("couldn't release the lock after {why}: {e}");
            }
        };
        let (bytes, etag) = match files.get(&http, path, super::transport::MAX_FILE) {
            Ok(got) => got,
            Err(e) => {
                unlock("a failed download");
                return self.error(open_message(&e));
            }
        };
        // Left from an earlier run, owned by no checkout: kept, never replaced.
        let mut kept = None;
        if std::fs::symlink_metadata(&local).is_ok() {
            match self.keep(&account.id, path, &local, "earlier copy") {
                Ok(at) => kept = Some(at),
                Err(e) => {
                    unlock("a failed write");
                    return self.error(format!("Couldn't move an earlier copy of the file out of the way ({e}), so nothing was replaced."));
                }
            }
        }
        if let Err(e) = crate::paths::write_atomic(&local, &bytes) {
            unlock("a failed write");
            return self.error(format!("Couldn't save the file on this computer ({e})."));
        }
        let Some(stamp) = checkout::stamp(&local) else {
            unlock("a failed write");
            return self.error("Couldn't save the file on this computer.");
        };
        let status = if read_only { SyncStatus::ReadOnly } else { SyncStatus::Saved };
        book.list.push(Checkout {
            account: account.id.clone(),
            path: path.to_string(),
            remote: path.to_string(),
            local: local.clone(),
            etag,
            lock,
            app: app.clone(),
            tracks_remote: !read_only,
            status,
            synced: Some(Synced::of(stamp, &bytes)),
        });
        book.runtime.insert(rt_key(&account.id, path), Runtime { lock_checked: Some(self.now()), ..Runtime::default() });
        self.save_book(book);
        drop(guard);
        self.remember(&account.id, path, &app);
        self.status(path, status, kept.map(|at| format!("An earlier copy found on this computer is kept at {}", at.display())));
        self.launch(&app, &local);
    }

    /// Locks `path` when the server offers locking. `Ok(None)`: no lock (the server has none, or
    /// can't lock this file), and `If-Match` alone guards saves. `Err`: the open stops; the reason
    /// was emitted, [`Event::CloudLockedBy`] when someone else holds the lock.
    fn take_lock(&self, files: &Files<'_>, http: &H, path: &str) -> Result<Option<HeldLock>, ()> {
        let caps = files.capabilities(http).map_err(|e| self.error(open_message(&e)))?;
        if !caps.locking {
            return Ok(None);
        }
        match files.lock(http, path, LOCK_SECS) {
            Ok(lock) => Ok(Some(HeldLock::from_dav(lock))),
            // LOCK doesn't say who; the last listing of the folder may have. Without it the name is
            // empty and the UI words it as "someone else".
            Err(DavError::Locked) => {
                let (by, app) = self.listed_lock(path);
                self.emit(Event::CloudLockedBy { path: path.to_string(), by, app });
                Err(())
            }
            Err(e @ (DavError::Unauthorized | DavError::NotFound | DavError::Transport(_))) => {
                self.error(open_message(&e));
                Err(())
            }
            Err(e) => {
                log::info!("couldn't lock a cloud file ({e}); relying on version checks alone");
                Ok(None)
            }
        }
    }

    /// Shows the files still open from the last run and watches them again (their uploads, locks
    /// and conflicts carry on). Called once when the backend starts.
    pub fn resume(&self) {
        let mut guard = self.lock_book();
        let book = match self.book(&mut guard) {
            Ok(b) => b,
            Err(e) => return self.error(format!("Couldn't read the list of open cloud files ({e}).")),
        };
        book.runtime.clear();
        let shown: Vec<(String, SyncStatus)> = book
            .list
            .iter()
            .map(|c| (c.path.clone(), if c.status == SyncStatus::Uploading { SyncStatus::WaitingToUpload } else { c.status }))
            .collect();
        drop(guard);
        for (path, status) in shown {
            self.status(&path, status, None);
        }
    }

    /// What the backend does once at start: says which account is still connected (so the UI
    /// learns of it without signing in again), then [`Self::resume`].
    pub fn start(&self) {
        // Under the book lock, as Disconnect and a sign-in's check are: an account removed or
        // replaced meanwhile is never announced after the event that removed it.
        let guard = self.lock_book();
        if let Some(account) = self.account() {
            self.emit(Event::CloudConnected(account));
        }
        drop(guard);
        self.resume();
    }

    /// Watches the open files for good: [`Self::start`], then [`Self::tick`] once a second.
    pub fn run_sync(&self) {
        self.start();
        loop {
            self.tick();
            std::thread::sleep(POLL_EVERY);
        }
    }

    /// One pass of the watcher: refreshes locks that are due and uploads saves that have settled.
    /// Only the connected account's files are touched.
    pub fn tick(&self) {
        let mut guard = self.lock_book();
        let Ok(book) = self.book(&mut guard) else { return };
        if book.list.is_empty() {
            return;
        }
        let Some((account, dav)) = self.connected() else { return };
        let keys: Vec<String> = book.list.iter().filter(|c| c.account == account.id).map(|c| c.path.clone()).collect();
        let now = self.now();
        for key in keys {
            self.refresh_lock(book, &dav, &account.id, &key, now);
            self.watch(book, &dav, &account.id, &key, now);
        }
    }

    /// Refreshes the file's lock at half its timeout. A lock the server no longer knows is taken
    /// again; if someone else has it now, saves still carry `If-Match`.
    fn refresh_lock(&self, book: &mut Book, dav: &DavAccount, account: &str, key: &str, now: Instant) {
        let Some(i) = book.index(account, key) else { return };
        let Some(lock) = book.list[i].lock.clone() else { return };
        let rt = book.runtime.entry(rt_key(account, key)).or_default();
        let half = Duration::from_secs(u64::from(lock.timeout_secs / 2));
        if rt.lock_checked.is_some_and(|t| now.saturating_duration_since(t) < half) {
            return;
        }
        rt.lock_checked = Some(now);
        let http = self.http();
        let files = Files::new(dav);
        match files.refresh(&http, &lock.to_dav()) {
            Ok(()) => {}
            Err(e @ (DavError::Transport(_) | DavError::Unauthorized)) => log::info!("couldn't refresh a cloud file lock ({e}); trying later"),
            Err(e) => {
                log::info!("a cloud file lock is gone ({e}); locking again");
                let fresh = match files.lock(&http, &lock.path, LOCK_SECS) {
                    Ok(l) => Some(HeldLock::from_dav(l)),
                    Err(e) => {
                        log::info!("couldn't lock the cloud file again ({e}); version checks still guard saves");
                        None
                    }
                };
                book.list[i].lock = fresh;
                self.save_book(book);
            }
        }
    }

    /// Uploads the working copy once a change has settled for [`SETTLE`].
    fn watch(&self, book: &mut Book, dav: &DavAccount, account: &str, key: &str, now: Instant) {
        let Some(i) = book.index(account, key) else { return };
        let c = &book.list[i];
        if !c.tracks_remote {
            return;
        }
        let attempt = self.attempt.load(Ordering::SeqCst);
        let rt = book.runtime.entry(rt_key(account, key)).or_default();
        if c.status == SyncStatus::ReconnectNeeded && rt.rejected_under == Some(attempt) {
            return;
        }
        if rt.retry_at.is_some_and(|t| now < t) {
            return;
        }
        // Missing for a moment while an app swaps in its new version: wait for it.
        let Some(stamp) = checkout::stamp(&c.local) else { return };
        if c.synced.as_ref().is_some_and(|s| s.stamp == stamp) {
            rt.seen = None;
            return;
        }
        match rt.seen {
            Some((seen, since)) if seen == stamp => {
                if now.saturating_duration_since(since) < SETTLE {
                    return;
                }
            }
            _ => {
                rt.seen = Some((stamp, now));
                return;
            }
        }
        self.upload(book, dav, account, key, now);
    }

    /// Uploads the working copy over the version it started from. A newer version on the server
    /// gives [`SyncStatus::Conflict`] and stops uploads of this file; a failure leaves the
    /// working copy alone and retries later.
    fn upload(&self, book: &mut Book, dav: &DavAccount, account: &str, key: &str, now: Instant) -> Upload {
        let Some(i) = book.index(account, key) else { return Upload::Failed };
        let c = book.list[i].clone();
        let Some(before) = checkout::stamp(&c.local) else { return Upload::Busy };
        let bytes = match std::fs::read(&c.local) {
            Ok(b) => b,
            Err(e) => {
                log::info!("couldn't read a cloud working copy ({e}); trying later");
                return Upload::Busy;
            }
        };
        if checkout::stamp(&c.local) != Some(before) {
            book.runtime.entry(rt_key(account, key)).or_default().seen = None;
            return Upload::Busy;
        }
        let synced = Synced::of(before, &bytes);
        if c.synced.as_ref().is_some_and(|s| s.sha256 == synced.sha256) {
            // Touched but not changed.
            book.list[i].synced = Some(synced);
            self.save_book(book);
            return Upload::Saved;
        }
        self.status(key, SyncStatus::Uploading, None);
        let result = put_checked(&Files::new(dav), &self.http(), &c, bytes);
        let rt = book.runtime.entry(rt_key(account, key)).or_default();
        rt.seen = None;
        let c = &mut book.list[i];
        let (status, detail, outcome) = match result {
            Ok(etag) => {
                c.etag = etag;
                c.synced = Some(synced);
                rt.retry_at = None;
                rt.backoff = Duration::ZERO;
                (SyncStatus::Saved, None, Upload::Saved)
            }
            Err(DavError::Conflict) => {
                c.tracks_remote = false;
                log::info!("a cloud file changed on the server and here; uploads stop until the user chooses");
                (
                    SyncStatus::Conflict,
                    Some("Someone changed this file in the cloud while you were editing it. Choose which version to keep.".to_string()),
                    Upload::Failed,
                )
            }
            Err(DavError::Unauthorized) => {
                rt.rejected_under = Some(self.attempt.load(Ordering::SeqCst));
                (SyncStatus::ReconnectNeeded, Some(explain_dav(&DavError::Unauthorized).0), Upload::Failed)
            }
            Err(e) => {
                rt.backoff = if rt.backoff.is_zero() { FIRST_BACKOFF } else { (rt.backoff * 2).min(MAX_BACKOFF) };
                rt.retry_at = Some(now + rt.backoff);
                let detail = match &e {
                    DavError::Locked => "Someone else has this file locked. Your changes are kept here and uploaded when they're done.".to_string(),
                    other => format!("{} Your changes are kept here and uploaded when it works again.", explain_dav(other).0),
                };
                log::info!("couldn't upload a cloud file ({e}); retrying in {:?}", rt.backoff);
                (SyncStatus::WaitingToUpload, Some(detail), Upload::Failed)
            }
        };
        c.status = status;
        self.save_book(book);
        self.status(key, status, detail);
        outcome
    }

    /// The user is finished with the file: uploads what is left, releases the lock, and removes
    /// the working copy once the server has every change. Anything unsaved keeps the file open.
    pub fn done(&self, path: &str) {
        let Some((account, dav)) = self.connected() else { return self.error("Connect to your cloud first.") };
        let mut guard = self.lock_book();
        let book = match self.book(&mut guard) {
            Ok(b) => b,
            Err(e) => return self.error(format!("Couldn't read the list of open cloud files ({e}).")),
        };
        let Some(i) = book.find(&account.id, path) else { return self.error("That file isn't open from the cloud.") };
        let key = book.list[i].path.clone();
        match book.list[i].status {
            SyncStatus::Conflict => {
                return self.status(&key, SyncStatus::Conflict, Some("Choose which version to keep before closing this file.".to_string()));
            }
            SyncStatus::CloseInAppFirst => {
                return self.status(&key, SyncStatus::CloseInAppFirst, Some("Close the document in its app, then confirm.".to_string()));
            }
            _ => {}
        }
        let busy = |this: &Self| {
            this.status(&key, SyncStatus::WaitingToUpload, Some("The file is being saved right now. Choose Done again in a moment.".to_string()));
        };
        match self.changed(book, i) {
            // Missing: an app may be swapping in a save right now, or the copy is gone.
            None => {
                return self.status_forgettable(
                    &key,
                    SyncStatus::WaitingToUpload,
                    "The file isn't on this computer right now; an app may be saving it. Choose Done again in a moment, or stop tracking it."
                        .to_string(),
                );
            }
            Some(false) => {}
            Some(true) if !book.list[i].tracks_remote => {
                let local = book.list[i].local.display().to_string();
                return self.status_forgettable(
                    &key,
                    SyncStatus::ReadOnly,
                    format!("This file was opened read-only, so your changes weren't saved to the cloud. They're kept at {local}."),
                );
            }
            Some(true) => match self.upload(book, &dav, &account.id, &key, self.now()) {
                Upload::Saved if self.changed(book, i) == Some(false) => {}
                // Its status says why.
                Upload::Failed => return,
                _ => return busy(self),
            },
        }
        // The copy goes before the lock and the record: a save that lands now is never deleted.
        let c = book.list[i].clone();
        if !self.retire(&c) {
            return busy(self);
        }
        book.list.remove(i);
        book.runtime.remove(&rt_key(&account.id, &key));
        self.save_book(book);
        if let Some(lock) = &c.lock
            && let Err(e) = Files::new(&dav).unlock(&self.http(), &lock.to_dav())
        {
            log::warn!("couldn't release a cloud file lock ({e}); it runs out on its own");
        }
        // A save that landed after the copy was retired would sit in the cache untracked, and the
        // next disconnect would delete it: it is kept instead, and the user is told where.
        let detail = if std::fs::symlink_metadata(&c.local).is_ok() {
            match self.keep(&account.id, &key, &c.local, "saved after closing") {
                Ok(at) => format!("Closed. A save made while it was closing wasn't uploaded; it is kept at {}", at.display()),
                Err(e) => {
                    // It can't be moved aside, so it stays open: watched and uploaded like any
                    // other save (If-Match still guards it), and Disconnect still refuses.
                    book.list.push(Checkout { lock: None, status: SyncStatus::WaitingToUpload, ..c });
                    self.save_book(book);
                    return self.status(
                        &key,
                        SyncStatus::WaitingToUpload,
                        Some(format!("A save made while the file was closing couldn't be moved aside ({e}), so the file stays open and that save will be uploaded.")),
                    );
                }
            }
        } else {
            CLOSED_DETAIL.to_string()
        };
        self.status(&key, SyncStatus::Saved, Some(detail));
    }

    /// Stops tracking `path` without saving it back: the working copy moves to the kept folder
    /// (never deleted), the lock is released and the file is closed. The last status names where
    /// the copy went. If the copy can't be kept, nothing changes and the file stays open.
    pub fn forget(&self, path: &str) {
        let Some((account, dav)) = self.connected() else { return self.error("Connect to your cloud first.") };
        let mut guard = self.lock_book();
        let book = match self.book(&mut guard) {
            Ok(b) => b,
            Err(e) => return self.error(format!("Couldn't read the list of open cloud files ({e}).")),
        };
        let Some(i) = book.find(&account.id, path) else { return self.error("That file isn't open from the cloud.") };
        // Asked before the copy moves: whatever the server doesn't have is said so.
        let unsaved = self.changed(book, i) != Some(false);
        let c = book.list[i].clone();
        // The copy goes first: if it can't be kept, the checkout (and its lock) stays.
        let mut detail = if std::fs::symlink_metadata(&c.local).is_ok() {
            match self.keep(&account.id, &c.path, &c.local, "kept when you stopped tracking") {
                Ok(at) => format!("{CLOSED_DETAIL}. You stopped tracking this file; your copy is kept at {}", at.display()),
                Err(e) => {
                    return self.error(format!("Couldn't keep your copy of the file ({e}), so it is still open and nothing was removed."));
                }
            }
        } else {
            format!("{CLOSED_DETAIL}. You stopped tracking this file; there was no copy of it on this computer.")
        };
        if unsaved {
            detail.push_str(if detail.ends_with('.') { " " } else { ". " });
            detail.push_str("Your latest changes were not saved to the cloud.");
        }
        book.list.remove(i);
        book.runtime.remove(&rt_key(&account.id, &c.path));
        self.save_book(book);
        drop(guard);
        if let Some(lock) = &c.lock
            && let Err(e) = Files::new(&dav).unlock(&self.http(), &lock.to_dav())
        {
            log::warn!("couldn't release a cloud file lock ({e}); it runs out on its own");
        }
        // Read-only: from now on nothing of it is uploaded.
        self.status(&c.path, SyncStatus::ReadOnly, Some(detail));
    }

    /// Moves every regular file still in `account`'s cache to the kept folder, so removing the
    /// cache deletes no user bytes. Symlinks are skipped, never followed. Stops at the first file
    /// that can't be moved, and says which.
    pub(super) fn sweep_cache(&self, account: &str) -> Result<(), String> {
        let cache = self.cache_dir(account)?;
        // Missing, or a link (removed as a link, never followed): nothing of ours inside.
        if !std::fs::symlink_metadata(&cache).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        let mut pending = vec![cache.clone()];
        while let Some(dir) = pending.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| format!("{} couldn't be read ({e})", dir.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                if meta.is_dir() {
                    pending.push(path);
                } else if meta.is_file() {
                    let relative = path.strip_prefix(&cache).map_err(|_| format!("{} is outside the cache", path.display()))?;
                    let server: String = relative.components().map(|c| format!("/{}", c.as_os_str().to_string_lossy())).collect();
                    self.keep(account, &server, &path, "left in cache").map_err(|e| format!("{} ({e})", path.display()))?;
                }
            }
        }
        Ok(())
    }

    /// Removes a fully synced working copy, unless a save lands while it is removed: it is first
    /// renamed to a private name, checked against the synced version, and only then deleted.
    /// `false`: the copy changed (or is mid-save) and stays.
    fn retire(&self, c: &Checkout) -> bool {
        let inside = self.cache_dir(&c.account).is_ok_and(|cache| c.local.starts_with(cache));
        let Some(synced) = c.synced.as_ref() else { return false };
        if !inside {
            // Not ours to delete; only the record goes.
            return true;
        }
        let name = c.local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(closing) = (1..=99).map(|n| c.local.with_file_name(format!(".~{name}.closing{n}"))).find(|p| std::fs::symlink_metadata(p).is_err())
        else {
            return false;
        };
        if std::fs::rename(&c.local, &closing).is_err() {
            return false;
        }
        let unchanged = read_settled(&closing).is_some_and(|(stamp, bytes)| stamp == synced.stamp || checkout::digest(&bytes) == synced.sha256);
        if !unchanged {
            // Put it back, unless the app has already saved a newer one in its place. Then this
            // changed version would be in the cache with nothing tracking it: it is kept instead.
            let back = std::fs::symlink_metadata(&c.local).is_err() && std::fs::rename(&closing, &c.local).is_ok();
            if !back {
                match self.keep(&c.account, &c.path, &closing, "changed while closing") {
                    Ok(at) => self.error(format!("A version saved while the file was closing is kept at {}.", at.display())),
                    Err(e) => {
                        self.error(format!("A version saved while the file was closing couldn't be kept ({e}); it is at {}.", closing.display()))
                    }
                }
            }
            return false;
        }
        if let Err(e) = std::fs::remove_file(&closing) {
            log::warn!("couldn't remove a synced cloud working copy ({e})");
        }
        // A save that landed after the rename is a new file: it stays, and so does the checkout.
        std::fs::symlink_metadata(&c.local).is_err()
    }

    /// Moves `local` (a file of `account`'s `path`) to where kept copies live, outside the
    /// account's cache. Returns where it went.
    pub(super) fn keep(&self, account: &str, path: &str, local: &Path, label: &str) -> Result<PathBuf, String> {
        let root = self.cache_dir(account).map(|_| self.paths.cloud_root().join("kept").join(account))?;
        let at = checkout::kept_place(&root, path, label)?;
        std::fs::rename(local, &at).map_err(|e| e.to_string())?;
        Ok(at)
    }

    /// Whether the working copy differs from the version last in step with the server. `None`:
    /// it is missing, as it is for a moment while an app swaps in a save.
    fn changed(&self, book: &mut Book, i: usize) -> Option<bool> {
        let c = &book.list[i];
        let stamp = checkout::stamp(&c.local)?;
        if c.synced.as_ref().is_some_and(|s| s.stamp == stamp) {
            return Some(false);
        }
        let Ok(bytes) = std::fs::read(&c.local) else { return Some(true) };
        let same = c.synced.as_ref().is_some_and(|s| s.sha256 == checkout::digest(&bytes));
        if same {
            book.list[i].synced = Some(Synced::of(stamp, &bytes));
        }
        Some(!same)
    }

    /// Settles a conflict on `path` (see the spec's "After a conflict"). `UseMine` is sent only
    /// after the UI has asked the user to confirm.
    pub fn resolve(&self, path: &str, choice: ConflictChoice) {
        let Some((account, dav)) = self.connected() else { return self.error("Connect to your cloud first.") };
        let mut guard = self.lock_book();
        let book = match self.book(&mut guard) {
            Ok(b) => b,
            Err(e) => return self.error(format!("Couldn't read the list of open cloud files ({e}).")),
        };
        let Some(i) = book.find(&account.id, path) else { return self.error("That file isn't open from the cloud.") };
        let status = book.list[i].status;
        match (choice, status) {
            (ConflictChoice::KeepBoth, SyncStatus::Conflict) => self.keep_both(book, i, &dav, &account.login),
            (ConflictChoice::UseMine, SyncStatus::Conflict) => self.use_mine(book, i, &dav),
            (ConflictChoice::UseTheirs, SyncStatus::Conflict) => {
                let c = &mut book.list[i];
                c.status = SyncStatus::CloseInAppFirst;
                c.tracks_remote = false;
                let key = c.path.clone();
                self.save_book(book);
                self.status(
                    &key,
                    SyncStatus::CloseInAppFirst,
                    Some("Close the document in its app, then confirm. Your version is kept as a backup on this computer.".to_string()),
                );
            }
            (ConflictChoice::TheirsClosed, SyncStatus::CloseInAppFirst) => self.use_theirs(book, i, &dav),
            (ConflictChoice::TheirsClosed, _) => self.error("Choose to use their version first."),
            _ => self.error("There's no conflict to settle on that file."),
        }
    }

    /// Uploads the working copy as a new `name (conflict <date> <login>).ext` beside the
    /// original, and makes later saves go there.
    fn keep_both(&self, book: &mut Book, i: usize, dav: &DavAccount, login: &str) {
        let c = book.list[i].clone();
        let Some((stamp, bytes)) = read_settled(&c.local) else { return self.error("Couldn't read your version of the file.") };
        let files = Files::new(dav);
        let http = self.http();
        let label = format!("conflict {} {}", minute(), name_safe(login));
        let mut made = None;
        for n in 1..=CONFLICT_NAMES {
            let target = format!("{}{}", parent(&c.remote), checkout::labelled(file_name(&c.remote), &label, n));
            match files.create(&http, &target, bytes.clone()) {
                Ok(etag) => {
                    made = Some((target, etag));
                    break;
                }
                Err(DavError::Conflict) => {}
                Err(e) => return self.error(format!("Couldn't upload your copy. {}", explain_dav(&e).0)),
            }
        }
        let Some((target, etag)) = made else {
            return self.error("Couldn't find a free name for your copy in that folder. Rename or move the earlier conflict copies, then try again.");
        };
        if let Some(lock) = &c.lock
            && let Err(e) = files.unlock(&http, &lock.to_dav())
        {
            log::info!("couldn't release the original's lock ({e}); it runs out on its own");
        }
        let entry = &mut book.list[i];
        entry.remote = target.clone();
        entry.etag = etag;
        entry.lock = None;
        entry.synced = Some(Synced::of(stamp, &bytes));
        entry.tracks_remote = true;
        entry.status = SyncStatus::Saved;
        let key = entry.path.clone();
        book.runtime.insert(rt_key(&c.account, &key), Runtime::default());
        self.save_book(book);
        self.status(&key, SyncStatus::Saved, Some(format!("Saving to {}", file_name(&target))));
    }

    /// Replaces the server's version with the working copy, with no version check.
    fn use_mine(&self, book: &mut Book, i: usize, dav: &DavAccount) {
        let c = book.list[i].clone();
        let Some((stamp, bytes)) = read_settled(&c.local) else { return self.error("Couldn't read your version of the file.") };
        let files = Files::new(dav);
        let http = self.http();
        let lock = c.lock.as_ref().map(HeldLock::to_dav);
        let result = match files.put(&http, &c.remote, bytes.clone(), None, lock.as_ref()) {
            // A lock the server no longer knows: the overwrite was confirmed either way.
            Err(DavError::Conflict) if lock.is_some() => files.put(&http, &c.remote, bytes.clone(), None, None),
            other => other,
        };
        match result {
            Ok(etag) => {
                let entry = &mut book.list[i];
                entry.etag = etag;
                entry.synced = Some(Synced::of(stamp, &bytes));
                entry.tracks_remote = true;
                entry.status = SyncStatus::Saved;
                let key = entry.path.clone();
                book.runtime.insert(rt_key(&c.account, &key), Runtime::default());
                self.save_book(book);
                self.status(&key, SyncStatus::Saved, None);
            }
            Err(DavError::Locked) => self.error("Someone else has this file locked, so your version couldn't replace theirs yet."),
            Err(e) => self.error(format!("Couldn't upload your version. {}", explain_dav(&e).0)),
        }
    }

    /// After the app has closed the document: downloads the server's version, moves the working
    /// copy to a backup beside it, puts theirs in its place and opens it again.
    fn use_theirs(&self, book: &mut Book, i: usize, dav: &DavAccount) {
        let c = book.list[i].clone();
        // Download first: a failure leaves the working copy as it is.
        let (bytes, etag) = match Files::new(dav).get(&self.http(), &c.remote, super::transport::MAX_FILE) {
            Ok(got) => got,
            Err(e) => return self.error(format!("Couldn't download their version. {}", explain_dav(&e).0)),
        };
        let mut kept = None;
        if std::fs::symlink_metadata(&c.local).is_ok() {
            match self.keep(&c.account, &c.path, &c.local, &format!("my version {}", minute())) {
                Ok(at) => kept = Some(at),
                Err(e) => return self.error(format!("Couldn't keep your version as a backup ({e}), so nothing was replaced.")),
            }
        }
        if let Err(e) = crate::paths::write_atomic(&c.local, &bytes) {
            return self.error(format!("Couldn't save their version on this computer ({e}). Your version is kept as a backup."));
        }
        let Some(stamp) = checkout::stamp(&c.local) else { return self.error("Couldn't save their version on this computer.") };
        let entry = &mut book.list[i];
        entry.etag = etag;
        entry.synced = Some(Synced::of(stamp, &bytes));
        entry.tracks_remote = true;
        entry.status = SyncStatus::Saved;
        let (key, app, local) = (entry.path.clone(), entry.app.clone(), entry.local.clone());
        book.runtime.insert(rt_key(&c.account, &key), Runtime { lock_checked: Some(self.now()), ..Runtime::default() });
        self.save_book(book);
        let detail = kept.map(|b| format!("Your version is kept at {}", b.display()));
        self.status(&key, SyncStatus::Saved, detail);
        self.launch(&app, &local);
    }
}

/// PUT with `If-Match`, sending the lock when one is held. A 412 with a lock may mean only that
/// the server forgot the lock, so it is retried once without: `If-Match` still decides.
fn put_checked<H: Http>(files: &Files<'_>, http: &H, c: &Checkout, bytes: Vec<u8>) -> craft_nextcloud::Result<String> {
    let Some(lock) = c.lock.as_ref().map(HeldLock::to_dav) else { return files.put(http, &c.remote, bytes, Some(&c.etag), None) };
    match files.put(http, &c.remote, bytes.clone(), Some(&c.etag), Some(&lock)) {
        Err(DavError::Conflict) => match files.put(http, &c.remote, bytes, Some(&c.etag), None) {
            // Our lock still stands, so the 412 was the version check.
            Err(DavError::Locked) => Err(DavError::Conflict),
            other => other,
        },
        other => other,
    }
}

/// The working copy's bytes and the stamp they were read at, if it didn't change while read.
fn read_settled(local: &Path) -> Option<(checkout::Stamp, Vec<u8>)> {
    let before = checkout::stamp(local)?;
    let bytes = std::fs::read(local).ok()?;
    (checkout::stamp(local) == Some(before)).then_some((before, bytes))
}

fn open_message(e: &DavError) -> String {
    match e {
        DavError::NotFound => "That file wasn't found in your cloud.".to_string(),
        other => explain_dav(other).0,
    }
}
