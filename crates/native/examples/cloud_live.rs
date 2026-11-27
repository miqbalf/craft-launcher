//! Fork-only: drives the launcher's cloud service against a real Nextcloud server, the way the
//! backend does, with an in-memory keychain. Not for upstream; drop it before any PR.
//!
//! Environment:
//! - `CLOUD_LIVE_SERVER`: the server address as a user would type it.
//! - `CLOUD_LIVE_APPROVE`: a program that approves the sign-in. It gets the sign-in address on
//!   stdin (never in its arguments: the address carries the flow's token).
//! - `CLOUD_LIVE_SAVE`: a program that plays the app saving a file: it gets the working copy's
//!   path as its argument and rewrites the file in place (write elsewhere, then rename).
//! - `CLOUD_LIVE_FILES`: the server paths to open, comma-separated (e.g. `/DL1/sample.docx`).
//! - `CLOUD_LIVE_CONFLICT`: one of them; after it is opened, the program waits for the file
//!   `<data>/go-conflict` (the test script changes the server copy as another client meanwhile),
//!   then saves, expects a conflict, and settles it with Keep both. When the file says `refused`,
//!   the server turned the other client away (the launcher's lock), and the save just uploads.
//! - `CLOUD_LIVE_DATA`: an empty folder for the launcher's data.
//!
//! Prints one line per event, without the sign-in address or any secret.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use artcraft_launcher_core::cloud::{ConflictChoice, SyncStatus};
use artcraft_launcher_core::protocol::Event;
use artcraft_launcher_native::cloud::CloudService;
use artcraft_launcher_native::cloud::transport::Transport;
use artcraft_launcher_native::http::Http;
use artcraft_launcher_native::paths::Paths;

type Events = Arc<Mutex<Vec<Event>>>;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn describe(e: &Event) -> String {
    match e {
        Event::CloudLoginUrl(_) => "CloudLoginUrl (address not shown)".to_string(),
        Event::CloudConnected(a) => format!("CloudConnected login={} server={}", a.login, a.server),
        Event::CloudConnectFailed(m) => format!("CloudConnectFailed {m}"),
        Event::CloudListed { path, entries, folder_only } => match entries {
            Ok(list) => format!(
                "CloudListed {path} folder_only={folder_only}: {}",
                list.iter()
                    .map(|x| format!(
                        "{}{}{}",
                        x.name,
                        if x.is_dir { "/" } else { "" },
                        x.locked_by.as_ref().map(|l| format!(" [locked:{l}]")).unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(m) => format!("CloudListed {path} error: {m}"),
        },
        Event::CloudFileStatus { path, status, detail, forgettable, closed, .. } => {
            format!("CloudFileStatus {path} {status:?} closed={closed} forgettable={forgettable} {}", detail.clone().unwrap_or_default())
        }
        Event::CloudError(m) => format!("CloudError {m}"),
        Event::CloudDisconnected => "CloudDisconnected".to_string(),
        other => format!("{:?}", std::mem::discriminant(other)),
    }
}

/// The latest status of `path`, if any.
fn status_of(events: &Events, path: &str) -> Option<(SyncStatus, bool)> {
    let list = events.lock().ok()?;
    list.iter().rev().find_map(|e| match e {
        Event::CloudFileStatus { path: p, status, closed, .. } if p == path => Some((*status, *closed)),
        _ => None,
    })
}

fn count_status(events: &Events, path: &str, want: SyncStatus) -> usize {
    events
        .lock()
        .map_or(0, |l| l.iter().filter(|e| matches!(e, Event::CloudFileStatus { path: p, status, .. } if p == path && *status == want)).count())
}

fn find_copy(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "kept") {
                continue;
            }
            if let Some(found) = find_copy(&p, name) {
                return Some(found);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

fn wait_until(what: &str, svc: &impl Fn(), limit: Duration, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < limit {
        svc();
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("timed out waiting for {what}"))
}

fn run_save(save: &str, file: &Path) -> Result<(), String> {
    let ok = Command::new(save).arg(file).status().map_err(|e| format!("save helper: {e}"))?.success();
    if ok { Ok(()) } else { Err("save helper failed".to_string()) }
}

fn main() -> Result<(), String> {
    let server = env("CLOUD_LIVE_SERVER")?;
    let approve = env("CLOUD_LIVE_APPROVE")?;
    let save = env("CLOUD_LIVE_SAVE")?;
    let files: Vec<String> = env("CLOUD_LIVE_FILES")?.split(',').map(str::to_string).collect();
    let conflict = std::env::var("CLOUD_LIVE_CONFLICT").ok();
    let data = PathBuf::from(env("CLOUD_LIVE_DATA")?);

    let events: Events = Arc::default();
    let sink = events.clone();
    let paths = Paths::under(&data);
    let launched: Arc<Mutex<Vec<(String, PathBuf)>>> = Arc::default();
    let record = launched.clone();
    let svc = CloudService::new(&paths, craft_suite_account::Memory::default(), Transport::new(&Http::new()?), move |e| {
        println!("event: {}", describe(&e));
        if let Ok(mut l) = sink.lock() {
            l.push(e);
        }
    })
    .with_opener(move |url| {
        let mut child = Command::new(&approve).stdin(Stdio::piped()).spawn().map_err(|e| format!("approve helper: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(url.as_bytes()).map_err(|e| format!("approve helper: {e}"))?;
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    })
    .with_launcher(move |app, file| {
        println!("launch: {app} {}", file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if let Ok(mut l) = record.lock() {
            l.push((app.to_string(), file.to_path_buf()));
        }
        Ok(())
    });
    let svc = Arc::new(svc);
    let tick = {
        let svc = svc.clone();
        move || svc.tick()
    };

    svc.connect(&server);
    if !events.lock().map_err(|_| "poisoned")?.iter().any(|e| matches!(e, Event::CloudConnected(_))) {
        return Err("sign-in failed".to_string());
    }
    svc.list("/");
    let folder = files.first().and_then(|f| f.rsplit_once('/')).map(|(d, _)| d.to_string()).unwrap_or_default();
    svc.list(&folder);
    svc.search("sample");
    svc.shared();
    svc.recent();

    let cloud_root = paths.cloud_root();
    for path in &files {
        svc.list(&folder);
        svc.open(path, None, false);
        let name = path.rsplit('/').next().unwrap_or_default();
        let copy = find_copy(&cloud_root, name).ok_or(format!("no working copy for {path}"))?;
        let app = launched.lock().map_err(|_| "poisoned")?.iter().rev().find(|(_, f)| f == &copy).map(|(a, _)| a.clone());
        println!("opened {path} in {} as {}", app.unwrap_or_else(|| "NOTHING".to_string()), copy.display());
        svc.list(&folder);

        if conflict.as_deref() == Some(path.as_str()) {
            let go = data.join("go-conflict");
            println!("waiting for {}", go.display());
            wait_until("the other client's change", &|| {}, Duration::from_secs(120), || go.exists())?;
            // The server refused the other client's change (the file is locked for this one): a
            // conflict can't arise, so the save must simply go up.
            if std::fs::read_to_string(&go).is_ok_and(|s| s.trim() == "refused") {
                println!("the other client's change to {path} was refused while the launcher holds the lock");
                let saved_before = count_status(&events, path, SyncStatus::Saved);
                run_save(&save, &copy)?;
                wait_until("the upload", &tick, Duration::from_secs(60), || count_status(&events, path, SyncStatus::Saved) > saved_before)?;
                println!("uploaded {path}");
                continue;
            }
            run_save(&save, &copy)?;
            wait_until("a conflict", &tick, Duration::from_secs(60), || status_of(&events, path).is_some_and(|(s, _)| s == SyncStatus::Conflict))?;
            println!("conflict detected for {path}");
            svc.resolve(path, ConflictChoice::KeepBoth);
            svc.list(&folder);
            continue;
        }

        let saved_before = count_status(&events, path, SyncStatus::Saved);
        run_save(&save, &copy)?;
        wait_until("the upload", &tick, Duration::from_secs(60), || count_status(&events, path, SyncStatus::Saved) > saved_before)?;
        println!("uploaded {path}");
    }

    svc.list(&folder);
    for path in &files {
        svc.done(path);
    }
    // The conflict copy (Keep both) is tracked under its new name: close whatever is still open.
    let open: Vec<String> = {
        let list = events.lock().map_err(|_| "poisoned")?;
        let mut latest: Vec<(String, bool)> = Vec::new();
        for e in list.iter() {
            if let Event::CloudFileStatus { path, closed, .. } = e {
                latest.retain(|(p, _)| p != path);
                latest.push((path.clone(), *closed));
            }
        }
        latest.into_iter().filter(|(_, closed)| !closed).map(|(p, _)| p).collect()
    };
    for path in &open {
        println!("closing still-open {path}");
        svc.done(path);
    }
    wait_until("every file closed", &tick, Duration::from_secs(60), || {
        let list = events.lock().map(|l| l.clone()).unwrap_or_default();
        let mut latest: Vec<(String, bool)> = Vec::new();
        for e in &list {
            if let Event::CloudFileStatus { path, closed, .. } = e {
                latest.retain(|(p, _)| p != path);
                latest.push((path.clone(), *closed));
            }
        }
        latest.iter().all(|(_, c)| *c)
    })?;
    svc.list(&folder);
    svc.disconnect();
    println!("finished");
    Ok(())
}
