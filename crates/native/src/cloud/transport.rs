//! The ureq transport behind `craft-nextcloud`: sends each request as given, follows no redirect,
//! and hands back whatever the server answered, status included.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

use craft_nextcloud::http::{Http as DavHttp, Method, Request, Response};

/// Largest answer body read when a request names no limit of its own (`Request::max_body`), so a
/// server can't make the launcher buffer an endless one. `craft-nextcloud` sets a limit on every
/// request it sends; this only guards one that doesn't.
const DEFAULT_MAX_BODY: u64 = 64 * 1024 * 1024;

/// Largest cloud file opened. Files are held in memory while they move.
pub const MAX_FILE: usize = 256 * 1024 * 1024;

/// Why a request never produced an answer. Holds no header, so no credential can reach a log.
#[derive(Debug)]
pub struct TransportError(String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Cloud requests over the launcher's HTTPS settings (TLS, proxy, User-Agent).
#[derive(Clone)]
pub struct Transport {
    agent: ureq::Agent,
    transfer: ureq::Agent,
}

impl Transport {
    pub fn new(http: &crate::http::Http) -> Self {
        Self { agent: http.dav_agent(), transfer: http.dav_transfer_agent() }
    }
}

/// Whether `req` moves a file's contents (a download or upload under the WebDAV files root),
/// which gets the transfer agent and the file size limit rather than a listing's.
fn is_transfer(req: &Request) -> bool {
    matches!(req.method, Method::Get | Method::Put) && req.url.contains("/remote.php/dav/files/")
}

/// How many body bytes `req` may bring back: the limit the request carries, else the default for
/// its kind. Never more than [`MAX_FILE`].
fn read_limit(req: &Request) -> u64 {
    let fallback = if is_transfer(req) { MAX_FILE as u64 } else { DEFAULT_MAX_BODY };
    req.max_body.map_or(fallback, |m| m as u64).min(MAX_FILE as u64)
}

/// Reads `body` to its end, but stops one byte past `limit`: an answer over it is refused without
/// reading the rest.
fn read_capped(body: impl Read, limit: u64) -> Result<Vec<u8>, TransportError> {
    let mut out = Vec::new();
    body.take(limit.saturating_add(1)).read_to_end(&mut out).map_err(|e| TransportError(format!("unreadable answer ({e})")))?;
    if out.len() as u64 > limit {
        return Err(TransportError("the answer is too large".to_string()));
    }
    Ok(out)
}

impl DavHttp for Transport {
    type Error = TransportError;

    fn send(&self, req: Request) -> Result<Response, TransportError> {
        let agent = if is_transfer(&req) { &self.transfer } else { &self.agent };
        let limit = read_limit(&req);
        let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder.body(req.body).map_err(|e| TransportError(format!("bad request ({e})")))?;
        let mut resp = agent.run(request).map_err(|e| TransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers: BTreeMap<String, String> =
            resp.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned()))).collect();
        let body = read_capped(resp.body_mut().with_config().reader(), limit)?;
        Ok(Response { status, headers, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_larger_than_the_old_cap_is_read_when_the_request_allows_it() {
        let req = Request::new(Method::Propfind, "https://cloud.example.com/remote.php/dav/files/a/").max_body(64 << 20);
        assert!(read_limit(&req) > 33 << 20, "the request's own limit applies");
        let small = Request::new(Method::Get, "https://cloud.example.com/ocs/v1.php/cloud/capabilities").max_body(1 << 20);
        assert!(read_limit(&small) < read_limit(&req));
    }

    #[test]
    fn a_request_without_a_limit_still_has_one() {
        let req = Request::new(Method::Propfind, "https://cloud.example.com/remote.php/dav/files/a/");
        assert!(req.max_body.is_none());
        assert!(read_limit(&req) <= MAX_FILE as u64);
        let huge = Request::new(Method::Get, "https://cloud.example.com/remote.php/dav/files/a/x").max_body(usize::MAX);
        assert!(read_limit(&huge) <= MAX_FILE as u64, "never above the file limit");
    }

    #[test]
    fn reading_stops_just_past_the_limit() {
        // An endless body: reading it all would never finish.
        let endless = std::io::repeat(b'x');
        assert!(read_capped(endless, 1024).is_err());
        assert_eq!(read_capped(&b"short"[..], 1024).ok().as_deref(), Some(&b"short"[..]));
    }
}
