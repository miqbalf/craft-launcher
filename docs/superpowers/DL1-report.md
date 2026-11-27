# DL1 report: Craft apps on Nextcloud again, and as the Mac's default apps

Date: 2026-11-27. Branch: miqbalf/craft-launcher `feat/nextcloud-cloud-files`.
Brief: `docs/superpowers/DL1-brief.md`.

## Summary

- The launcher's Cloud feature works against workspace.treeo.id. A temporary user was used for the
  test and then deleted. The test covered sign-in through Login Flow v2, listing, search,
  shared-with-me, and opening a .docx, .xlsx, .pptx and .pdf in the right app. It also covered
  locking, uploading a save back with If-Match, Done (which unlocks and cleans up) and Disconnect.
  - Prod needed no change.
  - It found one bug: the lock holder was shown as raw XML. It is fixed in craft-libs and the
    launcher's pin is moved.
- The Mac test build failed only because GitHub never gave the arm64 job a runner. It now builds
  one universal, ad-hoc signed app on the Intel runner, as a .dmg and a zipped .app.
- `packaging/macos/set-craft-defaults.sh` is written. Its dry run, and a real set, check and restore
  on a throwaway CI Mac, pass against the exact app versions installed on the owner's Mac. It was
  not run on the owner's Mac.

## Root cause of the CI failure

Runs 44776225523 and 44771282929 (2026-11-20) failed in the `macos-latest` (arm64) job alone. Its
error was "The job was not acquired by Runner of type hosted even after multiple attempts". GitHub
also warned "Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer
queue times". The job got no runner for 15 minutes and was failed. The `macos-15-intel` job passed
in both runs. The code was never the problem.

Fix: one job on `macos-15-intel`. It adds both Rust targets and runs the repo's own
`packaging/macos/package.sh --arch universal`. With no signing secrets, that script signs ad-hoc
and does not notarize. The arm64 half is cross-compiled, so the app still runs natively on Apple
silicon. The old artifact was a bare binary in a tarball. The new one is a real
`ArtCraft Launcher.app`, with its Info.plist, icon and the craft-ai-mcp helper.

## What changed

craft-launcher (BASE `86f9fd4`, the brief's starting point, which is 29 commits ahead of
main `bbb3844`):

| Commit | Change |
|---|---|
| `001396b` | ci(fork-only): universal Mac test app on the Intel runner; first version of `set-craft-defaults.sh`; `.github/scripts/make-office-samples.py` (our own minimal docx/xlsx/pptx/pdf) |
| `ed4331a` | ci(fork-only): the defaults check moves to its own workflow `craft-defaults.yml`, with a JXA probe. The Mac build skips pushes that only touch the defaults check |
| `c30549a` | deps(fork-only): craft-libs fork rev `4d43879` → `7a3278e` (all 7 pins, including craft-quit in the app crate), Cargo.lock |
| `c983041` | packaging(macos): the script uses the CoreServices type functions (JXA has no bridge for the `UTType` class); the probe is removed |
| `e3c9243` | test(fork-only): `crates/native/examples/cloud_live.rs`, a live check of `CloudService` against a real server |
| (this commit) | this report |

craft-libs, miqbalf `feat/nextcloud-adapter`, fast-forwarded from `4d43879` to `7a3278e`:
"craft-nextcloud: name a WebDAV lock's holder without markup". When files_lock records a WebDAV
lock, Nextcloud 35 reports the `d:owner` of the LOCK request as the holder's display name. Our
`<d:owner><d:href>user</d:href></d:owner>` therefore came back as the text
`<d:href xmlns:d="DAV:">user</d:href>`, and the Cloud page showed that markup. LOCK now sends the
owner as plain text. A holder name that still has markup in it (a lock taken by an older client)
is read as its text. Two tests were added in `crates/nextcloud/tests/it/main.rs`.

Up to date: the branch was already 0 commits behind miqbalf main (`bbb3844`), so no merge was
needed. storytold/craft-launcher main is 68 commits further on. Following the brief, that was not
merged.

The `.github/` files, `crates/native/examples/cloud_live.rs` and the fork craft-libs pin are
fork-only. Drop them before any upstream PR. None was opened, and nothing went to storytold.

## Checks

On udinpc (Linux), with craft-libs at `7a3278e`:

| Check | Result |
|---|---|
| `cargo test -p craft-nextcloud` | 38 + 1 passed, including the 2 new tests |
| `cargo clippy -p craft-nextcloud --all-targets -D warnings`, `cargo fmt --check` | clean |
| `cargo fmt --all --check` (launcher) | clean, after formatting the example |
| `cargo clippy -p artcraft-launcher-native --all-targets -D warnings` | clean |
| `cargo clippy -p artcraft-launcher --all-targets -D warnings` | clean |
| `cargo test -p artcraft-launcher-native` | 376 + 71 passed. The 4 ignored tests were already ignored on main (they need the network or real releases) |
| `cargo test -p artcraft-launcher-ui-egui` | 108 + 171 + 14 small binaries, all passed |
| `cargo test -p artcraft-launcher-core` | 83 passed, 1 already ignored on main |
| shellcheck 0.10 (`set-craft-defaults.sh`) | clean |
| actionlint 1.7.7 | it only flags the `macos-15-intel` label as unknown. That tool version predates the label, which GitHub accepts |

Not run: `cargo xtask wasm` (core, ui-egui and web are unchanged).

## CI runs and artifacts

- **Craft defaults check:** https://github.com/miqbalf/craft-launcher/actions/runs/45952693555 (success).
  - Artifact: `craft-defaults-dry-run`. It holds `dry-run.txt`, `set.txt`, `after-set.txt`,
    `restore.txt`, `after-restore.txt`, `backup.json`, `open.txt` and screenshots.
- **Mac test build:** https://github.com/miqbalf/craft-launcher/actions/runs/45952693607.
  - Artifact: `artcraft-launcher-macos-universal`. It holds
    `artcraft-launcher-0.19.2-macos-universal.dmg` and `ArtCraft-Launcher-macos-universal.app.zip`.
  - Its first step runs `cargo test -p artcraft-launcher-native --test it cloud` on macOS, which
    compiles the macOS-only open path.
  - The earlier, cancelled run 45951645400 had already passed that step and reached packaging.
- One step in the defaults job checks the launcher's macOS open path. It is the same `open -a <app> <file>`
  that `process::open_with` uses, run against the upstream bundles with the sample files:
  - WordCraft: window "sample - WordCraft", text rendered.
  - GridCraft: window "sample.xlsx".
  - DeckCraft: slide "Craft cloud test slide" rendered.
  - PdfCraft: window "sample.pdf — PdfCraft".

## Prod test (workspace.treeo.id)

Setup:
- A temporary user was made with `occ user:add --password-from-env`. The
  password was random, kept in mode-600 files, and passed on stdin over ssh and `docker exec -i`.
  It never appeared in argv.
- The user was deleted afterwards (`occ user:delete`, then a check that no dl1 user is left). The
  password and netrc files on the Mac and on udinpc were removed.
- No other prod change was made.

Method: `cloud_live` ran on udinpc. It drives `CloudService` exactly as the backend does, using
the real ureq transport and an in-memory keychain:
- The sign-in address goes on stdin to a helper. Acting as a person in a browser, the helper opens
  the address, logs in as the temporary user and presses Grant.
- The "app" is a helper that saves the working copy atomically (it writes a temporary file and
  renames it). The zip files get one extra part, and the PDF gets a trailing comment.
- An independent download afterwards confirmed what the server held.

Results:

| Step | Result |
|---|---|
| status.php check, Login Flow v2, polling, app password | OK. `CloudConnected` with the temporary login |
| List `/`, list `/DL1` | OK |
| Search "sample" (WebDAV SEARCH) | OK, 4 hits, server-side (`folder_only=false`) |
| Shared with me (OCS) | OK, empty |
| Open .docx / .xlsx / .pptx / .pdf | Opened in wordcraft / gridcraft / deckcraft / pdfcraft. Working copy in `cloud/<account>/DL1/…` |
| Lock while open | files_lock 35.0.1 is enabled on prod. Every open file showed as locked. The holder showed as raw XML before the fix and as the temporary login name after it |
| Save goes back (If-Match PUT) | All 5 went Uploading → Saved. The server copies held the saved part or marker |
| Another client writing while the file is open | Refused with HTTP 423 because of the launcher's lock. So no conflict can happen on prod while a file is checked out (see Concerns) |
| Done | All 5 closed (`closed=true`). The listing showed them unlocked. The local cache held only `checkouts.json` and `recent.json` |
| Disconnect | `CloudDisconnected` |

The first run stopped after the 423 (it expected a conflict) and left 5 locks. These were removed
with UNLOCK as the test user before the second run. The second run passed completely.

Login Flow v2 only needed the normal browser steps. Nothing on the server had to change, so there
was no reason to stop.

## Install steps (owner)

1. Open https://github.com/miqbalf/craft-launcher/actions/runs/45952693607. Under Artifacts,
   download `artcraft-launcher-macos-universal`. You need to be signed in to GitHub. Or use
   `gh run download 45952693607 -R miqbalf/craft-launcher -n artcraft-launcher-macos-universal`.
2. Unzip it. Then either:
   - open `artcraft-launcher-0.19.2-macos-universal.dmg` and drag **ArtCraft Launcher** out of it
     to a folder such as Downloads, or
   - unzip `ArtCraft-Launcher-macos-universal.app.zip`.
3. In Terminal, run `xattr -dr com.apple.quarantine ~/Downloads/"ArtCraft Launcher.app"`.
4. Run `mv ~/Downloads/"ArtCraft Launcher.app" /Applications/`.
5. Open it. Go to **Cloud** → Connect to cloud, enter `workspace.treeo.id`, and approve in the
   browser. The first time the app password is saved, macOS asks for Keychain access. Allow it.
6. Click a .docx, .xlsx, .pptx or .pdf. It opens in the installed Craft app. Each save is
   uploaded. Press **Done** when you finish with the file.

The app is ad-hoc signed and not notarized, which is why step 3 is needed. It reports itself as
version 0.19.2. The launcher's own self-update may offer the storytold release. That release has no
Cloud page, so decline it while you test this build.

## Defaults script

`packaging/macos/set-craft-defaults.sh [--dry-run | --restore] [--apps-dir DIR]… [--backup FILE] [--register]`

- It uses only bash, `osascript -l JavaScript` and LaunchServices: `LSCopy/LSSetDefaultRoleHandlerForContentType`
  with the role `kLSRolesAll`, plus `UTTypeConformsTo` and `UTTypeCreatePreferredIdentifierForTag`.
  No Homebrew, no duti and no Xcode are needed.
- It finds Craft apps (`ai.storyteller.*`, or a name ending in "Craft") in /Applications and
  ~/Applications, and reads `CFBundleDocumentTypes`. It uses the `LSItemContentTypes` entries. A
  `CFBundleTypeExtensions` entry is resolved to its type and marked `[from .ext]`.
- Before any change, it saves the current handlers to
  `~/Library/Application Support/craft-defaults-backup.json`. A later run only adds types the file
  does not hold yet, so the original handlers are never overwritten. CI checks this.
  `--restore` puts them back.
- When two Craft apps claim the same type, the winner is chosen in this order:
  1. the specialist for the type's family: PdfCraft for PDF, VectorCraft for SVG, AI and EPS,
     PhotoCraft for images, SoundCraft for audio, FilmCraft for video, DeckCraft, GridCraft and
     WordCraft for their kinds;
  2. an app that declares the type itself, over one that only lists the file extension;
  3. LSHandlerRank, then the Editor role, then the fewest declared types.
- It never sets abstract types (public.data, public.image, …) or `public.html`. WordCraft declares
  `public.html`, but setting it would take .html files away from the browser.
- On CI it set all 61 types. A dry run afterwards showed every one of them on its Craft app.
  `--restore` then returned the Apple handlers (Preview, TextEdit, Music, Safari, …).

Dry run on CI, against the upstream bundles of the versions installed on the owner's Mac:
WordCraft 0.11.0, DeckCraft 0.11.1, GridCraft 0.12.1, PdfCraft 0.14.0, PhotoCraft 0.16.0,
VectorCraft 0.17.0, DesignCraft 0.13.0, CADCraft 0.13.0, FilmCraft 0.13.0, SoundCraft 0.13.1,
EffectCraft 0.17.0 and LightCraft 0.14.0. LightCraft declares no document types, so it is not
listed.

On the runner, `--register` had just registered the apps, so for types where the runner had no
other handler, "was" already names a Craft app. On the owner's Mac the "was" column will show
that Mac's own handlers.

```text
Would set (type -> app, current handler):
  ai.storyteller.cadcraft.dwg -> CADCraft  (was: ai.storyteller.cadcraft)
  ai.storyteller.cadcraft.dxf -> CADCraft  (was: ai.storyteller.cadcraft)
  ai.storyteller.deckcraft.presentation -> DeckCraft  (was: ai.storyteller.deckcraft)
  ai.storyteller.designcraft.document -> DesignCraft  (was: ai.storyteller.designcraft)
  ai.storyteller.designcraft.idml -> DesignCraft  (was: ai.storyteller.designcraft)
  ai.storyteller.effectcraft.document -> EffectCraft  (was: ai.storyteller.effectcraft)
  ai.storyteller.filmcraft.project -> FilmCraft  (was: ai.storyteller.filmcraft)
  ai.storyteller.photocraft.document -> PhotoCraft  (was: ai.storyteller.photocraft)
  ai.storyteller.photocraft.kra -> PhotoCraft  (was: ai.storyteller.photocraft)
  ai.storyteller.photocraft.ora -> PhotoCraft  (was: ai.storyteller.photocraft)
  ai.storyteller.photocraft.pdn -> PhotoCraft  (was: ai.storyteller.photocraft)
  ai.storyteller.photocraft.qoi -> PhotoCraft  (was: ai.storyteller.photocraft)
  ai.storyteller.soundcraft.session -> SoundCraft  (was: ai.storyteller.soundcraft)
  ai.storyteller.vectorcraft.document -> VectorCraft  (was: ai.storyteller.vectorcraft)
  com.adobe.encapsulated-postscript [from .eps] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  com.adobe.illustrator.ai-image [from .ai] -> VectorCraft  (was: com.apple.Preview)
  com.adobe.pdf -> PdfCraft  (was: com.apple.Preview)
  com.adobe.photoshop-image -> PhotoCraft  (was: com.apple.Preview)
  com.adobe.photoshop-large-image -> PhotoCraft  (was: ai.storyteller.photocraft)
  com.compuserve.gif -> PhotoCraft  (was: com.apple.Preview)
  com.ilm.openexr-image -> PhotoCraft  (was: com.apple.Preview)
  com.microsoft.bmp -> PhotoCraft  (was: com.apple.Preview)
  com.microsoft.excel.sheet.binary.macroenabled -> GridCraft  (was: ai.storyteller.gridcraft)
  com.microsoft.ico -> PhotoCraft  (was: com.apple.Preview)
  com.microsoft.waveform-audio -> SoundCraft  (was: com.apple.Music)
  com.microsoft.word.doc -> WordCraft  (was: com.apple.TextEdit)
  com.truevision.tga-image -> PhotoCraft  (was: com.apple.Preview)
  dyn.age80c3u [from .af] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age80c3xeqz30w35s [from .afdesign] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age80c3xuszva [from .afpub] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age80c4py [from .ait] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age80n5pg [from .emf] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age81q25yqz01a5dbsvwu [from .vctemplate] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  dyn.age81s5pg [from .wmf] -> VectorCraft  (was: ai.storyteller.vectorcraft)
  net.daringfireball.markdown -> WordCraft  (was: com.apple.ScriptEditor2)
  org.oasis-open.opendocument.spreadsheet -> GridCraft  (was: ai.storyteller.gridcraft)
  org.oasis-open.opendocument.text -> WordCraft  (was: com.apple.TextEdit)
  org.openxmlformats.presentationml.presentation -> DeckCraft  (was: ai.storyteller.deckcraft)
  org.openxmlformats.presentationml.slideshow -> DeckCraft  (was: ai.storyteller.deckcraft)
  org.openxmlformats.presentationml.template -> DeckCraft  (was: ai.storyteller.deckcraft)
  org.openxmlformats.spreadsheetml.sheet -> GridCraft  (was: ai.storyteller.gridcraft)
  org.openxmlformats.spreadsheetml.sheet.macroenabled -> GridCraft  (was: ai.storyteller.gridcraft)
  org.openxmlformats.wordprocessingml.document -> WordCraft  (was: com.apple.TextEdit)
  org.openxmlformats.wordprocessingml.template -> WordCraft  (was: ai.storyteller.wordcraft)
  org.webmproject.webp -> PhotoCraft  (was: com.apple.Preview)
  org.xiph.flac -> SoundCraft  (was: ai.storyteller.soundcraft)
  public.aifc-audio -> SoundCraft  (was: com.apple.Music)
  public.aiff-audio -> SoundCraft  (was: com.apple.Music)
  public.avif -> PhotoCraft  (was: com.apple.Preview)
  public.comma-separated-values-text -> GridCraft  (was: ai.storyteller.gridcraft)
  public.jpeg -> PhotoCraft  (was: com.apple.Preview)
  public.midi-audio -> SoundCraft  (was: ai.storyteller.soundcraft)
  public.mp3 -> SoundCraft  (was: com.apple.Music)
  public.pbm -> PhotoCraft  (was: com.apple.Preview)
  public.plain-text -> WordCraft  (was: com.apple.TextEdit)
  public.png -> PhotoCraft  (was: com.apple.Preview)
  public.radiance -> PhotoCraft  (was: com.apple.Preview)
  public.rtf -> WordCraft  (was: com.apple.TextEdit)
  public.svg-image [from .svg] -> VectorCraft  (was: com.apple.Safari)
  public.tab-separated-values-text -> GridCraft  (was: ai.storyteller.gridcraft)
  public.tiff -> PhotoCraft  (was: com.apple.Preview)

Claimed by more than one Craft app:
  ai.storyteller.cadcraft.dxf: CADCraft over VectorCraft (declares the type itself (the other only an extension))
  com.adobe.pdf: PdfCraft over VectorCraft (specialist for com.adobe.pdf)
  com.compuserve.gif: PhotoCraft over VectorCraft (specialist for public.image)
  com.microsoft.bmp: PhotoCraft over VectorCraft (specialist for public.image)
  org.webmproject.webp: PhotoCraft over VectorCraft (specialist for public.image)
  public.jpeg: PhotoCraft over VectorCraft (specialist for public.image)
  public.png: PhotoCraft over VectorCraft (specialist for public.image)
  public.tiff: PhotoCraft over VectorCraft (specialist for public.image)

Skipped:
  public.html (WordCraft): abstract or shared type, never set
```

For the controller, after the owner has seen this list:
1. Run `packaging/macos/set-craft-defaults.sh --dry-run` on the Mac to see that Mac's real "was"
   column.
2. Then run it with no flag.
3. `--restore` undoes it.

## BASE..HEAD

- craft-launcher: `86f9fd4..` (HEAD = the commit that adds this report, after `e3c9243`).
- craft-libs (miqbalf `feat/nextcloud-adapter`): `4d43879..7a3278e`.

## Concerns

- **Conflicts can't be tested on prod.** While the launcher holds a files_lock lock, another
  client's write gets 423, even from the same user. So prod never reaches the conflict path while
  a file is checked out. That path (412 → Keep both, Use mine or Use theirs) is covered only by the
  fake-server tests in `crates/native/tests/it/cloud_sync.rs`.
- **Defaults are broad.** `public.plain-text` and Markdown go to WordCraft. That also catches
  text-like files with no handler of their own. Images go to PhotoCraft (away from Preview), audio
  to SoundCraft (away from Music), and .svg to VectorCraft (away from Safari). These follow the
  brief. The owner should read the list before the controller runs the script.
- **Prod Nextcloud was redeployed during the test window.** Both `nextcloud-web-*` containers
  were recreated at 06:22–06:24 UTC (new `…20261127T06…` names). I did not trigger this. It is
  probably another lane or Coolify. The test runs before and after it worked.
- **Ephemeral sign-in tokens in my session output.** While I was mapping the browser flow, two
  Login Flow `stateToken`s for flows that no longer exist were printed in my session output. They
  were never in a log file or this report. They are single-use, and the user they belonged to is
  deleted.
- **Not notarized.** The Mac app is ad-hoc signed only. The xattr step is required, and Keychain
  prompts may repeat after each rebuild, because the ad-hoc signature changes.
- **Not done:** the brief's macOS GUI flow in the launcher itself (clicking in the Cloud page on a
  real Mac). It is covered by parts: the service against prod on Linux, plus `open -a` with the
  real apps on a CI Mac. The owner's first run of the installed app is the end-to-end check.
