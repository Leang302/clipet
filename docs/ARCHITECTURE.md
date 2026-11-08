# Clipet — Architecture

See `docs/PROGRESS.md` for current status and what's actually been verified vs. just written.

## Stack

- **Shell / native layer:** Tauri 2 (Rust). Chosen over Electron for idle memory/CPU footprint (spec §18 targets) and because the background-first model (no main window, tray/menu bar only, spec §3) maps directly onto Tauri's window-is-optional architecture.
- **Frontend:** React 19 + TypeScript, built with Vite. Renders the popup, settings window, screenshot notification, and annotation editor — each a small, purpose-built view, never a persistent dashboard (spec §3).
- **Storage:** SQLite via `rusqlite` (bundled, no system SQLite dependency) for item metadata; a managed `images/` directory under the OS app-data dir for image/screenshot binaries. Thumbnails are not pre-generated yet (deferred — see Known gaps).

## Process model

One Rust process, one (initially hidden) webview window labeled `history`, plus windows added later for the screenshot notification and annotation editor. A background `std::thread` runs the clipboard monitor for the life of the process; it is not tied to any window being open.

```
┌─────────────────────────────────────────────┐
│ Tauri process (Rust)                         │
│                                               │
│  ┌──────────────┐   polls every 400ms        │
│  │ clipboard.rs │──────────────┐              │
│  │ (bg thread)  │              ▼              │
│  └──────────────┘        system clipboard     │
│         │ writes                              │
│         ▼                                     │
│  ┌──────────────┐        ┌─────────────────┐ │
│  │   db.rs      │◄──────►│  Tauri commands │◄┼── webview (React)
│  │  (SQLite)    │        │   (lib.rs)      │ │
│  └──────────────┘        └─────────────────┘ │
│                                 │              │
│                           emits "clipflow://*"│
│                           events to frontend   │
│  ┌──────────────┐                             │
│  │  tray icon   │── menu clicks → same        │
│  │  + menu      │   command/event path         │
│  └──────────────┘                             │
└─────────────────────────────────────────────┘
```

## Clipboard monitoring

`app/src-tauri/src/clipboard.rs`. A background thread owns one `arboard::Clipboard` handle and polls it every 400ms (polling, not OS change-notification, because no cross-platform "clipboard changed" OS hook exists that Tauri exposes directly — this is the same approach used by essentially every cross-platform clipboard-history tool). Each tick:

1. If paused (tray toggle), skip.
2. Try `get_text()`. If present and its SHA-256 differs from the last-seen hash **and** from `last_self_write_hash` (set whenever Clipet itself writes to the clipboard, e.g. during paste), classify as `link` if it looks like a bare URL, else `text`, and insert a row.
3. Otherwise try `get_image()`. If present and new, decode via the `image` crate, save as PNG under `images/<id>.png`, insert a row with `content` = filename.

**Known gap, called out explicitly rather than hidden:** Clipet's monitor currently asks `arboard` only for plain text and raw RGBA images. It does **not yet ingest** file references (`public.file-url` / `CF_HDROP`), HTML, or RTF, even though the installed `arboard` version can read native file lists. So "copy a file in Finder/Explorer" and "copy rich text from Word" are not yet captured — the spec requires these (§4). Closing the rich-format gap needs monitor integration plus direct platform code where the abstraction does not expose a format:
- **macOS:** `NSPasteboard` via `objc2-app-kit` to read `public.file-url`, `public.html`, `public.rtf` pasteboard types.
- **Windows:** `CF_HDROP` (file lists) and `CF_HTML`/`CF_RTF` via `windows-rs` clipboard APIs.

This is scoped as follow-up native work, not a design decision to skip these formats permanently.

## Paste handoff and pacing

Paste preparation runs on a blocking worker rather than Tauri's UI thread, and a backend mutex prevents overlapping clipboard writes/keystrokes. On macOS, Clipet records the frontmost application before opening the popup, explicitly reactivates that process, and polls the real frontmost-process state at 5ms intervals. A normal text paste therefore proceeds as soon as focus is confirmed instead of always paying the old 180ms delay. Other platforms retain a short 60ms handoff fallback until an equivalent foreground-process check is implemented.

Single items are fully prepared before Clipet hides. Mixed text/image selections still use sequential transfer because they cannot be represented as one homogeneous native batch; those payloads are preloaded and use content-aware pacing (short for text, longer for decoded images). Errors after a partial mixed transfer report the completed count instead of claiming full success.

## Background lifecycle & tray

`app/src-tauri/src/lib.rs`. On `setup`:
- macOS: `app.set_activation_policy(Accessory)` to hide the Dock icon.
- Builds the tray icon + menu (Open History, Capture Screenshot, Pause/Resume, Settings, Storage Usage, Quit).
- Registers the global shortcut (⌘/Ctrl+Shift+V) via `tauri-plugin-global-shortcut`, toggling the `history` window.
- Registers `tauri-plugin-autostart` for "start at login" (spec §15) — wiring exists; the settings-window toggle to call its enable/disable commands is still to be connected to the UI.
- The `history` window hides itself on focus loss instead of staying around, keeping with "no persistent window" (spec §3).

## Frontend ↔ backend contract

Tauri `invoke` commands (see `lib.rs`): `get_history`, `get_paused`/`set_paused`, `storage_usage_mb`, `hide_history`, plus the item-mutation commands added alongside the real UI port (`toggle_favorite`, `toggle_pinned`, `delete_item`, `clear_history`, `paste_item`). Events pushed from Rust to the frontend: `clipflow://new-item` (a row was inserted — frontend should refetch or prepend), `clipflow://history-opened` (global shortcut or tray fired), `clipflow://capture-requested`, `clipflow://open-settings`.

## Multi-image transfer (spec §11)

An all-image selection is validated and transferred in one clipboard generation and one synthesized paste event, eliminating the race where sequential writes replaced image 1 before the destination had consumed it.

- **macOS:** one ordered `NSPasteboardItem` per image. Every item exposes both its exact PNG bytes (`public.png`) and canonical file URL (`public.file-url`), covering bitmap-oriented editors and attachment/file-oriented apps without guessing a single representation. Clipet reads the batch back and verifies every item, representation, and order before hiding the popup.
- **Other platforms:** `arboard` writes and verifies one native file list (including `CF_HDROP` on Windows) in selection order.
- **Mixed selections:** retain the fully-preloaded sequential fallback described above.

Destination applications still decide which pasteboard items and formats they accept, so the compatibility matrix across Word, Slack, Mail, Finder/Explorer, Photoshop, and the user's actual target apps remains a required interactive check. Export/combine/contact-sheet and drag-and-drop remain future explicit fallbacks.

## Smart Snippets

- `src-tauri/src/snippets/engine.rs` is the platform-independent core. It tracks the text left of the caret and decides Pass / Expand / Escape / Prompt, synchronously, when Space or Tab arrives. Static snippets match the trailing word exactly as before; a snippet with parameters completes only on Tab, either from inline args (`;killport 5173`, found by a bounded backward scan) or by asking for missing values (`Prompt`). It is fully unit-tested.
- `snippets/params.rs` is pure parameter logic: `{{name}}` parsing (with `\{{literal}}` escapes), argument tokenizing (quotes, `\"`, `\\`), per-type validation (text/integer/url/date/dropdown) and literal-substitution rendering. It never evaluates or executes anything.
- `snippets/store.rs` keeps snippets and their settings in a **separate `snippets.sqlite3`**, so no history, cache or image cleanup can delete them. Schema changes go through `PRAGMA user_version` migrations (v3 adds `parameters`, v4 adds per-OS `variants`; old rows read back as static, single-template snippets).
- `snippets/mod.rs` (`SnippetService`) holds the live index (each snippet's template for the current OS), swapped atomically on every change. It also owns the pause state (an atomic wall-clock deadline, a timer thread for the tray, and a generation counter so the hook drops half-typed words), plus the Tauri commands. `snippet_save` rejects parameter configs that don't match the template's placeholders, and variants that don't use the same parameters.
- `snippets/prompt.rs` runs the missing-parameter popup (`prompt` window): one pending request at a time, identified by a token so it completes or cancels once. See `docs/factory/progress.md` for the insertion safety rules.
- `snippets/macos.rs` is the native layer: a session `CGEventTap` thread, a replacement worker, and main-thread injection via enigo. Injected events carry `EVENT_SOURCE_USER_DATA` marks so the tap ignores them. Keys typed during a replacement are held and replayed in order. A popup confirmation reuses the paste handoff (`return_focus_to_previous_app`) and inserts only after re-confirming the app, the focused field and the text before the caret.
- `snippets/windows.rs` is the `WH_KEYBOARD_LL` hook behind the same interface. It expands static and inline-argument snippets and opens the parameter popup; a confirmation re-activates the captured window and inserts only if the same control still has focus (Windows can't read the text before the caret in general).
- `snippets/transfer.rs` exports text snippets to a versioned JSON file and imports such files, validating every record with the editor's rules and never overwriting an existing snippet.

## Known gaps (tracked, not hidden)

- No file/HTML/RTF clipboard capture yet (see above).
- No native screenshot capture yet (spec §7) — needs macOS (`CGDisplayCreateImage` / `screencapture`-equivalent) and Windows (GDI) specific code, plus Screen Recording permission handling on macOS.
- No thumbnail pre-generation — the frontend currently loads full images; fine at prototype scale, needs revisiting against the spec's "avoid loading all image binaries into memory" target (§12/§18) once image volume grows.
- Mixed text/image transfer still needs payload-aware sequential pacing because there is no portable destination-consumption acknowledgement.
- Native multi-image acceptance is destination-specific and still needs interactive compatibility testing; Clipet can verify the clipboard payload, not what a third-party app chooses to consume.
- Windows has never been built or run — everything here is written against documented cross-platform Tauri/Rust APIs but is unverified on Windows until run there.
