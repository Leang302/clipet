use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Clone)]
pub struct ClipItem {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub preview: String,
    pub source_app: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub use_count: i64,
    pub favorite: bool,
    pub pinned: bool,
    pub edited: bool,
    /// One of the `OCR_*` states. Only meaningful for image items.
    pub ocr_status: i64,
    /// Text recognized in an image, for search.
    pub ocr_text: Option<String>,
    /// JSON array of recognized lines with normalized, top-left-origin boxes:
    /// `[{"text": "...", "x": 0.1, "y": 0.2, "w": 0.3, "h": 0.04}, ...]`.
    pub ocr_lines: Option<String>,
    /// Why the last recognition attempt failed, in plain language.
    pub ocr_error: Option<String>,
}

pub const OCR_PENDING: i64 = 0;
pub const OCR_DONE: i64 = 1;
pub const OCR_FAILED: i64 = 2;
/// Set while an image is being recognized. Anything still in this state at
/// startup crashed or was interrupted mid-OCR; it is marked failed so a bad
/// image can't crash-loop the app.
pub const OCR_RUNNING: i64 = 3;

const ITEM_COLUMNS: &str = "id, kind, content, preview, source_app, created_at, last_used_at, use_count, favorite, pinned, edited, ocr_status, ocr_text, ocr_lines, ocr_error";

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipItem> {
    Ok(ClipItem {
        id: row.get(0)?,
        kind: row.get(1)?,
        content: row.get(2)?,
        preview: row.get(3)?,
        source_app: row.get(4)?,
        created_at: row.get(5)?,
        last_used_at: row.get(6)?,
        use_count: row.get(7)?,
        favorite: row.get::<_, i64>(8)? != 0,
        pinned: row.get::<_, i64>(9)? != 0,
        edited: row.get::<_, i64>(10)? != 0,
        ocr_status: row.get(11)?,
        ocr_text: row.get(12)?,
        ocr_lines: row.get(13)?,
        ocr_error: row.get(14)?,
    })
}

pub fn open(data_dir: &PathBuf) -> Connection {
    std::fs::create_dir_all(data_dir).expect("failed to create app data dir");
    std::fs::create_dir_all(data_dir.join("images")).expect("failed to create images dir");
    let conn =
        Connection::open(data_dir.join("clipflow.sqlite3")).expect("failed to open sqlite db");
    // Capture, OCR and cleanup each use their own connection; wait for a
    // briefly locked database instead of failing (and dropping a capture).
    conn.busy_timeout(std::time::Duration::from_secs(3)).ok();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS clip_items (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            content TEXT NOT NULL,
            preview TEXT NOT NULL,
            source_app TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL,
            last_used_at INTEGER,
            use_count INTEGER NOT NULL DEFAULT 0,
            favorite INTEGER NOT NULL DEFAULT 0,
            pinned INTEGER NOT NULL DEFAULT 0,
            content_hash TEXT NOT NULL
        )",
        [],
    )
    .expect("failed to create clip_items table");
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_clip_items_hash ON clip_items(content_hash)",
        [],
    )
    .ok();
    // Migration for DBs created before the `edited` column existed; ignore the
    // error when it's already there.
    conn.execute(
        "ALTER TABLE clip_items ADD COLUMN edited INTEGER NOT NULL DEFAULT 0",
        [],
    )
    .ok();
    // OCR columns (same additive, idempotent pattern). Existing images start
    // pending, so history captured before OCR existed gets indexed too.
    for column in [
        "ocr_status INTEGER NOT NULL DEFAULT 0",
        "ocr_text TEXT",
        "ocr_lines TEXT",
        // Which OCR version produced ocr_text; older ones are re-scanned.
        "ocr_version INTEGER NOT NULL DEFAULT 0",
        "ocr_error TEXT",
    ] {
        conn.execute(&format!("ALTER TABLE clip_items ADD COLUMN {column}"), [])
            .ok();
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_clip_items_ocr ON clip_items(ocr_status, kind)",
        [],
    )
    .ok();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .ok();
    conn
}

pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .ok();
}

pub struct OcrJob {
    pub id: String,
    pub file: String,
    /// Already indexed by an older OCR version (keeps its text meanwhile).
    pub rescan: bool,
}

/// Next image to OCR: first-time scans (newest first) before re-scans of
/// images indexed by an older `version`.
pub fn next_ocr_job(conn: &Connection, version: i64) -> Option<OcrJob> {
    let first = conn
        .query_row(
            "SELECT id, content FROM clip_items
             WHERE kind IN ('image', 'screenshot') AND ocr_status = ?1
             ORDER BY created_at DESC LIMIT 1",
            params![OCR_PENDING],
            |row| Ok(OcrJob { id: row.get(0)?, file: row.get(1)?, rescan: false }),
        )
        .ok();
    first.or_else(|| {
        conn.query_row(
            "SELECT id, content FROM clip_items
             WHERE kind IN ('image', 'screenshot') AND ocr_status = ?1 AND ocr_version < ?2
             ORDER BY created_at DESC LIMIT 1",
            params![OCR_DONE, version],
            |row| Ok(OcrJob { id: row.get(0)?, file: row.get(1)?, rescan: true }),
        )
        .ok()
    })
}

pub fn set_ocr_status(conn: &Connection, id: &str, status: i64) {
    conn.execute(
        "UPDATE clip_items SET ocr_status = ?1 WHERE id = ?2",
        params![status, id],
    )
    .ok();
}

pub fn set_ocr_version(conn: &Connection, id: &str, version: i64) {
    conn.execute(
        "UPDATE clip_items SET ocr_version = ?1 WHERE id = ?2",
        params![version, id],
    )
    .ok();
}

/// Stores a result. A row deleted while its OCR ran simply isn't updated.
pub fn set_ocr_result(conn: &Connection, id: &str, text: &str, lines_json: &str, version: i64) -> bool {
    conn.execute(
        "UPDATE clip_items
         SET ocr_status = ?1, ocr_text = ?2, ocr_lines = ?3, ocr_version = ?4, ocr_error = NULL
         WHERE id = ?5",
        params![OCR_DONE, text, lines_json, version, id],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Records a failure. Text from an earlier successful scan is kept.
pub fn set_ocr_failed(conn: &Connection, id: &str, error: &str, version: i64) {
    conn.execute(
        "UPDATE clip_items SET ocr_status = ?1, ocr_error = ?2, ocr_version = ?3 WHERE id = ?4",
        params![OCR_FAILED, error, version, id],
    )
    .ok();
}

/// Puts one image at the front of the queue (keeps its current text).
pub fn queue_ocr(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE clip_items SET ocr_status = ?1, ocr_error = NULL
         WHERE id = ?2 AND kind IN ('image', 'screenshot')",
        params![OCR_PENDING, id],
    )
    .ok();
}

/// After a settings change: re-scan everything in the background.
pub fn mark_all_ocr_stale(conn: &Connection) {
    conn.execute("UPDATE clip_items SET ocr_version = 0 WHERE ocr_status = ?1", params![OCR_DONE])
        .ok();
    requeue_failed_ocr(conn);
}

pub fn requeue_failed_ocr(conn: &Connection) {
    conn.execute(
        "UPDATE clip_items SET ocr_status = ?1, ocr_error = NULL WHERE ocr_status = ?2",
        params![OCR_PENDING, OCR_FAILED],
    )
    .ok();
}

/// Startup recovery: an OCR interrupted by a crash or quit is not retried
/// automatically (it may be what crashed); everything else resumes.
pub fn fail_interrupted_ocr(conn: &Connection) {
    conn.execute(
        "UPDATE clip_items SET ocr_status = ?1, ocr_error = 'Reading was interrupted. Use Re-scan to try again.'
         WHERE ocr_status = ?2",
        params![OCR_FAILED, OCR_RUNNING],
    )
    .ok();
}

#[derive(Debug, PartialEq, Eq)]
pub struct OcrCounts {
    pub pending: i64,
    pub stale: i64,
    pub done: i64,
    pub failed: i64,
}

pub fn ocr_counts(conn: &Connection, version: i64) -> OcrCounts {
    conn.query_row(
        "SELECT
            COALESCE(SUM(ocr_status IN (?1, ?4)), 0),
            COALESCE(SUM(ocr_status = ?2 AND ocr_version < ?5), 0),
            COALESCE(SUM(ocr_status = ?2), 0),
            COALESCE(SUM(ocr_status = ?3), 0)
         FROM clip_items WHERE kind IN ('image', 'screenshot')",
        params![OCR_PENDING, OCR_DONE, OCR_FAILED, OCR_RUNNING, version],
        |row| Ok(OcrCounts { pending: row.get(0)?, stale: row.get(1)?, done: row.get(2)?, failed: row.get(3)? }),
    )
    .unwrap_or(OcrCounts { pending: 0, stale: 0, done: 0, failed: 0 })
}

/// An existing image with the same pixels: `(id, file name)`.
pub fn find_image_by_hash(conn: &Connection, hash: &str) -> Option<(String, String)> {
    conn.query_row(
        "SELECT id, content FROM clip_items
         WHERE content_hash = ?1 AND kind IN ('image', 'screenshot')
         ORDER BY created_at DESC LIMIT 1",
        params![hash],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

/// The newest item of `kind` recorded with `hash`.
pub fn find_item_by_hash(conn: &Connection, kind: &str, hash: &str) -> Option<String> {
    conn.query_row(
        "SELECT id FROM clip_items WHERE content_hash = ?1 AND kind = ?2 ORDER BY created_at DESC LIMIT 1",
        params![hash, kind],
        |row| row.get(0),
    )
    .ok()
}

pub fn update_text(conn: &Connection, id: &str, content: &str, preview: &str, hash: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE clip_items SET content = ?1, preview = ?2, content_hash = ?3, edited = 1 WHERE id = ?4",
        params![content, preview, hash, id],
    )
    .map(|_| ())
    .map_err(|e| format!("could not save the text: {e}"))
}

pub fn set_content_hash(conn: &Connection, id: &str, hash: &str) {
    conn.execute(
        "UPDATE clip_items SET content_hash = ?1 WHERE id = ?2",
        params![hash, id],
    )
    .ok();
}

/// Moves an existing item to the top of history (a repeat copy).
pub fn touch_item(conn: &Connection, id: &str, created_at: i64) {
    conn.execute(
        "UPDATE clip_items SET created_at = ?1 WHERE id = ?2",
        params![created_at, id],
    )
    .ok();
}

/// Derived files that belong to an item besides its main content file.
pub fn derived_image_files(id: &str) -> [String; 2] {
    [format!("{id}.original.png"), thumbnail_file(id)]
}

pub fn thumbnail_file(id: &str) -> String {
    format!("{id}.thumb.png")
}

pub fn insert_item(
    conn: &Connection,
    id: &str,
    kind: &str,
    content: &str,
    preview: &str,
    source_app: &str,
    created_at: i64,
    content_hash: &str,
) {
    conn.execute(
        "INSERT INTO clip_items (id, kind, content, preview, source_app, created_at, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            kind,
            content,
            preview,
            source_app,
            created_at,
            content_hash
        ],
    )
    .expect("failed to insert clip item");
}

pub fn most_recent_hash(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT content_hash FROM clip_items ORDER BY created_at DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .ok()
}

pub fn list_items(conn: &Connection, limit: i64) -> Vec<ClipItem> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM clip_items ORDER BY created_at DESC LIMIT ?1"
        ))
        .unwrap();
    let rows = stmt.query_map(params![limit], row_to_item).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

pub fn get_item(conn: &Connection, id: &str) -> Option<ClipItem> {
    conn.query_row(
        &format!("SELECT {ITEM_COLUMNS} FROM clip_items WHERE id = ?1"),
        params![id],
        row_to_item,
    )
    .ok()
}

/// An annotation changed the pixels, so the image is queued for OCR again.
pub fn mark_edited(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE clip_items SET edited = 1, ocr_status = ?1, ocr_text = NULL, ocr_lines = NULL, ocr_error = NULL
         WHERE id = ?2",
        params![OCR_PENDING, id],
    )
    .ok();
}

pub fn set_favorite(conn: &Connection, id: &str, favorite: bool) {
    conn.execute(
        "UPDATE clip_items SET favorite = ?1 WHERE id = ?2",
        params![favorite as i64, id],
    )
    .ok();
}

pub fn set_pinned(conn: &Connection, id: &str, pinned: bool) {
    conn.execute(
        "UPDATE clip_items SET pinned = ?1 WHERE id = ?2",
        params![pinned as i64, id],
    )
    .ok();
}

pub fn bump_usage(conn: &Connection, id: &str, used_at: i64) {
    conn.execute(
        "UPDATE clip_items SET use_count = use_count + 1, last_used_at = ?1 WHERE id = ?2",
        params![used_at, id],
    )
    .ok();
}

fn remove_image_files(images_dir: &std::path::Path, id: &str, content: &str) {
    std::fs::remove_file(images_dir.join(content)).ok();
    for derived in derived_image_files(id) {
        std::fs::remove_file(images_dir.join(derived)).ok();
    }
}

pub fn delete_item(conn: &Connection, data_dir: &PathBuf, id: &str) {
    if let Some(item) = get_item(conn, id) {
        if item.kind == "image" || item.kind == "screenshot" {
            remove_image_files(&data_dir.join("images"), id, &item.content);
        }
    }
    conn.execute("DELETE FROM clip_items WHERE id = ?1", params![id])
        .ok();
}

pub fn clear_all(conn: &Connection, data_dir: &PathBuf) {
    conn.execute("DELETE FROM clip_items", []).ok();
    let images_dir = data_dir.join("images");
    if let Ok(entries) = std::fs::read_dir(&images_dir) {
        for entry in entries.flatten() {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

/// The "Clear" action used day to day: deletes everything EXCEPT favorited
/// and pinned items (and the deleted items' image files, including any
/// preserved original and thumbnail). Snippets live in a separate database
/// and are never touched.
pub fn clear_non_favorites(conn: &Connection, data_dir: &PathBuf) {
    let images_dir = data_dir.join("images");
    let mut stmt = conn
        .prepare("SELECT id, kind, content FROM clip_items WHERE favorite = 0 AND pinned = 0")
        .unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    for (id, kind, content) in rows {
        if kind == "image" || kind == "screenshot" {
            remove_image_files(&images_dir, &id, &content);
        }
    }
    conn.execute("DELETE FROM clip_items WHERE favorite = 0 AND pinned = 0", [])
        .ok();
}

/// Which items automatic cleanup may remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Retention {
    /// 0 = keep forever (the default; cleanup never runs until a user picks a period).
    pub days: u32,
    pub include_favorites: bool,
    pub include_pinned: bool,
}

const RETENTION_KEY: &str = "history.retention";

impl Default for Retention {
    fn default() -> Self {
        Self { days: 0, include_favorites: false, include_pinned: false }
    }
}

pub fn load_retention(conn: &Connection) -> Retention {
    get_setting(conn, RETENTION_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn save_retention(conn: &Connection, retention: &Retention) {
    if let Ok(json) = serde_json::to_string(retention) {
        set_setting(conn, RETENTION_KEY, &json);
    }
}

/// Items captured before `cutoff` (epoch ms) that `retention` lets cleanup
/// remove. Based on the original capture time; an item dated in the future
/// (the clock moved back) is never expired.
const EXPIRED_WHERE: &str = "created_at < ?1 AND (favorite = 0 OR ?2) AND (pinned = 0 OR ?3)";

pub fn count_expired(conn: &Connection, retention: &Retention, now: i64) -> i64 {
    if retention.days == 0 {
        return 0;
    }
    let cutoff = now - i64::from(retention.days) * 86_400_000;
    conn.query_row(
        &format!("SELECT COUNT(*) FROM clip_items WHERE {EXPIRED_WHERE}"),
        params![cutoff, retention.include_favorites, retention.include_pinned],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// Deletes expired items in one transaction, then removes the Clipet-managed
/// image files no remaining item uses. Files that file items point to belong
/// to the user and are never touched. Returns how many items were removed.
pub fn delete_expired(conn: &Connection, data_dir: &PathBuf, retention: &Retention, now: i64) -> Result<usize, String> {
    if retention.days == 0 {
        return Ok(0);
    }
    let cutoff = now - i64::from(retention.days) * 86_400_000;
    let flags = params![cutoff, retention.include_favorites, retention.include_pinned];
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let images: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare(&format!(
                "SELECT id, content FROM clip_items WHERE {EXPIRED_WHERE} AND kind IN ('image', 'screenshot')"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(flags, |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let removed = tx
        .execute(&format!("DELETE FROM clip_items WHERE {EXPIRED_WHERE}"), flags)
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;

    // After the commit: a crash here leaves only unused files, never rows
    // pointing at deleted images.
    let images_dir = data_dir.join("images");
    for (id, content) in images {
        let still_used: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM clip_items WHERE content = ?1 AND kind IN ('image', 'screenshot'))", params![content], |row| row.get(0))
            .unwrap_or(true);
        if !still_used {
            remove_image_files(&images_dir, &id, &content);
        }
    }
    Ok(removed)
}

pub fn storage_usage_bytes(data_dir: &PathBuf) -> u64 {
    fn dir_size(p: &std::path::Path) -> u64 {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_dir() {
                        total += dir_size(&entry.path());
                    } else {
                        total += meta.len();
                    }
                }
            }
        }
        total
    }
    dir_size(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests run concurrently on separate threads within the same process, so a
    // timestamp-based name isn't a reliable way to get a unique directory per
    // test (two tests can start within the same clock tick and collide on the
    // same path, silently sharing — and corrupting — each other's SQLite DB).
    // An atomic counter guarantees uniqueness regardless of timing.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("clipflow-test-{}-{}", std::process::id(), n));
        p
    }

    #[test]
    fn insert_and_list_roundtrip() {
        let dir = temp_dir();
        let conn = open(&dir);
        insert_item(
            &conn, "a", "text", "hello", "hello", "TestApp", 100, "hash-a",
        );
        insert_item(
            &conn,
            "b",
            "link",
            "https://x.com",
            "https://x.com",
            "TestApp",
            200,
            "hash-b",
        );

        let items = list_items(&conn, 10);
        assert_eq!(items.len(), 2);
        // newest first
        assert_eq!(items[0].id, "b");
        assert_eq!(items[1].id, "a");
        assert_eq!(items[0].kind, "link");
        assert!(!items[0].favorite);
        assert!(!items[0].pinned);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn favorite_pin_and_usage_mutations() {
        let dir = temp_dir();
        let conn = open(&dir);
        insert_item(&conn, "a", "text", "hello", "hello", "", 100, "hash-a");

        set_favorite(&conn, "a", true);
        set_pinned(&conn, "a", true);
        bump_usage(&conn, "a", 999);

        let item = get_item(&conn, "a").expect("item should exist");
        assert!(item.favorite);
        assert!(item.pinned);
        assert_eq!(item.use_count, 1);
        assert_eq!(item.last_used_at, Some(999));

        bump_usage(&conn, "a", 1000);
        let item2 = get_item(&conn, "a").unwrap();
        assert_eq!(item2.use_count, 2);

        set_favorite(&conn, "a", false);
        assert!(!get_item(&conn, "a").unwrap().favorite);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_row_and_image_file() {
        let dir = temp_dir();
        let conn = open(&dir);
        let img_path = dir.join("images").join("pic.png");
        std::fs::write(&img_path, b"fake-png-bytes").unwrap();
        insert_item(
            &conn, "img1", "image", "pic.png", "4x4", "", 100, "hash-img",
        );

        assert!(img_path.exists());
        delete_item(&conn, &dir, "img1");
        assert!(get_item(&conn, "img1").is_none());
        assert!(
            !img_path.exists(),
            "deleting an image item should remove its backing file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_all_empties_table_and_images_dir() {
        let dir = temp_dir();
        let conn = open(&dir);
        insert_item(&conn, "a", "text", "hello", "hello", "", 100, "hash-a");
        std::fs::write(dir.join("images").join("orphan.png"), b"x").unwrap();

        clear_all(&conn, &dir);

        assert_eq!(list_items(&conn, 10).len(), 0);
        let remaining: Vec<_> = std::fs::read_dir(dir.join("images")).unwrap().collect();
        assert_eq!(remaining.len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_non_favorites_keeps_favorited_items_and_files() {
        let dir = temp_dir();
        let conn = open(&dir);
        insert_item(
            &conn,
            "keep-text",
            "text",
            "important",
            "important",
            "",
            100,
            "hash-1",
        );
        set_favorite(&conn, "keep-text", true);

        std::fs::write(dir.join("images").join("keep.png"), b"keep-bytes").unwrap();
        insert_item(
            &conn, "keep-img", "image", "keep.png", "4x4", "", 200, "hash-2",
        );
        set_favorite(&conn, "keep-img", true);

        std::fs::write(dir.join("images").join("drop.png"), b"drop-bytes").unwrap();
        insert_item(
            &conn, "drop-img", "image", "drop.png", "4x4", "", 300, "hash-3",
        );
        insert_item(
            &conn,
            "drop-text",
            "text",
            "forget me",
            "forget me",
            "",
            400,
            "hash-4",
        );

        clear_non_favorites(&conn, &dir);

        let remaining = list_items(&conn, 10);
        let remaining_ids: Vec<_> = remaining.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            remaining.len(),
            2,
            "only the two favorited items should remain"
        );
        assert!(remaining_ids.contains(&"keep-text"));
        assert!(remaining_ids.contains(&"keep-img"));

        assert!(
            dir.join("images").join("keep.png").exists(),
            "favorited image's file must survive the clear"
        );
        assert!(
            !dir.join("images").join("drop.png").exists(),
            "non-favorited image's file must be removed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn most_recent_hash_tracks_latest_insert() {
        let dir = temp_dir();
        let conn = open(&dir);
        assert_eq!(most_recent_hash(&conn), None);
        insert_item(&conn, "a", "text", "hello", "hello", "", 100, "hash-a");
        insert_item(&conn, "b", "text", "world", "world", "", 200, "hash-b");
        assert_eq!(most_recent_hash(&conn), Some("hash-b".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }
}
