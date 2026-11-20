# Nextcloud cloud files in the launcher

Status: approved by owner 2026-11-20. Built in fork `miqbalf/craft-launcher`, aimed upstream at `storytold/craft-launcher`.

## Intent

Give the Craft App Suite a cloud start screen, like the ones office suites offer, with a
"Connect to cloud" step:

1. Click **Connect to cloud** and choose Nextcloud.
2. Type the server address. Log in through the browser.
3. Browse the Nextcloud folders inside the launcher.
4. Click a file. The right Craft app opens it: `.docx` in WordCraft, `.pdf` in PdfCraft, `.xlsx`
   in GridCraft, `.pptx` in DeckCraft, and so on.
5. Saves go back to Nextcloud automatically, without overwriting anyone else's work.

Why: WordCraft and the other Craft apps are good editors, but they have no cloud storage. The
owner uses Nextcloud and wants to keep documents on their own server, with native apps and no
web view.

## Goals

- Connect one Nextcloud account from the launcher. The password is never stored in plain text.
- Browse folders, files shared with me, and recent files. Search by file name.
- Open a file in the matching installed Craft app with one click.
- Upload every save back to the same Nextcloud file.
- Lock the file in Nextcloud while it is open, so other users see it is being edited.
- Detect a conflicting remote change, ask the user, and keep both versions. Never overwrite
  silently.
- Make no changes to the Craft apps. Only the launcher changes.

## Non-goals (this design)

- Live co-editing (several people typing at once). That is a separate later project (yrs, RFC-0020).
- Embedding Nextcloud's web interface in the launcher (rejected approach B).
- Apps reading or writing Nextcloud directly through their own Open/Save pages (approach C, a later
  step that reuses this work).
- Web build of the launcher. This is desktop only (`crates/native`, `apps/artcraft-launcher`).
- Several accounts or providers at once. The code should not prevent it later.
- Offline editing with later sync.

## Anti-goals (failure even if it "works")

- A save that silently replaces someone else's newer version.
- A Nextcloud password or app password in a plain-text file or in logs.
- Losing local edits: an upload failure must never delete or replace the working copy.
- Blocking the launcher window on network calls. Everything slow runs on worker threads, as the
  launcher already does.

## Constraints

- Pure Rust, matching the launcher. HTTP through the existing `ureq` dependency. No webview.
- Follow the launcher's split: pure logic in `crates/core` (no I/O), I/O in `crates/native`, UI in
  `crates/ui-egui`, using the existing `Command` / `Event` protocol between UI and backend.
- Follow the repo's `AGENTS.md` house rules (no `unwrap`/`expect`/`panic`, accessibility, i18n
  through `tl!`).
- The repo's `AGENTS.md` also requires:
  - Every Cloud action is reachable without the GUI, through the same `Command`s and the
    launcher's automation and CLI surface (rule 14).
  - Reusable parsers belong in craft-libs (rule 9).
  - A new dependency, such as a credential-store crate, needs a licence on the allowlist (rule 26).
  - No webview (rule 11).
  - No other company's product names in user-visible feature names (rule 22): the button says
    "Connect to cloud" and names Nextcloud only as the provider.
- Build and test on the home servers (pipit or arboy), not on the Mac. Manual macOS app testing is
  done by the owner.

## Approaches

| | Approach | Trade-off |
|---|---|---|
| **A (chosen)** | Native Nextcloud browser in the launcher; apps open a local working copy that the launcher uploads on save | Works with every Craft app today, no app changes. Apps' own Save As still points at the local cache. |
| B | Embed Nextcloud's web interface in a webview and intercept file clicks | Looks the most like the web interface, but adds a browser engine to a pure-Rust app, and intercepting the web UI is fragile. Still needs A's sync part. |
| C | Each app talks to Nextcloud directly through the craft-cloud `CloudBackend` (RFC-0024) | Cleanest long term, but needs an adapter plus wiring in every app before anything works. |

The owner picked **A**. Its login, credential storage, WebDAV listing and conditional upload are the
parts C needs later, so the work carries over.

## Design

### 1. Connecting (Nextcloud Login Flow v2)

- A **Connect to cloud** button on a new **Cloud** page. The first provider is Nextcloud.
- The user types the server address (for example `https://cloud.example.com`). The launcher
  normalises it: adds `https://`, drops a pasted `/index.php/...` or `/apps/...` tail, and keeps a sub-path install such as `https://host/nextcloud`. Then it checks `<base>/status.php` (as built: this check runs before Login Flow starts).
- The launcher starts Nextcloud Login Flow v2 (`POST /index.php/login/v2`). It opens the returned
  login URL in the system browser and polls the returned endpoint until the user approves.
  The result is the server, the login name and an **app password** that the user can revoke in
  Nextcloud.
- (Changed, see As built: account storage.) The app password goes into the OS credential store: macOS Keychain, Windows Credential Manager,
  Secret Service on Linux. Launcher settings keep only the server URL, login name and display name.
- When storytold/craft-launcher#366 ("Make the launcher the suite's keychain") lands, credential
  storage moves behind it. Until then, use a small `CredentialStore` trait so it can be swapped.
- **Disconnect** deletes the stored app password and the local cache, after warning about unsynced
  files. (Changed, see As built: Disconnect refuses while files are open and keeps backups.)

### 2. Browsing

- The Cloud page lists a folder through WebDAV `PROPFIND` (Depth 1) on
  `/remote.php/dav/files/<user>/<path>`. It shows name, type icon, size, modified time, and whether
  the file is shared or locked.
- Parse the multistatus with `craft-dav` from craft-libs if it can be added as a dependency
  cleanly. If not, use a small parser in `crates/core` with the same hostile-input limits.
- Sections: **Files** (folder tree with breadcrumbs), **Shared with me**, **Recent**. Recent comes
  from the launcher's own record of files opened through it.
- Search by name uses WebDAV `SEARCH`. If the server refuses it, fall back to filtering the current
  folder. (Changed, see As built: the fallback only applies to non-auth errors.)
- Files with no matching installed Craft app are shown but cannot be opened. A hint offers to
  install the right app when one exists in the catalog.

Sketch:

```text
┌ ArtCraft Launcher ─────────────────────────────────────────────┐
│ Apps | Cloud                                   ● cloud.example │
├────────────────────────────────────────────────────────────────┤
│ Files › Projects › 2026                         [search…]      │
│  📁 Drafts                                                      │
│  📄 report.docx        WordCraft     2 MB   today 10:12   🔒 Ana│
│  📊 budget.xlsx        GridCraft    80 KB   yesterday          │
│  📑 contract.pdf       PdfCraft    1.1 MB   12 Nov             │
├─ Open from cloud ──────────────────────────────────────────────┤
│  report.docx   WordCraft   ✓ saved to cloud 10:14   [Done]     │
└────────────────────────────────────────────────────────────────┘
```

### 3. Choosing the app

- A table in `crates/core` maps file extensions to catalog app ids:
  - WordCraft: docx, doc, dotx, odt, rtf, txt, md
  - GridCraft: xlsx, xls, ods, csv
  - DeckCraft: pptx, ppt, odp
  - PdfCraft: pdf
  - image, vector and video apps: their main formats
- Each app's own declared file types are the source of truth for the final list. The builder
  reads them from each app's packaging (macOS `Info.plist` document types or the Linux desktop
  entry) and keeps the table in sync.
- If an extension maps to several apps, the first one in the table wins. A right-click
  **Open with** menu lists the others. (Not built, see As built.)

### 4. Opening (check-out)

On a click:

1. **Lock** the file with WebDAV `LOCK` when the server has the `files_lock` app (checked through
   the OCS capabilities). If someone else holds the lock, offer **Open read-only** (no uploads) or
   **Cancel**.
2. **Download** it into the cache: `<launcher data>/cloud/<account>/<server path>`, keeping the real
   file name so the app's title bar and Recent list read correctly. Record the ETag and file id. (File id not built, see As built.)
3. **Launch** the app with the file through the existing `install::launch`. On macOS use
   `open -a <App> <file>`, so an app that is already running gets an "open document" event. All
   four office apps already handle that event (`apps/*/src/apple_events.rs`).
4. Add the file to the **Open from cloud** panel with its status.

A file already checked out is not downloaded again. The existing working copy opens, so unsynced
local edits are never thrown away.

### 5. Saving back (sync)

- Watch each checked-out file for changes. Apps save atomically (write a temporary file, then
  rename), so watch by path and modification time and size. A one-second poll is enough.
- When a change has been stable for about two seconds, upload it with
  `PUT … If-Match: <last ETag>`, sending the lock token when one is held. On success, store the new
  ETag from the PUT response.
- **412 Precondition Failed** (someone changed the file in Nextcloud):
  - Stop uploading that file. Mark it **Conflict** in the panel and send a desktop notification. (Notification not built, see As built.)
  - Offer: **Keep both** (upload mine as `name (conflict <date> <user>).docx`, the default), **Use
    mine** (overwrite after explicit confirmation), **Use theirs** (download theirs, keep mine as
    a local backup file).
- Network failure: keep retrying with backoff. Never touch the working copy. The panel shows
  **Waiting to upload**.
- Status values: Opening, Saved, Uploading, Waiting to upload, Conflict, Read-only.

### 6. Closing (check-in)

- The launcher cannot tell when an app closes one document. So the panel has a **Done** button per
  file: final upload, unlock, then remove the local copy if it is fully synced. (Changed, see As
  built: Done order.)
- When the launcher quits with files still open, it uploads what it can and keeps their locks and
  working copies. (Not built, see As built: no quit-time flush.) It shows them again on the next start.
- Locks are refreshed while a file is checked out, so a long editing session does not lose its
  lock. The refresh interval and lock timeout are the builder's call.

### 7. Protocol additions (sketch)

```rust
// crates/core protocol
Command::CloudConnect { server: String }
Command::CloudDisconnect
Command::CloudList { path: String }
Command::CloudOpen { path: String, app: Option<String> }
Command::CloudDone { path: String }
Command::CloudResolve { path: String, choice: ConflictChoice }

Event::CloudLoginUrl(String)
Event::CloudConnected(Account)
Event::CloudListed { path: String, entries: Result<Vec<CloudEntry>, String> }
Event::CloudFileStatus { path: String, status: SyncStatus }
```

## Upstream path (decided by the owner: aim for an official feature)

The owner wants this upstream in `storytold/craft-launcher`, contributed as GitHub user `miqbalf`.
So the storytold rules in `AGENTS.md` are requirements, not guidance.

Reuse what upstream already merged, instead of writing launcher-only copies:

- **Credentials:** `craft-suite-account` (storytold/craft-libs#665, merged). It is the
  provider-neutral suite account list with the token in the OS keychain, through `keyring` 4.x
  (MIT OR Apache-2.0, on the allowlist). It replaces the `CredentialStore` trait above.
- **File contract:** `craft-cloud::Files` (storytold/craft-libs#160, merged): `list`,
  `shared_with_me`, `read`, `write(if_match)`, `create`, `stat`. The launcher's Nextcloud client
  implements this trait, so the same adapter later serves approach C (apps opening Nextcloud
  directly).
- **Multistatus parsing:** `craft-dav`. It already exists at the launcher's pinned craft-libs `rev`.
- The launcher pins craft-libs at `453b0da7…`, which predates `craft-cloud` and
  `craft-suite-account`. Moving the `rev` is its own PR (rule 10).

The WebDAV/Nextcloud client belongs in craft-libs (rule 9), as `craft-webdav` / `craft-nextcloud`.
That is RFC-0024 step 2, written up by wdunn001, with code in wdunn001/collab-craft. It must be
coordinated: offer to do it or to help, never duplicate it.

Approach A adds something RFC-0024 does not describe: the launcher checks files out to a local
working copy and syncs them. Propose it on storytold/craft-launcher#131 ("Nextcloud or open source
cloud integration") and get maintainer feedback before the feature PRs. If maintainers ask for an
RFC, write one in craft-contrib.

PR order (each small, one concern):

1. Comment on #131: proposal plus a link to this design. Claim the launcher part.
2. Coordinate with wdunn001 on RFC-0024 step 2. Land `craft-webdav` + `craft-nextcloud` in
   craft-libs, with the two correctness fixes RFC-0024 names: `If-None-Match: *` on create, and
   the ETag taken from the PUT response.
3. craft-launcher: move the craft-libs `rev` (its own PR).
4. craft-launcher: Connect to cloud (Login Flow v2 + craft-suite-account), plus disconnect.
5. craft-launcher: Cloud page browsing (Files, Shared with me, Recent, search).
6. craft-launcher: open, sync, conflict handling, Done (macOS).
7. Later: Windows and Linux opening, then Flatpak, then more apps.

While upstream review is pending, the fork's feature branch may carry steps 2–6 together, to prove
the whole flow works. Each step is then cut into its own PR from that branch.

## Decided by the owner

- Desktop launcher, native browsing, approach A.
- Nextcloud first, connected the way another desktop office app's connect-to-cloud flow works ("Connect to
  cloud", server address, browser login).
- Clicking a file opens the right Craft app window. These are separate programs, so there are no
  shared tabs.

## Calls made in this design (owner can override)

- Login Flow v2 with an app password, not storing the user's real password.
- OS credential store through `craft-suite-account` (see Upstream path).
- Lock with `files_lock` when available. Without it, rely on `If-Match` alone.
- **Keep both** as the default conflict answer.
- **v1 platform: macOS only.** Keep `core` and the protocol platform-neutral. Windows, Linux and
  FreeBSD are follow-ups. Flatpak needs a cache folder both sandboxes can read and a Secret
  Service permission, so it is out of v1.
- **v1 apps: WordCraft, GridCraft, DeckCraft, PdfCraft only.** Each is confirmed to handle the
  macOS open-document event. Files for other apps are listed but say "Opening from the cloud isn't
  supported for this app yet" (rule 15). Other apps are added one at a time after checking they
  accept a file to open.
- **Automation:** add the `Command`/`Event` variants so every Cloud action is a command. A
  launcher CLI or MCP entry point is out of v1. The launcher has none today, so building one is its
  own project.
- **After a conflict:**
  - **Keep both:** my version is uploaded as `name (conflict <date> <user>).ext`. From then on the
    working copy tracks that conflict file (its path and ETag), so later saves update the conflict
    copy and never the original. The panel shows the new name.
  - **Use mine:** asks for confirmation, then does a forced PUT with no `If-Match` over the
    original. The working copy keeps tracking the original, with the new ETag.
  - **Use theirs:** uploads stop for that file. The launcher asks the user to close the document
    in the app, because the app still holds the old version in memory. After the user confirms, it
    moves the working copy to a local backup, downloads theirs, and opens it again in the app. Any
    upload before that confirmation is refused.
- A **Done** button, because the launcher cannot see when an app closes a document.
- Cache lives in the launcher's data folder, keyed by account and server path.

## Left to the builder

- Exact crate and module layout inside `core` / `native` / `ui-egui`.
- Polling intervals, backoff, lock refresh timing.
- Parser choice (`craft-dav` or local).
- Icons, exact UI layout and copy, within the sketch above and the house accessibility rules.
- Test strategy. Expected: unit tests in `core` for URL normalisation, extension mapping and
  multistatus parsing, plus a fake WebDAV server in `native` tests for lock, PUT/If-Match and 412.

## As built (2026-11-20)

This section records where the build differs from the design above. Where the two disagree, this
section wins.

### Account storage
- The app password is stored in the OS keychain through craft-suite-account's `TokenStore`, under
  the handle `artcraft-launcher-nextcloud:<handle>`.
- A non-secret record (server, login name, display name) is kept in `cloud/account.json`.
- The account is not stored in the suite account list. That list holds Craft deployment accounts.
  Putting a Nextcloud account there could be mistaken for one, and Disconnect would sign the user
  out of the suite. The launcher never calls the suite's sign-in, active-account, discard or forget
  functions.
- If this is moved into the suite list later, it needs a one-time record migration.

### Connecting
- The `status.php` check runs before Login Flow v2 starts. A server that fails it gets a clear
  "doesn't look like a Nextcloud server" error and no browser opens.
- Connecting a different account is refused while files are open, so no open file is orphaned.
- At start, the backend emits `CloudConnected` for an existing account, before it resumes file
  statuses. The UI otherwise cannot learn about an existing connection.
- A 401 shows a **Reconnect** button in the account bar and on the listing error.

### Browsing
- Search falls back to filtering the current folder only on a non-auth error. A 401 is shown as an
  error. The page does not yet say that results came from the folder only.
- **Shared with me** uses the OCS shares API (`shared_with_me=true`). Search (WebDAV `SEARCH`) and
  shared-with-me were both added to craft-nextcloud, in the craft-libs fork, because it had neither
  (rule 9).
- The lock holder is shown in the listing. A lock held by an app is worded separately from one held
  by a person. A 423 on open names the holder from the last folder listing.
- Files that are not W-writable still open as editable. Saves then fail with 403.

### Local backups
- All local backups go under `cloud/kept/<account>/…`, with a label in the file name:
  "my version", "saved after closing", "left in cache", "kept when you stopped tracking".
- Disconnect never deletes `kept/`.

### Disconnect
- Disconnect refuses while files are open. It does not warn and carry on.
- Before it removes anything, it sweeps leftover cache files into `kept/` (label "left in
  cache"). If a file cannot be kept, Disconnect refuses.
- Only then are the app password, the cache and `account.json` removed.

### Done (check-in)
- Done renames the working copy to a private name, verifies it is unchanged and uploaded, and
  deletes it. Only then does it send UNLOCK.
- A save that lands during Done is kept (label "saved after closing"). If it cannot be moved
  aside, the file goes back on the open list as Waiting to upload, and the save is uploaded.
- A missing working copy no longer counts as clean. Done waits instead.

### Case-twin guard
- Two server paths that differ only by case (or that point at the same file on disk) cannot take
  over each other's working copy. The second open is refused. The compare uses lower-casing, not
  Unicode normalisation, so names that differ only in NFC/NFD form on APFS are not caught.

### Stop tracking
- A new command, `CloudForget { path }`, shown as **Stop tracking (keep a local copy)**. It moves
  the working copy to `kept/`, unlocks, drops the check-out, and names the kept path. It never
  deletes data.
- `Event::CloudFileStatus` gained a `forgettable` flag. It is true only for read-only copies with
  local changes and for waits where the copy is missing. The UI offers the action only then,
  behind a confirmation. If the changes are not in Nextcloud, the text says so.
- Its reason: without it, a read-only copy with edits, or a deleted copy, would block Done and
  Disconnect for good.

### Not built yet
- File id and rename-following. Check-outs are tracked by path only.
- **Open with** for files that map to several apps.
- Desktop notification on conflict. Only the status event is sent.
- Quit-time flush. Open files are resumed on the next start instead.
- A label for search results that came from the folder fallback.

### Not tested
- The macOS app launch code (`install::open_file`, `process::open_with`) has not been compiled or
  run. It is the owner's Task 8 step, on a Mac.

### Fork-only dependency
- The launcher pins craft-libs at the fork-only rev `63ac40b`. It carries the craft-nextcloud
  adapter, search, shared-with-me and the lock holder. This deviates from rule 10 (move the rev in
  its own PR). It must be upstreamed and the rev moved before any upstream PR.
