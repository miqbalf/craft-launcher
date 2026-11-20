//! Signing in, signing out and finding the connected account again.

use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::time::Instant;

use artcraft_launcher_core::cloud::{CloudAccount, normalize_server};
use artcraft_launcher_core::protocol::Event;
use craft_nextcloud::http::Http;
use craft_nextcloud::login::LoginFlow;
use craft_nextcloud::{Account as DavAccount, Error as DavError};
use craft_suite_account::{NewAccount, Secret, Shared, TokenStore};

use super::{CloudService, Session};

/// Polls that may fail in a row (network blips) before the sign-in is abandoned.
const POLL_FAILURES: u32 = 3;

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    /// The connected account, looked up in the keychain-backed list the first time.
    pub fn account(&self) -> Option<CloudAccount> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.account.clone()).or_else(|| self.load())
    }

    pub(super) fn session_dav(&self) -> Option<DavAccount> {
        self.account()?;
        self.session.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.dav.clone())
    }

    /// Finds the signed-in cloud account the suite account list holds, with its token.
    fn load(&self) -> Option<CloudAccount> {
        let Shared::Session { account, token } = self.store.shared() else { return None };
        if !account.linked || !(account.service.starts_with("https://") || account.service.starts_with("http://")) {
            return None;
        }
        let cloud =
            CloudAccount { id: account.id.clone(), server: account.service.clone(), login: account.user.clone(), display: account.display.clone() };
        let dav = DavAccount::new(account.service, account.user, token.expose());
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(Session { account: cloud.clone(), dav });
        Some(cloud)
    }

    /// Signs in to `server` (as typed): emits [`Event::CloudLoginUrl`], opens it, waits for the
    /// user to approve, then emits [`Event::CloudConnected`]. Blocks until then, or until
    /// [`Self::cancel_connect`], a newer sign-in, or the time limit.
    pub fn connect(&self, server: &str) {
        let base = match normalize_server(server) {
            Ok(b) => b,
            Err(message) => return self.emit(Event::CloudError(message)),
        };
        let mine = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
        let http = self.http();
        let flow = match LoginFlow::start(&http, &base) {
            Ok(f) => f,
            Err(e) => return self.emit(Event::CloudError(start_message(&e))),
        };
        // The server picks this address: only a secure page (or this computer) is opened.
        let url = flow.login_url().to_string();
        if !(url.starts_with("https://") || crate::http::is_loopback(&url)) {
            return self.emit(Event::CloudError("That server sent a sign-in address that isn't secure, so it wasn't opened.".to_string()));
        }
        self.emit(Event::CloudLoginUrl(url.clone()));
        if let Err(e) = (self.open_url)(&url) {
            log::warn!("couldn't open the sign-in page: {e}");
        }
        let Some(approved) = self.wait_for_approval(&flow, &http, mine) else { return };
        if self.attempt.load(Ordering::SeqCst) != mine {
            return;
        }
        match self.record(&base, &approved.login_name, approved.app_password.expose()) {
            Ok(account) => self.emit(Event::CloudConnected(account)),
            Err(message) => self.emit(Event::CloudError(message)),
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
                    self.emit(Event::CloudError(start_message(&e)));
                    return None;
                }
            }
            if started.elapsed() >= self.give_up_after {
                self.emit(Event::CloudError("The sign-in wasn't approved in time. Connect again to retry.".to_string()));
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

    /// Saves the sign-in (token to the keychain, account to the list) and makes it current.
    fn record(&self, base: &str, login: &str, password: &str) -> Result<CloudAccount, String> {
        let new = NewAccount { service: base.to_string(), user: login.to_string(), display: login.to_string(), email: None, linked: true };
        let saved = self
            .store
            .sign_in(&new, &Secret::new(password), jiff::Timestamp::now().as_second())
            .map_err(|e| format!("Couldn't save your sign-in on this computer ({e})."))?;
        let account = CloudAccount { id: saved.id, server: base.to_string(), login: login.to_string(), display: login.to_string() };
        let dav = DavAccount::new(base, login, password);
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(Session { account: account.clone(), dav });
        *self.last_listed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(account)
    }

    /// Forgets the account: its keychain token, its record, and its folder under the cloud root
    /// (nothing outside it). Emits [`Event::CloudDisconnected`], or an error if a step failed.
    pub fn disconnect(&self) {
        self.attempt.fetch_add(1, Ordering::SeqCst);
        let Some(account) = self.account() else { return self.emit(Event::CloudDisconnected) };
        let mut failed = Vec::new();
        if let Err(e) = self.store.discard(&account.id) {
            failed.push(format!("the account record ({e})"));
        }
        // The record may already be gone while the keychain entry is not.
        if let Err(e) = self.store.forget(&account.id) {
            failed.push(format!("the saved sign-in ({e})"));
        }
        if let Err(e) = self.remove_cache(&account.id) {
            failed.push(format!("the downloaded copies ({e})"));
        }
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.last_listed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if failed.is_empty() {
            self.emit(Event::CloudDisconnected);
        } else {
            self.emit(Event::CloudError(format!("Couldn't remove {}.", failed.join(", "))));
        }
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
