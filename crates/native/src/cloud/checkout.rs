//! What the launcher keeps about each file opened from the cloud (`checkouts.json`) and about the
//! files opened lately (`recent.json`). Neither file holds a secret: a lock token is not one, and
//! the sign-in stays in the keychain.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use artcraft_launcher_core::cloud::SyncStatus;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many opened files `recent.json` remembers.
pub(super) const RECENT_MAX: usize = 50;

/// A file's modification time and size: what the watcher compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    secs: u64,
    nanos: u32,
    size: u64,
}

/// The stamp of the file at `path`, if it is there.
pub(super) fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let since = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).unwrap_or_default();
    Some(Stamp { secs: since.as_secs(), nanos: since.subsec_nanos(), size: meta.len() })
}

/// The working copy's state that matches the server's version at the checkout's ETag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Synced {
    pub stamp: Stamp,
    pub sha256: String,
}

impl Synced {
    pub fn of(stamp: Stamp, bytes: &[u8]) -> Self {
        Self { stamp, sha256: digest(bytes) }
    }
}

pub(super) fn digest(bytes: &[u8]) -> String {
    crate::http::hex(&Sha256::digest(bytes))
}

/// A WebDAV lock the launcher holds.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct HeldLock {
    pub token: String,
    pub path: String,
    pub timeout_secs: u32,
}

impl HeldLock {
    pub fn from_dav(lock: craft_nextcloud::dav::Lock) -> Self {
        Self { token: lock.token, path: lock.path, timeout_secs: lock.timeout_secs }
    }

    pub fn to_dav(&self) -> craft_nextcloud::dav::Lock {
        craft_nextcloud::dav::Lock { token: self.token.clone(), path: self.path.clone(), timeout_secs: self.timeout_secs }
    }
}

/// One file opened from the cloud. No `Debug`: the lock token stays out of logs.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Checkout {
    /// The account the file belongs to.
    pub account: String,
    /// The server path it was opened as: the key commands and events use.
    pub path: String,
    /// The server path saves go to. The opened path, or the conflict copy after "Keep both".
    pub remote: String,
    /// The working copy the app edits.
    pub local: PathBuf,
    /// The ETag of the server's version at `remote` that the working copy was last in step with.
    pub etag: String,
    pub lock: Option<HeldLock>,
    pub app: String,
    /// Whether saves are uploaded. Off while read-only, in conflict, or waiting for the app to
    /// close the document before "Use theirs".
    pub tracks_remote: bool,
    pub status: SyncStatus,
    /// The working copy as it was when last in step with `etag`.
    pub synced: Option<Synced>,
}

#[derive(Default, Serialize, Deserialize)]
struct CheckoutsFile {
    checkouts: Vec<Checkout>,
}

/// What the watcher remembers between polls; not saved.
#[derive(Default)]
pub(super) struct Runtime {
    /// The stamp last seen, and since when it has not changed.
    pub seen: Option<(Stamp, Instant)>,
    pub retry_at: Option<Instant>,
    pub backoff: Duration,
    /// When the lock was last taken or refreshed (`None`: refresh at the next poll).
    pub lock_checked: Option<Instant>,
    /// The sign-in attempt a rejected upload was made under; uploads wait for a new one.
    pub rejected_under: Option<u64>,
}

/// The key of a file's [`Runtime`]: its account and the path it was opened as.
pub(super) fn rt_key(account: &str, path: &str) -> (String, String) {
    (account.to_string(), path.to_string())
}

/// The open files, as loaded from `checkouts.json`.
pub(super) struct Book {
    file: PathBuf,
    pub list: Vec<Checkout>,
    /// Keyed by [`rt_key`]: two accounts may have the same path open.
    pub runtime: HashMap<(String, String), Runtime>,
    /// The files open for live editing. Not saved: their apps can't be followed across a restart.
    pub live: Vec<super::live::LiveSession>,
}

impl Book {
    /// Reads `file`. A missing file is an empty book; an unreadable one is an error, so nothing
    /// acts on (or deletes) working copies the launcher can no longer account for.
    pub fn load(file: PathBuf) -> Result<Self, String> {
        let list = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<CheckoutsFile>(&bytes).map_err(|e| format!("{} is unreadable ({e})", file.display()))?.checkouts,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("couldn't read {} ({e})", file.display())),
        };
        Ok(Self { file, list, runtime: HashMap::new(), live: Vec::new() })
    }

    /// Writes the book atomically (a private temporary file renamed into place, mode 0600).
    pub fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&CheckoutsFile { checkouts: self.list.clone() }).map_err(|e| e.to_string())?;
        crate::paths::write_atomic(&self.file, &bytes)
    }

    /// The index of `account`'s checkout opened as, or now saving to, `path`.
    pub fn find(&self, account: &str, path: &str) -> Option<usize> {
        self.list.iter().position(|c| c.account == account && (c.path == path || c.remote == path))
    }

    /// The index of `account`'s checkout opened as `path`, exactly: the watcher's lookup.
    pub fn index(&self, account: &str, path: &str) -> Option<usize> {
        self.list.iter().position(|c| c.account == account && c.path == path)
    }

    /// Whether a file of `account` is open, checked out or live.
    pub fn has_account(&self, account: &str) -> bool {
        self.list.iter().any(|c| c.account == account) || self.live.iter().any(|l| l.account == account)
    }

    /// Whether a file of an account other than `account` is open.
    pub fn has_other_than(&self, account: &str) -> bool {
        self.list.iter().any(|c| c.account != account) || self.live.iter().any(|l| l.account != account)
    }

    /// Whether `local` would be the same file on disk as some checkout's working copy.
    pub fn owns_slot(&self, local: &Path) -> bool {
        self.list.iter().any(|c| same_slot(&c.local, local))
    }
}

/// Where the working copy of `server_path` lives under `cache`: the same folders and real file
/// name. Refuses anything that could land outside `cache`.
pub(super) fn working_copy(cache: &Path, server_path: &str) -> Result<PathBuf, String> {
    let mut out = cache.to_path_buf();
    let mut any = false;
    for segment in server_path.split('/').filter(|s| !s.is_empty()) {
        let mut parts = Path::new(segment).components();
        let plain = matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None));
        if !plain || segment.contains(['\\', '\0']) {
            return Err("That file's name can't be used on this computer.".to_string());
        }
        out.push(segment);
        any = true;
    }
    if any { Ok(out) } else { Err("That isn't a file.".to_string()) }
}

/// Whether `a` and `b` may name the same file. macOS folds letter case by default, and a server
/// can hold `Report.docx` beside `report.docx`, so paths that differ only in case count as the
/// same; so do two names of one existing file (same device and inode, on Unix).
pub(super) fn same_slot(a: &Path, b: &Path) -> bool {
    if a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    false
}

/// Where a file the user must not lose (their version after "Use theirs", an earlier copy found
/// in the cache) is kept: under `kept_root` (outside every account's cache, so disconnecting never
/// deletes it), in the server path's folders, as `name (label).ext` with a free name.
pub(super) fn kept_place(kept_root: &Path, server_path: &str, label: &str) -> Result<PathBuf, String> {
    let beside = working_copy(kept_root, server_path)?;
    let dir = beside.parent().ok_or("no folder to keep the copy in")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
    free_sibling(&beside, label).ok_or_else(|| "no free name to keep the copy under".to_string())
}

/// A name beside `path` that is not taken: `stem (label).ext`, then `stem (label) 2.ext`, ...
pub(super) fn free_sibling(path: &Path, label: &str) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    (1..=99).map(|n| path.with_file_name(labelled(&name, label, n))).find(|p| std::fs::symlink_metadata(p).is_err())
}

/// `stem (label).ext`, with ` n` after the parenthesis from 2 on.
pub(super) fn labelled(name: &str, label: &str, n: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let count = if n > 1 { format!(" {n}") } else { String::new() };
    format!("{stem} ({label}){count}{ext}")
}

/// One file in the launcher's own Recent list.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Recent {
    pub account: String,
    pub path: String,
    pub name: String,
    pub app: String,
    /// Seconds since the Unix epoch.
    pub opened: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct RecentFile {
    recent: Vec<Recent>,
}

/// The Recent list in `file`, newest first. An unreadable file reads as empty: it only lists.
pub(super) fn read_recent(file: &Path) -> Vec<Recent> {
    std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<RecentFile>(&b).ok()).map(|f| f.recent).unwrap_or_default()
}

/// Puts `item` first in `file`, once per account and path, keeping at most [`RECENT_MAX`].
pub(super) fn remember(file: &Path, item: Recent) -> Result<(), String> {
    let mut recent = read_recent(file);
    recent.retain(|r| !(r.account == item.account && r.path == item.path));
    recent.insert(0, item);
    recent.truncate(RECENT_MAX);
    let bytes = serde_json::to_vec_pretty(&RecentFile { recent }).map_err(|e| e.to_string())?;
    crate::paths::write_atomic(file, &bytes)
}
