use crate::db;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub struct MonitorState {
    pub paused: AtomicBool,
    // Hash of the last clipboard content WE wrote ourselves, so our own writes
    // (e.g. a future "copy annotated screenshot back to clipboard" action) never
    // get re-captured into history as if the user copied something new.
    pub last_self_write_hash: Mutex<Option<String>>,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            last_self_write_hash: Mutex::new(None),
        }
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn spawn_monitor(app: AppHandle, data_dir: PathBuf, monitor: Arc<MonitorState>) {
    std::thread::spawn(move || {
        let conn = db::open(&data_dir);
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[clipflow] failed to access system clipboard: {e}");
                return;
            }
        };

        // Seed with whatever is already on the clipboard at launch so we don't
        // immediately re-ingest it as a "new" copy.
        let mut last_hash = clipboard
            .get_text()
            .ok()
            .map(|t| hash_bytes(t.as_bytes()))
            .or_else(|| db::most_recent_hash(&conn));

        loop {
            std::thread::sleep(Duration::from_millis(400));

            if monitor.paused.load(Ordering::Relaxed) {
                continue;
            }

            if let Ok(text) = clipboard.get_text() {
                if !text.trim().is_empty() {
                    let h = hash_bytes(text.as_bytes());
                    if Some(&h) != last_hash.as_ref() && !is_self_write(&monitor, &h) {
                        ingest_text(&conn, &app, &text, &h);
                        last_hash = Some(h);
                    }
                    continue;
                }
            }

            if let Ok(img) = clipboard.get_image() {
                let h = hash_bytes(&img.bytes);
                if Some(&h) != last_hash.as_ref() && !is_self_write(&monitor, &h) {
                    ingest_image(&conn, &app, &data_dir, &img, &h);
                    last_hash = Some(h);
                }
            }
        }
    });
}

fn is_self_write(monitor: &Arc<MonitorState>, hash: &str) -> bool {
    monitor
        .last_self_write_hash
        .lock()
        .unwrap()
        .as_deref()
        .map(|h| h == hash)
        .unwrap_or(false)
}

fn ingest_text(conn: &Connection, app: &AppHandle, text: &str, hash: &str) {
    let id = uuid_like();
    let preview: String = text.chars().take(120).collect();
    let created_at = chrono::Utc::now().timestamp_millis();
    let kind = if looks_like_url(text) { "link" } else { "text" };
    db::insert_item(conn, &id, kind, text, &preview, "", created_at, hash);
    let _ = app.emit("clipflow://new-item", &id);
}

fn ingest_image(
    conn: &Connection,
    app: &AppHandle,
    data_dir: &PathBuf,
    img: &arboard::ImageData,
    hash: &str,
) {
    // Copying an image that's already in history moves it to the top instead
    // of storing (and OCR-ing) a second identical file.
    if let Some((existing, _)) = db::find_image_by_hash(conn, hash) {
        db::touch_item(conn, &existing, chrono::Utc::now().timestamp_millis());
        let _ = app.emit("clipflow://new-item", &existing);
        return;
    }
    let id = uuid_like();
    let file_name = format!("{id}.png");
    let path = data_dir.join("images").join(&file_name);

    let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.to_vec());
    if let Some(buf) = buf {
        if let Err(e) = buf.save(&path) {
            eprintln!("[clipflow] failed to save clipboard image: {e}");
            return;
        }
    } else {
        return;
    }

    let preview = format!("{}x{}", img.width, img.height);
    let created_at = chrono::Utc::now().timestamp_millis();
    db::insert_item(
        conn, &id, "image", &file_name, &preview, "", created_at, hash,
    );
    let _ = app.emit("clipflow://new-item", &id);
    crate::ocr::wake(app);
}

fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with("http://") || s.starts_with("https://")) && !s.contains(' ') && !s.contains('\n')
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos)
}
