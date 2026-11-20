//! The ureq transport behind `craft-nextcloud`: sends each request as given, follows no redirect,
//! and hands back whatever the server answered, status included.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

use craft_nextcloud::http::{Http as DavHttp, Request, Response};

/// Largest answer body read: a little over what `craft-nextcloud` will parse (32 MiB), so a
/// server can't make the launcher buffer an endless one.
const MAX_BODY: u64 = 33 * 1024 * 1024;

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
}

impl Transport {
    pub fn new(http: &crate::http::Http) -> Self {
        Self { agent: http.dav_agent() }
    }
}

impl DavHttp for Transport {
    type Error = TransportError;

    fn send(&self, req: Request) -> Result<Response, TransportError> {
        let mut builder = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder.body(req.body).map_err(|e| TransportError(format!("bad request ({e})")))?;
        let mut resp = self.agent.run(request).map_err(|e| TransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers: BTreeMap<String, String> =
            resp.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned()))).collect();
        let mut body = Vec::new();
        resp.body_mut()
            .with_config()
            .reader()
            .take(MAX_BODY.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|e| TransportError(format!("unreadable answer ({e})")))?;
        if body.len() as u64 > MAX_BODY {
            return Err(TransportError("the answer is too large".to_string()));
        }
        Ok(Response { status, headers, body })
    }
}
