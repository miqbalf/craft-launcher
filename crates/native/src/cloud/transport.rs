//! The ureq transport behind `craft-nextcloud`: sends each request as given, follows no redirect,
//! and hands back whatever the server answered, status included.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

use craft_nextcloud::http::{Http as DavHttp, Method, Request, Response};

/// Largest answer body read: a little over what `craft-nextcloud` will parse (32 MiB), so a
/// server can't make the launcher buffer an endless one.
const MAX_BODY: u64 = 33 * 1024 * 1024;

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

impl DavHttp for Transport {
    type Error = TransportError;

    fn send(&self, req: Request) -> Result<Response, TransportError> {
        let (agent, max_body) = if is_transfer(&req) { (&self.transfer, MAX_FILE as u64) } else { (&self.agent, MAX_BODY) };
        let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder.body(req.body).map_err(|e| TransportError(format!("bad request ({e})")))?;
        let mut resp = agent.run(request).map_err(|e| TransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers: BTreeMap<String, String> =
            resp.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned()))).collect();
        let mut body = Vec::new();
        resp.body_mut()
            .with_config()
            .reader()
            .take(max_body.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|e| TransportError(format!("unreadable answer ({e})")))?;
        if body.len() as u64 > max_body {
            return Err(TransportError("the answer is too large".to_string()));
        }
        Ok(Response { status, headers, body })
    }
}
