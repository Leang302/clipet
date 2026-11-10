//! The sync engine: what an item looks like on the wire, what this device
//! sends for each logged change, and how a received change is applied —
//! deduplication, tombstones and last-writer-wins (docs/SYNC.md §5).
//!
//! Pure SQLite and file work; it knows nothing about the network.

use super::store::{self, LogEvent};
use crate::db;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

const TEXT_KINDS: [&str; 3] = ["text", "link", "code"];
const IMAGE_KINDS: [&str; 2] = ["image", "screenshot"];
/// Upper bound on decoded image size, so a hostile PNG can't exhaust memory.
const MAX_IMAGE_SIDE: u32 = 16_384;
const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

fn is_text_kind(kind: &str) -> bool {
    TEXT_KINDS.contains(&kind)
}

fn is_image_kind(kind: &str) -> bool {
    IMAGE_KINDS.contains(&kind)
}

/// An item as it travels between devices. Images carry their PNG bytes
/// separately (see `wire`); `hash` is the SHA-256 of the RGBA pixels, the
/// same hash capture uses, and is checked on arrival.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ItemState {
    pub id: String,
    pub kind: String,
    /// Content of text kinds; `None` for images.
    pub text: Option<String>,
    pub preview: String,
    pub created_at: i64,
    pub pinned: bool,
    pub pinned_at: i64,
    pub edited: bool,
    pub edited_at: i64,
    pub hash: String,
}

/// What a device accepts. Each side applies its own and the sender also
/// honors the receiver's, so filtered content never leaves the device.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub text: bool,
    pub images: bool,
    pub pins: bool,
    pub max_text_bytes: u64,
    pub max_image_bytes: u64,
}

impl Prefs {
    pub fn from_settings(s: &store::SyncSettings) -> Self {
        Self {
            text: s.sync_text,
            images: s.sync_images,
            pins: s.sync_pins,
            max_text_bytes: u64::from(s.max_text_kb) * 1024,
            max_image_bytes: u64::from(s.max_image_mb) * 1024 * 1024,
        }
    }

    /// The stricter of two preference sets.
    pub fn intersect(self, other: Prefs) -> Prefs {
        Prefs {
            text: self.text && other.text,
            images: self.images && other.images,
            pins: self.pins && other.pins,
            max_text_bytes: self.max_text_bytes.min(other.max_text_bytes),
            max_image_bytes: self.max_image_bytes.min(other.max_image_bytes),
        }
    }
}

/// The parts of a `clip_items` row sync needs (more than `db::ClipItem`).
#[derive(Debug)]
struct Row {
    id: String,
    kind: String,
    content: String,
    preview: String,
    created_at: i64,
    pinned: bool,
    pinned_at: i64,
    edited: bool,
    edited_at: i64,
    hash: String,
}

fn load_row(conn: &Connection, id: &str) -> rusqlite::Result<Option<Row>> {
    conn.query_row(
        "SELECT id, kind, content, preview, created_at, pinned, pinned_at, edited, edited_at, content_hash
         FROM clip_items WHERE id = ?1",
        params![id],
        |r| {
            Ok(Row {
                id: r.get(0)?,
                kind: r.get(1)?,
                content: r.get(2)?,
                preview: r.get(3)?,
                created_at: r.get(4)?,
                pinned: r.get::<_, i64>(5)? != 0,
                pinned_at: r.get(6)?,
                edited: r.get::<_, i64>(7)? != 0,
                edited_at: r.get(8)?,
                hash: r.get(9)?,
            })
        },
    )
    .optional()
}

// ---------------------------------------------------------------- sending

#[derive(Debug, PartialEq, Eq)]
pub enum Outgoing {
    Put(ItemState, Option<Vec<u8>>),
    Del,
    /// Filtered out (type off, too large, file item) or already gone.
    Skip,
}

/// Builds what to send for one logged change, from the item's current
/// state. `prefs` is the intersection of both devices' preferences.
pub fn outgoing(conn: &Connection, data_dir: &Path, event: &LogEvent, prefs: &Prefs) -> Outgoing {
    if event.op == store::DEL {
        return Outgoing::Del;
    }
    let Ok(Some(row)) = load_row(conn, &event.item_id) else {
        return Outgoing::Skip;
    };
    let (pinned, pinned_at) = if prefs.pins { (row.pinned, row.pinned_at) } else { (false, 0) };
    let mut state = ItemState {
        id: row.id,
        kind: row.kind.clone(),
        text: None,
        preview: row.preview,
        created_at: row.created_at,
        pinned,
        pinned_at,
        edited: row.edited,
        edited_at: row.edited_at,
        hash: row.hash,
    };
    if is_text_kind(&row.kind) {
        if !prefs.text || row.content.len() as u64 > prefs.max_text_bytes {
            return Outgoing::Skip;
        }
        // A pairing invitation copied out of a chat is a one-time credential,
        // not history worth spreading.
        if row.content.trim_start().starts_with(super::pairing::PREFIX) {
            return Outgoing::Skip;
        }
        state.text = Some(row.content);
        Outgoing::Put(state, None)
    } else if is_image_kind(&row.kind) {
        if !prefs.images {
            return Outgoing::Skip;
        }
        let path = data_dir.join("images").join(&row.content);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() <= prefs.max_image_bytes => {}
            _ => return Outgoing::Skip,
        }
        match std::fs::read(&path) {
            Ok(bytes) => Outgoing::Put(state, Some(bytes)),
            Err(_) => Outgoing::Skip,
        }
    } else {
        // File lists are paths on this machine; never synced.
        Outgoing::Skip
    }
}

// ---------------------------------------------------------------- receiving

pub struct Ctx<'a> {
    /// This device's endpoint id.
    pub me: &'a str,
    /// The sending device's endpoint id.
    pub peer: &'a str,
    pub data_dir: &'a Path,
    /// This device's own preferences (enforced on receipt too).
    pub prefs: &'a Prefs,
    pub now: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Inserted(String),
    Updated(String),
    Deleted(String),
    Unchanged,
    /// Not applied, permanently (filtered, invalid, deleted earlier…).
    Ignored(&'static str),
}

impl Applied {
    pub fn changed(&self) -> bool {
        matches!(self, Applied::Inserted(_) | Applied::Updated(_) | Applied::Deleted(_))
    }
}

/// Ids name image files, so only a conservative alphabet is accepted.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Last-writer-wins: a later timestamp wins; on a tie the larger device id
/// wins, so both devices settle on the same value. A zero clock means "never
/// changed on the sender" and never overrides local state.
fn remote_wins(remote_at: i64, local_at: i64, ctx: &Ctx) -> bool {
    remote_at > 0 && (remote_at > local_at || (remote_at == local_at && ctx.peer > ctx.me))
}

/// The local item a remote id refers to: the same id, or an alias.
fn resolve(conn: &Connection, remote_id: &str) -> rusqlite::Result<Option<Row>> {
    if let Some(row) = load_row(conn, remote_id)? {
        return Ok(Some(row));
    }
    match store::alias(conn, remote_id) {
        Some(local) => load_row(conn, &local),
        None => Ok(None),
    }
}

/// An existing item with the same content, for deduplication.
fn find_duplicate(conn: &Connection, state: &ItemState) -> rusqlite::Result<Option<Row>> {
    let kinds: &[&str] = if is_text_kind(&state.kind) { &TEXT_KINDS } else { &IMAGE_KINDS };
    let placeholders = kinds.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
    let id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM clip_items WHERE content_hash = ?1 AND kind IN ({placeholders})
                 ORDER BY created_at DESC LIMIT 1"
            ),
            params![state.hash],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => load_row(conn, &id),
        None => Ok(None),
    }
}

/// Decodes and checks a received PNG: within size limits, and its pixels
/// hash to what the sender claimed. Returns the preview text ("WxH").
fn verify_png(bytes: &[u8], expected_hash: &str) -> Result<String, &'static str> {
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let pixels = reader.decode().map_err(|_| "the image could not be decoded")?.to_rgba8();
    if crate::clipboard::hash_bytes(pixels.as_raw()) != expected_hash {
        return Err("the image does not match its hash");
    }
    Ok(format!("{}x{}", pixels.width(), pixels.height()))
}

/// Write-then-rename, so an interruption never leaves a truncated image.
fn write_image(data_dir: &Path, file: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = data_dir.join("images");
    let temp = dir.join(format!("{file}.syncing"));
    std::fs::write(&temp, bytes)
        .and_then(|_| std::fs::rename(&temp, dir.join(file)))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not save a synced image: {e}")
        })
}

/// Applies a received `put`. `Err` means a transient failure (the cursor
/// must not move); everything permanent is `Ok(Ignored)`.
pub fn apply_put(conn: &Connection, ctx: &Ctx, state: &ItemState, blob: Option<&[u8]>) -> Result<Applied, String> {
    let db_err = |e: rusqlite::Error| e.to_string();
    if !valid_id(&state.id) {
        return Ok(Applied::Ignored("invalid item id"));
    }
    let text_kind = is_text_kind(&state.kind);
    let image_kind = is_image_kind(&state.kind);
    if !text_kind && !image_kind {
        return Ok(Applied::Ignored("unsupported kind"));
    }
    if text_kind && !ctx.prefs.text || image_kind && !ctx.prefs.images {
        return Ok(Applied::Ignored("type not synced on this device"));
    }
    if text_kind {
        let Some(text) = &state.text else { return Ok(Applied::Ignored("missing text")) };
        if text.len() as u64 > ctx.prefs.max_text_bytes {
            return Ok(Applied::Ignored("too large"));
        }
    }
    if image_kind && blob.is_some_and(|b| b.len() as u64 > ctx.prefs.max_image_bytes) {
        return Ok(Applied::Ignored("too large"));
    }
    if store::is_tombstoned(conn, &state.id) {
        return Ok(Applied::Ignored("deleted earlier"));
    }
    let (pinned, pinned_at) = if ctx.prefs.pins { (state.pinned, state.pinned_at) } else { (false, 0) };

    let mut local = resolve(conn, &state.id).map_err(db_err)?;
    if local.is_none() {
        if let Some(dup) = find_duplicate(conn, state).map_err(db_err)? {
            store::set_alias(conn, &state.id, &dup.id).map_err(db_err)?;
            local = Some(dup);
        }
    }

    let Some(local) = local else {
        // New item.
        let retention = db::load_retention(conn);
        if retention.days > 0 {
            let cutoff = ctx.now - i64::from(retention.days) * 86_400_000;
            if state.created_at < cutoff && !(pinned && !retention.include_pinned) {
                return Ok(Applied::Ignored("older than this device's history period"));
            }
        }
        let (content, preview) = if text_kind {
            (state.text.clone().unwrap_or_default(), state.preview.chars().take(120).collect::<String>())
        } else {
            let Some(bytes) = blob else { return Ok(Applied::Ignored("missing image data")) };
            if !valid_hash(&state.hash) {
                return Ok(Applied::Ignored("invalid hash"));
            }
            let preview = match verify_png(bytes, &state.hash) {
                Ok(p) => p,
                Err(reason) => return Ok(Applied::Ignored(reason)),
            };
            let file = format!("{}.png", state.id);
            write_image(ctx.data_dir, &file, bytes)?;
            (file, preview)
        };
        conn.execute(
            "INSERT INTO clip_items (id, kind, content, preview, source_app, created_at, content_hash,
                                     pinned, pinned_at, edited, edited_at, origin)
             VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                state.id, state.kind, content, preview, state.created_at, state.hash,
                pinned as i64, pinned_at, state.edited as i64, state.edited_at, ctx.peer
            ],
        )
        .map_err(db_err)?;
        return Ok(Applied::Inserted(state.id.clone()));
    };

    if store::is_tombstoned(conn, &local.id) {
        return Ok(Applied::Ignored("deleted earlier"));
    }

    // Merge into the existing item.
    let mut changed = false;
    if state.created_at > local.created_at {
        conn.execute("UPDATE clip_items SET created_at = ?1 WHERE id = ?2", params![state.created_at, local.id])
            .map_err(db_err)?;
        changed = true;
    }
    if remote_wins(pinned_at, local.pinned_at, ctx) && (pinned != local.pinned || pinned_at != local.pinned_at) {
        conn.execute(
            "UPDATE clip_items SET pinned = ?1, pinned_at = ?2 WHERE id = ?3",
            params![pinned as i64, pinned_at, local.id],
        )
        .map_err(db_err)?;
        changed = true;
    }
    if remote_wins(state.edited_at, local.edited_at, ctx) && state.hash != local.hash {
        if text_kind && is_text_kind(&local.kind) {
            let text = state.text.clone().unwrap_or_default();
            let preview: String = text.chars().take(120).collect();
            conn.execute(
                "UPDATE clip_items SET content = ?1, preview = ?2, content_hash = ?3, edited = ?4, edited_at = ?5
                 WHERE id = ?6",
                params![text, preview, state.hash, state.edited as i64, state.edited_at, local.id],
            )
            .map_err(db_err)?;
            changed = true;
        } else if image_kind && is_image_kind(&local.kind) {
            if let Some(bytes) = blob {
                if !valid_hash(&state.hash) {
                    return Ok(Applied::Ignored("invalid hash"));
                }
                let preview = match verify_png(bytes, &state.hash) {
                    Ok(p) => p,
                    Err(reason) => return Ok(Applied::Ignored(reason)),
                };
                write_image(ctx.data_dir, &local.content, bytes)?;
                std::fs::remove_file(ctx.data_dir.join("images").join(db::thumbnail_file(&local.id))).ok();
                conn.execute(
                    "UPDATE clip_items SET preview = ?1, content_hash = ?2, edited = ?3, edited_at = ?4,
                                           ocr_status = ?5, ocr_text = NULL, ocr_lines = NULL, ocr_error = NULL
                     WHERE id = ?6",
                    params![preview, state.hash, state.edited as i64, state.edited_at, db::OCR_PENDING, local.id],
                )
                .map_err(db_err)?;
                changed = true;
            }
        }
    }
    Ok(if changed { Applied::Updated(local.id) } else { Applied::Unchanged })
}

/// Applies a received delete: removes the local item (and its files) and
/// remembers the ids so no stale or repeated `put` brings it back.
pub fn apply_del(conn: &Connection, ctx: &Ctx, item_id: &str, at: i64) -> Result<Applied, String> {
    if !valid_id(item_id) {
        return Ok(Applied::Ignored("invalid item id"));
    }
    let db_err = |e: rusqlite::Error| e.to_string();
    let local = resolve(conn, item_id).map_err(db_err)?;
    store::add_tombstone(conn, item_id, at).map_err(db_err)?;
    let Some(local) = local else {
        return Ok(Applied::Unchanged);
    };
    store::add_tombstone(conn, &local.id, at).map_err(db_err)?;
    db::delete_item(conn, &ctx.data_dir.to_path_buf(), &local.id);
    // Not re-sent from here: the delete came from the peer.
    conn.execute("DELETE FROM sync_events WHERE item_id = ?1", params![local.id]).map_err(db_err)?;
    Ok(Applied::Deleted(local.id))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sync::store::{PUT, DEL, append_event};

    pub struct Device {
        pub id: String,
        pub dir: std::path::PathBuf,
        pub conn: Connection,
        pub prefs: Prefs,
    }

    /// The receiving clock in these tests: day 100.
    pub const NOW: i64 = 100 * 86_400_000;

    pub fn device(id: &str) -> Device {
        let dir = crate::sync::test_dir(id);
        let conn = db::open(&dir);
        let mut settings = store::SyncSettings::default();
        settings.sync_images = true;
        Device { id: id.to_string(), dir, conn, prefs: Prefs::from_settings(&settings) }
    }

    /// Delivers everything `from` logged after `to`'s cursor, the way a
    /// session does: in order, one transaction per event.
    pub fn sync(from: &Device, to: &Device) -> Vec<Applied> {
        store::add_peer(&to.conn, &from.id, &from.id, None, 0).ok();
        let cursor = store::cursor(&to.conn, &from.id);
        let prefs = from.prefs.intersect(to.prefs);
        let mut results = Vec::new();
        for event in store::events_after(&from.conn, cursor, 1000) {
            let ctx = Ctx { me: &to.id, peer: &from.id, data_dir: &to.dir, prefs: &to.prefs, now: NOW };
            let tx = to.conn.unchecked_transaction().unwrap();
            let applied = match outgoing(&from.conn, &from.dir, &event, &prefs) {
                Outgoing::Put(state, blob) => apply_put(&tx, &ctx, &state, blob.as_deref()).unwrap(),
                Outgoing::Del => apply_del(&tx, &ctx, &event.item_id, event.at).unwrap(),
                Outgoing::Skip => Applied::Ignored("skipped by sender"),
            };
            store::advance_cursor(&tx, &from.id, event.seq).unwrap();
            tx.commit().unwrap();
            results.push(applied);
        }
        results
    }

    pub fn copy_text(d: &Device, id: &str, text: &str, at: i64) {
        db::insert_item(&d.conn, id, "text", text, text, "", at, &crate::clipboard::hash_bytes(text.as_bytes()));
        append_event(&d.conn, id, PUT, at).unwrap();
    }

    fn png(width: u32, height: u32, shade: u8) -> (Vec<u8>, String) {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([shade, 0, 0, 255]));
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        (bytes, crate::clipboard::hash_bytes(img.as_raw()))
    }

    fn copy_image(d: &Device, id: &str, shade: u8, at: i64) -> String {
        let (bytes, hash) = png(4, 3, shade);
        let file = format!("{id}.png");
        std::fs::write(d.dir.join("images").join(&file), bytes).unwrap();
        db::insert_item(&d.conn, id, "image", &file, "4x3", "", at, &hash);
        append_event(&d.conn, id, PUT, at).unwrap();
        hash
    }

    fn item(d: &Device, id: &str) -> Option<db::ClipItem> {
        db::get_item(&d.conn, id)
    }

    fn count(d: &Device) -> usize {
        db::list_items(&d.conn, 1000).len()
    }

    #[test]
    fn text_arrives_with_its_kind_time_and_origin() {
        let (a, b) = (device("a1"), device("b1"));
        copy_text(&a, "t1", "hello", 100);
        db::insert_item(&a.conn, "l1", "link", "https://x.com", "https://x.com", "", 200, "hl");
        append_event(&a.conn, "l1", PUT, 200).unwrap();
        assert_eq!(sync(&a, &b), [Applied::Inserted("t1".into()), Applied::Inserted("l1".into())]);
        let t = item(&b, "t1").unwrap();
        assert_eq!((t.kind.as_str(), t.content.as_str(), t.created_at, t.origin.as_str()), ("text", "hello", 100, "a1"));
        assert_eq!(item(&b, "l1").unwrap().kind, "link");
        // Nothing new: a second sync transfers nothing.
        assert!(sync(&a, &b).is_empty());
    }

    #[test]
    fn received_items_are_not_logged_so_they_never_loop_back() {
        let (a, b) = (device("a2"), device("b2"));
        copy_text(&a, "t1", "hello", 100);
        sync(&a, &b);
        assert!(store::events_after(&b.conn, 0, 10).is_empty());
        assert!(sync(&b, &a).is_empty());
    }

    #[test]
    fn duplicate_content_is_merged_not_stored_twice() {
        let (a, b) = (device("a3"), device("b3"));
        copy_text(&a, "ta", "same text", 100);
        copy_text(&b, "tb", "same text", 50);
        assert_eq!(sync(&a, &b), [Applied::Updated("tb".into())]);
        assert_eq!(count(&b), 1);
        // The re-copy moved B's entry to the newer time.
        assert_eq!(item(&b, "tb").unwrap().created_at, 100);
        // A later pin of A's copy reaches B's entry through the alias.
        db::set_pinned(&a.conn, "ta", true);
        append_event(&a.conn, "ta", PUT, 300).unwrap();
        sync(&a, &b);
        assert!(item(&b, "tb").unwrap().pinned);
    }

    #[test]
    fn applying_the_same_event_twice_changes_nothing() {
        let (a, b) = (device("a4"), device("b4"));
        copy_text(&a, "t1", "x", 100);
        sync(&a, &b);
        // Simulate a crash before the cursor was saved: replay from 0.
        b.conn.execute("UPDATE sync_peers SET cursor = 0", []).unwrap();
        assert_eq!(sync(&a, &b), [Applied::Unchanged]);
        assert_eq!(count(&b), 1);
    }

    #[test]
    fn delete_removes_the_item_and_stale_puts_cannot_bring_it_back() {
        let (a, b) = (device("a5"), device("b5"));
        copy_text(&a, "t1", "secret", 100);
        sync(&a, &b);
        db::delete_item(&a.conn, &a.dir, "t1");
        append_event(&a.conn, "t1", DEL, 200).unwrap();
        assert_eq!(sync(&a, &b), [Applied::Deleted("t1".into())]);
        assert!(item(&b, "t1").is_none());
        // An old put arriving late (another path, a replay) is ignored.
        let ctx = Ctx { me: "b5", peer: "a5", data_dir: &b.dir, prefs: &b.prefs, now: 0 };
        let stale = ItemState {
            id: "t1".into(), kind: "text".into(), text: Some("secret".into()), preview: "secret".into(),
            created_at: 100, pinned: false, pinned_at: 0, edited: false, edited_at: 0,
            hash: crate::clipboard::hash_bytes(b"secret"),
        };
        assert_eq!(apply_put(&b.conn, &ctx, &stale, None).unwrap(), Applied::Ignored("deleted earlier"));
    }

    #[test]
    fn delete_of_an_aliased_item_removes_the_local_copy() {
        let (a, b) = (device("a6"), device("b6"));
        copy_text(&a, "ta", "dup", 100);
        copy_text(&b, "tb", "dup", 100);
        sync(&a, &b);
        db::delete_item(&a.conn, &a.dir, "ta");
        append_event(&a.conn, "ta", DEL, 200).unwrap();
        assert_eq!(sync(&a, &b), [Applied::Deleted("tb".into())]);
        assert_eq!(count(&b), 0);
    }

    #[test]
    fn concurrent_pin_changes_converge_to_the_latest() {
        let (a, b) = (device("a7"), device("b7"));
        copy_text(&a, "t1", "x", 100);
        sync(&a, &b);
        // Both change the pin while apart: A pins at 10, B unpins later at 20.
        a.conn.execute("UPDATE clip_items SET pinned = 1, pinned_at = 10 WHERE id = 't1'", []).unwrap();
        append_event(&a.conn, "t1", PUT, 10).unwrap();
        b.conn.execute("UPDATE clip_items SET pinned = 0, pinned_at = 20 WHERE id = 't1'", []).unwrap();
        append_event(&b.conn, "t1", PUT, 20).unwrap();
        sync(&a, &b);
        sync(&b, &a);
        assert!(!item(&a, "t1").unwrap().pinned);
        assert!(!item(&b, "t1").unwrap().pinned);
    }

    #[test]
    fn pin_tie_is_broken_by_device_id_identically_on_both_sides() {
        let (a, b) = (device("a8"), device("b8"));
        copy_text(&a, "t1", "x", 100);
        sync(&a, &b);
        a.conn.execute("UPDATE clip_items SET pinned = 1, pinned_at = 50 WHERE id = 't1'", []).unwrap();
        append_event(&a.conn, "t1", PUT, 50).unwrap();
        b.conn.execute("UPDATE clip_items SET pinned = 0, pinned_at = 50 WHERE id = 't1'", []).unwrap();
        append_event(&b.conn, "t1", PUT, 50).unwrap();
        sync(&a, &b);
        sync(&b, &a);
        // "b8" > "a8": B's value wins everywhere.
        assert_eq!(item(&a, "t1").unwrap().pinned, item(&b, "t1").unwrap().pinned);
        assert!(!item(&a, "t1").unwrap().pinned);
    }

    #[test]
    fn an_unpinned_remote_never_unpins_a_legacy_local_pin() {
        let (a, b) = (device("a9"), device("b9"));
        copy_text(&b, "tb", "x", 100);
        b.conn.execute("UPDATE clip_items SET pinned = 1, pinned_at = 0", []).unwrap();
        copy_text(&a, "ta", "x", 200);
        sync(&a, &b);
        assert!(item(&b, "tb").unwrap().pinned);
    }

    #[test]
    fn later_text_edit_wins() {
        let (a, b) = (device("a10"), device("b10"));
        copy_text(&a, "t1", "draft", 100);
        sync(&a, &b);
        db::update_text(&a.conn, "t1", "final", "final", &crate::clipboard::hash_bytes(b"final")).unwrap();
        append_event(&a.conn, "t1", PUT, 300).unwrap();
        sync(&a, &b);
        let t = item(&b, "t1").unwrap();
        assert_eq!((t.content.as_str(), t.edited), ("final", true));
    }

    #[test]
    fn items_older_than_the_receivers_history_period_are_skipped() {
        let (a, b) = (device("a11"), device("b11"));
        db::save_retention(&b.conn, &db::Retention { days: 1, include_favorites: false, include_pinned: false });
        copy_text(&a, "old", "old", 1);
        copy_text(&a, "new", "new", NOW - 1000);
        let results = sync(&a, &b);
        assert!(matches!(results[0], Applied::Ignored(_)));
        assert_eq!(results[1], Applied::Inserted("new".into()));
    }

    #[test]
    fn filters_apply_on_both_ends() {
        let (mut a, mut b) = (device("a12"), device("b12"));
        copy_text(&a, "t1", "text", 100);
        copy_image(&a, "i1", 10, 200);
        // Receiver doesn't take images: the sender skips them.
        b.prefs.images = false;
        assert_eq!(sync(&a, &b), [Applied::Inserted("t1".into()), Applied::Ignored("skipped by sender")]);
        // Sender's size limit.
        a.prefs.max_text_bytes = 3;
        b.prefs.images = true;
        copy_text(&a, "t2", "too long", 300);
        assert_eq!(sync(&a, &b), [Applied::Ignored("skipped by sender")]);
        // File items never travel.
        db::insert_item(&a.conn, "f1", "file", "[]", "f", "", 400, "hf");
        append_event(&a.conn, "f1", PUT, 400).unwrap();
        assert_eq!(sync(&a, &b), [Applied::Ignored("skipped by sender")]);
    }

    #[test]
    fn pins_are_not_shared_when_pin_sync_is_off() {
        let (mut a, b) = (device("a13"), device("b13"));
        a.prefs.pins = false;
        copy_text(&a, "t1", "x", 100);
        db::set_pinned(&a.conn, "t1", true);
        sync(&a, &b);
        assert!(!item(&b, "t1").unwrap().pinned);
    }

    #[test]
    fn images_transfer_and_are_verified() {
        let (a, b) = (device("a14"), device("b14"));
        let hash = copy_image(&a, "i1", 10, 100);
        assert_eq!(sync(&a, &b), [Applied::Inserted("i1".into())]);
        let i = item(&b, "i1").unwrap();
        assert_eq!((i.kind.as_str(), i.preview.as_str()), ("image", "4x3"));
        let bytes = std::fs::read(b.dir.join("images").join(&i.content)).unwrap();
        assert_eq!(verify_png(&bytes, &hash), Ok("4x3".to_string()));
        // The same picture copied on B already: merged by pixel hash.
        let c = device("c14");
        copy_image(&c, "ic", 10, 50);
        assert_eq!(sync(&a, &c), [Applied::Updated("ic".into())]);
    }

    #[test]
    fn corrupt_or_mismatched_images_are_rejected() {
        let b = device("b15");
        let ctx = Ctx { me: "b15", peer: "a15", data_dir: &b.dir, prefs: &b.prefs, now: 0 };
        let (bytes, _) = png(2, 2, 1);
        let (_, other_hash) = png(2, 2, 2);
        let state = ItemState {
            id: "i1".into(), kind: "image".into(), text: None, preview: String::new(), created_at: 1,
            pinned: false, pinned_at: 0, edited: false, edited_at: 0, hash: other_hash.clone(),
        };
        assert_eq!(apply_put(&b.conn, &ctx, &state, Some(&bytes)).unwrap(), Applied::Ignored("the image does not match its hash"));
        // A truncated transfer.
        let state = ItemState { hash: crate::clipboard::hash_bytes(b"x"), ..state };
        assert!(matches!(apply_put(&b.conn, &ctx, &state, Some(&bytes[..bytes.len() / 2])).unwrap(), Applied::Ignored(_)));
        assert_eq!(count(&b), 0);
        assert!(!b.dir.join("images").join("i1.png").exists());
    }

    #[test]
    fn hostile_ids_are_rejected() {
        let b = device("b16");
        let ctx = Ctx { me: "b16", peer: "a16", data_dir: &b.dir, prefs: &b.prefs, now: 0 };
        let state = ItemState {
            id: "../../evil".into(), kind: "text".into(), text: Some("x".into()), preview: "x".into(),
            created_at: 1, pinned: false, pinned_at: 0, edited: false, edited_at: 0, hash: "h".into(),
        };
        assert_eq!(apply_put(&b.conn, &ctx, &state, None).unwrap(), Applied::Ignored("invalid item id"));
        assert_eq!(apply_del(&b.conn, &ctx, "../x", 1).unwrap(), Applied::Ignored("invalid item id"));
    }

    #[test]
    fn offline_catch_up_delivers_everything_in_order() {
        let (a, b) = (device("a17"), device("b17"));
        copy_text(&a, "first", "1", 1);
        sync(&a, &b);
        // B is offline while A copies 20 items.
        for n in 0..20 {
            copy_text(&a, &format!("n{n}"), &format!("item {n}"), 10 + n);
        }
        let results = sync(&a, &b);
        assert_eq!(results.len(), 20);
        assert!(results.iter().all(|r| matches!(r, Applied::Inserted(_))));
        assert_eq!(count(&b), 21);
    }
}
