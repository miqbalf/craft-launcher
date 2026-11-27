#!/usr/bin/env bash
# Make each installed Craft app the macOS default ("all" roles) for the file types its Info.plist
# declares (CFBundleDocumentTypes: LSItemContentTypes, and CFBundleTypeExtensions resolved to their
# type identifiers). Uses only what macOS ships: bash, osascript (JavaScript for Automation) and
# LaunchServices. No Homebrew, no duti.
#
# Usage: packaging/macos/set-craft-defaults.sh [--dry-run | --restore] [--apps-dir DIR]... [--backup FILE] [--register]
#
#   (no mode)        back up the current handlers, then set each type to its Craft app
#   --dry-run        print what would be set (and what is skipped or contested); change nothing
#   --restore        put back the handlers saved in the backup file
#   --apps-dir DIR   where to look for Craft apps (repeatable); default /Applications and ~/Applications
#   --backup FILE    default: ~/Library/Application Support/craft-defaults-backup.json
#   --register       register the apps found with LaunchServices first (for apps outside
#                    /Applications, e.g. on a CI runner); not needed for apps in /Applications
#
# The backup is written before anything changes. A later run only adds types the backup does not
# hold yet, so the handlers from before the first run are never overwritten by Craft ones.
# When two Craft apps claim one type, the more specific app wins (see choose() below); every such
# choice is printed. Abstract types (public.data, public.image, ...) and public.html are never set.
set -euo pipefail

MODE="set"
BACKUP="${HOME}/Library/Application Support/craft-defaults-backup.json"
REGISTER=0
DIRS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) MODE=dry-run; shift ;;
    --restore) MODE=restore; shift ;;
    --apps-dir) DIRS+=("$2"); shift 2 ;;
    --backup) BACKUP="$2"; shift 2 ;;
    --register) REGISTER=1; shift ;;
    -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ ${#DIRS[@]} -eq 0 ]]; then
  DIRS=(/Applications "${HOME}/Applications")
fi
if [[ "$(uname -s)" != Darwin ]]; then
  echo "error: this script is for macOS" >&2
  exit 1
fi

if [[ "$REGISTER" = 1 && "$MODE" != restore ]]; then
  LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
  for d in "${DIRS[@]}"; do
    [[ -d "$d" ]] || continue
    for app in "$d"/*.app; do
      [[ -d "$app" ]] && "$LSREGISTER" -f "$app"
    done
  done
fi

JXA="$(mktemp -t craft-defaults).js"
trap 'rm -f "$JXA"' EXIT
cat >"$JXA" <<'JS'
// JXA reaches LaunchServices and the CoreServices type functions (checked on macOS 15), but not
// the UTType class of UniformTypeIdentifiers, which has no bridge metadata.
ObjC.import('Foundation');
ObjC.import('CoreServices');

const ROLES_ALL = 0xFFFFFFFF; // kLSRolesAll
// Types never set: setting them would capture every file that has no handler of its own, or (for
// public.html) take web pages away from the browser.
const NEVER = new Set(['public.item', 'public.data', 'public.content', 'public.composite-content',
  'public.text', 'public.image', 'public.movie', 'public.video', 'public.audio',
  'public.audiovisual-content', 'public.folder', 'public.directory', 'public.archive',
  'public.executable', 'public.html', 'public.json', 'public.xml']);
// The specialist for a family of types, most specific family first: PdfCraft for PDF, VectorCraft
// for vector formats, PhotoCraft for raster images, and so on.
const SPECIALISTS = [
  ['com.adobe.pdf', 'pdfcraft'],
  ['public.svg-image', 'vectorcraft'],
  ['com.adobe.illustrator.ai-image', 'vectorcraft'],
  ['com.adobe.encapsulated-postscript', 'vectorcraft'],
  ['public.image', 'photocraft'],
  ['public.audio', 'soundcraft'],
  ['public.movie', 'filmcraft'],
  ['public.presentation', 'deckcraft'],
  ['public.spreadsheet', 'gridcraft'],
  ['public.delimited-values-text', 'gridcraft'],
  ['public.text', 'wordcraft'],
];
const RANK = { Owner: 3, Default: 2, Alternate: 1, None: 0 };

function str(x) { return (x === undefined || x === null) ? null : ObjC.unwrap(x); }

// A CFStringRef that a Copy/Create function returned, as a JS string (null for none).
function refString(ref) {
  try {
    const s = ObjC.unwrap(ObjC.castRefToObject(ref));
    return (typeof s === 'string' && s.length) ? s : null;
  } catch (e) {
    return null;
  }
}

function handler(uti) {
  return refString($.LSCopyDefaultRoleHandlerForContentType($(uti), ROLES_ALL));
}

function setHandler(uti, bundleId) {
  return $.LSSetDefaultRoleHandlerForContentType($(uti), ROLES_ALL, $(bundleId));
}

function conforms(uti, parent) {
  return !!$.UTTypeConformsTo($(uti), $(parent));
}

function utiForExtension(ext) {
  return refString($.UTTypeCreatePreferredIdentifierForTag($('public.filename-extension'), $(ext), $()));
}

function readPlist(path) {
  const d = $.NSDictionary.dictionaryWithContentsOfFile(path);
  return (!d || d.isNil()) ? null : ObjC.deepUnwrap(d);
}

function readJson(path) {
  const s = $.NSString.stringWithContentsOfFileEncodingError(path, $.NSUTF8StringEncoding, null);
  return (!s || s.isNil()) ? null : JSON.parse(ObjC.unwrap(s));
}

function writeJson(path, value) {
  const fm = $.NSFileManager.defaultManager;
  const dir = $(path).stringByDeletingLastPathComponent;
  fm.createDirectoryAtPathWithIntermediateDirectoriesAttributesError(dir, true, $(), null);
  const ok = $(JSON.stringify(value, null, 2) + '\n').writeToFileAtomicallyEncodingError(path, true, $.NSUTF8StringEncoding, null);
  if (!ok) throw new Error('could not write ' + path);
}

// Craft apps in the folders given: bundle id ai.storyteller.* (or a name ending in "Craft"),
// with document types. The launcher opens no documents and is left out.
function findApps(dirs) {
  const fm = $.NSFileManager.defaultManager;
  const apps = [];
  const seen = new Set();
  for (const dir of dirs) {
    const list = fm.contentsOfDirectoryAtPathError($(dir), null);
    if (!list || list.isNil()) continue;
    for (const name of ObjC.deepUnwrap(list).sort()) {
      if (!name.endsWith('.app')) continue;
      const path = dir.replace(/\/$/, '') + '/' + name;
      const info = readPlist(path + '/Contents/Info.plist');
      if (!info) continue;
      const id = info.CFBundleIdentifier || '';
      const display = info.CFBundleName || name.replace(/\.app$/, '');
      if (!(id.startsWith('ai.storyteller.') || /Craft$/.test(display))) continue;
      if (id.endsWith('artcraft-launcher') || !Array.isArray(info.CFBundleDocumentTypes)) continue;
      if (seen.has(id)) continue;
      seen.add(id);
      apps.push({ id, name: display, path, key: id.split('.').pop().toLowerCase(), types: info.CFBundleDocumentTypes });
    }
  }
  return apps;
}

// Every (type, app) claim: tier 2 when the app names the type in LSItemContentTypes, tier 1 when
// it only lists an extension that resolves to it.
function claims(apps) {
  const byUti = new Map();
  const add = (uti, app, tier, t, ext) => {
    if (!uti) return;
    const list = byUti.get(uti) || [];
    const old = list.find((c) => c.app.id === app.id);
    const c = { app, tier, rank: RANK[t.LSHandlerRank] || 0, editor: t.CFBundleTypeRole === 'Editor' ? 1 : 0, ext };
    if (!old) list.push(c);
    else if (c.tier > old.tier) Object.assign(old, c);
    byUti.set(uti, list);
  };
  for (const app of apps) {
    app.count = 0;
    for (const t of app.types) {
      for (const uti of (t.LSItemContentTypes || [])) { add(uti, app, 2, t, null); app.count++; }
      for (const ext of (t.CFBundleTypeExtensions || [])) {
        if (ext === '*') continue;
        add(utiForExtension(ext), app, 1, t, ext);
        app.count++;
      }
    }
  }
  return byUti;
}

// The app that gets `uti` among its claimants, and why.
function choose(uti, list) {
  if (list.length === 1) return { pick: list[0], why: null };
  for (const [family, key] of SPECIALISTS) {
    const c = list.find((x) => x.app.key === key);
    if (c && conforms(uti, family)) return { pick: c, why: 'specialist for ' + family };
  }
  const sorted = list.slice().sort((a, b) => (b.tier - a.tier) || (b.rank - a.rank) || (b.editor - a.editor)
    || (a.app.count - b.app.count) || a.app.name.localeCompare(b.app.name));
  const [a, b] = sorted;
  let why = 'fewest declared types';
  if (a.tier !== b.tier) why = 'declares the type itself (the other only an extension)';
  else if (a.rank !== b.rank) why = 'higher LSHandlerRank';
  else if (a.editor !== b.editor) why = 'Editor role';
  else if (a.app.count === b.app.count) why = 'name order';
  return { pick: a, why };
}

function plan(apps) {
  const out = { set: [], skipped: [], contested: [] };
  const byUti = claims(apps);
  for (const uti of Array.from(byUti.keys()).sort()) {
    const list = byUti.get(uti);
    if (NEVER.has(uti)) { out.skipped.push({ uti, why: 'abstract or shared type, never set', by: list.map((c) => c.app.name) }); continue; }
    const { pick, why } = choose(uti, list);
    if (why) out.contested.push({ uti, winner: pick.app.name, others: list.filter((c) => c !== pick).map((c) => c.app.name), why });
    out.set.push({ uti, app: pick.app, ext: pick.ext, current: handler(uti) });
  }
  return out;
}

function run(argv) {
  const [mode, backupPath, ...dirs] = argv;
  if (mode === 'restore') {
    const backup = readJson(backupPath);
    if (!backup || !backup.handlers) { console.log('No backup at ' + backupPath + '; nothing to restore.'); return; }
    let failed = 0;
    for (const uti of Object.keys(backup.handlers).sort()) {
      const prev = backup.handlers[uti];
      if (!prev) { console.log(uti + ' : had no default before; left as ' + (handler(uti) || 'none')); continue; }
      const rc = setHandler(uti, prev);
      if (rc !== 0) failed++;
      console.log(uti + ' -> ' + prev + (rc === 0 ? '' : '  FAILED (OSStatus ' + rc + ')'));
    }
    console.log('Restored from ' + backupPath + (failed ? ' with ' + failed + ' failure(s)' : ''));
    if (failed) throw new Error(failed + ' type(s) could not be restored');
    return;
  }

  const apps = findApps(dirs);
  if (!apps.length) { console.log('No Craft apps found in ' + dirs.join(', ')); return; }
  console.log('Craft apps: ' + apps.map((a) => a.name + ' (' + a.id + ')').join(', '));
  const p = plan(apps);
  const dry = mode === 'dry-run';
  console.log('');
  console.log(dry ? 'Would set (type -> app, current handler):' : 'Setting (type -> app, previous handler):');

  if (!dry) {
    // Back up first; keep handlers recorded by an earlier run, so the originals survive.
    const backup = readJson(backupPath) || { version: 1, created: new Date().toISOString(), handlers: {} };
    for (const s of p.set) if (!(s.uti in backup.handlers)) backup.handlers[s.uti] = s.current;
    backup.updated = new Date().toISOString();
    writeJson(backupPath, backup);
  }
  let failed = 0;
  for (const s of p.set) {
    let note = '';
    if (!dry) {
      const rc = setHandler(s.uti, s.app.id);
      if (rc !== 0) { failed++; note = '  FAILED (OSStatus ' + rc + ')'; }
    }
    const via = s.ext ? ' [from .' + s.ext + ']' : '';
    console.log('  ' + s.uti + via + ' -> ' + s.app.name + '  (was: ' + (s.current || 'none') + ')' + note);
  }
  if (p.contested.length) {
    console.log('');
    console.log('Claimed by more than one Craft app:');
    for (const c of p.contested) console.log('  ' + c.uti + ': ' + c.winner + ' over ' + c.others.join(', ') + ' (' + c.why + ')');
  }
  if (p.skipped.length) {
    console.log('');
    console.log('Skipped:');
    for (const s of p.skipped) console.log('  ' + s.uti + ' (' + s.by.join(', ') + '): ' + s.why);
  }
  console.log('');
  if (dry) console.log('Dry run: nothing changed. A real run first saves the current handlers to ' + backupPath);
  else console.log('Saved the previous handlers to ' + backupPath + '; undo with --restore.' + (failed ? ' ' + failed + ' failure(s).' : ''));
  if (failed) throw new Error(failed + ' type(s) could not be set');
}
JS

osascript -l JavaScript "$JXA" "$MODE" "$BACKUP" "${DIRS[@]}"
