//! Connecting, browsing and disconnecting the cloud account, against a fake server and an
//! in-memory keychain.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use artcraft_launcher_core::cloud::CloudEntry;
use artcraft_launcher_core::protocol::Event;
use artcraft_launcher_native::cloud::{CloudService, SEARCH_PATH, SHARED_PATH};
use artcraft_launcher_native::paths::Paths;
use craft_nextcloud::http::{Http, Method, Request, Response};
use craft_suite_account::{Memory, NewAccount, Secret, Store, accounts};

const SERVER: &str = "http://127.0.0.1:8080";
fn token_handle() -> String {
    format!("artcraft-launcher-nextcloud:{}", accounts::handle(SERVER, "alice"))
}

const PASSWORD: &str = "app-pass-w0rd-NEVER-ON-DISK";

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

/// A server whose answers a test scripts per request.
#[derive(Clone)]
struct Fake {
    handler: Arc<Handler>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        Self { handler: Arc::new(handler), seen: Arc::default() }
    }
}

impl Http for Fake {
    type Error = String;

    fn send(&self, req: Request) -> Result<Response, String> {
        self.seen.lock().unwrap().push(format!("{} {}", req.method, req.url));
        Ok((self.handler)(&req))
    }
}

fn json(body: &str) -> Response {
    Response::new(200, body.as_bytes().to_vec())
}

fn multistatus(items: &[(&str, bool)]) -> Response {
    let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#);
    xml.push_str(&response("/remote.php/dav/files/alice/", true));
    for (name, dir) in items {
        xml.push_str(&response(&format!("/remote.php/dav/files/alice/{name}"), *dir));
    }
    xml.push_str("</d:multistatus>");
    Response::new(207, xml.into_bytes())
}

fn response(href: &str, dir: bool) -> String {
    let kind = if dir {
        "<d:resourcetype><d:collection/></d:resourcetype>"
    } else {
        "<d:resourcetype/><d:getcontentlength>12</d:getcontentlength><d:getlastmodified>Tue, 03 Nov 2026 10:00:00 GMT</d:getlastmodified>"
    };
    format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop>{kind}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
}

/// A server that completes the Login Flow on the first poll and lists three entries.
fn friendly() -> Fake {
    Fake::new(|req| {
        if req.url.ends_with("/status.php") {
            json(r#"{"installed":true,"maintenance":false,"version":"30.0.1.2","productname":"Nextcloud"}"#)
        } else if req.url.ends_with("/index.php/login/v2") {
            json(&format!(r#"{{"login":"{SERVER}/login/v2/flow/xyz","poll":{{"token":"tok","endpoint":"{SERVER}/login/v2/poll"}}}}"#))
        } else if req.url.ends_with("/login/v2/poll") {
            json(&format!(r#"{{"server":"{SERVER}","loginName":"alice","appPassword":"{PASSWORD}"}}"#))
        } else if req.method == Method::Propfind {
            multistatus(&[("zeta.docx", false), ("Alpha", true), ("beta.pdf", false)])
        } else {
            Response::new(404, Vec::new())
        }
    })
}

struct Rig {
    _root: tempfile::TempDir,
    paths: Paths,
    keychain: Memory,
    events: Arc<Mutex<Vec<Event>>>,
    opened: Arc<Mutex<Vec<String>>>,
    service: Arc<CloudService<Memory, Fake>>,
}

fn rig(fake: Fake) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::under(root.path());
    let keychain = Memory::new();
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let (sink, seen) = (events.clone(), opened.clone());
    let service = CloudService::new(&paths, keychain.clone(), fake, move |e| sink.lock().unwrap().push(e))
        .with_opener(move |url| {
            seen.lock().unwrap().push(url.to_string());
            Ok(())
        })
        .with_timing(Duration::ZERO, Duration::from_secs(5));
    Rig { _root: root, paths, keychain, events, opened, service: Arc::new(service) }
}

impl Rig {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn connected(fake: Fake) -> Rig {
        let rig = rig(fake);
        rig.service.connect(SERVER);
        assert!(rig.service.account().is_some(), "connect should succeed: {:?}", rig.events());
        rig.events.lock().unwrap().clear();
        rig
    }
}

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn connect_stores_token_only_in_keychain() {
    let rig = rig(friendly());
    rig.service.connect(SERVER);
    let account = rig.service.account().unwrap();
    assert!(rig.keychain.has(&token_handle()), "the token should be in the keychain");
    assert_eq!(rig.keychain.count(), 1);
    assert_eq!(account.id, accounts::handle(SERVER, "alice"));

    let mut files = Vec::new();
    files_under(rig._root.path(), &mut files);
    assert!(!files.is_empty(), "the account record should have been written");
    for file in files {
        let bytes = std::fs::read(&file).unwrap();
        assert!(!bytes.windows(PASSWORD.len()).any(|w| w == PASSWORD.as_bytes()), "password found in {}", file.display());
    }
    let shown = format!("{:?}", rig.events());
    assert!(!shown.contains(PASSWORD), "the password reached an event");
}

#[test]
fn connect_emits_login_url_then_connected() {
    let rig = rig(friendly());
    rig.service.connect(SERVER);
    let events = rig.events();
    assert!(matches!(events.first(), Some(Event::CloudLoginUrl(u)) if u == &format!("{SERVER}/login/v2/flow/xyz")), "{events:?}");
    assert!(matches!(events.last(), Some(Event::CloudConnected(a)) if a.login == "alice"), "{events:?}");
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(*rig.opened.lock().unwrap(), vec![format!("{SERVER}/login/v2/flow/xyz")]);
}

#[test]
fn disconnect_removes_token_and_cache() {
    let rig = Rig::connected(friendly());
    let account = rig.service.account().unwrap();
    let cache = rig.paths.cloud_root().join(&account.id);
    std::fs::create_dir_all(cache.join("docs")).unwrap();
    std::fs::write(cache.join("docs/a.docx"), b"x").unwrap();
    let other = rig.paths.cloud_root().join("someone-else");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("keep.txt"), b"keep").unwrap();
    std::fs::create_dir_all(&rig.paths.data).unwrap();
    std::fs::write(rig.paths.data.join("installed.json"), b"{}").unwrap();

    rig.service.disconnect();

    assert!(!rig.keychain.has(&token_handle()), "the token should be gone");
    assert!(!rig.paths.cloud_root().join("account.json").exists());
    assert!(!cache.exists(), "the account's cache should be gone");
    assert_eq!(std::fs::read(other.join("keep.txt")).unwrap(), b"keep");
    assert!(rig.paths.data.join("installed.json").exists());
    assert!(rig.service.account().is_none());
    assert!(matches!(rig.events().as_slice(), [Event::CloudDisconnected]), "{:?}", rig.events());
}

fn listed(events: &[Event]) -> Vec<CloudEntry> {
    match events {
        [Event::CloudListed { entries: Ok(entries), .. }] => entries.clone(),
        other => panic!("expected one listing, got {other:?}"),
    }
}

#[test]
fn list_maps_entries_and_sorts_folders_first() {
    let rig = Rig::connected(friendly());
    rig.service.list("/");
    let entries = listed(&rig.events());
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Alpha", "beta.pdf", "zeta.docx"]);
    assert!(entries[0].is_dir);
    let file = &entries[2];
    assert_eq!((file.path.as_str(), file.size, file.is_dir), ("/zeta.docx", 12, false));
    assert_eq!(file.modified, 1_793_700_000, "Tue, 03 Nov 2026 10:00:00 GMT");
}

#[test]
fn unauthorized_list_emits_reconnect_needed_error() {
    let rig = Rig::connected(Fake::new(|req| if req.method == Method::Propfind { Response::new(401, Vec::new()) } else { friendly().send_ok(req) }));
    rig.service.list("/");
    let events = rig.events();
    let error = events.iter().find_map(|e| if let Event::CloudError(m) = e { Some(m.clone()) } else { None }).expect("a CloudError");
    assert!(error.to_lowercase().contains("reconnect"), "{error}");
    assert!(events.iter().any(|e| matches!(e, Event::CloudListed { entries: Err(_), .. })), "{events:?}");
    assert!(!format!("{events:?}").contains(PASSWORD));
}

impl Fake {
    fn send_ok(&self, req: &Request) -> Response {
        (self.handler)(req)
    }
}

#[test]
fn search_falls_back_to_listed_names_when_the_server_cannot_search() {
    let rig = Rig::connected(Fake::new(|req| match req.method {
        Method::Search => Response::new(501, Vec::new()),
        _ => friendly().send_ok(req),
    }));
    rig.service.list("/");
    rig.events.lock().unwrap().clear();
    rig.service.search("BETA");
    match rig.events().as_slice() {
        [Event::CloudListed { path, entries: Ok(found) }] => {
            assert_eq!(path, SEARCH_PATH);
            assert_eq!(found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["beta.pdf"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn search_does_not_fall_back_on_unauthorized() {
    let rig = Rig::connected(Fake::new(|req| match req.method {
        Method::Search => Response::new(401, Vec::new()),
        _ => friendly().send_ok(req),
    }));
    rig.service.list("/");
    rig.events.lock().unwrap().clear();
    rig.service.search("beta");
    let events = rig.events();
    assert!(events.iter().any(|e| matches!(e, Event::CloudError(m) if m.to_lowercase().contains("reconnect"))), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::CloudListed { entries: Ok(_), .. })), "{events:?}");
}

#[test]
fn shared_lists_what_others_shared() {
    let rig = Rig::connected(Fake::new(|req| {
        if req.url.contains("/ocs/v2.php/apps/files_sharing/") {
            json(
                r#"{"ocs":{"meta":{"statuscode":200},"data":[{"path":"/Team plan.docx","item_type":"file","uid_owner":"bob","permissions":3,"item_size":5}]}}"#,
            )
        } else {
            friendly().send_ok(req)
        }
    }));
    rig.service.shared();
    match rig.events().as_slice() {
        [Event::CloudListed { path, entries: Ok(found) }] => {
            assert_eq!(path, SHARED_PATH);
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].path.as_str(), found[0].shared), ("/Team plan.docx", true));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn cancel_stops_a_waiting_sign_in_quietly() {
    let fake = Fake::new(|req| if req.url.ends_with("/login/v2/poll") { Response::new(404, Vec::new()) } else { friendly().send_ok(req) });
    let rig = rig(fake);
    let service = rig.service.clone();
    let waiting = std::thread::spawn(move || service.connect(SERVER));
    std::thread::sleep(Duration::from_millis(100));
    rig.service.cancel_connect();
    waiting.join().unwrap();
    assert!(rig.service.account().is_none());
    assert!(!rig.events().iter().any(|e| matches!(e, Event::CloudConnected(_) | Event::CloudError(_))), "{:?}", rig.events());
}

#[test]
fn an_insecure_login_address_is_not_opened() {
    let fake = Fake::new(|req| {
        if req.url.ends_with("/status.php") {
            friendly().send_ok(req)
        } else if req.url.ends_with("/index.php/login/v2") {
            json(r#"{"login":"file:///etc/passwd","poll":{"token":"t","endpoint":"http://127.0.0.1:8080/p"}}"#)
        } else {
            Response::new(404, Vec::new())
        }
    });
    let rig = rig(fake);
    rig.service.connect(SERVER);
    assert!(rig.opened.lock().unwrap().is_empty());
    assert!(matches!(rig.events().as_slice(), [Event::CloudError(_)]), "{:?}", rig.events());
}

#[test]
fn a_bad_server_address_is_an_error_before_any_request() {
    let fake = friendly();
    let seen = fake.seen.clone();
    let rig = rig(fake);
    rig.service.connect("http://example.com");
    assert!(seen.lock().unwrap().is_empty());
    assert!(matches!(rig.events().as_slice(), [Event::CloudError(_)]));
}

#[test]
fn a_server_that_is_not_nextcloud_stops_before_the_login_flow() {
    for body in [r#"{"installed":false,"version":"30.0.1"}"#, r#"{"installed":true}"#, "<html>hello</html>"] {
        let fake = Fake::new(move |_| json(body));
        let seen = fake.seen.clone();
        let rig = rig(fake);
        rig.service.connect(SERVER);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen, [format!("GET {SERVER}/status.php")], "{body}");
        assert!(matches!(rig.events().as_slice(), [Event::CloudError(m)] if m.contains("Nextcloud")), "{body}: {:?}", rig.events());
    }
    let missing = Fake::new(|_| Response::new(404, Vec::new()));
    let rig = rig(missing);
    rig.service.connect(SERVER);
    assert!(matches!(rig.events().as_slice(), [Event::CloudError(_)]));
}

fn suite_account(dir: &std::path::Path, tokens: &Memory) -> (Store<Memory>, String) {
    let store = Store::at(Some(dir.to_path_buf()), tokens.clone());
    let new = NewAccount { service: "https://suite.example".into(), user: "ada".into(), display: "Ada".into(), email: None, linked: true };
    let saved = store.sign_in(&new, &Secret::new("suite-token"), 1).unwrap();
    (store, saved.id)
}

#[test]
fn disconnect_leaves_suite_accounts_alone() {
    let rig = rig(friendly());
    let (suite, id) = suite_account(&rig._root.path().join("suite"), &rig.keychain);
    rig.service.connect(SERVER);
    assert!(rig.service.account().is_some());
    assert_eq!(suite.accounts().unwrap().active.as_deref(), Some(id.as_str()), "connect must not change the suite's active account");
    rig.service.disconnect();
    assert!(rig.service.account().is_none());
    let list = suite.accounts().unwrap();
    assert_eq!(list.active.as_deref(), Some(id.as_str()));
    assert!(list.get(&id).is_some());
    assert!(suite.has_token(&id).unwrap(), "the suite token must survive");
}

#[test]
fn load_ignores_suite_accounts() {
    let rig = rig(friendly());
    let (_suite, _id) = suite_account(&rig._root.path().join("suite"), &rig.keychain);
    assert!(rig.service.account().is_none());
    rig.service.list("/");
    assert!(matches!(rig.events().as_slice(), [Event::CloudListed { entries: Err(_), .. }]), "{:?}", rig.events());
}
