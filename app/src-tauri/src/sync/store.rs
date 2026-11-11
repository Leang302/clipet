//! Sync bookkeeping in `clipflow.sqlite3`: settings, trusted devices and
//! their cursors, tombstones, aliases, and this device's change log (outbox).
//! See docs/SYNC.md §2–3.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

const SETTINGS_KEY: &str = "sync.settings";
const DEVICE_NAME_KEY: &str = "sync.device_name";
/// Tombstones and delete events older than this are pruned. A device offline
/// for longer could bring a deleted item back (documented in SYNC.md).
pub const TOMBSTONE_TTL_MS: i64 = 180 * 86_400_000;

pub const PUT: &str = "put";
pub const DEL: &str = "del";

/// Idempotent, additive schema setup. Runs from `db::open`.
pub fn migrate(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             item_id TEXT NOT NULL,
             op TEXT NOT NULL,
             at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_sync_events_item ON sync_events(item_id);
         CREATE TABLE IF NOT EXISTS sync_peers (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             added_at INTEGER NOT NULL,
             cursor INTEGER NOT NULL DEFAULT 0,
             last_seen INTEGER,
             last_sync INTEGER,
             addr TEXT
         );
         CREATE TABLE IF NOT EXISTS sync_tombstones (
             id TEXT PRIMARY KEY,
             at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sync_aliases (
             remote_id TEXT PRIMARY KEY,
             local_id TEXT NOT NULL
         );",
    )
    .ok();
    // Clipet Connect: per-device grants and screen arrangement.
    for column in [
        "allow_clipboard INTEGER NOT NULL DEFAULT 1",
        "allow_control INTEGER NOT NULL DEFAULT 0",
        "side TEXT",
    ] {
        conn.execute(&format!("ALTER TABLE sync_peers ADD COLUMN {column}"), []).ok();
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct SyncSettings {
    /// Clipet Connect is on: the device network runs.
    pub enabled: bool,
    /// Clipboard history sync (needs `enabled`); input sharing is separate.
    pub clipboard: bool,
    pub paused: bool,
    pub sync_text: bool,
    pub sync_images: bool,
    pub sync_pins: bool,
    /// Largest text item sent or accepted, in KB.
    pub max_text_kb: u32,
    /// Largest image sent or accepted, in MB.
    pub max_image_mb: u32,
}

/// Hard caps regardless of settings (the wire protocol enforces them too).
pub const MAX_TEXT_KB: u32 = 2048;
pub const MAX_IMAGE_MB: u32 = 25;

impl Default for SyncSettings {
    fn default() -> Self {
        // Conservative: off until enabled, images opt-in.
        Self {
            enabled: false,
            clipboard: true,
            paused: false,
            sync_text: true,
            sync_images: false,
            sync_pins: true,
            max_text_kb: 1024,
            max_image_mb: 10,
        }
    }
}

impl SyncSettings {
    pub fn clamped(self) -> Self {
        Self {
            max_text_kb: self.max_text_kb.clamp(1, MAX_TEXT_KB),
            max_image_mb: self.max_image_mb.clamp(1, MAX_IMAGE_MB),
            ..self
        }
    }
}

pub fn load_settings(conn: &Connection) -> SyncSettings {
    crate::db::get_setting(conn, SETTINGS_KEY)
        .and_then(|json| serde_json::from_str::<SyncSettings>(&json).ok())
        .unwrap_or_default()
        .clamped()
}

pub fn save_settings(conn: &Connection, settings: &SyncSettings) {
    if let Ok(json) = serde_json::to_string(settings) {
        crate::db::set_setting(conn, SETTINGS_KEY, &json);
    }
}

pub fn load_device_name(conn: &Connection) -> Option<String> {
    crate::db::get_setting(conn, DEVICE_NAME_KEY).filter(|n| !n.trim().is_empty())
}

pub fn save_device_name(conn: &Connection, name: &str) {
    crate::db::set_setting(conn, DEVICE_NAME_KEY, name);
}

// ---------------------------------------------------------------- outbox

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    pub seq: i64,
    pub item_id: String,
    pub op: String,
    pub at: i64,
}

/// Records a change to `item_id`, replacing its older events (only the latest
/// state of an item is ever sent). Returns the new sequence number.
pub fn append_event(conn: &Connection, item_id: &str, op: &str, at: i64) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM sync_events WHERE item_id = ?1", params![item_id])?;
    tx.execute(
        "INSERT INTO sync_events (item_id, op, at) VALUES (?1, ?2, ?3)",
        params![item_id, op, at],
    )?;
    let seq = tx.last_insert_rowid();
    if op == DEL {
        tx.execute(
            "INSERT OR REPLACE INTO sync_tombstones (id, at) VALUES (?1, ?2)",
            params![item_id, at],
        )?;
    }
    tx.commit()?;
    Ok(seq)
}

pub fn events_after(conn: &Connection, seq: i64, limit: i64) -> Vec<LogEvent> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT seq, item_id, op, at FROM sync_events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![seq, limit], |row| {
        Ok(LogEvent { seq: row.get(0)?, item_id: row.get(1)?, op: row.get(2)?, at: row.get(3)? })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

#[cfg(test)]
pub fn max_seq(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM sync_events", [], |row| row.get(0))
        .unwrap_or(0)
}

/// Queues every existing, syncable item this device has not logged yet, so
/// devices paired later receive the history (filters still apply at send
/// time). Items that came from another device are not re-sent.
pub fn backfill(conn: &Connection) -> usize {
    conn.execute(
        "INSERT INTO sync_events (item_id, op, at)
         SELECT id, 'put', created_at FROM clip_items
         WHERE kind != 'file' AND origin = ''
           AND id NOT IN (SELECT item_id FROM sync_events)
         ORDER BY created_at ASC",
        [],
    )
    .unwrap_or(0)
}

/// Drops events for items that no longer exist (cleared or expired locally)
/// and old tombstones.
pub fn prune(conn: &Connection, now: i64) {
    let cutoff = now - TOMBSTONE_TTL_MS;
    conn.execute(
        "DELETE FROM sync_events WHERE op = 'put' AND item_id NOT IN (SELECT id FROM clip_items)",
        [],
    )
    .ok();
    conn.execute("DELETE FROM sync_events WHERE op = 'del' AND at < ?1", params![cutoff]).ok();
    conn.execute("DELETE FROM sync_tombstones WHERE at < ?1", params![cutoff]).ok();
    conn.execute(
        "DELETE FROM sync_aliases WHERE local_id NOT IN (SELECT id FROM clip_items)",
        [],
    )
    .ok();
}

// ---------------------------------------------------------------- peers

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub added_at: i64,
    pub cursor: i64,
    pub last_seen: Option<i64>,
    pub last_sync: Option<i64>,
    /// Last known address (JSON `EndpointAddr`), a fallback when address
    /// lookup is unavailable.
    #[serde(skip)]
    pub addr: Option<String>,
    /// Exchange clipboard history with this device.
    pub allow_clipboard: bool,
    /// This device may control this computer's mouse and keyboard.
    pub allow_control: bool,
    /// Where this device's screen sits, as seen from this computer.
    pub side: Option<crate::input::geometry::Side>,
}

fn row_to_peer(row: &rusqlite::Row<'_>) -> rusqlite::Result<Peer> {
    Ok(Peer {
        id: row.get(0)?,
        name: row.get(1)?,
        added_at: row.get(2)?,
        cursor: row.get(3)?,
        last_seen: row.get(4)?,
        last_sync: row.get(5)?,
        addr: row.get(6)?,
        allow_clipboard: row.get::<_, i64>(7)? != 0,
        allow_control: row.get::<_, i64>(8)? != 0,
        side: row.get::<_, Option<String>>(9)?.and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok()),
    })
}

const PEER_COLUMNS: &str = "id, name, added_at, cursor, last_seen, last_sync, addr, allow_clipboard, allow_control, side";

pub fn list_peers(conn: &Connection) -> Vec<Peer> {
    let Ok(mut stmt) = conn.prepare(&format!("SELECT {PEER_COLUMNS} FROM sync_peers ORDER BY added_at")) else {
        return Vec::new();
    };
    stmt.query_map([], row_to_peer)
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
pub fn get_peer(conn: &Connection, id: &str) -> Option<Peer> {
    conn.query_row(
        &format!("SELECT {PEER_COLUMNS} FROM sync_peers WHERE id = ?1"),
        params![id],
        row_to_peer,
    )
    .ok()
}

/// Trusts a device. Re-pairing a known device keeps its cursor, so nothing
/// it already sent is transferred again.
pub fn add_peer(conn: &Connection, id: &str, name: &str, addr: Option<&str>, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_peers (id, name, added_at, addr) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, addr = COALESCE(excluded.addr, addr)",
        params![id, name, now, addr],
    )
    .map(|_| ())
}

pub fn remove_peer(conn: &Connection, id: &str) {
    conn.execute("DELETE FROM sync_peers WHERE id = ?1", params![id]).ok();
}

pub fn rename_peer(conn: &Connection, id: &str, name: &str) {
    conn.execute("UPDATE sync_peers SET name = ?1 WHERE id = ?2", params![name, id]).ok();
}

pub fn set_allow_clipboard(conn: &Connection, id: &str, allow: bool) {
    conn.execute("UPDATE sync_peers SET allow_clipboard = ?1 WHERE id = ?2", params![allow as i64, id]).ok();
}

pub fn set_allow_control(conn: &Connection, id: &str, allow: bool) {
    conn.execute("UPDATE sync_peers SET allow_control = ?1 WHERE id = ?2", params![allow as i64, id]).ok();
}

pub fn set_side(conn: &Connection, id: &str, side: Option<crate::input::geometry::Side>) {
    let side = side.and_then(|s| serde_json::to_value(s).ok()).and_then(|v| v.as_str().map(str::to_string));
    if side.is_some() {
        // One device per side.
        conn.execute("UPDATE sync_peers SET side = NULL WHERE side = ?1 AND id != ?2", params![side, id]).ok();
    }
    conn.execute("UPDATE sync_peers SET side = ?1 WHERE id = ?2", params![side, id]).ok();
}

pub fn set_peer_addr(conn: &Connection, id: &str, addr: &str) {
    conn.execute("UPDATE sync_peers SET addr = ?1 WHERE id = ?2", params![addr, id]).ok();
}

pub fn cursor(conn: &Connection, peer: &str) -> i64 {
    conn.query_row("SELECT cursor FROM sync_peers WHERE id = ?1", params![peer], |row| row.get(0))
        .unwrap_or(0)
}

/// Only ever moves forward.
pub fn advance_cursor(conn: &Connection, peer: &str, seq: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sync_peers SET cursor = MAX(cursor, ?1) WHERE id = ?2",
        params![seq, peer],
    )
    .map(|_| ())
}

pub fn mark_seen(conn: &Connection, peer: &str, now: i64) {
    conn.execute("UPDATE sync_peers SET last_seen = ?1 WHERE id = ?2", params![now, peer]).ok();
}

pub fn mark_synced(conn: &Connection, peer: &str, now: i64) {
    conn.execute(
        "UPDATE sync_peers SET last_sync = ?1, last_seen = ?1 WHERE id = ?2",
        params![now, peer],
    )
    .ok();
}

// ---------------------------------------------------------------- conflicts

pub fn is_tombstoned(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT 1 FROM sync_tombstones WHERE id = ?1", params![id], |_| Ok(()))
        .is_ok()
}

pub fn add_tombstone(conn: &Connection, id: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sync_tombstones (id, at) VALUES (?1, ?2)",
        params![id, at],
    )
    .map(|_| ())
}

pub fn alias(conn: &Connection, remote_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT local_id FROM sync_aliases WHERE remote_id = ?1",
        params![remote_id],
        |row| row.get(0),
    )
    .ok()
}

pub fn set_alias(conn: &Connection, remote_id: &str, local_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sync_aliases (remote_id, local_id) VALUES (?1, ?2)",
        params![remote_id, local_id],
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn temp_db() -> (std::path::PathBuf, Connection) {
        let dir = crate::sync::test_dir("store");
        let conn = db::open(&dir);
        (dir, conn)
    }

    #[test]
    fn newer_event_replaces_older_ones_for_the_same_item() {
        let (_dir, conn) = temp_db();
        append_event(&conn, "a", PUT, 1).unwrap();
        append_event(&conn, "b", PUT, 2).unwrap();
        let s3 = append_event(&conn, "a", PUT, 3).unwrap();
        let events = events_after(&conn, 0, 100);
        assert_eq!(events.iter().map(|e| e.item_id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(events[1].seq, s3);
        assert_eq!(max_seq(&conn), s3);
        // A cursor between the two still sees the newer event.
        assert_eq!(events_after(&conn, events[0].seq, 100).len(), 1);
    }

    #[test]
    fn delete_event_records_a_tombstone() {
        let (_dir, conn) = temp_db();
        append_event(&conn, "a", PUT, 1).unwrap();
        append_event(&conn, "a", DEL, 2).unwrap();
        let events = events_after(&conn, 0, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].op, DEL);
        assert!(is_tombstoned(&conn, "a"));
    }

    #[test]
    fn backfill_queues_local_syncable_items_once() {
        let (_dir, conn) = temp_db();
        db::insert_item(&conn, "t", "text", "x", "x", "", 1, "h1");
        db::insert_item(&conn, "f", "file", "[]", "f", "", 2, "h2");
        db::insert_item(&conn, "r", "text", "y", "y", "", 3, "h3");
        conn.execute("UPDATE clip_items SET origin = 'peer' WHERE id = 'r'", []).unwrap();
        assert_eq!(backfill(&conn), 1);
        assert_eq!(backfill(&conn), 0);
        assert_eq!(events_after(&conn, 0, 10)[0].item_id, "t");
    }

    #[test]
    fn prune_drops_events_for_removed_items_and_old_tombstones() {
        let (_dir, conn) = temp_db();
        db::insert_item(&conn, "kept", "text", "x", "x", "", 1, "h");
        append_event(&conn, "kept", PUT, 1).unwrap();
        append_event(&conn, "gone", PUT, 1).unwrap();
        append_event(&conn, "old-del", DEL, 1).unwrap();
        prune(&conn, TOMBSTONE_TTL_MS + 10);
        let ids: Vec<_> = events_after(&conn, 0, 10).into_iter().map(|e| e.item_id).collect();
        assert_eq!(ids, ["kept"]);
        assert!(!is_tombstoned(&conn, "old-del"));
    }

    #[test]
    fn peers_keep_their_cursor_when_paired_again_and_cursor_never_rewinds() {
        let (_dir, conn) = temp_db();
        add_peer(&conn, "p", "Mac", None, 1).unwrap();
        advance_cursor(&conn, "p", 10).unwrap();
        advance_cursor(&conn, "p", 4).unwrap();
        assert_eq!(cursor(&conn, "p"), 10);
        add_peer(&conn, "p", "MacBook", Some("{}"), 2).unwrap();
        let peer = get_peer(&conn, "p").unwrap();
        assert_eq!((peer.name.as_str(), peer.cursor, peer.addr.as_deref()), ("MacBook", 10, Some("{}")));
        remove_peer(&conn, "p");
        assert!(list_peers(&conn).is_empty());
    }

    #[test]
    fn grants_default_to_clipboard_only_and_sides_are_exclusive() {
        use crate::input::geometry::Side;
        let (_dir, conn) = temp_db();
        add_peer(&conn, "mac", "Mac", None, 1).unwrap();
        add_peer(&conn, "pc", "PC", None, 2).unwrap();
        let mac = &list_peers(&conn)[0];
        assert!(mac.allow_clipboard && !mac.allow_control && mac.side.is_none());
        set_allow_control(&conn, "mac", true);
        set_side(&conn, "mac", Some(Side::Right));
        set_side(&conn, "pc", Some(Side::Right));
        let peers = list_peers(&conn);
        assert!(peers[0].allow_control);
        assert_eq!((peers[0].side, peers[1].side), (None, Some(Side::Right)));
        set_side(&conn, "pc", None);
        assert_eq!(list_peers(&conn)[1].side, None);
    }

    #[test]
    fn settings_default_conservatively_and_are_clamped() {
        let (_dir, conn) = temp_db();
        let s = load_settings(&conn);
        assert!(!s.enabled && !s.sync_images && s.sync_text);
        save_settings(&conn, &SyncSettings { max_image_mb: 999, ..s });
        assert_eq!(load_settings(&conn).max_image_mb, MAX_IMAGE_MB);
    }
}
