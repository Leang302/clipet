# Clipet Connect — devices, clipboard sync, mouse & keyboard sharing

Clipboard sync details (outbox, conflicts, privacy) are in `SYNC.md`; this
document covers the device layer they share and input sharing. Section 6
lists what is verified and what is not — **nothing here has run on a Mac
yet, and no two real computers have been connected.**

## 1. Starting point (repository inspection)

Already in place (branch `feat/device-sync`):

| Need | Existing code | Reused as |
|---|---|---|
| Device identity | `sync/identity.rs` — Ed25519 key in Keychain / Credential Manager | unchanged |
| Pairing | `sync/pairing.rs`, `sync/net.rs` — invitation + 6-digit check | extended (nearby + code, approval on both sides) |
| Trust | `sync_peers` table; `Gate` hook rejects unknown peers after TLS | extended with per-device grants |
| Transport | iroh 1.3 endpoint, one QUIC connection per peer, relay fallback | + a separate input connection |
| Clipboard sync | `sync/engine.rs`, `store.rs`, `journal.rs` | unchanged, now optional per device |
| Native input | `snippets/windows.rs` (`WH_KEYBOARD_LL`, `SendInput`), `snippets/macos.rs` (`CGEventTap`) | same APIs, new module |
| Permissions | `accessibility_trusted()`, `open_accessibility_settings` | reused for input sharing |

Module layout (adapted to the repo instead of a new top-level crate):

```
src-tauri/src/sync/      device identity, discovery, pairing, trust, transport, clipboard sync
src-tauri/src/input/     protocol, keymap, geometry (edges), session state, windows.rs, macos.rs
src/ConnectSettings.tsx  Settings → Clipet Connect
```

## 2. Device layer

- **Clipet Connect on/off** starts the iroh endpoint. Clipboard sync and input
  sharing are separate switches on top of it; either works without the other.
- **Per-device grants** (`sync_peers`): `allow_clipboard` (default on) and
  `allow_control` — "this device may control this computer" (default **off**).
  A grant is checked by the receiving side, so each computer decides who may
  control it.
- **Discovery.** mDNS (`iroh-mdns-address-lookup`, service `_clipet._udp`)
  finds Clipet devices on the LAN, and lets paired devices find each other's
  new LAN addresses without the internet. While Add Device is open, the
  device also announces its name so others can list it under Nearby.
- **Pairing, two ways:**
  1. *Nearby (same network):* the host shows a 6-digit pairing code; the other
     device picks it from Nearby and types the code. Both run SPAKE2 (a
     password-authenticated key exchange, `spake2` crate) with the code,
     bound to both device keys, so a device that doesn't know the code — or a
     machine in the middle — gets one guess and learns nothing. The host
     then approves the named device.
  2. *Invitation (any network):* as before (256-bit one-time secret), and now
     the joiner also confirms the verification code, so both devices approve.
- **Connections** stay as in SYNC.md; status is Online / Connecting / Offline.

## 3. Input sharing

### Roles and session

One computer is the **controller** (its keyboard and mouse are in use), the
other is **controlled**. A computer is never both. A session:

1. Controller decides to switch (cursor pushed against the arranged edge, or
   the switch hotkey). It opens (or reuses) the input connection, ALPN
   `clipet/input/1`, a QUIC connection separate from clipboard sync so a
   large image transfer never delays input.
2. `Enter{session, seq, side, fraction}` → controlled checks the grant (and asks
   locally if "Require confirmation" is on), replies `Accepted{os, screens}`
   or `Denied{reason}`.
3. Controller hides/parks its cursor and forwards: `Move{dx,dy}` (coalesced),
   `Button`, `Scroll`, `Key{hid, down, repeat}`, plus `Heartbeat` every 250 ms.
4. Ends with `Leave` (hotkey, emergency shortcut, Stop Sharing), with
   `ReturnControl{fraction}` from the controlled side (cursor pushed back
   across the shared edge), or by timeout/disconnect.

Ordering and replay: one QUIC stream per session (ordered, encrypted and
replay-protected by QUIC/TLS 1.3, no 0-RTT). Every message also carries the
session id and a strictly increasing sequence number; anything from another
session or not newer than the last one is dropped.

Stuck input: the controlled side tracks every key and button it pressed and
releases them when the session ends for any reason, including no heartbeat
for 1.5 s. The controller never switches while a local mouse button is held.

### Pointer

- The controller parks its cursor (Windows: pinned with `SetCursorPos`, events
  swallowed; macOS: hidden and detached with
  `CGAssociateMouseAndMouseCursorPosition(false)`) and sends deltas in
  device-independent units (Windows pixels ÷ monitor scale; macOS points).
- The controlled side keeps a virtual cursor in its own desktop coordinates,
  converts with its own scale, clamps to its displays, and injects **absolute**
  positions (Windows `SendInput` with `MOUSEEVENTF_ABSOLUTE|VIRTUALDESK`;
  macOS `CGEventCreateMouseEvent`), so pointer acceleration is applied once.
- macOS needs click counts for double-clicks; the injector derives them from
  timing and distance. Drags post `*MouseDragged` while a button is down.
- Scrolling is sent in wheel units (120 = one notch); "Invert scrolling" fixes
  natural-vs-classic direction differences.

### Screen edges, multiple monitors, arrangement

- Each device stores, per peer, which side that peer is on (left / right /
  above / below), set in the arrangement editor. The controller sends its side
  in `Enter`; the controlled side returns across the opposite side.
- An **edge band** is every display edge on that side with no other display
  beyond it, so the switch works on the outer edge of a multi-monitor desk and
  never on an edge between two local monitors. Positions cross as a fraction
  of the band's length, so a cursor at mid-height lands at mid-height.
- Anti-flapping: a push must continue against the edge for the configured
  delay (default 0 ms, at least two outward events), and switching back is
  ignored for 400 ms after a switch.

### Keyboard

- Keys travel as USB HID usage codes: physical positions, not characters. The
  controlled computer's own layout and IME turn them into text, so
  international layouts follow the target's input settings.
- Windows: scan codes from `WH_KEYBOARD_LL`, injected with `KEYEVENTF_SCANCODE`.
  macOS: `kVK_*` key codes from the event tap, injected with
  `CGEventCreateKeyboardEvent`, with modifier flags kept on every event.
- Modifiers: **"Ctrl acts as ⌘ on a Mac" (default on).** Controlling a Mac
  from a PC, Ctrl ↔ ⌘ (and Windows key ↔ Control), so Ctrl+C copies on the Mac.
  The reverse mapping applies Mac → PC. Alt = Option either way.
- Key repeat follows the controller's own repeat events.
- Never stored or logged. Only forwarded during an active session.

### Hotkeys (handled locally, never forwarded)

- Switch: Ctrl+Alt+Shift+arrow toward the device; the opposite arrow comes back.
- **Emergency release: Ctrl+Alt+Shift+Esc** (on a Mac keyboard,
  Control+Option+Shift+Esc) — ends the session immediately on the controller,
  and the controlled side releases every key and button.

### Platform limits

- macOS: the controller needs **Accessibility** to read and swallow input; the
  controlled Mac needs **Accessibility** to post input. Secure Input (password
  fields, some terminals) stops a Mac from *seeing* keystrokes while it is the
  controller; Clipet detects it and says so.
- Windows: Clipet can't capture or inject into windows running as administrator
  unless Clipet also runs elevated (UIPI). Ctrl+Alt+Del, the lock screen and UAC
  prompts are always local; after them, use the emergency shortcut if needed.
- Windows can't hide the pointer system-wide safely, so while a PC controls a
  Mac its own pointer stays parked (visible) in the middle of its screen.
- Clipet's hotkeys (Ctrl+Alt+Shift+Esc / arrows) belong to Clipet whenever
  input sharing is on, so other apps can't use those exact chords.
- Files can't be dragged between computers; dragging within one screen works.
- The controlled computer's screen is not streamed — you look at it.

## 4. Settings and storage

`app_settings` key `connect.input` (JSON): `enabled` (default false),
`mouse`, `keyboard`, `edge_switch`, `hotkey_switch`, `edge_delay_ms`,
`require_confirmation` (default true), `swap_ctrl_cmd` (default true),
`invert_scroll`, plus per-peer `side` in `sync_peers.side`. Clipboard sync keeps
`sync.settings` (now with its own `clipboard` switch).

## 5. Security summary

- Unknown devices are rejected at the TLS layer; input connections also need
  the `allow_control` grant **on the controlled computer**.
- Input sharing is off by default; confirmation per session is on by default.
- Revoking a device closes its connections and blocks new ones; it can't erase
  clipboard items the device already received.
- No keystrokes or clipboard content are logged; logs carry event counts and
  short device ids only.
- Internet (relay) connections work for clipboard sync. Input sharing is
  allowed only over a direct connection by default ("Allow over the
  internet" is off) because relayed latency isn't interactive.

## 6. Status

### Done and verified on Windows 11 (this machine)

| Area | How it was verified |
|---|---|
| Pairing by invitation, with approval on **both** devices | e2e test over real iroh (loopback) |
| Pairing a **nearby** device by its 6-digit code (mDNS + SPAKE2) | e2e test, and on the real network: discovery by name, code pairing, sync through n0, reconnect by key |
| Wrong code: one guess, then Add Device closes | e2e test |
| Per-device grants (clipboard / control) and revocation | e2e tests (sync and input) |
| Clipboard sync on/off independent of input sharing | e2e (presence-only sessions) |
| Input protocol, sequence/replay checks, heartbeat watchdog | unit + e2e tests with a mock OS layer |
| Edge switching, return at the same relative height, multi-monitor edge bands, different resolutions and DPI | unit tests (geometry) + e2e |
| Ctrl↔⌘ mapping; keys, clicks and scrolling forwarded | e2e |
| Emergency shortcut, Stop Sharing from either side, stuck-key release on disconnect | e2e |
| Confirmation prompt on the controlled computer | e2e |
| Windows OS layer: real display list with per-monitor DPI, hooks install, `SendInput` absolute mapping exact to the pixel | opt-in test on this PC (three monitors, 125% laptop) |
| Settings → Clipet Connect (devices, grants, arrangement drag/select, input and clipboard options, nearby join, both pairing paths) | headless UI regression suite with a mocked backend |

### Written and type-checked for macOS, never run

`input/macos.rs` (event-tap capture, `CGEventPost` injection, click counts,
modifier flags, cursor hide/detach) and every other macOS path were
**type-checked for `aarch64-apple-darwin` from Windows** (Zig supplies the
macOS headers). Nothing was linked or executed; no macOS build exists yet.

### Not verified at all (needs two real computers)

Everything a person feels: latency, smoothness, scroll speed and direction,
double-click timing on macOS, shortcuts in real apps, IME and international
layouts, Windows capture while someone types for real, the banner never
taking focus, firewall / Local Network / Accessibility prompts, and every
Windows ↔ macOS combination.

### Not implemented

- QR codes (pairing uses the nearby code or a pasted invitation instead).
- "Sync on metered networks" (metered connections aren't detected).
- File drag-and-drop between computers; file items in the clipboard.
- Switching while a mouse button is held (blocked on purpose).

## 7. Permissions and setup

- **Windows:** nothing to grant. Windows Defender Firewall may ask about
  network access the first time; allow private networks (needed for direct
  LAN connections; relayed connections work without it). To control
  administrator windows, Clipet itself must run as administrator.
- **macOS:** System Settings → Privacy & Security → **Accessibility** → allow
  Clipet (needed both to control and to be controlled), then turn "Share mouse
  and keyboard" off and on. macOS 15+ also asks for **Local Network** access;
  allow it, or Nearby and direct connections won't work. Unsigned builds:
  right-click → Open the first time.

## 8. Manual test guide (Windows PC + Mac)

Setup on both: install, Settings → Clipet Connect → turn on. On the Mac,
grant Accessibility and Local Network.

1. **Nearby pairing.** PC: Add Device. Mac: Join a Device → pick the PC under
   Nearby → type the code. PC: Approve. Both list each other as Online.
2. **Invitation pairing (another network).** Remove the device on both, put
   the Mac on a phone hotspot, pair with the invitation; both screens show the
   same code; click "Codes match" on the Mac and Approve on the PC.
3. **Grants and arrangement.** On the Mac, turn on "Can control this computer"
   for the PC. On both, turn on "Share mouse and keyboard". On the PC, drag the
   Mac to the side where it physically sits.
4. **PC → Mac at the edge.** Push the pointer off that edge; the Mac asks
   (Allow); the pointer appears on the Mac at the same height. Move, click,
   double-click a file in Finder, right-click, drag a window, scroll in Safari
   (wrong direction? turn on "Reverse scrolling").
5. **Keyboard.** Type in TextEdit: letters, numbers, symbols, Enter, arrows,
   F-keys, key repeat. Ctrl+C / Ctrl+V on the PC keyboard copy and paste on the
   Mac. Alt+arrow acts as Option+arrow.
6. **Back.** Push across the shared edge on the Mac: control returns, with the
   pointer at the same height on the PC.
7. **Hotkeys.** Ctrl+Alt+Shift+→ switches to the Mac; ← returns.
8. **Emergency.** While on the Mac, hold Shift and press Ctrl+Alt+Shift+Esc:
   control returns at once, and Shift isn't stuck on either computer.
9. **Disconnect.** While on the Mac with a key held, turn off the PC's Wi-Fi:
   within about 2 seconds the Mac releases everything, and the PC keyboard
   works locally.
10. **Mac → PC.** Repeat 3–9 the other way (arrange the PC on the Mac, allow
    control on the PC). ⌘C / ⌘V must act as Ctrl+C / Ctrl+V on the PC.
11. **Monitors and scaling.** On the PC (three monitors, 125% on the laptop),
    only the outer edge switches; the edge between two local monitors never
    does.
12. **Clipboard during sharing.** Copy on the Mac while controlling it, come
    back, paste on the PC from Clipet's history.
13. **Revocation.** Turn off "Can control this computer" mid-session: control
    returns immediately; a new attempt is refused with an explanation.
14. **Restricted contexts.** Mac password field (Secure Input): the Mac can't
    forward typing as controller, and Clipet says so. PC elevated app or UAC
    prompt: nothing is injected there.
15. **Latency.** With "Allow over the internet" on and a relayed connection,
    note the lag; with it off, a relayed session is refused with a clear
    message.

## 9. Remaining risks

- Unrun macOS code: event-tap details (Secure Input, flags, Caps Lock
  injection not toggling the light), double-click counts, focus behaviour of
  the non-focusable banner, cursor hide/show balance after a crash.
- A Windows controller must never get stuck "remote". The emergency shortcut,
  Stop Sharing, the 1.5 s watchdog on the other side and a disconnect all
  return control; the secure desktop (Ctrl+Alt+Del) is invisible to hooks, so
  the emergency shortcut covers it afterwards.
- Pointer feel (speed across DPI levels, macOS acceleration) needs tuning on
  real hardware.
- mDNS is blocked on some networks (guest Wi-Fi, client isolation); the
  invitation path still works there.
