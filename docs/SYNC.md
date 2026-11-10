# Clipet — Device Sync

Status: Phases 1–3 implemented; Phase 4 automated tests pass on Windows (see
§9). **Sync between a real Windows PC and a real Mac, and across two different
networks, has not been verified yet.** The macOS build has not been compiled
or run since sync was added.

## 1. What exists today (inspection summary)

| Area | Where | Notes for sync |
|---|---|---|
| Capture | `clipboard.rs` — a thread polls the clipboard every 400 ms | Text/links are always inserted as new rows; images and file lists are deduplicated by hash (a repeat copy moves the old row to the top via `touch_item`). Clipboards marked private by the source app (`org.nspasteboard.ConcealedType` / `TransientType` on macOS, `ExcludeClipboardContentFromMonitorProcessing` on Windows) are never recorded. There is no per-app exclusion list. |
| Storage | `db.rs` — `clipflow.sqlite3`, table `clip_items`, plus `app_settings` (key/value) | Items have a locally generated `id` (hex nanoseconds), `kind` (`text`, `link`, `code`, `image`, `screenshot`, `file`), `content` (text, or an image file name under `images/`), `content_hash` (SHA-256 of the text, or of the image's RGBA pixels). Additive migrations via `ALTER TABLE … ADD COLUMN` that ignore "already exists". |
| Mutations | `lib.rs` Tauri commands | `toggle_pinned`, `toggle_favorite`, `delete_item`, `clear_non_favorites`, `clear_history`, `update_item_text`, `save_annotated_image(_as_new)`. |
| Cleanup | `retention.rs` | Optional age-based expiry, run hourly; may spare favorites/pinned. |
| Snippets | `snippets/` — separate `snippets.sqlite3` | Untouched by sync. |
| UI | `App.tsx`, `HistorySettings.tsx` (the Settings dialog) | Backend pushes `clipflow://new-item` / `clipflow://item-updated`; the list refetches. |
| Runtime | Tauri 2's tokio runtime | iroh runs on it directly. |

## 2. Architecture

```
clipboard.rs / lib.rs commands          (existing capture + mutations)
        │  after writing clip_items, in the same connection
        ▼
sync/journal ── sync_events (outbox: one row per item, newest change) ──┐
        │ notify (watch channel)                                         │
        ▼                                                                │
sync/net (iroh)                                                          │
  accept loop ── hook: reject non-trusted peers after the TLS handshake  │
  peer tasks ─── dial each trusted device, back off, keep one connection │
  session ────── per connection: serve my log ▲ / pull theirs ▼          │
        │                                                                │
sync/wire  (length-prefixed frames over any AsyncRead/AsyncWrite) ◄──────┘
        │
sync/engine (pure: build outgoing item state, apply incoming state,
             conflict rules, filters) ── writes clip_items + images/
sync/store  (peers, cursors, tombstones, aliases, settings)
sync/pairing (invites, verification codes, pairing handshake)
sync/identity (device key in the OS keychain, device name)
sync/mod.rs  (SyncService: lifecycle, status, Tauri commands)
```

Boundaries:

- **Capture → journal.** The capture and mutation code calls `journal::put(item)`
  or `journal::del(item)` after its own write. That is the only coupling; the
  journal is a no-op while sync is off.
- **Journal → transport.** The journal is a local, restart-safe outbox in the
  same SQLite file (no second history database).
- **Engine and wire know nothing about iroh.** A session is "two byte streams
  to an authenticated peer id". A future transport (for example an encrypted
  relay through a user's own cloud folder) can reuse the engine and the frames.
- **Transport → UI.** `SyncService` owns the iroh endpoint and emits
  `clipet://sync-status`; the React page calls `sync_*` commands.

### Event log (outbox) and cursors

`sync_events(seq INTEGER PRIMARY KEY AUTOINCREMENT, item_id, op, at)` holds
**this device's own changes only**. `op` is `put` (the item exists or changed:
created, pinned, edited, re-copied) or `del`. A new change for an item replaces
that item's older events, so the log holds at most one row per item plus
deletions. Outgoing item state is read from `clip_items` **when it is sent**,
so the log never stores a second copy of clipboard content, and an item
removed by clearing or retention simply isn't sent.

Each trusted peer row stores `cursor`: the highest `seq` of **that peer's**
log this device has applied. On every connection each side asks the other for
events after its cursor and then keeps the stream open for live changes. Each
event is applied and the cursor advanced in one SQLite transaction, so a crash
or a dropped connection resumes exactly where it stopped. Nothing is "sent
and forgotten": a device that was offline for three hours just asks for
everything after its cursor.

Sync is **pairwise**: devices exchange only their own changes and never relay
another device's. Every pair of devices that should share history is paired
with each other. This keeps the trust model simple: data only ever comes from
a device you approved.

### Connections

- iroh `Endpoint` with the `N0` preset: QUIC, NAT traversal (hole punching),
  relay fallback, and address lookup by public key.
- Two ALPNs: `clipet/sync/1` and `clipet/pair/1`. An `after_handshake` hook
  closes any sync connection whose authenticated endpoint id is not a trusted
  device, and any pairing connection while no invitation is open, before any
  Clipet code reads from it.
- Each trusted device gets a peer task that dials it, with exponential back-off
  (2 s → 5 min), reset by "Sync Now". Both sides dial; if two connections end
  up open, the one dialed by the device with the smaller id is kept, so both
  sides make the same choice.
- A session opens one bidirectional stream per direction:
  `Request{cursor, prefs}` → `Event…` frames, `CaughtUp`, then live events and
  a heartbeat every 20 s.

## 3. Database changes (all additive, in `clipflow.sqlite3`)

- `clip_items`: `origin TEXT NOT NULL DEFAULT ''` (endpoint id of the device the
  item came from; empty for local items), `pinned_at INTEGER NOT NULL DEFAULT 0`,
  `edited_at INTEGER NOT NULL DEFAULT 0` (last-writer-wins clocks).
- `sync_events` — outbox, described above.
- `sync_peers(id PRIMARY KEY, name, added_at, cursor, last_seen, last_sync)`.
- `sync_tombstones(id PRIMARY KEY, at)` — deleted item ids (kept 180 days).
- `sync_aliases(remote_id PRIMARY KEY, local_id)` — a remote item that matched an
  existing local item by content.
- Settings live in `app_settings` under `sync.settings` and `sync.device_name`.

## 4. Pairing and trust

1. Device A: **Add Device** creates an invitation:
   `clipet-invite:` + base64url(JSON{version, A's endpoint address, A's name,
   32 random bytes, expiry}). It is valid for 10 minutes and for a single
   attempt. It is shared out of band (copy/paste via any channel).
2. Device B: **Join** with the invitation. B dials **A's public key** (so B is
   talking to A, authenticated by TLS), and sends the 32-byte secret and its
   name. A checks the secret in constant time and closes the invitation,
   whatever the result.
3. Both screens show a 6-digit **verification code** derived from
   SHA-256(`clipet-pair-v1` ‖ A's id ‖ B's id ‖ secret). The user compares them
   and approves on A. Only then does A store B as trusted and tell B, which
   stores A.

The security credential is the 256-bit secret delivered with A's public key,
over a channel authenticated to that key. The 6-digit code is only a human
cross-check (it catches an invitation that leaked and was used by someone
else first: the name and code on A won't match B's).

**Keys.** Each device's identity is an Ed25519 key (iroh `SecretKey`). It is
stored in the OS credential store via `keyring`: Keychain on macOS, Credential
Manager on Windows. If the store is unavailable, the key is kept in
`sync_identity.key` in the app data folder and the Device Sync page says so.
Trusted devices are public keys in `sync_peers`. They are not secret, but
anyone who can write to your user profile could add one. Such a person can
already read your clipboard history directly.

**Removing a device** deletes it from `sync_peers` and closes its connection;
the hook rejects it from then on, in both directions. Removal **cannot** delete
anything the other device already received. That stays in its history until
deleted there. Remove the device on both sides so neither keeps retrying.

## 5. Sync behavior and conflict policy

What a `put` carries: id, kind, content (text) or PNG bytes (images),
preview, created_at, pinned + pinned_at, edited + edited_at, content hash.

Applying an incoming `put` for item *X*:

1. **Deleted wins.** If *X* (or the local item it aliases) has a tombstone, the
   event is ignored. A deleted item can't come back from a stale or repeated
   event.
2. **Find the local item:** same id → that item; else an alias → its target;
   else an item of the same kind group and content hash → record an alias
   *X* → that item (duplicate content isn't stored twice); else insert *X* as
   new, with `origin` = the sender.
3. **Merge:**
   - `created_at` = the later of the two (a re-copy moves it to the top on both).
   - Pinned state: last-writer-wins on `pinned_at`; on a tie the larger
     endpoint id wins (deterministic on both sides).
   - Content (edited text, annotated images): last-writer-wins on
     `edited_at`, same tie-break.
4. Retention: a new item already older than this device's retention period
   is skipped. History limits apply as usual afterwards.

Applying a `del`: delete the local item (and its image files) and record
tombstones for both the remote id and the resolved local id.

Ordering and repeats: events from one peer arrive in `seq` order on one
stream; anything at or below the cursor is skipped, and applying the same
`put` twice gives the same result, so replays after a crash are harmless.
Loops can't happen: received items never enter the local outbox.

Deliberately **local-only** (not propagated): Clear, Clear All, retention
cleanup, favorites, usage counts, OCR text (each device runs its own OCR),
file-list items (paths mean nothing on another machine).

## 6. Privacy defaults and settings

- Sync is **off** until enabled. Turning it on queues the existing history
  (filtered by the settings below) for devices you pair; nothing leaves the
  device before a pairing is approved.
- Defaults: text and links **on**; images **off**; pinned state **on**;
  maximum item size 1 MB (text) / 10 MB (images); the hard limit is 25 MB.
- Filters are applied by the **sender at send time** (turning a type off stops
  it immediately, including queued items) and the sender also honors the
  receiver's preferences. Items skipped this way are not re-sent if the setting
  is later turned on.
- **Pause** stops all sync traffic but keeps recording local changes, so
  everything catches up on resume. **Disable** stops the network entirely and
  stops recording.
- Private clipboard content is never recorded, so it is never synced.
- Text that is a Clipet pairing invitation (`clipet-invite:…`) is never sent:
  it is a one-time credential. Copying an invitation from the Device Sync page
  doesn't record it in history at all.
- Logs contain item counts, sizes and short device ids. They never contain
  clipboard content or keys.
- Not implemented: "sync on metered networks". Clipet does not detect metered
  connections yet, so sync ignores them.

## 7. Third-party infrastructure

With the default `N0` preset, Clipet uses services run by number 0 (n0), the
iroh maintainers:

- **Relays** `use1-1`, `usw1-1`, `euc1-1`, `aps1-1` `.relay.n0.iroh.link` —
  forward encrypted QUIC packets when no direct path exists, and help
  hole punching. They can see device public keys, IP addresses, timing and
  traffic volume, not content (end-to-end encrypted with each device's key).
  n0 describes the public relays as free, rate-limited, without uptime
  guarantees, and suitable for development and testing.
- **Address lookup** `dns.iroh.link` (pkarr): each device publishes a signed
  record of its current relay and addresses under its public key, so paired
  devices find each other after IP changes. This reveals that a key is online
  and its addresses.

No Clipet-run server and no account. Restrictive networks that block UDP and
the relays' HTTPS can prevent both direct and relayed connections; sync then
waits and catches up later. Self-hosted relays (`iroh-relay`) can be added
later via `RelayMode::Custom` without changing the sync engine.

## 8. Technical risks

- Public n0 relays are best-effort; heavy image sync over a relay may be
  rate-limited.
- Clock skew affects last-writer-wins ordering of pins/edits between devices
  with badly wrong clocks (the outcome is still identical on both).
- macOS: the Keychain may ask for permission after every unsigned rebuild.
- Initial sync of a large image history can take a while; it is resumable.
- First-run OS prompts: Windows Defender Firewall may ask to allow Clipet on
  private/public networks (declining blocks direct LAN connections; relayed
  sync still works), and macOS 15+ asks for Local Network access for the same
  reason.

## 9. Verification status

### Automated — passing on Windows 11 (`cargo test`, `npm run test:ui`)

Rust (`src-tauri/src/sync/`):

- `engine`: text/link arrive with kind, time and origin; received items never
  loop back; duplicate content merges via aliases; replayed events are no-ops;
  deletes win and stale puts can't resurrect; aliased deletes; concurrent pin
  changes converge; equal-time pin ties resolve identically on both sides;
  legacy pins are never undone by a zero clock; later edits win; retention
  skip; sender and receiver filters; pin sync off; image transfer with pixel
  hash check; corrupt/mismatched/truncated images rejected; hostile ids
  rejected; 20-item offline catch-up in order.
- `wire`: round trips, interrupted image = error (never applied), oversized and
  malformed frames rejected before allocating.
- `pairing`: invite round trip (incl. line breaks from chat apps), damaged and
  newer-version invites, verification codes bind both keys and the secret.
- `store`: outbox compaction, tombstones, backfill, pruning, cursors never
  rewind, conservative defaults.
- End-to-end over real iroh endpoints on loopback (`sync::tests`): pairing with
  matching codes; live sync both ways; 20 items while the other device is
  down, delivered after it restarts; an image; a delete; revoked device
  rejected (including a dial already in flight at removal); tampered
  invitation refused and the invite closed after one attempt; declined pairing
  trusts nobody; an unpaired device gets nothing from the sync protocol; pause
  stops traffic, paused devices refuse connections, resume catches up.
  The suite was run repeatedly to check for timing flakiness.
- Opt-in (`cargo test -- --ignored`): pairing and sync through n0's relays and
  address lookup on the real internet, including reconnecting by key alone
  after a restart with stored addresses wiped (passes; both endpoints were on
  one machine). Windows Credential Manager stores and returns a device key.

UI (`tests/ui-regression.cjs`, mocked backend): the Settings row, enabling,
images off by default, invitation with expiry, private copy of the invitation,
approval with the code, device listed online, remove confirmation, Esc back.

### Manual, two real devices — not done yet

Before each run: install the build on both machines, open Settings → Device
Sync, turn sync on. Allow the firewall / Local Network prompts.

1. Pair: Add Device on the Mac, copy the invitation to the PC (any chat or
   email), Join a Device on the PC. Both show the same 6-digit code; approve on
   the Mac. Both list each other as Online.
2. Windows → macOS: copy text on the PC; it appears in the Mac's Clipet history
   within a few seconds and the Mac's clipboard is unchanged.
3. macOS → Windows: same in reverse, with a link.
4. Same Wi-Fi, then different networks (put one device on a phone hotspot);
   repeat 2–3.
5. Offline catch-up: quit Clipet on the Mac, copy 20 items on the PC, wait,
   start Clipet on the Mac: all 20 arrive in order.
6. Restart recovery: turn image sync on (both sides), copy a large screenshot,
   quit the receiver mid-transfer, restart it: the image arrives once, intact.
7. Duplicates: copy the same text on both devices: one entry on each side.
8. Pins: with the PC's Clipet quit, pin an item on the Mac; a minute later
   start the PC and unpin the same item there before the Mac reconnects
   (pause sync on the Mac meanwhile). Resume: the later change (unpinned)
   wins on both.
9. Delete on the PC: gone on the Mac, and doesn't come back after restarts.
10. Remove the Mac on the PC: the Mac shows the PC as offline with an error;
    the Mac's new copies don't reach the PC.
11. Turn images off on the PC: its screenshots stay local, text still syncs.
12. Copy a password from a password manager: it is not recorded, not synced.
13. Pause on one device: the other shows it offline; resume: it catches up.
