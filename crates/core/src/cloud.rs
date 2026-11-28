//! Cloud files: the pure types and rules behind the Cloud page (no I/O).

use serde::{Deserialize, Serialize};

/// The `path` of the `Event::CloudListed` that answers `CloudShared`. Real folder paths start
/// with `/`, so this can't be one.
pub const SHARED_PATH: &str = "@shared";
/// The `path` of the `Event::CloudListed` that answers `CloudSearch`.
pub const SEARCH_PATH: &str = "@search";

/// The message of a listing or status refused because the sign-in is no longer valid (HTTP 401):
/// the UI offers Reconnect beside it.
pub const SIGN_IN_EXPIRED: &str = "Your cloud sign-in is no longer valid. Reconnect to continue.";

/// Catalog ids of the office apps a cloud file can open in.
pub const CLOUD_APPS: [&str; 4] = ["wordcraft", "gridcraft", "deckcraft", "pdfcraft"];

/// Catalog ids of the apps that can join a live editing session (`--live-session`): their cloud
/// files open live when the server offers it, and through the check-out otherwise.
pub const LIVE_APPS: [&str; 3] = ["wordcraft", "gridcraft", "deckcraft"];

/// What the launcher tells the user when a live-capable file opens through the check-out instead,
/// because the server offers no live session for it (no Craft Office app, no access, or its relay
/// is out of reach).
pub const LIVE_UNAVAILABLE: &str = "Live editing isn't available on this server; changes upload when you save.";

/// Extensions (lowercase) per app, in the order of [`CLOUD_APPS`].
const EXTENSIONS: [&[&str]; 4] =
    [&["docx", "doc", "dotx", "odt", "rtf", "txt", "md"], &["xlsx", "xls", "ods", "csv"], &["pptx", "ppt", "odp"], &["pdf"]];

/// The catalog app that opens `file_name`, chosen by its extension (any letter case).
pub fn app_for(file_name: &str) -> Option<&'static str> {
    let (_, ext) = file_name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    CLOUD_APPS.iter().zip(EXTENSIONS).find(|(_, exts)| exts.contains(&ext.as_str())).map(|(app, _)| *app)
}

/// Turns what the user typed or pasted into the server's base URL, with no trailing slash.
///
/// A missing scheme becomes `https://`. The sub-path of a server in a folder is kept, but a
/// pasted web-interface address (`/index.php/...`, `/apps/...`), query and fragment are dropped.
/// Plain `http://` is refused except for loopback (`127.0.0.1`, `::1`, `localhost`).
pub fn normalize_server(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter the address of your server".to_string());
    }
    let (scheme, rest) = if let Some(r) = strip_prefix_ci(input, "https://") {
        ("https", r)
    } else if let Some(r) = strip_prefix_ci(input, "http://") {
        ("http", r)
    } else if input.contains("://") {
        return Err("Use an https:// address".to_string());
    } else {
        ("https", input)
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains(['@', ' ']) {
        return Err("That doesn't look like a server address".to_string());
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    if host.is_empty() {
        return Err("That doesn't look like a server address".to_string());
    }
    if scheme == "http" && !matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "::1" | "localhost") {
        return Err("Use an https:// address; plain http is only allowed for this computer".to_string());
    }
    let mut kept = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if seg == "index.php" || seg == "apps" || seg == "remote.php" || seg == "login" {
            break;
        }
        kept.push('/');
        kept.push_str(seg);
    }
    Ok(format!("{scheme}://{authority}{kept}"))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// One file or folder in the user's cloud.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
    pub etag: String,
    pub shared: bool,
    /// Who has the file locked: `None` when it isn't. An empty name is a lock whose holder the
    /// server doesn't give, and the UI words it as "Locked" or "Someone".
    pub locked_by: Option<String>,
    /// The lock is held by an editing app on a person's behalf, not by a person.
    #[serde(default)]
    pub lock_by_app: bool,
    /// The user may change it. A read-only share still lists, but opens read-only.
    #[serde(default = "yes")]
    pub writable: bool,
    /// The server's file id, which a live session is asked for by. `None` when the listing
    /// didn't give one.
    #[serde(default)]
    pub file_id: Option<u64>,
}

fn yes() -> bool {
    true
}

/// Where an open cloud file stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncStatus {
    Opening,
    Saved,
    Uploading,
    WaitingToUpload,
    ReconnectNeeded,
    Conflict,
    ReadOnly,
    CloseInAppFirst,
    /// Open in its app for live editing: the server saves every change, so the launcher holds no
    /// lock and uploads nothing. Ends when the app quits.
    Live,
}

/// How the user settles a file changed both here and in the cloud.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictChoice {
    KeepBoth,
    UseMine,
    UseTheirs,
    TheirsClosed,
}

/// The connected cloud account. Never holds a password.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudAccount {
    pub id: String,
    pub server: String,
    pub login: String,
    pub display: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_subpath() {
        assert_eq!(normalize_server("host/nextcloud/index.php/apps/files/?dir=/"), Ok("https://host/nextcloud".to_string()));
    }

    #[test]
    fn normalize_adds_https_and_trims_slash() {
        assert_eq!(normalize_server("cloud.example.com/"), Ok("https://cloud.example.com".to_string()));
    }

    #[test]
    fn normalize_refuses_plain_http_except_loopback() {
        assert!(normalize_server("http://cloud.example.com").is_err());
        assert_eq!(normalize_server("http://127.0.0.1:8080"), Ok("http://127.0.0.1:8080".to_string()));
        assert!(normalize_server("http://localhost").is_ok());
        assert!(normalize_server("http://[::1]:81").is_ok());
        assert!(normalize_server("http://127.0.0.1.evil.com").is_err());
        assert!(normalize_server("   ").is_err());
    }

    #[test]
    fn app_for_is_case_insensitive() {
        assert_eq!(app_for("Report.DOCX"), Some("wordcraft"));
        assert_eq!(app_for("a.pdf"), Some("pdfcraft"));
        assert_eq!(app_for("x.ods"), Some("gridcraft"));
        assert_eq!(app_for("y.odp"), Some("deckcraft"));
        assert_eq!(app_for("photo.jpg"), None);
        assert_eq!(app_for("noext"), None);
    }
}
