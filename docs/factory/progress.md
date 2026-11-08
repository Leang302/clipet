# Progress — Clipet UI redesign + Smart Snippets

Last updated: 2026-11-08. Single source of truth for status; supersedes the
earlier append-log versions of this file.

Verification legend: **Unit** = Rust `cargo test --lib`; **UI** = the repo's
headless Playwright suite (`npm run test:ui`, mocks the Tauri bridge — tests
the webview, not the OS); **Visual** = rendered screenshots inspected;
**Native** = exercised in the real packaged app on a real OS.
No item below has **Native** verification yet.

Current totals: 113 Rust tests pass · 164/164 UI checks pass · `tsc --noEmit`
clean · `npm run build` clean · 0 Rust build warnings (macOS) · Windows
cross-compile (`cargo xwin check --target x86_64-pc-windows-msvc`) clean of
new warnings.

## Status by spec part

| Part | Item | Implemented | Verified |
|---|---|---|---|
| B1 | Icon-only nav rail (Lucide), tooltips, active/hover/focus states | Yes | UI, Visual |
| B2 | Glass design: the window paints its own navy/teal (dark) or pastel (light) backdrop; panels, cards, fields and buttons are frosted glass (tint + backdrop blur + light edge). No dependence on what is behind the window; reduced-transparency fallback | Yes | UI, Visual (both themes). Replaced the earlier native-vibrancy approach, which turned muddy gray over dark desktops |
| B3 | Light/Dark/System tokens, reduced motion/transparency | Yes (pre-existing tokens extended) | Visual (light + dark) |
| B4 | History list: type, per-file-extension, pinned/favorite and selection icons → Lucide (no decorative emoji left in the UI) | Yes | UI, Visual |
| B5 | Detail panel: Paste first & primary, Delete last & destructive, Favorite/Pin with state (`aria-pressed`), icons on actions | Yes | UI, Visual |
| B6 | Search: icon, clear button, focus ring, labeled sort control (shared by history and snippets) | Yes | UI |
| B7 | Settings consolidated into one dialog (Appearance, OCR, history, recording, clear) | Yes | UI, Visual |
| C2/C3 | `{{name}}` parser, quoting/escaping, Text/Integer/URL/Date/Dropdown validation, renderer | Yes | Unit (30+ tests) |
| C4 | Editor: auto-detected parameters, per-type config, live preview | Yes | UI, Visual |
| D1 | `;killport 5173` + Tab expands inline; Space never completes a parameterized snippet | Yes (engine + existing macOS insertion path) | Unit. **Native not exercised** |
| D2 | Missing/invalid value → popup; confirm inserts into original field; Esc/click-away cancels | Yes (macOS and Windows) | Unit (engine), UI (popup window), Windows compiles. **Native not exercised on either OS** |
| D3 | Exact whitespace; no leftover delimiter; static Space snippets unchanged | Yes | Unit |
| D4 | Secure input skip, IME check at insert, double-submit guard, verify-before-replace | Yes (macOS) | Logic reviewed; **Native not exercised** |
| E | Per-OS template variants (`content` = fallback), same-parameters rule, OS-specific preview/paste | Yes | Unit, UI |
| F | 13 optional presets, never overwrite | Yes | UI, Visual. Kill Port macOS command checked on macOS (read-only half); Linux/Windows variants untested |
| G | Snippet manager: plain/parameterized filter, `{ }` badge, Test/fill-in panel, Duplicate | Yes | UI, Visual |
| H | No command execution; values never logged; raw templates can't be pasted | Yes | Unit (`render_for_paste`), code review |
| I | Migrations v3 (`parameters`) and v4 (`variants`), backward compatible | Yes | Unit (v1→, v3→ fixtures, idempotency) |
| I | Export (JSON to Downloads, never overwrites a file) / import (validated per record, never overwrites a snippet, reports skips) | Yes | Unit (round trip, rejection reasons), UI |

## How the parameter popup works (macOS) — and its safety rules

1. `;keyword` (+ any args) + Tab with a missing/invalid value →
   `Decision::Prompt` (engine). The hook consumes Tab, captures the
   frontmost app, its focused window and focused text field, delivers any
   keys typed meanwhile to that app, and opens the `prompt` window. **The
   typed invocation is not touched.** Secure input → no popup, Tab delivered.
2. Insert (Enter): values are validated again by the backend (errors keep
   the popup open). Then the existing paste handoff
   (`return_focus_to_previous_app`) returns focus, and insertion is
   **refused** unless: the same app is frontmost (no fallback to another
   app); the same text field has focus (when the app exposes it); the text
   before the caret is still exactly the invocation (when the field exposes
   its text); no secure input; no active IME. Only then is the invocation
   erased and the text inserted, through the same held-key queue and
   main-thread path as typed expansions. A refusal is shown in the popup
   ("nothing was inserted — your shortcut was left as it was").
3. Esc → focus returns to the app, nothing changes. Clicking elsewhere →
   cancel without pulling focus back (400 ms grace for macOS activation
   blips). Each request has a token; it can complete or cancel once.

**Windows** follows the same flow: it captures the foreground window and its
focused control (`GetGUIThreadInfo`), brings the window back with
`SetForegroundWindow` (confirmed within 500 ms) and inserts only if the
same control has focus. Windows has no general way to read the text before
the caret, so that third check isn't available there.

Known limitations: when an app exposes neither its focused element nor its
text (some Electron apps), only the app match is enforced (on Windows,
single-window apps like Chrome/Electron report one control, so the window
match is effectively the check). Unicode text typed inline as arguments is
not replaced (per-character erase isn't reliable for combining scripts) —
the user can enter such values in the popup instead. No password-field
detection exists on Windows (pre-existing gap).

## Product decisions made without asking (reversible)

- Parameterized snippets complete only with **Tab** (Space never), even when
  the global trigger is Space-only.
- Positional args map onto the **configured parameter order**, so per-OS
  variants with different placeholder order behave the same.
- Paste/Copy of a parameterized snippet from Clipet's own window opens the
  fill-in panel; the backend rejects pasting a raw `{{…}}` template.
- Per-OS variants must use exactly the default template's parameters.
- Popup position: centered on the current screen (not at the caret).

## Next actions

1. **User:** run the real app on macOS and walk Scenarios 1–8 (especially
   2, 3, 4, 6 in a browser field, VS Code, Terminal, TextEdit). Check
   `~/Library/Logs/com.clipflow.app/snippets.log` for `prompt:` lines if
   something misbehaves.
2. Same walkthrough on a real Windows PC (the build has only been
   cross-compiled, never run there).
