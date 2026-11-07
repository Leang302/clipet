# Clipet

A fast, private clipboard manager and screenshot tool for macOS and Windows.
Clipet remembers what you copy (text, links, code, images, screenshots and real
files), lets you find it again in a keystroke, and pastes it back into the app
you were using. Everything stays on your computer.

Built with Tauri 2 (Rust) + React/TypeScript + SQLite.

![Search finds Korean text inside a screenshot](docs/screenshots/search-text-in-images.png)

## Features

**Clipboard history**
- Press **⌘⇧V** (Ctrl+Shift+V on Windows) anywhere to open the popup, pick an item with the arrow keys and press **Enter**. Clipet hands focus back to the exact window you came from and pastes there.
- Text, links, code, images, screenshots and **files and folders** are all recorded, sorted into categories, and can be favorited or pinned.
- Select several items (Shift / ⌘-click) to paste them in order, or several images to paste them as one batch.
- The popup can be dragged by its sidebar, search bar, header or shortcut bar, and it stays on screen.

**Search that reads your images**
- Search matches text *inside* screenshots and images, read on-device by Apple Vision (macOS) or Windows OCR. Korean + English work out of the box, and matches are highlighted on the image.
- Korean substring search (매출 finds 매출관리내역), case- and space-insensitive matching, and exact matches ranked first.

**Real files, not just their names**

![File entry with Paste File, Copy Path, Copy File Link, Reveal and Open](docs/screenshots/files.png)

- Copying files in Finder or Explorer records the actual file references (in order, including folders and non-Latin names), never the file contents.
- **Paste File** pastes the files themselves into any app that accepts files. You can also **Copy Path**, **Copy File Link** (`file://`, properly encoded), **Reveal in Finder / Show in Explorer**, or **Open**.
- Moved, renamed or deleted files and disconnected drives are flagged instead of failing silently.

**Long text, comfortably**

![Full text viewer with find and highlights](docs/screenshots/text-viewer.png)

- Long text and code get a short preview and an **Expand** button. The viewer has find with highlights and next/previous, line wrapping, Copy All, editing, and character and line counts. The history list stays usable next to it.

**Smart Snippets**

![Snippets](docs/screenshots/snippets.png)

- Type a shortcut such as `;sig` followed by Space or Tab in any app, and it expands into saved text or an image.
- Each snippet chooses what follows the text: nothing (the default), one space, or a new line. The key you press to expand is never inserted.
- Snippets are stored separately from history, so clearing history never removes them.

**Annotate screenshots**

![Annotation editor](docs/screenshots/annotation-editor.png)

- Hover over an image and click **Edit**: pen, highlighter, arrows, shapes, text, blur, pixelate, crop, undo/redo and zoom.
- **Save as New** (the default; the original is untouched), **Replace Original**, or **Save & Paste**.

**Privacy and housekeeping**

![History settings](docs/screenshots/history-settings.png)

- Copies that password managers mark as private are never recorded. Recording can be paused for 15 minutes, an hour, or until you resume.
- Optional automatic cleanup: keep history forever (the default), or for 1, 7, 30 or 90 days, or a custom number of days. Favorites, pinned items and snippets are protected, and Clipet shows how many items a setting would delete before it deletes anything.
- No accounts, no cloud, no telemetry. Text recognition runs on-device.

![Dark theme](docs/screenshots/history-dark.png)

## Install

Builds are written to `release/` (not committed to git, see [Development](#development) to build them):

| Platform | File |
|---|---|
| macOS (Apple Silicon and Intel) | `Clipet_0.1.0_universal.dmg` |
| Windows x64 | `Clipet_0.1.0_x64-setup.exe` |

**macOS:** open the DMG and drag Clipet to Applications. To paste and expand snippets, Clipet needs
**System Settings → Privacy & Security → Accessibility** turned on for Clipet. It asks the first time.

> The macOS build is signed for local use but not notarized. The Windows build
> compiles and packages, but hasn't yet been tested on a Windows PC.

## Good to know

- **Password fields:** while a password field has focus, macOS hides typing from every app, so snippets
  can't expand there. Clipet respects that and doesn't work around it. To insert a snippet into a password
  field, open Clipet (⌘⇧V) → **Snippets** → **Enter**. If a browser leaves its password field
  focused in the background, typing stays hidden everywhere. Leave that field or restart the
  browser, and the Snippets view will tell you which app is blocking it.
- **Typed snippet expansion** is macOS-only for now. On Windows, snippets can be pasted from the popup.
- **Logs** (no clipboard content or typed text is ever logged): `~/Library/Logs/com.clipflow.app/snippets.log`.

## Development

Requirements: Node 20+, Rust (stable), and the Tauri 2 prerequisites for your OS.

```sh
cd app
npm install
npm run tauri dev          # run with hot reload
```

| Command (in `app/`) | What it does |
|---|---|
| `npm run tauri:build:mac` | Signed universal macOS `.app` + `.dmg` (requires a signing identity) |
| `npm run tauri build -- --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis` | Windows installer, cross-compiled from macOS (needs `cargo-xwin`, `nsis`, `llvm` on PATH) |
| `cd src-tauri && cargo test` | Rust unit and integration tests |
| `npm run test:ui` | Headless UI regression tests (Playwright, mocked Tauri bridge) |
| `npm run screenshots` | Regenerates the README screenshots from sample data |

On macOS, a development build inherits Accessibility permission from the app that launched it (VS Code, Terminal or iTerm), so enable that app under Accessibility if paste is blocked.

### Project layout

```
app/
  src/                 React UI: popup (App.tsx), Snippets, Editor, FileCard, TextViewer, HistorySettings
  src-tauri/src/
    lib.rs             Tauri commands, paste pipeline, tray, window handling
    clipboard.rs       Clipboard monitor: text, images, file references, private-copy detection
    db.rs              SQLite history store, migrations, retention cleanup
    paste_target.rs    macOS: return focus to the exact window and send ⌘V
    snippets/          Text expander: engine, store, macOS keyboard hook
    ocr/               On-device text recognition (Vision / Windows OCR), benchmark
    retention.rs       Automatic history cleanup
  tests/               UI regression tests
  scripts/             Build and screenshot scripts
docs/
  ARCHITECTURE.md      Design and data flow
  PROGRESS.md          Detailed change log, verification notes and known gaps
  screenshots/         Images used in this README
release/               Built installers
```

## Status

Clipet 0.1.0 is in active development. See [docs/PROGRESS.md](docs/PROGRESS.md) for what was
verified on which platform and what is still pending.
