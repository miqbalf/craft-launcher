//! Grammar check and AI help in the Craft apps (DG1): the launcher asks the server's Craft Office
//! app for a short-lived assist token with the account's app password, the same way it asks for a
//! live ticket, and hands the token, never the password, to the apps:
//!
//! - in every live session file, under `"assist"`, renewed with the ticket;
//! - in one account-wide file, `<data>/assist/assist.json` (mode 0600), kept fresh while the
//!   launcher runs and is signed in, for every other file the apps open: a checked-out cloud
//!   file, or a file on this computer (the owner's "always on when signed in"). It goes when the
//!   account is disconnected, or the server stops offering assist.
//!
//! Both read `{"v":1, "server": "...", "assist": {"token", "expiresAt", "proof", "ai"}}`. A server
//! without the token route (an older Craft Office) answers 404: no file, nothing else changes. The
//! token never reaches a log, an event or an error message.

use std::time::{Duration, Instant};

use craft_nextcloud::http::{Http, Method, Request};
use craft_suite_account::TokenStore;
use serde::{Deserialize, Serialize};

use super::CloudService;
use super::live::{TicketError, basic};

/// The assist token route of the server's Craft Office app, under the server's base URL.
pub const ASSIST_TOKEN_PATH: &str = "/ocs/v2.php/apps/craft_office/api/v1/assist/token";
/// The account-wide file's format.
pub const ASSIST_FILE_VERSION: u32 = 1;
/// Largest answer read: it is a few hundred bytes.
const MAX_ANSWER: usize = 16 * 1024;
/// Longest token accepted; the server checks it, this keeps a hostile answer small.
const MAX_TOKEN: usize = 4096;
/// After a refusal (no such route, assist not offered to this user), asked again this much later.
const REFUSED_RETRY: Duration = Duration::from_secs(10 * 60);
const FIRST_RETRY: Duration = Duration::from_secs(15);
const MAX_RETRY: Duration = Duration::from_secs(2 * 60);

/// An assist token, as the server issued it. No `Debug`: the token is a secret.
#[derive(Clone)]
pub(super) struct AssistToken {
    token: String,
    expires_in: u64,
    proof: bool,
    ai: bool,
    asked_at: jiff::Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    token: String,
    expires_in: u64,
    #[serde(default)]
    proof: bool,
    #[serde(default)]
    ai: bool,
}

#[derive(Deserialize)]
struct OcsAnswer {
    ocs: OcsBody,
}

#[derive(Deserialize)]
struct OcsBody {
    data: Answer,
}

/// Asks `server` for an assist token as `login` with the app password `password`.
pub(super) fn request_assist<H: Http>(
    http: &H,
    server: &str,
    login: &str,
    password: &str,
    asked_at: jiff::Timestamp,
) -> Result<AssistToken, TicketError> {
    let url = format!("{}{ASSIST_TOKEN_PATH}", server.trim_end_matches('/'));
    let req = Request::new(Method::Post, url)
        .header("authorization", basic(login, password))
        .header("ocs-apirequest", "true")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(b"{}".to_vec())
        .max_body(MAX_ANSWER);
    let resp = http.send(req).map_err(|e| TicketError::Transport(e.to_string()))?;
    if !(200..300).contains(&resp.status) {
        return Err(TicketError::Status(resp.status));
    }
    if resp.body.len() > MAX_ANSWER {
        return Err(TicketError::Malformed("too large"));
    }
    let data = serde_json::from_slice::<OcsAnswer>(&resp.body).map_err(|_| TicketError::Malformed("not an assist token"))?.ocs.data;
    if data.token.is_empty() || data.token.len() > MAX_TOKEN || !data.token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        return Err(TicketError::Malformed("no usable assist token"));
    }
    if data.expires_in == 0 {
        return Err(TicketError::Malformed("an expired assist token"));
    }
    Ok(AssistToken { token: data.token, expires_in: data.expires_in, proof: data.proof, ai: data.ai, asked_at })
}

/// The `"assist"` object the apps read. No `Debug`: it holds the token.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AssistRecord<'a> {
    token: &'a str,
    /// RFC 3339, UTC: when it was asked for plus its lifetime.
    expires_at: String,
    proof: bool,
    ai: bool,
}

impl AssistToken {
    pub(super) fn record(&self) -> AssistRecord<'_> {
        let lasts = i64::try_from(self.expires_in).unwrap_or(i64::MAX);
        let expires_at = self.asked_at.checked_add(jiff::SignedDuration::from_secs(lasts)).unwrap_or(self.asked_at);
        AssistRecord { token: &self.token, expires_at: expires_at.to_string(), proof: self.proof, ai: self.ai }
    }
}

#[derive(Serialize)]
struct AssistFile<'a> {
    v: u32,
    server: &'a str,
    assist: AssistRecord<'a>,
}

/// When the account-wide token is asked for again.
#[derive(Default)]
pub(super) struct AssistState {
    next_at: Option<Instant>,
    backoff: Duration,
}

impl<S: TokenStore + Send + Sync + 'static, H: Http + Clone + Send + 'static> CloudService<S, H> {
    /// A fresh assist token for the connected account, or `None` (signed out, an older server,
    /// assist not offered): the session file then simply has none.
    pub(super) fn assist_token(&self) -> Result<AssistToken, TicketError> {
        let Some((server, login, password)) = self.session_credentials() else { return Err(TicketError::Status(401)) };
        request_assist(&self.http(), &server, &login, password.expose(), (self.wall)())
    }

    /// Once a watcher pass: keeps `<data>/assist/assist.json` fresh while signed in (renewed at
    /// four fifths of its life, as the live ticket), and removes it once signed out or refused.
    pub(super) fn tick_assist(&self) {
        let now = self.now();
        let Some(account) = self.account() else {
            self.remove_assist_file();
            *self.assist.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = AssistState::default();
            return;
        };
        {
            let state = self.assist.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.next_at.is_some_and(|at| now < at) {
                return;
            }
        }
        let result = self.assist_token();
        let mut state = self.assist.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(t) => {
                let file = AssistFile { v: ASSIST_FILE_VERSION, server: &account.server, assist: t.record() };
                let written = serde_json::to_vec(&file).map_err(|e| e.to_string()).and_then(|bytes| {
                    super::live::private_dir(&self.paths.assist_root())?;
                    crate::paths::write_atomic(&self.paths.assist_file(), &bytes)
                });
                match written {
                    Ok(()) => {
                        state.next_at = Some(now + super::live::refresh_after(t.expires_in));
                        state.backoff = Duration::ZERO;
                    }
                    Err(e) => {
                        log::warn!("couldn't write the grammar and AI sign-in for the apps ({e})");
                        state.backoff = next_backoff(state.backoff);
                        state.next_at = Some(now + state.backoff);
                    }
                }
            }
            // No such route (an older Craft Office), assist not for this user, or the sign-in
            // refused: the apps say to sign in; asked again much later.
            Err(TicketError::Status(status @ (401 | 403 | 404))) => {
                log::info!("the server offers no grammar and AI sign-in (HTTP {status})");
                self.remove_assist_file();
                state.backoff = Duration::ZERO;
                state.next_at = Some(now + REFUSED_RETRY);
            }
            // A passing failure: the file still holds a token until it runs out.
            Err(e) => {
                log::info!("couldn't renew the grammar and AI sign-in ({e})");
                state.backoff = next_backoff(state.backoff);
                state.next_at = Some(now + state.backoff);
            }
        }
    }

    /// Removes the account-wide assist file (signed out): the apps then say to sign in.
    pub(super) fn remove_assist_file(&self) {
        if let Err(e) = std::fs::remove_file(self.paths.assist_file())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("couldn't remove the grammar and AI sign-in file ({e})");
        }
    }
}

fn next_backoff(b: Duration) -> Duration {
    if b.is_zero() { FIRST_RETRY } else { (b * 2).min(MAX_RETRY) }
}
