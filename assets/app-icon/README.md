# udinCraft Launcher app icon

The app icon is the **udinCraft mark in colour**, exactly as supplied in
[`docs/brand/udincraft-mark.svg`](../../docs/brand/udincraft-mark.svg). Every file in this
directory is rendered from it by `packaging/icons.sh`.

**These files are the owner's brand. They are not open source and not covered by the launcher's
MIT or Apache-2.0 license.** Their terms are in
[`docs/brand/LICENSE-brand.txt`](../../docs/brand/LICENSE-brand.txt): use them only as part of
udinCraft Launcher, never redraw, recolor or crop them, and remove them from forks and modified
versions.

## Files

The file names keep upstream's `artcraft-launcher` and `ai.storyteller.artcraft-launcher`, because
the packaging and the installed apps look for them by those names.

- `artcraft-launcher.svg`: a copy of `docs/brand/udincraft-mark.svg`; also the hicolor scalable icon.
- `artcraft-launcher-1024.png`: 1024 px render, with extra transparent margin for Apple's icon grid.
- `artcraft-launcher.icns`: macOS bundle icon (`CFBundleIconFile`).
- `artcraft-launcher.ico`: Windows icon, 16–256 px, embedded in the `.exe` by
  `apps/artcraft-launcher/build.rs` and used by the MSI.
- `hicolor/<size>/apps/ai.storyteller.artcraft-launcher.png` (16–512) and
  `hicolor/scalable/apps/ai.storyteller.artcraft-launcher.svg`: Linux icon theme.
- The tray / menu-bar icon is the same mark, rendered into `docs/brand/udincraft-mark-44.png`
  (macOS, 22 pt @2x) and `docs/brand/udincraft-mark-64.png` (Windows, Linux, FreeBSD).

## Regenerate

Run `packaging/icons.sh` on any OS. It needs `resvg`; the `.ico` and `.icns` are packed by
`cargo xtask ico`. Never edit these files by hand: change the source only if the owner
supplies a new mark in `docs/brand/`.
