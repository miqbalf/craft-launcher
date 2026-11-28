//! Fork-only DG1 harness (not shipped): runs the launcher's real cloud service against a real
//! Nextcloud on Linux, where the launcher UI doesn't open cloud files yet, so the grammar and AI
//! hand-off can be tested end to end: the account-wide `assist/assist.json`, and the `assist`
//! field of a live session file.
//!
//! Environment:
//! - `DG1_SERVER`, `DG1_LOGIN`, `DG1_PW_FILE` (0600, the app password), `DG1_ROOT` (data folder).
//! - `DG1_OPEN`: a server path to open live (optional), started with `DG1_APP_CMD` (a program
//!   given `<session file> <working copy>`).
//! - Stops when `<DG1_ROOT>/stop` appears; signs out (Disconnect) when `<DG1_ROOT>/disconnect`
//!   appears.
//!
//! Prints one line per event, without any secret.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use artcraft_launcher_core::protocol::Event;
use artcraft_launcher_native::cloud::transport::Transport;
use artcraft_launcher_native::cloud::{CloudService, LiveProcess};
use artcraft_launcher_native::http::Http;
use artcraft_launcher_native::paths::Paths;
use craft_suite_account::{Memory, Secret, TokenStore, accounts};

struct Proc(Arc<Mutex<Child>>);

impl LiveProcess for Proc {
    fn exited(&mut self) -> bool {
        self.0.lock().map_or(true, |mut c| !matches!(c.try_wait(), Ok(None)))
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn run() -> Result<(), String> {
    let server = env("DG1_SERVER")?;
    let login = env("DG1_LOGIN")?;
    let password = std::fs::read_to_string(env("DG1_PW_FILE")?).map_err(|e| e.to_string())?.trim().to_string();
    let root = PathBuf::from(env("DG1_ROOT")?);
    let paths = Paths::under(&root);
    let keychain = Memory::new();
    keychain
        .put(&format!("artcraft-launcher-nextcloud:{}", accounts::handle(&server, &login)), &Secret::new(&password))
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(paths.cloud_root()).map_err(|e| e.to_string())?;
    let record = serde_json::json!({"server": server, "login": login, "display": login}).to_string();
    std::fs::write(paths.cloud_root().join("account.json"), record).map_err(|e| e.to_string())?;
    let http = Http::new().map_err(|e| e.to_string())?;
    let app_cmd = std::env::var("DG1_APP_CMD").ok();
    let service = CloudService::new(&paths, keychain, Transport::new(&http), |e: Event| match e {
        Event::CloudFileStatus { path, status, detail, .. } => println!("EVENT status {path} {status:?} {}", detail.unwrap_or_default()),
        Event::CloudDisconnected => println!("EVENT disconnected"),
        Event::CloudError(m) => println!("EVENT error {m}"),
        other => println!("EVENT {}", format!("{other:?}").chars().take(80).collect::<String>()),
    })
    .with_launcher(|app: &str, file: &Path| {
        println!("CHECKOUT {app} {}", file.display());
        Ok(())
    })
    .with_opener(|_| Ok(()))
    .with_live_launcher(move |app: &str, session: &Path, file: &Path| {
        let cmd = app_cmd.clone().ok_or("DG1_APP_CMD is not set")?;
        let child = Command::new(cmd).arg(session).arg(file).spawn().map_err(|e| format!("spawn {app}: {e}"))?;
        println!("LIVE {app} session={} file={}", session.display(), file.display());
        Ok(Box::new(Proc(Arc::new(Mutex::new(child)))) as Box<dyn LiveProcess>)
    });
    println!("ASSIST-FILE {}", paths.assist_file().display());
    service.start();
    if let Ok(path) = std::env::var("DG1_OPEN") {
        service.open(&path, None, false);
    }
    let (stop, disconnect) = (root.join("stop"), root.join("disconnect"));
    loop {
        service.tick();
        if disconnect.exists() {
            let _ = std::fs::remove_file(&disconnect);
            service.disconnect();
        }
        if stop.exists() {
            println!("STOP");
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dg1_assist: {e}");
        std::process::exit(1);
    }
}
