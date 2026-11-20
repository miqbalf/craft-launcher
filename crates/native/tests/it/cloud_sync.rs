//! Opening cloud files, saving them back and settling conflicts, against an in-memory WebDAV
//! server, an injected app launcher and an injected clock (no sleeps).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{ConflictChoice, SyncStatus};
use artcraft_launcher_core::protocol::Event;
use artcraft_launcher_native::cloud::CloudService;
use artcraft_launcher_native::paths::Paths;
use craft_nextcloud::http::{Http, Method, Request, Response};
use craft_suite_account::{Memory, Secret, TokenStore, accounts};

const SERVER: &str = "http://127.0.0.1:8080";
const PASSWORD: &str = "app-pass-w0rd-NEVER-ON-DISK";
const FILE: &str = "/Docs/report.docx";
const FILES_ROOT: &str = "http://127.0.0.1:8080/remote.php/dav/files/alice";

/// One request as the fake saw it.
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    if_match: Option<String>,
    lock_if: Option<String>,
}

#[derive(Default)]
struct Server {
    files: HashMap<String, (Vec<u8>, String)>,
    locks: HashMap<String, String>,
    /// What a listing says about a lock: `nc:lock-owner-displayname` and `nc:lock-owner-type`.
    lock_owners: HashMap<String, (String, u8)>,
    next: u64,
    down: bool,
    unauthorized: bool,
    seen: Vec<Seen>,
    /// Who the next sign-in approves as (`alice` when empty).
    login: String,
    /// Runs while an UNLOCK is in flight.
    on_unlock: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Requests of a method answered with a status instead (e.g. every PUT with 403).
    refuse: Vec<(Method, u16)>,
    /// Files listed as not writable for this user (`oc:permissions` without `W`).
    read_only: Vec<String>,
    /// The lock timeout the server grants (`Timeout: Second-N`); none is sent when unset.
    grant: Option<u32>,
}

impl Server {
    fn tag(&mut self) -> String {
        self.next += 1;
        format!("etag-{}", self.next)
    }
}

/// An in-memory WebDAV server: files with ETags, exclusive locks, If-Match and If-None-Match.
#[derive(Clone, Default)]
struct Dav(Arc<Mutex<Server>>);

impl Dav {
    fn with_file(path: &str, bytes: &[u8]) -> Dav {
        let dav = Dav::default();
        dav.change(path, bytes);
        dav
    }

    /// Someone else saves `bytes` at `path`: a new ETag.
    fn change(&self, path: &str, bytes: &[u8]) {
        let mut s = self.0.lock().unwrap();
        let tag = s.tag();
        s.files.insert(path.to_string(), (bytes.to_vec(), tag));
    }

    fn lock_by_other(&self, path: &str) {
        self.0.lock().unwrap().locks.insert(path.to_string(), "opaquelocktoken:somebody-else".to_string());
    }

    fn remote(&self, path: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().files.get(path).map(|(b, _)| b.clone())
    }

    fn etag(&self, path: &str) -> String {
        self.0.lock().unwrap().files.get(path).map(|(_, e)| e.clone()).unwrap()
    }

    /// The server lists `path` as locked by the person (`kind` 0) or app (`kind` 1) `name`.
    fn list_lock_owner(&self, path: &str, name: &str, kind: u8) {
        self.0.lock().unwrap().lock_owners.insert(path.to_string(), (name.to_string(), kind));
    }

    fn held_lock(&self, path: &str) -> Option<String> {
        self.0.lock().unwrap().locks.get(path).cloned()
    }

    fn seen(&self) -> Vec<Seen> {
        self.0.lock().unwrap().seen.clone()
    }

    fn puts(&self) -> Vec<Seen> {
        self.seen().into_iter().filter(|s| s.method == Method::Put).collect()
    }

    fn set_down(&self, down: bool) {
        self.0.lock().unwrap().down = down;
    }

    fn set_unauthorized(&self, yes: bool) {
        self.0.lock().unwrap().unauthorized = yes;
    }

    fn sign_in_as(&self, login: &str) {
        self.0.lock().unwrap().login = login.to_string();
    }

    fn during_unlock(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.0.lock().unwrap().on_unlock = Some(Arc::new(hook));
    }

    fn clear_seen(&self) {
        self.0.lock().unwrap().seen.clear();
    }

    /// Answers every `method` request with `status` from now on.
    fn refuse(&self, method: Method, status: u16) {
        self.0.lock().unwrap().refuse.push((method, status));
    }

    fn list_read_only(&self, path: &str) {
        self.0.lock().unwrap().read_only.push(path.to_string());
    }

    fn grant_locks_for(&self, secs: u32) {
        self.0.lock().unwrap().grant = Some(secs);
    }
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(Ok(v)) = s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16))
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn unquote(tag: &str) -> String {
    tag.trim().trim_start_matches("W/").trim_matches('"').to_string()
}

impl Http for Dav {
    type Error = String;

    fn send(&self, req: Request) -> Result<Response, String> {
        let mut s = self.0.lock().unwrap();
        if s.down {
            return Err("connection refused".to_string());
        }
        if s.unauthorized {
            return Ok(Response::new(401, Vec::new()));
        }
        if req.url.ends_with("/status.php") {
            return Ok(Response::new(200, br#"{"installed":true,"version":"30.0.1","productname":"Nextcloud"}"#.to_vec()));
        }
        if req.url.ends_with("/index.php/login/v2") {
            let body = format!(r#"{{"login":"{SERVER}/login/v2/flow/x","poll":{{"token":"t","endpoint":"{SERVER}/login/v2/poll"}}}}"#);
            return Ok(Response::new(200, body.into_bytes()));
        }
        if req.url.ends_with("/login/v2/poll") {
            let login = if s.login.is_empty() { "alice".to_string() } else { s.login.clone() };
            let body = format!(r#"{{"server":"{SERVER}","loginName":"{login}","appPassword":"another-app-password"}}"#);
            return Ok(Response::new(200, body.into_bytes()));
        }
        if req.url.contains("/ocs/v1.php/cloud/capabilities") {
            return Ok(Response::new(200, br#"{"ocs":{"data":{"capabilities":{"files":{"locking":"1.0"}}}}}"#.to_vec()));
        }
        let Some(rest) = req.url.strip_prefix(FILES_ROOT) else { return Ok(Response::new(404, Vec::new())) };
        let path = decode(rest);
        let lock_if = req.get_header("if").map(str::to_string);
        let if_match = req.get_header("if-match").map(unquote);
        s.seen.push(Seen { method: req.method, path: path.clone(), if_match: if_match.clone(), lock_if: lock_if.clone() });
        if let Some((_, status)) = s.refuse.iter().find(|(m, _)| *m == req.method) {
            return Ok(Response::new(*status, Vec::new()));
        }
        let held = s.locks.get(&path).cloned();
        let sends_held = |held: &str| lock_if.as_deref().is_some_and(|h| h.contains(held));
        let grant = s.grant;
        let granted = |resp: Response| match grant {
            Some(secs) => resp.header("timeout", format!("Second-{secs}")),
            None => resp,
        };
        match req.method {
            Method::Get => match s.files.get(&path) {
                Some((bytes, tag)) => Ok(Response::new(200, bytes.clone()).header("etag", format!("\"{tag}\""))),
                None => Ok(Response::new(404, Vec::new())),
            },
            Method::Put => {
                if let Some(held) = &held
                    && !sends_held(held)
                {
                    return Ok(Response::new(423, Vec::new()));
                }
                let current = s.files.get(&path).map(|(_, t)| t.clone());
                if req.get_header("if-none-match") == Some("*") && current.is_some() {
                    return Ok(Response::new(412, Vec::new()));
                }
                if let Some(want) = &if_match
                    && current.as_ref() != Some(want)
                {
                    return Ok(Response::new(412, Vec::new()));
                }
                let tag = s.tag();
                s.files.insert(path, (req.body.clone(), tag.clone()));
                Ok(Response::new(201, Vec::new()).header("etag", format!("\"{tag}\"")))
            }
            Method::Lock => {
                if lock_if.is_some() {
                    return Ok(match &held {
                        Some(h) if sends_held(h) => granted(Response::new(200, Vec::new()).header("lock-token", format!("<{h}>"))),
                        _ => Response::new(412, Vec::new()),
                    });
                }
                if held.is_some() {
                    return Ok(Response::new(423, Vec::new()));
                }
                let resp = granted(Response::new(200, Vec::new()));
                let token = format!("opaquelocktoken:{}", s.tag());
                s.locks.insert(path, token.clone());
                Ok(resp.header("lock-token", format!("<{token}>")))
            }
            Method::Unlock => {
                if let Some(hook) = s.on_unlock.clone() {
                    hook();
                }
                let given = req.get_header("lock-token").map(|t| t.trim_matches(['<', '>']).to_string());
                if held.is_some() && held == given {
                    s.locks.remove(&path);
                    Ok(Response::new(204, Vec::new()))
                } else {
                    Ok(Response::new(409, Vec::new()))
                }
            }
            Method::Propfind => {
                let parent = path.trim_end_matches('/');
                let mut xml = String::from(
                    r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">"#,
                );
                xml.push_str(&format!("<d:response><d:href>/remote.php/dav/files/alice{path}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                for file in s.files.keys().filter(|f| f.rsplit_once('/').is_some_and(|(dir, _)| dir == parent)) {
                    let lock = match (s.locks.contains_key(file), s.lock_owners.get(file)) {
                        (true, Some((name, kind))) => format!(
                            "<nc:lock>1</nc:lock><nc:lock-owner-displayname>{name}</nc:lock-owner-displayname><nc:lock-owner-type>{kind}</nc:lock-owner-type>"
                        ),
                        (true, None) => "<nc:lock>1</nc:lock>".to_string(),
                        _ => String::new(),
                    };
                    let lock = if s.read_only.contains(file) { format!("{lock}<oc:permissions>RG</oc:permissions>") } else { lock };
                    xml.push_str(&format!("<d:response><d:href>/remote.php/dav/files/alice{file}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>9</d:getcontentlength>{lock}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                }
                xml.push_str("</d:multistatus>");
                Ok(Response::new(207, xml.into_bytes()))
            }
            _ => Ok(Response::new(405, Vec::new())),
        }
    }
}

type Launched = Arc<Mutex<Vec<(String, PathBuf)>>>;
/// What the injected app launcher does before it records a launch (`None`: it succeeds).
type LaunchHook = Arc<Mutex<Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>>>;
/// What runs just before a file action waits for the open files.
type PauseHook = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// The hooks a test can set on the service after building it.
#[derive(Clone, Default)]
struct Hooks {
    launch: LaunchHook,
    pause: PauseHook,
    revealed: Arc<Mutex<Vec<PathBuf>>>,
}

struct Rig {
    root: tempfile::TempDir,
    paths: Paths,
    keychain: Memory,
    dav: Dav,
    events: Arc<Mutex<Vec<Event>>>,
    launched: Launched,
    clock: Arc<Mutex<Instant>>,
    hooks: Hooks,
    service: CloudService<Memory, Dav>,
}

/// The wall clock every rig's conflict and backup names are dated with: fixed, so no test sees
/// the minute roll over.
fn wall() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_800_000_000).unwrap()
}

fn build(
    paths: &Paths,
    keychain: &Memory,
    dav: &Dav,
    events: &Arc<Mutex<Vec<Event>>>,
    launched: &Launched,
    clock: &Arc<Mutex<Instant>>,
    hooks: &Hooks,
) -> CloudService<Memory, Dav> {
    let (sink, seen, now) = (events.clone(), launched.clone(), clock.clone());
    let (launch, pause, revealed) = (hooks.launch.clone(), hooks.pause.clone(), hooks.revealed.clone());
    CloudService::new(paths, keychain.clone(), dav.clone(), move |e| sink.lock().unwrap().push(e))
        .with_launcher(move |app: &str, file: &Path| {
            let hook = launch.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook()?;
            }
            seen.lock().unwrap().push((app.to_string(), file.to_path_buf()));
            Ok(())
        })
        .with_pause_before_book(move || {
            let hook = pause.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
        })
        .with_revealer(move |dir: &Path| {
            revealed.lock().unwrap().push(dir.to_path_buf());
            Ok(())
        })
        .with_clock(move || *now.lock().unwrap())
        .with_wall_clock(wall)
        .with_opener(|_| Ok(()))
        .with_timing(Duration::ZERO, Duration::from_secs(5))
}

/// A connected account (record on disk, app password in the in-memory keychain) on `dav`.
fn rig(dav: Dav) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::under(root.path());
    let keychain = Memory::new();
    keychain.put(&format!("artcraft-launcher-nextcloud:{}", accounts::handle(SERVER, "alice")), &Secret::new(PASSWORD)).unwrap();
    std::fs::create_dir_all(paths.cloud_root()).unwrap();
    std::fs::write(paths.cloud_root().join("account.json"), format!(r#"{{"server":"{SERVER}","login":"alice","display":"alice"}}"#)).unwrap();
    let events = Arc::default();
    let launched = Arc::default();
    let clock = Arc::new(Mutex::new(Instant::now()));
    let hooks = Hooks::default();
    let service = build(&paths, &keychain, &dav, &events, &launched, &clock, &hooks);
    Rig { root, paths, keychain, dav, events, launched, clock, hooks, service }
}

impl Rig {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn statuses(&self, path: &str) -> Vec<SyncStatus> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::CloudFileStatus { path: p, status, .. } if p == path => Some(status),
                _ => None,
            })
            .collect()
    }

    fn status(&self, path: &str) -> Option<SyncStatus> {
        self.statuses(path).last().copied()
    }

    fn advance(&self, by: Duration) {
        let mut now = self.clock.lock().unwrap();
        *now += by;
    }

    /// A few seconds of the once-a-second sync loop.
    fn settle(&self) {
        for _ in 0..4 {
            self.service.tick();
            self.advance(Duration::from_secs(1));
        }
        self.service.tick();
    }

    /// Opens `path` for editing and returns the working copy the app was given.
    fn open(&self, path: &str) -> PathBuf {
        self.service.open(path, None, false);
        self.launched.lock().unwrap().last().map(|(_, p)| p.clone()).unwrap_or_else(|| panic!("nothing launched: {:?}", self.events()))
    }

    /// Writes `bytes` the way apps save: a sibling temporary file renamed over the target.
    fn save(&self, local: &Path, bytes: &[u8]) {
        let tmp = local.with_file_name(".~report.docx.tmp");
        std::fs::write(&tmp, bytes).unwrap();
        std::fs::rename(&tmp, local).unwrap();
    }

    fn rebuild(&mut self) {
        self.service = build(&self.paths, &self.keychain, &self.dav, &self.events, &self.launched, &self.clock, &self.hooks);
    }
}

#[test]
fn open_locks_downloads_and_launches() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let local = rig.open(FILE);
    assert!(rig.dav.held_lock(FILE).is_some(), "the file should be locked");
    assert_eq!(std::fs::read(&local).unwrap(), b"remote v1");
    assert!(local.starts_with(rig.paths.cloud_root()), "{}", local.display());
    assert_eq!(local.file_name().unwrap(), "report.docx", "the real file name is kept");
    assert_eq!(rig.launched.lock().unwrap()[0].0, "wordcraft");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));

    let mut files = Vec::new();
    files_under(rig.root.path(), &mut files);
    for file in files {
        let bytes = std::fs::read(&file).unwrap();
        assert!(!bytes.windows(PASSWORD.len()).any(|w| w == PASSWORD.as_bytes()), "password in {}", file.display());
    }
    assert!(!format!("{:?}", rig.events()).contains(PASSWORD));
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { files_under(&path, out) } else { out.push(path) }
    }
}

#[test]
fn opening_again_keeps_unsynced_edits() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_down(true);
    rig.save(&local, b"my unsynced edit");
    dav.set_down(false);
    dav.clear_seen();
    let again = rig.open(FILE);
    assert_eq!(again, local);
    assert_eq!(std::fs::read(&local).unwrap(), b"my unsynced edit");
    assert!(!dav.seen().iter().any(|s| s.method == Method::Get), "{:?}", dav.seen());
}

/// The last `CloudLockedBy` the service sent for `FILE`.
fn locked_event(rig: &Rig) -> Option<(String, bool)> {
    rig.events().into_iter().rev().find_map(|e| match e {
        Event::CloudLockedBy { path, by, app } if path == FILE => Some((by, app)),
        _ => None,
    })
}

#[test]
fn open_locked_by_other_names_the_holder_from_the_last_listing() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.lock_by_other(FILE);
    dav.list_lock_owner(FILE, "Bea Ortiz", 0);
    let rig = rig(dav);
    rig.service.list("/Docs");
    rig.service.open(FILE, None, false);
    assert_eq!(locked_event(&rig), Some(("Bea Ortiz".to_string(), false)));
}

#[test]
fn open_locked_by_an_app_says_so() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.lock_by_other(FILE);
    dav.list_lock_owner(FILE, "Bea Ortiz", 1);
    let rig = rig(dav);
    rig.service.list("/Docs");
    rig.service.open(FILE, None, false);
    assert_eq!(locked_event(&rig).map(|(_, app)| app), Some(true));
}

#[test]
fn open_locked_by_other_in_another_folder_leaves_the_name_empty() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.change("/Other/x.docx", b"x");
    dav.lock_by_other(FILE);
    dav.list_lock_owner(FILE, "Bea Ortiz", 0);
    let rig = rig(dav);
    rig.service.list("/Other");
    rig.service.open(FILE, None, false);
    assert_eq!(locked_event(&rig), Some((String::new(), false)));
}

#[test]
fn open_locked_by_other_offers_read_only() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.lock_by_other(FILE);
    let rig = rig(dav.clone());
    rig.service.open(FILE, None, false);
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudLockedBy { path, .. } if path == FILE)), "{:?}", rig.events());
    assert!(rig.launched.lock().unwrap().is_empty(), "nothing opens until the user picks read-only");
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudLockedBy { by, app: false, .. } if by.is_empty())), "no listing cached, so no name");

    rig.service.open(FILE, None, true);
    let local = rig.launched.lock().unwrap()[0].1.clone();
    assert_eq!(std::fs::read(&local).unwrap(), b"remote v1");
    assert_eq!(rig.status(FILE), Some(SyncStatus::ReadOnly));
    rig.save(&local, b"local scribbles");
    rig.settle();
    assert!(dav.puts().is_empty(), "{:?}", dav.puts());
    assert_eq!(dav.remote(FILE).unwrap(), b"remote v1");
}

#[test]
fn atomic_save_uploads_once() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let old = dav.etag(FILE);
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    let tmp = local.with_file_name(".~report.docx.tmp");
    std::fs::write(&tmp, b"my new version").unwrap();
    rig.settle();
    std::fs::rename(&tmp, &local).unwrap();
    rig.settle();
    rig.settle();

    let puts = dav.puts();
    assert_eq!(puts.len(), 1, "{puts:?}");
    assert_eq!(puts[0].path, FILE);
    assert_eq!(puts[0].if_match.as_deref(), Some(old.as_str()));
    assert!(puts[0].lock_if.as_deref().is_some_and(|h| h.contains(&dav.held_lock(FILE).unwrap())), "the lock token is sent");
    assert_eq!(dav.remote(FILE).unwrap(), b"my new version");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));
    assert!(rig.statuses(FILE).contains(&SyncStatus::Uploading));
}

#[test]
fn remote_change_gives_conflict_and_no_overwrite() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.change(FILE, b"their version");
    rig.save(&local, b"my version");
    rig.settle();
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::Conflict));
    assert_eq!(dav.remote(FILE).unwrap(), b"their version");
    assert_eq!(std::fs::read(&local).unwrap(), b"my version");

    // Once in conflict, further saves are not uploaded.
    dav.clear_seen();
    rig.save(&local, b"my version, edited more");
    rig.settle();
    assert!(dav.puts().is_empty(), "{:?}", dav.puts());
}

fn conflicted(rig: &Rig) -> PathBuf {
    let local = rig.open(FILE);
    rig.dav.change(FILE, b"their version");
    rig.save(&local, b"my version");
    rig.settle();
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::Conflict));
    rig.dav.clear_seen();
    local
}

#[test]
fn keep_both_retargets_to_conflict_copy() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let local = conflicted(&rig);
    rig.service.resolve(FILE, ConflictChoice::KeepBoth);
    let copy = rig.dav.puts().first().map(|p| p.path.clone()).expect("my version is uploaded");
    assert!(copy.starts_with("/Docs/report (conflict ") && copy.ends_with(").docx") && copy.contains("alice"), "{copy}");
    assert_eq!(rig.dav.remote(&copy).unwrap(), b"my version");
    assert_eq!(rig.dav.remote(FILE).unwrap(), b"their version");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));

    rig.dav.clear_seen();
    rig.save(&local, b"my version, edited more");
    rig.settle();
    let puts = rig.dav.puts();
    assert!(!puts.is_empty());
    assert!(puts.iter().all(|p| p.path == copy), "{puts:?}");
    assert_eq!(rig.dav.remote(&copy).unwrap(), b"my version, edited more");
    assert_eq!(rig.dav.remote(FILE).unwrap(), b"their version");
}

#[test]
fn keep_both_picks_a_free_name() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let _local = conflicted(&rig);
    // The name the launcher picks first is taken (the rig's wall clock is fixed, so the minute in
    // it can't roll over between here and the resolve).
    let stamp = wall().to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H%M").to_string();
    let taken = format!("/Docs/report (conflict {stamp} alice).docx");
    rig.dav.change(&taken, b"earlier copy");
    rig.service.resolve(FILE, ConflictChoice::KeepBoth);
    let created: Vec<_> = rig.dav.puts().into_iter().map(|p| p.path).collect();
    let last = created.last().unwrap();
    assert_ne!(last, &taken);
    assert_eq!(rig.dav.remote(&taken).unwrap(), b"earlier copy", "the earlier copy is untouched");
    assert_eq!(rig.dav.remote(last).unwrap(), b"my version");
    assert!(last.ends_with(") 2.docx") && last.contains(&stamp), "the next numbered name of the same minute: {last}");
}

#[test]
fn use_mine_overwrites_without_if_match() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let _local = conflicted(&rig);
    rig.service.resolve(FILE, ConflictChoice::UseMine);
    let puts = rig.dav.puts();
    assert_eq!(puts.len(), 1, "{puts:?}");
    assert_eq!((puts[0].path.as_str(), puts[0].if_match.as_deref()), (FILE, None));
    assert_eq!(rig.dav.remote(FILE).unwrap(), b"my version");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));
}

#[test]
fn use_theirs_requires_close_first() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let local = conflicted(&rig);
    rig.service.resolve(FILE, ConflictChoice::UseTheirs);
    assert_eq!(rig.status(FILE), Some(SyncStatus::CloseInAppFirst));
    rig.save(&local, b"my version, saved again before closing");
    rig.settle();
    assert!(rig.dav.puts().is_empty(), "{:?}", rig.dav.puts());
    assert_eq!(std::fs::read(&local).unwrap(), b"my version, saved again before closing");
    let launches = rig.launched.lock().unwrap().len();

    rig.service.resolve(FILE, ConflictChoice::TheirsClosed);
    assert_eq!(std::fs::read(&local).unwrap(), b"their version");
    let backups = kept_with(&rig, b"my version, saved again before closing");
    assert_eq!(backups.len(), 1, "my version is kept as a backup");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));
    assert!(rig.launched.lock().unwrap().len() > launches, "theirs is opened again in the app");
    assert!(rig.dav.puts().is_empty());
}

#[test]
fn network_down_waits_and_keeps_file() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_down(true);
    rig.save(&local, b"offline edit");
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::WaitingToUpload));
    assert_eq!(std::fs::read(&local).unwrap(), b"offline edit");
    assert_eq!(dav.remote(FILE).unwrap(), b"remote v1");

    dav.set_down(false);
    rig.advance(Duration::from_secs(10 * 60));
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"offline edit");
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));
    assert_eq!(std::fs::read(&local).unwrap(), b"offline edit");
}

#[test]
fn unauthorized_keeps_checkout() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_unauthorized(true);
    rig.save(&local, b"edit while signed out");
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::ReconnectNeeded));
    assert_eq!(std::fs::read(&local).unwrap(), b"edit while signed out");
    let record = std::fs::read_to_string(rig.paths.cloud_root().join("checkouts.json")).unwrap();
    assert!(record.contains("report.docx"), "{record}");
    // Opening again after the rejection still finds the same working copy.
    dav.set_unauthorized(false);
    assert_eq!(rig.open(FILE), local);
    assert_eq!(std::fs::read(&local).unwrap(), b"edit while signed out");
}

#[test]
fn resume_after_restart_uploads_offline_edit() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let old = dav.etag(FILE);
    let mut rig = rig(dav.clone());
    let local = rig.open(FILE);
    rig.save(&local, b"edited while the launcher was closed");
    rig.rebuild();
    rig.service.resume();
    rig.settle();
    let puts = dav.puts();
    assert_eq!(puts.len(), 1, "{puts:?}");
    assert_eq!(puts[0].if_match.as_deref(), Some(old.as_str()));
    assert_eq!(dav.remote(FILE).unwrap(), b"edited while the launcher was closed");
}

#[test]
fn done_uploads_unlocks_and_cleans() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    rig.save(&local, b"final words");
    // No settling: Done does the last upload itself.
    rig.service.done(FILE);
    assert_eq!(dav.remote(FILE).unwrap(), b"final words");
    assert!(dav.held_lock(FILE).is_none(), "unlocked");
    assert!(dav.seen().iter().any(|s| s.method == Method::Unlock));
    assert!(!local.exists(), "the synced copy is removed");
    let record = std::fs::read_to_string(rig.paths.cloud_root().join("checkouts.json")).unwrap();
    assert!(!record.contains("report.docx"), "{record}");
}

#[test]
fn done_keeps_the_copy_when_the_upload_fails() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    rig.save(&local, b"final words");
    dav.set_down(true);
    rig.service.done(FILE);
    assert_eq!(std::fs::read(&local).unwrap(), b"final words");
    assert_eq!(rig.status(FILE), Some(SyncStatus::WaitingToUpload));
    dav.set_down(false);
    assert!(dav.held_lock(FILE).is_some(), "the lock is kept until the copy is up");
}

#[test]
fn disconnect_with_open_file_is_refused_and_keeps_copy() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_down(true);
    rig.save(&local, b"not uploaded yet");
    rig.service.disconnect();
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudError(m) if m.contains("Finish or close"))), "{:?}", rig.events());
    assert_eq!(std::fs::read(&local).unwrap(), b"not uploaded yet");
    assert!(rig.service.account().is_some());
}

#[test]
fn recent_lists_opened_files_newest_first() {
    let dav = Dav::with_file(FILE, b"one");
    dav.change("/b.pdf", b"two");
    let rig = rig(dav);
    rig.open(FILE);
    rig.open("/b.pdf");
    rig.open(FILE);
    rig.events.lock().unwrap().clear();
    rig.service.recent();
    match rig.events().as_slice() {
        [Event::CloudRecentListed(list)] => {
            let paths: Vec<_> = list.iter().map(|e| e.path.as_str()).collect();
            assert_eq!(paths, [FILE, "/b.pdf"]);
        }
        other => panic!("{other:?}"),
    }
}

/// Files under the kept folder (outside every account's cache) holding exactly `bytes`.
fn kept_with(rig: &Rig, bytes: &[u8]) -> Vec<PathBuf> {
    let kept = rig.paths.cloud_root().join("kept");
    let mut files = Vec::new();
    if kept.exists() {
        files_under(&kept, &mut files);
    }
    files.into_iter().filter(|p| std::fs::read(p).unwrap() == bytes).collect()
}

fn disconnected(rig: &Rig) -> bool {
    rig.events().iter().any(|e| matches!(e, Event::CloudDisconnected))
}

#[test]
fn disconnect_keeps_my_version_after_use_theirs() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let _local = conflicted(&rig);
    rig.service.resolve(FILE, ConflictChoice::UseTheirs);
    rig.service.resolve(FILE, ConflictChoice::TheirsClosed);
    let told = rig.events().iter().rev().find_map(|e| match e {
        Event::CloudFileStatus { detail: Some(d), .. } if d.contains("kept at") => Some(d.clone()),
        _ => None,
    });
    rig.service.done(FILE);
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    let kept = kept_with(&rig, b"my version");
    assert_eq!(kept.len(), 1, "my version survives disconnecting");
    assert!(told.is_some_and(|d| d.contains(&kept[0].display().to_string())), "the user is told where it is");
}

#[test]
fn disconnect_keeps_an_earlier_copy_found_in_the_cache() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let stray = rig.paths.cloud_root().join(accounts::handle(SERVER, "alice")).join("Docs/report.docx");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"left from a crash").unwrap();
    rig.open(FILE);
    rig.service.done(FILE);
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    assert_eq!(kept_with(&rig, b"left from a crash").len(), 1);
}

#[test]
fn a_name_differing_only_in_case_does_not_take_over_an_open_copy() {
    let dav = Dav::with_file("/Docs/Report.docx", b"Report v1");
    dav.change(FILE, b"report v1");
    let rig = rig(dav.clone());
    let first = rig.open("/Docs/Report.docx");
    dav.set_down(true);
    rig.save(&first, b"unsynced Report edit");
    dav.set_down(false);
    let launches = rig.launched.lock().unwrap().len();

    rig.service.open(FILE, None, false);
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudError(m) if m.contains("nearly the same name"))), "{:?}", rig.events());
    assert_eq!(rig.launched.lock().unwrap().len(), launches);
    assert!(dav.held_lock(FILE).is_none(), "refused before locking");
    assert_eq!(std::fs::read(&first).unwrap(), b"unsynced Report edit");

    rig.advance(Duration::from_secs(60));
    rig.settle();
    assert_eq!(dav.remote("/Docs/Report.docx").unwrap(), b"unsynced Report edit");
    assert_eq!(dav.remote(FILE).unwrap(), b"report v1");
}

#[test]
fn done_keeps_a_save_that_lands_during_unlock() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    let late = local.clone();
    dav.during_unlock(move || std::fs::write(&late, b"saved during unlock").unwrap());
    rig.service.done(FILE);
    let told = rig.events().iter().rev().find_map(|e| match e {
        Event::CloudFileStatus { path, detail: Some(d), .. } if path == FILE => Some(d.clone()),
        _ => None,
    });
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    let kept = kept_with(&rig, b"saved during unlock");
    assert_eq!(kept.len(), 1, "the late save survives disconnecting");
    assert!(told.is_some_and(|d| d.contains(&kept[0].display().to_string())), "the user is told where it is");
}

#[test]
fn done_waits_while_the_copy_is_mid_save() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    let tmp = local.with_file_name(".~report.docx.tmp");
    std::fs::rename(&local, &tmp).unwrap();
    std::fs::write(&tmp, b"the save being swapped in").unwrap();
    rig.service.done(FILE);
    assert!(!rig.events().iter().any(|e| matches!(e, Event::CloudFileStatus { closed: true, .. })));
    std::fs::rename(&tmp, &local).unwrap();
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"the save being swapped in");
}

#[test]
fn the_watcher_ignores_another_accounts_file_at_the_same_path() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let mut rig = rig(dav.clone());
    let local = rig.open(FILE);
    let record = rig.paths.cloud_root().join("checkouts.json");
    let mut book: serde_json::Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    let theirs = rig.root.path().join("other-account-copy.docx");
    std::fs::write(&theirs, b"another account's edit").unwrap();
    let mut other = book["checkouts"][0].clone();
    other["account"] = "0123abcd".into();
    other["local"] = theirs.display().to_string().into();
    other["lock"] = serde_json::Value::Null;
    book["checkouts"].as_array_mut().unwrap().insert(0, other);
    std::fs::write(&record, serde_json::to_vec(&book).unwrap()).unwrap();
    rig.rebuild();
    rig.service.resume();

    rig.save(&local, b"alice's edit");
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"alice's edit");
    assert_eq!(std::fs::read(&theirs).unwrap(), b"another account's edit");
}

#[test]
fn connecting_another_account_is_refused_while_files_are_open() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.sign_in_as("bob");
    rig.service.connect(SERVER);
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudConnectFailed(m) if m.contains("different one"))), "{:?}", rig.events());
    assert_eq!(rig.service.account().unwrap().login, "alice");
    rig.save(&local, b"still alice's file");
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"still alice's file");
}

fn cache_file(rig: &Rig, relative: &str) -> PathBuf {
    rig.paths.cloud_root().join(accounts::handle(SERVER, "alice")).join(relative)
}

#[test]
fn disconnect_moves_stray_cache_files_to_kept() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let stray = cache_file(&rig, "Docs/stray.docx");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"untracked bytes").unwrap();
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    assert!(!stray.exists());
    assert_eq!(kept_with(&rig, b"untracked bytes").len(), 1, "the stray file is kept, bytes intact");
}

#[test]
fn disconnect_refuses_when_a_stray_file_cannot_be_kept() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let stray = cache_file(&rig, "Docs/stray.docx");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"untracked bytes").unwrap();
    // A file where the account's kept folder must go: nothing can be kept.
    let kept = rig.paths.cloud_root().join("kept");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join(accounts::handle(SERVER, "alice")), b"in the way").unwrap();

    rig.service.disconnect();
    assert!(!disconnected(&rig), "{:?}", rig.events());
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudError(m) if m.contains("stray.docx"))), "{:?}", rig.events());
    assert_eq!(std::fs::read(&stray).unwrap(), b"untracked bytes");
    assert!(rig.service.account().is_some(), "still connected");
}

#[test]
fn done_keeps_watching_a_late_save_it_cannot_move_aside() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    let kept = rig.paths.cloud_root().join("kept");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join(accounts::handle(SERVER, "alice")), b"in the way").unwrap();
    let late = local.clone();
    dav.during_unlock(move || std::fs::write(&late, b"saved during unlock").unwrap());
    rig.service.done(FILE);
    assert_ne!(rig.status(FILE), Some(SyncStatus::Saved), "not shown as safely saved");
    rig.advance(Duration::from_secs(60));
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"saved during unlock", "the late save is uploaded after all");
}

#[test]
fn forgetting_a_changed_read_only_copy_keeps_it_and_lets_disconnect_go() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    rig.service.open(FILE, None, true);
    let local = rig.launched.lock().unwrap()[0].1.clone();
    rig.save(&local, b"local scribbles");
    rig.service.done(FILE);
    rig.service.disconnect();
    assert!(!disconnected(&rig), "an open file blocks disconnecting: {:?}", rig.events());

    rig.service.forget(FILE);
    let kept = kept_with(&rig, b"local scribbles");
    assert_eq!(kept.len(), 1, "the edits are kept, bytes intact: {:?}", rig.events());
    assert!(!local.exists(), "the copy moved out of the cache");
    let told = last_closed(&rig, FILE).and_then(|(_, d)| d);
    assert!(told.as_deref().is_some_and(|d| d.contains(&kept[0].display().to_string())), "{told:?}");

    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    assert_eq!(kept_with(&rig, b"local scribbles").len(), 1, "disconnecting keeps it too");
}

#[test]
fn forget_refuses_when_the_copy_cannot_be_kept() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_down(true);
    rig.save(&local, b"unsynced edit");
    let blocker = rig.paths.cloud_root().join("kept").join(accounts::handle(SERVER, "alice"));
    std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
    std::fs::write(&blocker, b"in the way").unwrap();

    rig.service.forget(FILE);
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudError(_))), "{:?}", rig.events());
    assert_eq!(std::fs::read(&local).unwrap(), b"unsynced edit", "the copy stays where it was");
    assert!(dav.held_lock(FILE).is_some(), "still checked out, still locked");
    rig.service.disconnect();
    assert!(!disconnected(&rig), "still open, so disconnecting is still refused");

    std::fs::remove_file(&blocker).unwrap();
    dav.set_down(false);
    rig.service.forget(FILE);
    assert_eq!(kept_with(&rig, b"unsynced edit").len(), 1);
    assert!(dav.held_lock(FILE).is_none(), "stopping tracking releases the lock");
    assert_eq!(dav.remote(FILE).unwrap(), b"remote v1", "nothing is uploaded");
}

#[test]
fn start_announces_the_connection_before_the_open_files() {
    let mut rig = rig(Dav::with_file(FILE, b"remote v1"));
    rig.open(FILE);
    rig.rebuild();
    rig.events.lock().unwrap().clear();
    rig.service.start();
    let events = rig.events();
    let connected = events.iter().position(|e| matches!(e, Event::CloudConnected(a) if a.login == "alice"));
    let status = events.iter().position(|e| matches!(e, Event::CloudFileStatus { path, .. } if path == FILE));
    assert!(connected.is_some_and(|c| status.is_some_and(|s| c < s)), "{events:?}");
}

#[test]
fn start_without_an_account_announces_nothing() {
    let rig = rig(Dav::default());
    std::fs::remove_file(rig.paths.cloud_root().join("account.json")).unwrap();
    rig.service.start();
    assert!(!rig.events().iter().any(|e| matches!(e, Event::CloudConnected(_))), "{:?}", rig.events());
}

/// The last status of `path`: (status, detail, forgettable).
fn last_status(rig: &Rig, path: &str) -> Option<(SyncStatus, Option<String>, bool)> {
    rig.events().into_iter().rev().find_map(|e| match e {
        Event::CloudFileStatus { path: p, status, detail, forgettable, .. } if p == path => Some((status, detail, forgettable)),
        _ => None,
    })
}

/// The last status of `path` if it closed the file: (status, detail).
fn last_closed(rig: &Rig, path: &str) -> Option<(SyncStatus, Option<String>)> {
    rig.events().into_iter().rev().find_map(|e| match e {
        Event::CloudFileStatus { path: p, status, detail, closed, .. } if p == path => Some(closed.then_some((status, detail))),
        _ => None,
    })?
}

#[test]
fn forgetting_unsaved_edits_says_they_are_not_in_the_cloud() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    rig.save(&local, b"edit not uploaded yet");
    rig.service.forget(FILE);
    assert_eq!(kept_with(&rig, b"edit not uploaded yet").len(), 1, "{:?}", rig.events());
    let (_, detail, _) = last_status(&rig, FILE).unwrap();
    assert!(detail.as_deref().is_some_and(|d| d.contains("not saved to the cloud")), "{detail:?}");
    assert_eq!(dav.remote(FILE).unwrap(), b"remote v1");
}

#[test]
fn forgetting_a_synced_copy_does_not_warn() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    rig.open(FILE);
    rig.service.forget(FILE);
    let (_, detail) = last_closed(&rig, FILE).unwrap();
    assert!(detail.as_deref().is_some_and(|d| !d.contains("not saved")), "{detail:?}");
}

#[test]
fn only_a_read_only_edit_or_a_missing_copy_is_forgettable() {
    // Read-only with edits: Done is refused, and Stop tracking is the way out.
    let rig1 = rig(Dav::with_file(FILE, b"remote v1"));
    rig1.service.open(FILE, None, true);
    let local = rig1.launched.lock().unwrap()[0].1.clone();
    rig1.save(&local, b"scribbles");
    rig1.service.done(FILE);
    assert!(matches!(last_status(&rig1, FILE), Some((SyncStatus::ReadOnly, Some(_), true))), "{:?}", rig1.events());

    // The copy is missing.
    let rig2 = rig(Dav::with_file(FILE, b"remote v1"));
    let local = rig2.open(FILE);
    std::fs::remove_file(&local).unwrap();
    rig2.service.done(FILE);
    assert!(matches!(last_status(&rig2, FILE), Some((SyncStatus::WaitingToUpload, Some(_), true))), "{:?}", rig2.events());

    // A network failure: the edits can still go up later, so nothing offers to drop them.
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig3 = rig(dav.clone());
    let local = rig3.open(FILE);
    rig3.save(&local, b"offline edit");
    dav.set_down(true);
    rig3.service.done(FILE);
    assert!(matches!(last_status(&rig3, FILE), Some((SyncStatus::WaitingToUpload, _, false))), "{:?}", rig3.events());
    assert!(rig3.events().iter().all(|e| !matches!(e, Event::CloudFileStatus { forgettable: true, .. })));
}

/// Whether `checkouts.json` records `path` as open.
fn recorded(rig: &Rig, path: &str) -> bool {
    std::fs::read_to_string(rig.paths.cloud_root().join("checkouts.json")).is_ok_and(|r| r.contains(&format!("\"{path}\"")))
}

#[test]
fn a_failed_download_after_opening_closes_the_file_again_and_says_why() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.refuse(Method::Get, 500);
    let rig = rig(dav.clone());
    rig.service.open(FILE, None, false);
    assert!(rig.statuses(FILE).contains(&SyncStatus::Opening), "{:?}", rig.events());
    let (_, why) = last_closed(&rig, FILE).unwrap_or_else(|| panic!("the open is closed again: {:?}", rig.events()));
    assert!(why.is_some_and(|w| !w.is_empty()), "the reason is given");
    assert!(dav.held_lock(FILE).is_none(), "the lock is released");
    assert!(!recorded(&rig, FILE));
    assert!(rig.launched.lock().unwrap().is_empty());
}

#[test]
fn an_app_that_cannot_be_given_the_file_leaves_nothing_open() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    *rig.hooks.launch.lock().unwrap() = Some(Arc::new(|| Err("WordCraft isn't installed.".to_string())));
    rig.service.open(FILE, None, false);
    let (_, why) = last_closed(&rig, FILE).unwrap_or_else(|| panic!("the open is closed again: {:?}", rig.events()));
    assert!(why.is_some_and(|w| w.contains("isn't installed")), "the launcher's reason is given");
    assert!(dav.held_lock(FILE).is_none(), "the lock is released");
    assert!(!recorded(&rig, FILE), "no checkout is left");
    assert!(!cache_file(&rig, "Docs/report.docx").exists(), "the unchanged download is removed");
    rig.service.disconnect();
    assert!(disconnected(&rig), "nothing blocks disconnecting: {:?}", rig.events());
}

#[test]
fn a_save_the_server_refuses_for_good_stops_uploading_and_can_be_dropped() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.refuse(Method::Put, 403);
    rig.save(&local, b"edit to a read-only share");
    rig.settle();
    assert!(matches!(last_status(&rig, FILE), Some((SyncStatus::ReadOnly, Some(_), true))), "{:?}", rig.events());
    dav.clear_seen();
    rig.save(&local, b"edited again");
    rig.advance(Duration::from_secs(10 * 60));
    rig.settle();
    assert!(dav.puts().is_empty(), "no more uploads: {:?}", dav.puts());
    assert_eq!(std::fs::read(&local).unwrap(), b"edited again");
    rig.service.forget(FILE);
    assert_eq!(kept_with(&rig, b"edited again").len(), 1);
}

#[test]
fn a_save_whose_place_on_the_server_is_gone_offers_stop_tracking() {
    for status in [404, 409] {
        let dav = Dav::with_file(FILE, b"remote v1");
        let rig = rig(dav.clone());
        let local = rig.open(FILE);
        dav.refuse(Method::Put, status);
        rig.save(&local, b"edit after the folder went");
        rig.settle();
        assert!(matches!(last_status(&rig, FILE), Some((SyncStatus::WaitingToUpload, Some(_), true))), "{status}: {:?}", rig.events());
        assert_eq!(std::fs::read(&local).unwrap(), b"edit after the folder went");
    }
}

#[test]
fn a_conflict_or_a_rejected_sign_in_offers_stop_tracking() {
    let rig1 = rig(Dav::with_file(FILE, b"remote v1"));
    conflicted(&rig1);
    assert!(matches!(last_status(&rig1, FILE), Some((SyncStatus::Conflict, _, true))), "{:?}", rig1.events());

    let dav = Dav::with_file(FILE, b"remote v1");
    let rig2 = rig(dav.clone());
    let local = rig2.open(FILE);
    dav.set_unauthorized(true);
    rig2.save(&local, b"edit while signed out");
    rig2.settle();
    assert!(matches!(last_status(&rig2, FILE), Some((SyncStatus::ReconnectNeeded, _, true))), "{:?}", rig2.events());
}

#[test]
fn a_conflict_choice_the_server_refuses_stays_in_conflict_and_offers_stop_tracking() {
    for choice in [ConflictChoice::KeepBoth, ConflictChoice::UseMine] {
        let rig = rig(Dav::with_file(FILE, b"remote v1"));
        let local = conflicted(&rig);
        rig.dav.refuse(Method::Put, 409);
        rig.events.lock().unwrap().clear();
        rig.service.resolve(FILE, choice);
        assert!(matches!(last_status(&rig, FILE), Some((SyncStatus::Conflict, Some(_), true))), "{choice:?}: {:?}", rig.events());
        assert_eq!(std::fs::read(&local).unwrap(), b"my version");
    }
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let local = conflicted(&rig);
    rig.service.resolve(FILE, ConflictChoice::UseTheirs);
    rig.dav.refuse(Method::Get, 404);
    rig.service.resolve(FILE, ConflictChoice::TheirsClosed);
    assert!(matches!(last_status(&rig, FILE), Some((SyncStatus::CloseInAppFirst, Some(_), true))), "{:?}", rig.events());
    assert_eq!(std::fs::read(&local).unwrap(), b"my version", "nothing replaced");
}

#[test]
fn a_file_listed_as_not_writable_opens_read_only() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.list_read_only(FILE);
    let rig = rig(dav.clone());
    rig.service.list("/Docs");
    let local = rig.open(FILE);
    assert_eq!(rig.status(FILE), Some(SyncStatus::ReadOnly));
    assert!(dav.held_lock(FILE).is_none(), "nothing to lock for a file that can't be saved");
    rig.save(&local, b"local scribbles");
    rig.settle();
    assert!(dav.puts().is_empty(), "{:?}", dav.puts());
}

#[test]
fn an_open_that_waited_while_the_account_was_disconnected_does_nothing() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let (paused_tx, paused) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Arc::new(Mutex::new(go_rx));
    *rig.hooks.pause.lock().unwrap() = Some(Arc::new(move || {
        paused_tx.send(()).unwrap();
        go_rx.lock().unwrap().recv().unwrap();
    }));
    std::thread::scope(|scope| {
        let opening = scope.spawn(|| rig.service.open(FILE, None, false));
        paused.recv().unwrap();
        // The open has checked the account and now waits: the account goes meanwhile.
        rig.service.disconnect();
        assert!(disconnected(&rig), "{:?}", rig.events());
        go.send(()).unwrap();
        opening.join().unwrap();
    });
    assert!(rig.launched.lock().unwrap().is_empty(), "nothing opened for an account that is gone");
    assert!(dav.held_lock(FILE).is_none());
    assert!(!recorded(&rig, FILE));
    assert!(!rig.statuses(FILE).contains(&SyncStatus::Opening));
}

#[test]
fn a_sign_in_cancelled_while_it_waits_for_the_open_files_connects_nothing() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let (launching_tx, launching) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Arc::new(Mutex::new(go_rx));
    // The open holds the open files while it launches the app.
    *rig.hooks.launch.lock().unwrap() = Some(Arc::new(move || {
        launching_tx.send(()).unwrap();
        go_rx.lock().unwrap().recv().unwrap();
        Ok(())
    }));
    std::thread::scope(|scope| {
        let opening = scope.spawn(|| rig.service.open(FILE, None, false));
        launching.recv().unwrap();
        let connecting = scope.spawn(|| rig.service.connect(SERVER));
        // Long enough for the sign-in to be approved and to wait for the open files.
        std::thread::sleep(Duration::from_millis(300));
        rig.events.lock().unwrap().clear();
        rig.service.cancel_connect();
        go.send(()).unwrap();
        opening.join().unwrap();
        connecting.join().unwrap();
    });
    assert!(!rig.events().iter().any(|e| matches!(e, Event::CloudConnected(_) | Event::CloudConnectFailed(_))), "{:?}", rig.events());
}

#[test]
fn a_copy_a_crash_left_mid_done_is_watched_again() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let mut rig = rig(dav.clone());
    let local = rig.open(FILE);
    rig.save(&local, b"edit before the crash");
    let closing = local.with_file_name(".~report.docx.closing1");
    std::fs::rename(&local, &closing).unwrap();
    rig.rebuild();
    rig.service.resume();
    assert_eq!(std::fs::read(&local).unwrap(), b"edit before the crash", "back under its real name");
    rig.settle();
    assert_eq!(dav.remote(FILE).unwrap(), b"edit before the crash");
}

#[test]
fn disconnect_keeps_a_crash_leftover_under_its_real_name() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    let leftover = cache_file(&rig, "Docs/.~stray.docx.closing1");
    std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
    std::fs::write(&leftover, b"left mid-close").unwrap();
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    let kept = kept_with(&rig, b"left mid-close");
    assert_eq!(kept.len(), 1);
    let name = kept[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("stray") && name.ends_with(".docx"), "{name}");
}

#[test]
fn disconnect_with_an_unreadable_account_record_still_signs_out() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    assert!(rig.service.account().is_some());
    std::fs::write(rig.paths.cloud_root().join("account.json"), b"{ not json").unwrap();
    rig.service.disconnect();
    assert!(disconnected(&rig), "{:?}", rig.events());
    assert!(rig.service.account().is_none(), "the session is gone too");
    let token = format!("artcraft-launcher-nextcloud:{}", accounts::handle(SERVER, "alice"));
    assert!(rig.keychain.get(&token).unwrap().is_none(), "and so is its saved sign-in");
}

#[test]
fn a_save_changed_back_to_the_cloud_version_reads_as_saved() {
    let dav = Dav::with_file(FILE, b"remote v1");
    let rig = rig(dav.clone());
    let local = rig.open(FILE);
    dav.set_down(true);
    rig.save(&local, b"a change");
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::WaitingToUpload));
    rig.save(&local, b"remote v1");
    dav.set_down(false);
    dav.clear_seen();
    rig.advance(Duration::from_secs(10 * 60));
    rig.settle();
    assert_eq!(rig.status(FILE), Some(SyncStatus::Saved));
    assert!(dav.puts().is_empty(), "nothing to upload: {:?}", dav.puts());
}

#[test]
fn the_lock_is_refreshed_before_the_granted_timeout_runs_out() {
    let dav = Dav::with_file(FILE, b"remote v1");
    dav.grant_locks_for(60);
    let rig = rig(dav.clone());
    rig.open(FILE);
    dav.clear_seen();
    rig.advance(Duration::from_secs(45));
    rig.service.tick();
    assert!(dav.seen().iter().any(|s| s.method == Method::Lock && s.lock_if.is_some()), "{:?}", dav.seen());
}

#[test]
fn show_kept_files_reveals_the_kept_folder() {
    let rig = rig(Dav::with_file(FILE, b"remote v1"));
    rig.service.reveal_kept();
    assert!(rig.hooks.revealed.lock().unwrap().is_empty());
    assert!(rig.events().iter().any(|e| matches!(e, Event::CloudError(_))), "no kept files yet is said: {:?}", rig.events());
    let local = rig.open(FILE);
    rig.save(&local, b"kept edit");
    rig.service.forget(FILE);
    rig.service.reveal_kept();
    let revealed = rig.hooks.revealed.lock().unwrap().clone();
    let kept = kept_with(&rig, b"kept edit");
    assert!(revealed.len() == 1 && kept[0].starts_with(&revealed[0]), "{revealed:?} {kept:?}");
}
