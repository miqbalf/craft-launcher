//! Signing in, signing out and finding the connected account again.

use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::time::Instant;

use artcraft_launcher_core::cloud::{CloudAccount, normalize_server};
use artcraft_launcher_core::protocol::Event;
use craft_nextcloud::http::Http;
use craft_nextcloud::login::LoginFlow;
use craft_nextcloud::{Account as DavAccount, Error as DavError};
use craft_suite_account::{Secret, TokenStore, accounts::handle};
use serde::{Deserialize, Serialize};

use super::{CloudService, Session};

/// What is kept on disk about the connected account: nothing secret.
#[derive(Serialize, Deserialize)]
struct Record {
    server: String,
    login: String,
    display: String,
}

/// The keychain entry of an account's app password. Its own namespace: the suite account list
/// is never involved.
fn token_handle(server: &str, login: &str) -> String {
    format!("artcraft-launcher-nextcloud:{}", handle(server, login))
}

/// Polls that may fail in a row (network blips) before the sign-in is abandoned.
const POLL_FAILURES: u32 = 3;

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    /// The connected account, looked up in the keychain-backed list the first time.
    pub fn account(&self) -> Option<CloudAccount> {
        // The guard must be gone before `load`, which takes the same lock.
        let held = self.session.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.account.clone());
        held.or_else(|| self.load())
    }

    pub(super) fn session_dav(&self) -> Option<DavAccount> {
        self.account()?;
        self.session.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.dav.clone())
    }

    fn record_path(&self) -> std::path::PathBuf {
        self.paths.cloud_root().join("account.json")
    }

    fn read_record(&self) -> Option<Record> {
        serde_json::from_slice(&std::fs::read(self.record_path()).ok()?).ok()
    }

    /// Finds the connected account: the record on disk and its app password in the keychain.
    /// Either one missing means not connected.
    fn load(&self) -> Option<CloudAccount> {
        let record = self.read_record()?;
        let token = match self.tokens.get(&token_handle(&record.server, &record.login)) {
            Ok(Some(token)) => token,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("couldn't read the cloud sign-in from the keychain: {e}");
                return None;
            }
        };
        let cloud = CloudAccount {
            id: handle(&record.server, &record.login),
            server: record.server.clone(),
            login: record.login.clone(),
            display: record.display,
        };
        let dav = DavAccount::new(record.server, record.login, token.expose());
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(Session { account: cloud.clone(), dav });
        Some(cloud)
    }

    /// Signs in to `server` (as typed): emits [`Event::CloudLoginUrl`], opens it, waits for the
    /// user to approve, then emits [`Event::CloudConnected`]. Blocks until then, or until
    /// [`Self::cancel_connect`], a newer sign-in, or the time limit.
    pub fn connect(&self, server: &str) {
        let base = match normalize_server(server) {
            Ok(b) => b,
            Err(message) => return self.emit(Event::CloudConnectFailed(message)),
        };
        let mine = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
        let http = self.http();
        if let Err(message) = check_server(&http, &base) {
            return self.connect_failed(mine, message);
        }
        let flow = match LoginFlow::start(&http, &base) {
            Ok(f) => f,
            Err(e) => return self.connect_failed(mine, start_message(&e)),
        };
        // The server picks this address: only a secure page (or this computer) is opened.
        let url = flow.login_url().to_string();
        if !(url.starts_with("https://") || crate::http::is_loopback(&url)) {
            return self.connect_failed(mine, "That server sent a sign-in address that isn't secure, so it wasn't opened.".to_string());
        }
        if self.attempt.load(Ordering::SeqCst) != mine {
            return;
        }
        self.emit(Event::CloudLoginUrl(url.clone()));
        if let Err(e) = (self.open_url)(&url) {
            log::warn!("couldn't open the sign-in page: {e}");
        }
        let Some(approved) = self.wait_for_approval(&flow, &http, mine) else { return };
        // Checked and recorded under the book lock, as Disconnect and every file action run: no
        // file opens for the account being replaced, and a cancel or newer sign-in that came
        // first wins.
        let mut book = self.lock_book();
        if self.attempt.load(Ordering::SeqCst) != mine {
            return;
        }
        // Files still open from another account would be watched by nobody (or, worse, as this
        // one): they must be finished first.
        let refusal = match self.book(&mut book) {
            Ok(open) if open.has_other_than(&handle(&base, &approved.login_name)) => {
                Some("Finish or close the files open from your other cloud account before connecting a different one.".to_string())
            }
            Ok(_) => None,
            Err(e) => Some(format!("Couldn't read the list of open cloud files ({e}), so the new account wasn't connected.")),
        };
        if let Some(message) = refusal {
            return self.emit(Event::CloudConnectFailed(message));
        }
        match self.record(&base, &approved.login_name, approved.app_password.expose()) {
            Ok(account) => self.emit(Event::CloudConnected(account)),
            Err(message) => self.emit(Event::CloudConnectFailed(message)),
        }
    }

    /// Ends sign-in `mine` with `message`, unless a cancel or a newer sign-in already ended it.
    fn connect_failed(&self, mine: u64, message: String) {
        if self.attempt.load(Ordering::SeqCst) == mine {
            self.emit(Event::CloudConnectFailed(message));
        }
    }

    /// Stops a sign-in that is waiting for the browser. It ends without an event.
    pub fn cancel_connect(&self) {
        self.attempt.fetch_add(1, Ordering::SeqCst);
    }

    /// Polls until the user approves. `None` when stopped, timed out or failed (the last two
    /// have emitted their error).
    fn wait_for_approval(&self, flow: &LoginFlow, http: &H, mine: u64) -> Option<craft_nextcloud::login::AppPassword> {
        let started = Instant::now();
        let mut failures = 0;
        loop {
            self.sleep(mine);
            if self.attempt.load(Ordering::SeqCst) != mine {
                return None;
            }
            match flow.poll(http) {
                Ok(Some(done)) => return Some(done),
                Ok(None) => failures = 0,
                Err(DavError::Transport(why)) if failures + 1 < POLL_FAILURES => {
                    log::info!("sign-in check failed ({why}); trying again");
                    failures += 1;
                }
                Err(e) => {
                    self.connect_failed(mine, start_message(&e));
                    return None;
                }
            }
            if started.elapsed() >= self.give_up_after {
                self.connect_failed(mine, "The sign-in wasn't approved in time. Connect again to retry.".to_string());
                return None;
            }
        }
    }

    /// Waits one poll interval, in short slices so a cancel is noticed quickly.
    fn sleep(&self, mine: u64) {
        let end = Instant::now() + self.poll_every;
        while self.attempt.load(Ordering::SeqCst) == mine {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(std::time::Duration::from_millis(100)));
        }
    }

    /// Saves the sign-in: the app password to the keychain, the account (no secret) beside the
    /// working copies. The suite account list is not touched.
    fn record(&self, base: &str, login: &str, password: &str) -> Result<CloudAccount, String> {
        let token = token_handle(base, login);
        let saved = |e: &dyn std::fmt::Display| format!("Couldn't save your sign-in on this computer ({e}).");
        self.tokens.put(&token, &Secret::new(password)).map_err(|e| saved(&e))?;
        let record = Record { server: base.to_string(), login: login.to_string(), display: login.to_string() };
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| saved(&e))?;
        if let Err(e) = crate::paths::write_atomic(&self.record_path(), &bytes) {
            let _ = self.tokens.remove(&token);
            return Err(saved(&e));
        }
        let account = CloudAccount { id: handle(base, login), server: base.to_string(), login: login.to_string(), display: login.to_string() };
        let dav = DavAccount::new(base, login, password);
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(Session { account: account.clone(), dav });
        self.forget_listings();
        Ok(account)
    }

    /// Forgets the account: its app password, its record, and its folder under the cloud root
    /// (nothing outside it). Emits [`Event::CloudDisconnected`], or an error if a step failed.
    ///
    /// Refused while any of its files is still open from the cloud: their working copies may hold
    /// changes not uploaded yet, and the folder holding them would be deleted.
    pub fn disconnect(&self) {
        self.attempt.fetch_add(1, Ordering::SeqCst);
        // Held to the end, so the watcher can't start an upload for the account being removed.
        let mut book = self.lock_book();
        // A record gone or unreadable on disk still leaves the account signed in for this session:
        // it is the one forgotten then.
        let held = self.session.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.account.clone());
        let record = match (self.read_record(), held) {
            (Some(record), _) => record,
            (None, Some(a)) => Record { server: a.server, login: a.login, display: a.display },
            (None, None) => {
                self.forget_session();
                return self.emit(Event::CloudDisconnected);
            }
        };
        let id = handle(&record.server, &record.login);
        match self.book(&mut book) {
            Ok(open) if open.has_account(&id) => return self.emit(Event::CloudError("Finish or close your open cloud files first.".to_string())),
            Ok(_) => {}
            Err(e) => return self.emit(Event::CloudError(format!("Couldn't read the list of open cloud files ({e}), so nothing was removed."))),
        }
        // Nothing the server may not have is deleted: whatever is still in the cache is kept.
        if let Err(e) = self.sweep_cache(&id) {
            return self.emit(Event::CloudError(format!(
                "Couldn't keep a file left among the downloaded copies, so nothing was removed and you're still connected: {e}"
            )));
        }
        let mut failed = Vec::new();
        if let Err(e) = self.tokens.remove(&token_handle(&record.server, &record.login)) {
            failed.push(format!("the saved sign-in ({e})"));
        }
        if let Err(e) = self.remove_cache(&id) {
            failed.push(format!("the downloaded copies ({e})"));
        }
        // Last: while the record exists a failed step can be retried.
        if failed.is_empty()
            && let Err(e) = std::fs::remove_file(self.record_path())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("the account record ({e})"));
        }
        self.forget_session();
        if failed.is_empty() {
            self.emit(Event::CloudDisconnected);
        } else {
            self.emit(Event::CloudError(format!("Couldn't remove {}.", failed.join(", "))));
        }
    }

    /// Drops the signed-in account from memory: the watcher and every action see no account.
    fn forget_session(&self) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.forget_listings();
    }

    fn remove_cache(&self, id: &str) -> Result<(), String> {
        // An id is a hex handle; anything else could climb out of the cloud root.
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("unexpected account id".to_string());
        }
        let dir = self.paths.cloud_root().join(id);
        let Ok(meta) = std::fs::symlink_metadata(&dir) else { return Ok(()) };
        let removed = if meta.is_dir() { std::fs::remove_dir_all(&dir) } else { std::fs::remove_file(&dir) };
        removed.map_err(|e| e.to_string())
    }
}

/// What to tell the user when the sign-in couldn't start or finish.
fn start_message(e: &DavError) -> String {
    match e {
        DavError::NotFound => "That address doesn't look like a Nextcloud server.".to_string(),
        DavError::Transport(why) => format!("Couldn't reach the server ({why})."),
        DavError::Status { status, .. } => format!("The server answered HTTP {status}."),
        _ => "That server's answer wasn't a Nextcloud sign-in.".to_string(),
    }
}

/// Whether `base` is a Nextcloud server, from its `status.php`, before any sign-in starts.
fn check_server<H: Http>(http: &H, base: &str) -> Result<(), String> {
    let not_nextcloud = || "That address doesn't look like a Nextcloud server.".to_string();
    let request =
        craft_nextcloud::http::Request::new(craft_nextcloud::http::Method::Get, format!("{base}/status.php")).header("accept", "application/json");
    let resp = http.send(request).map_err(|e| format!("Couldn't reach the server ({e})."))?;
    if !resp.is_success() {
        return Err(not_nextcloud());
    }
    let status: serde_json::Value = serde_json::from_slice(&resp.body).map_err(|_| not_nextcloud())?;
    let named = ["productname", "version"].iter().any(|k| status.get(k).and_then(serde_json::Value::as_str).is_some_and(|v| !v.is_empty()));
    if status.get("installed").and_then(serde_json::Value::as_bool) == Some(true) && named { Ok(()) } else { Err(not_nextcloud()) }
}
