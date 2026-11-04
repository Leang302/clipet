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
}

pub fn open(data_dir: &PathBuf) -> Connection {
    std::fs::create_dir_all(data_dir).expect("failed to create app data dir");
    std::fs::create_dir_all(data_dir.join("images")).expect("failed to create images dir");
    let conn =
        Connection::open(data_dir.join("clipflow.sqlite3")).expect("failed to open sqlite db");
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
    conn
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
        .prepare(
            "SELECT id, kind, content, preview, source_app, created_at, last_used_at, use_count, favorite, pinned, edited
             FROM clip_items ORDER BY created_at DESC LIMIT ?1",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![limit], |row| {
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
            })
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

pub fn get_item(conn: &Connection, id: &str) -> Option<ClipItem> {
    conn.query_row(
        "SELECT id, kind, content, preview, source_app, created_at, last_used_at, use_count, favorite, pinned, edited
         FROM clip_items WHERE id = ?1",
        params![id],
        |row| {
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
            })
        },
    )
    .ok()
}

pub fn mark_edited(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE clip_items SET edited = 1 WHERE id = ?1",
        params![id],
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

pub fn delete_item(conn: &Connection, data_dir: &PathBuf, id: &str) {
    if let Some(item) = get_item(conn, id) {
        if item.kind == "image" || item.kind == "screenshot" {
            let path = data_dir.join("images").join(&item.content);
            std::fs::remove_file(path).ok();
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

/// The "Clear" action non-favorite items use day to day: deletes everything
/// EXCEPT favorited items (and their backing image files, including any
/// preserved `<id>.original.png`), so favorites survive a clear like most
/// clipboard managers' users expect.
pub fn clear_non_favorites(conn: &Connection, data_dir: &PathBuf) {
    let images_dir = data_dir.join("images");
    let mut stmt = conn
        .prepare("SELECT id, kind, content FROM clip_items WHERE favorite = 0")
        .unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    for (id, kind, content) in rows {
        if kind == "image" || kind == "screenshot" {
            std::fs::remove_file(images_dir.join(&content)).ok();
            std::fs::remove_file(images_dir.join(format!("{id}.original.png"))).ok();
        }
    }
    conn.execute("DELETE FROM clip_items WHERE favorite = 0", [])
        .ok();
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
