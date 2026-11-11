# Snippet expansion: how matching works

Clipet expands `;keyword` + Space/Tab no matter what order the text was typed in.
For example: type `name`, press Home, type `;`, press End, press Space.

## How it decides

1. **Keystroke buffer (fast path).** The keyboard hook remembers what you typed
   since the last caret move, in memory only. If you type `;name` in one go, this
   decides immediately, as before.
2. **Field check (cursor-aware path).** After a caret move (arrows, Home/End,
   click) or a paste, cut, undo or redo, keystrokes can't tell what sits before
   the caret. When a prefix was typed or pasted since the last trigger, Clipet
   swallows the Space/Tab and reads **at most 256 characters before the caret
   and 1 after it** from the focused field. It then matches that text with the
   same rules:
   - Windows: UI Automation `TextPattern`. If that's missing, a standard
     Edit/RichEdit control is read with `EM_GETSEL`/`WM_GETTEXT`.
   - macOS: Accessibility `AXSelectedTextRange` and `AXStringForRange`.
3. **Verification before erasing.** Every replacement is also checked against
   the field when the field can be read:
   - password fields and fields with selected text never expand;
   - a shortcut glued to earlier text (`abc;name`) doesn't expand;
   - the caret inside a word (`;na|me`, `;name|x`) doesn't expand.
4. **Fallback.** If the field can't be read, Clipet keeps the old behaviour.
   It only expands what the keystrokes saw typed, and it never erases based on
   a guess.

Text read from a field is used for one decision and then dropped. It is never
stored or logged. `snippets.log` records only the outcome, for example
`field read in 41ms -> expand` or `field unavailable: no text pattern`.

**Undo** (Settings → "Backspace right after an expansion brings back ;name",
off by default): pressing Backspace as the very next key after an expansion
restores the typed shortcut. It's offered only for single-line Latin text,
where one Backspace per character is exact.

## Compatibility

| Where | Field check | Notes |
|---|---|---|
| Windows: standard Edit/RichEdit controls (classic dialogs, WinForms, WordPad) | Yes | Verified live (WinForms TextBox) |
| Windows: Notepad (Windows 11), WPF/UWP/WinUI text boxes | Expected (UIA TextPattern) | Not yet verified live |
| Chrome / Edge text inputs (Windows & macOS) | Expected | Chromium may enable its accessibility tree the first time it's queried; not yet verified live |
| VS Code, Electron chat apps (Slack, Discord, Teams…) | Often partial or unavailable | Falls back to keystrokes; out-of-order edits may not expand |
| Telegram Desktop (Qt) | Unknown | Not verified |
| Terminals (Windows Terminal, consoles, iTerm) | Unavailable | Keystroke buffer only |
| macOS native text fields (Notes, TextEdit, Safari) | Expected (AX) | Code type-checked only; not run on a Mac |
| Password fields | Never expand | Windows: UIA `IsPassword` / `ES_PASSWORD`. macOS: secure input / `AXSecureTextField` |

## Manual test steps

Create a snippet `;name` → `Keo Kimleang` (insertion: exact). Test in a plain
text field (Windows: Notepad or the Run dialog. macOS: TextEdit or Notes),
then Chrome, VS Code and a chat app.

| # | Steps | Expected |
|---|---|---|
| 1 | Type `;name`, Space | `Keo Kimleang` |
| 2 | Type `name`, Home, `;`, End, Space | `Keo Kimleang`, with nothing left of `;name` and no extra space |
| 3 | Type `hello ;name`, Space | `hello Keo Kimleang` |
| 4 | Type `;nme`, Left ×2, `a`, End, Space | `Keo Kimleang` |
| 5 | Type `;namx`, Backspace, `e`, Space | `Keo Kimleang` |
| 6 | Copy `;name`, paste it, Space | `Keo Kimleang` |
| 7 | Type `;unknown`, Space | unchanged, plus a space |
| 8 | Type `abc;name`, Space | unchanged |
| 9 | Type `;name`, Left ×2, Space | `;na me` |
| 10 | Type `;name ;name` in one go (Space after each) | both expand |
| 11 | In a password field: test 1 and test 2 | never expands |
| 12 | Turn on undo; type `;name`, Space, wait a moment, Backspace | `;name ` comes back |
| 13 | Japanese/Korean/Chinese IME in native mode: test 1 | no expansion while the IME is active |
| 14 | Trigger mode "Tab": tests 1–2 with Tab | expands; Space does nothing special |

Windows automated live check (standard Win32 text box). It builds a separate
test app identity, so your real snippets and history aren't touched. **Don't
use the keyboard or mouse while it runs.** The script stops if its window
loses focus.

```powershell
# from app/src-tauri, with no other Clipet running
$env:TAURI_CONFIG = '{"identifier":"com.clipet.snippetuxtest","productName":"ClipetSnippetTest"}'
cargo build
.\target\debug\app.exe            # run once so it creates its data folder, then quit it
python ..\tests\snippet-expansion-seed.py undo-on
.\target\debug\app.exe            # start it again, leave it running
powershell -STA -File ..\tests\snippet-expansion-live.ps1 -Out results.txt
```

## Known limitations

- Out-of-order edits expand only where the field can be read (see the table).
  Elsewhere Clipet behaves as before.
- Reading the field takes about 3–60 ms. That cost applies only when the
  keystrokes can't decide, and to the swallowed key of an expansion, never to
  ordinary typing.
- Undo applies only if Backspace arrives after the expansion has finished
  (human typing speed). A Backspace pressed within a few milliseconds is
  delivered normally.
- Pasting with a mouse context menu isn't seen as a paste. A click followed by
  typing still works.
- Clipet has no per-app exclusion list for snippets yet; Clipet's own windows
  are always excluded.
