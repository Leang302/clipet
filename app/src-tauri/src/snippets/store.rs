//! Snippet persistence.
//!
//! Snippets are saved user content, not history: they live in their own
//! SQLite file (`snippets.sqlite3`), separate from clipboard history
//! (`clipflow.sqlite3`) and its `images/` cache. No history, cache or
//! temporary-data cleanup opens this file, so only an explicit snippet delete
//! can remove a snippet. Image snippets keep their PNG in `snippet_images/`
//! for the same reason: history cleanup only touches `images/`.

use super::engine::TriggerMode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SNIPPETS_DB_FILE: &str = "snippets.sqlite3";
pub const SNIPPET_IMAGES_DIR: &str = "snippet_images";

pub const TEXT: &str = "text";

/// What follows the inserted text: nothing ("exact", the default), one
/// space, or a line break. The trigger key itself is never inserted.
pub fn suffix_text(suffix: &str) -> &'static str {
    match suffix {
        "space" => " ",
        "newline" => "\n",
        _ => "",
    }
}

pub fn valid_suffix(suffix: &str) -> bool {
    matches!(suffix, "exact" | "space" | "newline")
}
pub const IMAGE: &str = "image";

/// Where an image snippet's PNG lives. `content` holds only the file name.
pub fn image_path(data_dir: &Path, snippet: &Snippet) -> Option<std::path::PathBuf> {
    (snippet.content_type == IMAGE).then(|| data_dir.join(SNIPPET_IMAGES_DIR).join(&snippet.content))
}

/// Bumped whenever a migration step is added below. Stored in SQLite's own
/// `PRAGMA user_version`, so each step runs exactly once per database.
const SCHEMA_VERSION: i64 = 4;

pub fn open(data_dir: &Path) -> Connection {
    std::fs::create_dir_all(data_dir).expect("failed to create app data dir");
    let conn =
        Connection::open(data_dir.join(SNIPPETS_DB_FILE)).expect("failed to open snippets db");
    migrate(&conn);
    conn
}

fn migrate(conn: &Connection) {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS snippets (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 keyword TEXT NOT NULL,
                 content TEXT NOT NULL,
                 content_type TEXT NOT NULL DEFAULT 'text',
                 category TEXT,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 favorite INTEGER NOT NULL DEFAULT 0,
                 use_count INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 last_used_at INTEGER,
                 app_rules TEXT NOT NULL DEFAULT ''
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_snippets_keyword
                 ON snippets(keyword COLLATE NOCASE);
             CREATE TABLE IF NOT EXISTS app_settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )
        .expect("failed to migrate database to schema version 1");
    }
    if version < 2 {
        // Per-snippet whitespace after the inserted text (see `suffix_text`).
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE snippets ADD COLUMN suffix TEXT NOT NULL DEFAULT 'exact';
             PRAGMA user_version = 2;
             COMMIT;",
        )
        .expect("failed to migrate database to schema version 2");
    }
    if version < 3 {
        // Parameter configuration for dynamic `{{name}}` snippets, stored as
        // a JSON array of `params::Parameter`. `'[]'` means "no parameters",
        // so every pre-existing (static) snippet keeps expanding exactly as
        // before (spec Part I: backward-compatible reads, no data loss).
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE snippets ADD COLUMN parameters TEXT NOT NULL DEFAULT '[]';
             PRAGMA user_version = 3;
             COMMIT;",
        )
        .expect("failed to migrate database to schema version 3");
    }
    if version < 4 {
        // Optional per-OS versions of the template, as a JSON object keyed by
        // "macos" / "windows" / "linux". `'{}'` means one template for every
        // OS (`content`), which is what every pre-v4 snippet reads back as.
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE snippets ADD COLUMN variants TEXT NOT NULL DEFAULT '{}';
             PRAGMA user_version = 4;
             COMMIT;",
        )
        .expect("failed to migrate database to schema version 4");
    }
    debug_assert!(SCHEMA_VERSION == 4, "add the new migration step above");
}

pub const PLATFORMS: [&str; 3] = ["macos", "windows", "linux"];

/// The OS key this build runs on, matching the `variants` JSON keys.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// Parses the `variants` column; anything unreadable means "no variants".
pub fn parse_variants(json: &str) -> std::collections::BTreeMap<String, String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The template a snippet uses on `platform`: its non-empty variant for that
/// OS if it has one, otherwise the default `content` (the documented fallback).
pub fn resolve_template(content: &str, variants_json: &str, platform: &str) -> String {
    parse_variants(variants_json)
        .remove(platform)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| content.to_string())
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    /// The shortcut without its prefix: `name` for `;name`.
    pub keyword: String,
    pub content: String,
    pub content_type: String,
    pub category: Option<String>,
    pub enabled: bool,
    pub favorite: bool,
    pub use_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub suffix: String,
    /// JSON array of `params::Parameter`; `"[]"` means a static snippet with
    /// no `{{placeholder}}`s, which is how every pre-v3 row reads back.
    pub parameters: String,
    /// JSON object of per-OS templates (see `resolve_template`); `"{}"` for none.
    pub variants: String,
}

const SNIPPET_COLUMNS: &str = "id, name, keyword, content, content_type, category, enabled, favorite, use_count, created_at, updated_at, last_used_at, suffix, parameters, variants";

fn row_to_snippet(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snippet> {
    Ok(Snippet {
        id: row.get(0)?,
        name: row.get(1)?,
        keyword: row.get(2)?,
        content: row.get(3)?,
        content_type: row.get(4)?,
        category: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        favorite: row.get::<_, i64>(7)? != 0,
        use_count: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        last_used_at: row.get(11)?,
        suffix: row.get(12)?,
        parameters: row.get(13)?,
        variants: row.get(14)?,
    })
}

pub fn list_snippets(conn: &Connection) -> Vec<Snippet> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SNIPPET_COLUMNS} FROM snippets ORDER BY keyword COLLATE NOCASE"
        ))
        .unwrap();
    stmt.query_map([], row_to_snippet)
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

pub fn get_snippet(conn: &Connection, id: &str) -> Option<Snippet> {
    conn.query_row(
        &format!("SELECT {SNIPPET_COLUMNS} FROM snippets WHERE id = ?1"),
        params![id],
        row_to_snippet,
    )
    .ok()
}

/// Another snippet already using `keyword` (case-insensitively, matching the
/// unique index), ignoring the snippet being edited.
pub fn keyword_owner(conn: &Connection, keyword: &str, except_id: Option<&str>) -> Option<Snippet> {
    conn.query_row(
        &format!(
            "SELECT {SNIPPET_COLUMNS} FROM snippets
             WHERE keyword = ?1 COLLATE NOCASE AND id != ?2"
        ),
        params![keyword, except_id.unwrap_or("")],
        row_to_snippet,
    )
    .optional()
    .ok()
    .flatten()
}

pub fn insert_snippet(conn: &Connection, snippet: &Snippet) -> Result<(), String> {
    conn.execute(
        "INSERT INTO snippets (id, name, keyword, content, content_type, category, enabled, favorite, use_count, created_at, updated_at, last_used_at, suffix, parameters, variants)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            snippet.id,
            snippet.name,
            snippet.keyword,
            snippet.content,
            snippet.content_type,
            snippet.category,
            snippet.enabled as i64,
            snippet.favorite as i64,
            snippet.use_count,
            snippet.created_at,
            snippet.updated_at,
            snippet.last_used_at,
            snippet.suffix,
            snippet.parameters,
            snippet.variants,
        ],
    )
    .map(|_| ())
    .map_err(|e| format!("could not save the snippet: {e}"))
}

pub fn update_snippet(
    conn: &Connection,
    id: &str,
    name: &str,
    keyword: &str,
    content: &str,
    content_type: &str,
    suffix: &str,
    enabled: bool,
    updated_at: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE snippets SET name = ?1, keyword = ?2, content = ?3, content_type = ?4, enabled = ?5, updated_at = ?6, suffix = ?8
             WHERE id = ?7",
            params![name, keyword, content, content_type, enabled as i64, updated_at, id, suffix],
        )
        .map_err(|e| format!("could not save the snippet: {e}"))?;
    if changed == 0 {
        return Err("that snippet no longer exists".to_string());
    }
    Ok(())
}

/// Replaces a snippet's parameter configuration (JSON array of
/// `params::Parameter`) and per-OS variants (JSON object), independent of the
/// other editable fields.
pub fn update_snippet_template_config(
    conn: &Connection,
    id: &str,
    parameters_json: &str,
    variants_json: &str,
    updated_at: i64,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE snippets SET parameters = ?1, variants = ?2, updated_at = ?3 WHERE id = ?4",
            params![parameters_json, variants_json, updated_at, id],
        )
        .map_err(|e| format!("could not save the snippet's parameters: {e}"))?;
    if changed == 0 {
        return Err("that snippet no longer exists".to_string());
    }
    Ok(())
}

pub fn set_snippet_enabled(conn: &Connection, id: &str, enabled: bool, updated_at: i64) {
    conn.execute(
        "UPDATE snippets SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
        params![enabled as i64, updated_at, id],
    )
    .ok();
}

pub fn delete_snippet(conn: &Connection, id: &str) {
    conn.execute("DELETE FROM snippets WHERE id = ?1", params![id])
        .ok();
}

pub fn bump_snippet_usage(conn: &Connection, id: &str, used_at: i64) {
    conn.execute(
        "UPDATE snippets SET use_count = use_count + 1, last_used_at = ?1 WHERE id = ?2",
        params![used_at, id],
    )
    .ok();
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SnippetSettings {
    pub enabled: bool,
    pub prefix: String,
    pub trigger_mode: TriggerMode,
    pub escape_enabled: bool,
    pub case_sensitive: bool,
    pub default_pause_minutes: u32,
    /// Backspace right after an expansion restores the typed shortcut. Off
    /// by default: a Backspace then only deletes one character, as before.
    pub undo_backspace: bool,
}

impl Default for SnippetSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: ";".to_string(),
            trigger_mode: TriggerMode::Space,
            escape_enabled: true,
            case_sensitive: true,
            default_pause_minutes: 15,
            undo_backspace: false,
        }
    }
}

const SETTINGS_KEY: &str = "snippets.settings";
const PAUSE_KEY: &str = "snippets.pause";

fn read_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

fn write_setting(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(|e| format!("could not save settings: {e}"))
}

/// Unknown or corrupt stored settings fall back to defaults field by field
/// (`#[serde(default)]`), so adding a setting never breaks older databases.
pub fn load_settings(conn: &Connection) -> SnippetSettings {
    read_setting(conn, SETTINGS_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn save_settings(conn: &Connection, settings: &SnippetSettings) -> Result<(), String> {
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    write_setting(conn, SETTINGS_KEY, &json)
}

/// Persisted pause, so a restart keeps a pause the user asked for instead of
/// silently resuming (a timed pause that expired meanwhile simply ends).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "until", rename_all = "lowercase")]
pub enum PauseState {
    Active,
    Until(i64),
    Indefinite,
}

pub fn load_pause(conn: &Connection) -> PauseState {
    read_setting(conn, PAUSE_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or(PauseState::Active)
}

pub fn save_pause(conn: &Connection, pause: PauseState) -> Result<(), String> {
    let json = serde_json::to_string(&pause).map_err(|e| e.to_string())?;
    write_setting(conn, PAUSE_KEY, &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::path::PathBuf;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self(std::env::temp_dir().join(format!(
                "clipet-snippet-test-{}-{n}",
                std::process::id()
            )))
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn snippet(id: &str, keyword: &str) -> Snippet {
        Snippet {
            id: id.into(),
            name: format!("Snippet {id}"),
            keyword: keyword.into(),
            content: format!("content of {id}"),
            content_type: "text".into(),
            category: None,
            enabled: true,
            favorite: false,
            use_count: 0,
            created_at: 1,
            updated_at: 1,
            last_used_at: None,
            suffix: "exact".into(),
            parameters: "[]".into(),
            variants: "{}".into(),
        }
    }

    #[test]
    fn crud_roundtrip_survives_reopen() {
        let dir = TempDir::new();
        {
            let conn = open(&dir.0);
            insert_snippet(&conn, &snippet("a", "name")).unwrap();
            update_snippet(&conn, "a", "Name", "name", "KimLeang", TEXT, "newline", true, 5).unwrap();
            set_snippet_enabled(&conn, "a", false, 6);
            bump_snippet_usage(&conn, "a", 7);
        }
        let conn = open(&dir.0);
        let got = get_snippet(&conn, "a").unwrap();
        assert_eq!(got.content, "KimLeang");
        assert_eq!(got.suffix, "newline");
        assert!(!got.enabled);
        assert_eq!(got.use_count, 1);
        assert_eq!(got.last_used_at, Some(7));
        assert_eq!(got.updated_at, 6);

        delete_snippet(&conn, "a");
        assert!(list_snippets(&conn).is_empty());
    }

    #[test]
    fn keywords_are_unique_case_insensitively() {
        let dir = TempDir::new();
        let conn = open(&dir.0);
        insert_snippet(&conn, &snippet("a", "name")).unwrap();
        assert_eq!(keyword_owner(&conn, "NAME", None).unwrap().id, "a");
        assert!(keyword_owner(&conn, "name", Some("a")).is_none());
        assert!(insert_snippet(&conn, &snippet("b", "Name")).is_err());
    }

    #[test]
    fn no_history_or_cache_cleanup_removes_snippets() {
        let dir = TempDir::new();
        insert_snippet(&open(&dir.0), &snippet("a", "name")).unwrap();
        let image = Snippet {
            content: "logo.png".into(),
            content_type: IMAGE.into(),
            ..snippet("b", "logo")
        };
        insert_snippet(&open(&dir.0), &image).unwrap();
        let image_file = image_path(&dir.0, &image).unwrap();
        std::fs::create_dir_all(image_file.parent().unwrap()).unwrap();
        std::fs::write(&image_file, b"png").unwrap();
        save_settings(
            &open(&dir.0),
            &SnippetSettings {
                prefix: "/".into(),
                ..SnippetSettings::default()
            },
        )
        .unwrap();

        let history = db::open(&dir.0);
        db::insert_item(&history, "h", "text", "x", "x", "", 1, "hash");
        std::fs::write(dir.0.join("images").join("cached.png"), b"x").unwrap();
        db::clear_non_favorites(&history, &dir.0);
        db::clear_all(&history, &dir.0);
        drop(history);
        // Even wiping the history database file and the image cache entirely
        // leaves snippets and their settings untouched.
        std::fs::remove_file(dir.0.join("clipflow.sqlite3")).unwrap();
        std::fs::remove_dir_all(dir.0.join("images")).unwrap();

        let conn = open(&dir.0);
        assert_eq!(list_snippets(&conn).len(), 2);
        assert_eq!(load_settings(&conn).prefix, "/");
        assert!(image_file.exists(), "image snippets keep their file");
    }

    #[test]
    fn settings_and_pause_roundtrip_with_defaults() {
        let dir = TempDir::new();
        let conn = open(&dir.0);
        assert_eq!(load_settings(&conn), SnippetSettings::default());
        assert_eq!(load_pause(&conn), PauseState::Active);

        let settings = SnippetSettings {
            trigger_mode: TriggerMode::Both,
            escape_enabled: false,
            ..SnippetSettings::default()
        };
        save_settings(&conn, &settings).unwrap();
        save_pause(&conn, PauseState::Until(123)).unwrap();
        assert_eq!(load_settings(&conn), settings);
        assert_eq!(load_pause(&conn), PauseState::Until(123));

        // A partial/older settings blob keeps defaults for missing fields.
        write_setting(&conn, SETTINGS_KEY, r#"{"prefix":"/"}"#).unwrap();
        let partial = load_settings(&conn);
        assert_eq!(partial.prefix, "/");
        assert!(partial.escape_enabled);
    }

    #[test]
    fn version_1_databases_gain_exact_suffix_and_keep_snippets() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        {
            // A database exactly as schema version 1 created it.
            let conn = Connection::open(dir.0.join(SNIPPETS_DB_FILE)).unwrap();
            conn.execute_batch(
                "CREATE TABLE snippets (id TEXT PRIMARY KEY, name TEXT NOT NULL, keyword TEXT NOT NULL,
                   content TEXT NOT NULL, content_type TEXT NOT NULL DEFAULT 'text', category TEXT,
                   enabled INTEGER NOT NULL DEFAULT 1, favorite INTEGER NOT NULL DEFAULT 0,
                   use_count INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL, last_used_at INTEGER, app_rules TEXT NOT NULL DEFAULT '');
                 CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO snippets (id, name, keyword, content, created_at, updated_at)
                   VALUES ('old', 'Old', 'old', 'Hello World', 1, 1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let conn = open(&dir.0);
        let old = get_snippet(&conn, "old").unwrap();
        assert_eq!((old.content.as_str(), old.suffix.as_str()), ("Hello World", "exact"));
        assert_eq!(old.parameters, "[]");
        assert_eq!(old.variants, "{}");
        assert_eq!(resolve_template(&old.content, &old.variants, "windows"), "Hello World");
    }

    #[test]
    fn version_3_databases_gain_empty_variants_and_keep_parameters() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        {
            let conn = Connection::open(dir.0.join(SNIPPETS_DB_FILE)).unwrap();
            conn.execute_batch(
                "CREATE TABLE snippets (id TEXT PRIMARY KEY, name TEXT NOT NULL, keyword TEXT NOT NULL,
                   content TEXT NOT NULL, content_type TEXT NOT NULL DEFAULT 'text', category TEXT,
                   enabled INTEGER NOT NULL DEFAULT 1, favorite INTEGER NOT NULL DEFAULT 0,
                   use_count INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL, last_used_at INTEGER, app_rules TEXT NOT NULL DEFAULT '',
                   suffix TEXT NOT NULL DEFAULT 'exact', parameters TEXT NOT NULL DEFAULT '[]');
                 CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO snippets (id, name, keyword, content, created_at, updated_at, parameters)
                   VALUES ('kp', 'Kill', 'killport', 'lsof {{port}}', 1, 1, '[{\"name\":\"port\",\"kind\":\"integer\",\"min\":null,\"max\":null,\"required\":true,\"default\":null}]');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let conn = open(&dir.0);
        let kp = get_snippet(&conn, "kp").unwrap();
        assert_eq!(kp.variants, "{}");
        assert!(kp.parameters.contains("\"port\""));
    }

    #[test]
    fn variants_resolve_per_platform_with_content_as_fallback() {
        let variants = r#"{"windows":"win {{p}}","linux":""}"#;
        assert_eq!(resolve_template("default {{p}}", variants, "windows"), "win {{p}}");
        assert_eq!(resolve_template("default {{p}}", variants, "macos"), "default {{p}}");
        // An empty variant counts as "no variant", not "insert nothing".
        assert_eq!(resolve_template("default {{p}}", variants, "linux"), "default {{p}}");
        assert_eq!(resolve_template("default", "not json", "macos"), "default");
    }

    #[test]
    fn template_config_update_leaves_other_fields_untouched() {
        let dir = TempDir::new();
        let conn = open(&dir.0);
        insert_snippet(&conn, &snippet("a", "killport")).unwrap();
        update_snippet_template_config(&conn, "a", r#"[{"name":"port"}]"#, r#"{"windows":"w"}"#, 42).unwrap();
        let got = get_snippet(&conn, "a").unwrap();
        assert_eq!(got.parameters, r#"[{"name":"port"}]"#);
        assert_eq!(got.variants, r#"{"windows":"w"}"#);
        assert_eq!(got.updated_at, 42);
        assert_eq!(got.content, "content of a");
        assert!(update_snippet_template_config(&conn, "missing", "[]", "{}", 1).is_err());
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = TempDir::new();
        let conn = open(&dir.0);
        migrate(&conn);
        migrate(&conn);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
