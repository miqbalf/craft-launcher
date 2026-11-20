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

use super::checkout::{self, Book, Checkout, HeldLock, Recent, Runtime, Synced};
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
        self.emit(Event::CloudFileStatus { path: path.to_string(), status, detail });
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
        if let Err(e) = place(&local, &bytes) {
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
        book.runtime.insert(path.to_string(), Runtime { lock_checked: Some(self.now()), ..Runtime::default() });
        self.save_book(book);
        drop(guard);
        self.remember(&account.id, path, &app);
        self.status(path, status, None);
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
            // The server doesn't say who; the UI words an empty name as "someone else".
            Err(DavError::Locked) => {
                self.emit(Event::CloudLockedBy { path: path.to_string(), by: String::new() });
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

    /// Watches the open files for good: [`Self::resume`], then [`Self::tick`] once a second.
    pub fn run_sync(&self) {
        self.resume();
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
            self.refresh_lock(book, &dav, &key, now);
            self.watch(book, &dav, &key, now);
        }
    }

    /// Refreshes the file's lock at half its timeout. A lock the server no longer knows is taken
    /// again; if someone else has it now, saves still carry `If-Match`.
    fn refresh_lock(&self, book: &mut Book, dav: &DavAccount, key: &str, now: Instant) {
        let Some(i) = book.list.iter().position(|c| c.path == key) else { return };
        let Some(lock) = book.list[i].lock.clone() else { return };
        let rt = book.runtime.entry(key.to_string()).or_default();
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
    fn watch(&self, book: &mut Book, dav: &DavAccount, key: &str, now: Instant) {
        let Some(i) = book.list.iter().position(|c| c.path == key) else { return };
        let c = &book.list[i];
        if !c.tracks_remote {
            return;
        }
        let attempt = self.attempt.load(Ordering::SeqCst);
        let rt = book.runtime.entry(key.to_string()).or_default();
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
        self.upload(book, dav, key, now);
    }

    /// Uploads the working copy over the version it started from. A newer version on the server
    /// gives [`SyncStatus::Conflict`] and stops uploads of this file; a failure leaves the
    /// working copy alone and retries later.
    fn upload(&self, book: &mut Book, dav: &DavAccount, key: &str, now: Instant) -> Upload {
        let Some(i) = book.list.iter().position(|c| c.path == key) else { return Upload::Failed };
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
            book.runtime.entry(key.to_string()).or_default().seen = None;
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
        let rt = book.runtime.entry(key.to_string()).or_default();
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
        if self.changed(book, i) {
            if !book.list[i].tracks_remote {
                let local = book.list[i].local.display().to_string();
                return self.status(
                    &key,
                    SyncStatus::ReadOnly,
                    Some(format!("This file was opened read-only, so your changes weren't saved to the cloud. They're kept at {local}.")),
                );
            }
            match self.upload(book, &dav, &key, self.now()) {
                Upload::Saved if !self.changed(book, i) => {}
                // Its status says why.
                Upload::Failed => return,
                _ => {
                    return self.status(
                        &key,
                        SyncStatus::WaitingToUpload,
                        Some("The file changed while it was being saved. Choose Done again.".to_string()),
                    );
                }
            }
        }
        let c = book.list.remove(i);
        book.runtime.remove(&key);
        if let Some(lock) = &c.lock
            && let Err(e) = Files::new(&dav).unlock(&self.http(), &lock.to_dav())
        {
            log::warn!("couldn't release a cloud file lock ({e}); it runs out on its own");
        }
        if let Ok(cache) = self.cache_dir(&account.id)
            && c.local.starts_with(&cache)
            && let Err(e) = std::fs::remove_file(&c.local)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("couldn't remove a synced cloud working copy ({e})");
        }
        self.save_book(book);
        self.status(&key, SyncStatus::Saved, Some(CLOSED_DETAIL.to_string()));
    }

    /// Whether the working copy differs from the version last in step with the server. A missing
    /// working copy has nothing to send.
    fn changed(&self, book: &mut Book, i: usize) -> bool {
        let c = &book.list[i];
        let Some(stamp) = checkout::stamp(&c.local) else { return false };
        if c.synced.as_ref().is_some_and(|s| s.stamp == stamp) {
            return false;
        }
        let Ok(bytes) = std::fs::read(&c.local) else { return true };
        let same = c.synced.as_ref().is_some_and(|s| s.sha256 == checkout::digest(&bytes));
        if same {
            book.list[i].synced = Some(Synced::of(stamp, &bytes));
        }
        !same
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
        book.runtime.insert(key.clone(), Runtime::default());
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
                book.runtime.insert(key.clone(), Runtime::default());
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
            let Some(backup) = checkout::free_sibling(&c.local, &format!("my version {}", minute())) else {
                return self.error("Couldn't find a free name to keep your version under.");
            };
            if let Err(e) = std::fs::rename(&c.local, &backup) {
                return self.error(format!("Couldn't keep your version as a backup ({e}), so nothing was replaced."));
            }
            kept = Some(backup);
        }
        if let Err(e) = place(&c.local, &bytes) {
            return self.error(format!("Couldn't save their version on this computer ({e}). Your version is kept as a backup."));
        }
        let Some(stamp) = checkout::stamp(&c.local) else { return self.error("Couldn't save their version on this computer.") };
        let entry = &mut book.list[i];
        entry.etag = etag;
        entry.synced = Some(Synced::of(stamp, &bytes));
        entry.tracks_remote = true;
        entry.status = SyncStatus::Saved;
        let (key, app, local) = (entry.path.clone(), entry.app.clone(), entry.local.clone());
        book.runtime.insert(key.clone(), Runtime { lock_checked: Some(self.now()), ..Runtime::default() });
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

/// Writes a freshly downloaded file at `local`. Something already there that no checkout owns
/// (left from an earlier run) is moved aside, never replaced.
fn place(local: &Path, bytes: &[u8]) -> Result<(), String> {
    if std::fs::symlink_metadata(local).is_ok() {
        let aside = checkout::free_sibling(local, "earlier copy").ok_or("no free name to move an earlier copy to")?;
        std::fs::rename(local, &aside).map_err(|e| e.to_string())?;
        log::info!("moved an earlier cloud working copy aside before downloading again");
    }
    crate::paths::write_atomic(local, bytes)
}

fn open_message(e: &DavError) -> String {
    match e {
        DavError::NotFound => "That file wasn't found in your cloud.".to_string(),
        other => explain_dav(other).0,
    }
}
