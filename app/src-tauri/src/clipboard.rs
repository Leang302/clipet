use crate::db;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub struct MonitorState {
    pub paused: AtomicBool,
    /// Epoch ms when a timed pause ends on its own; 0 = until resumed.
    pub paused_until: AtomicI64,
    // Hash of the last clipboard content WE wrote ourselves, so our own writes
    // (e.g. a future "copy annotated screenshot back to clipboard" action) never
    // get re-captured into history as if the user copied something new.
    pub last_self_write_hash: Mutex<Option<String>>,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            paused_until: AtomicI64::new(0),
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
                let until = monitor.paused_until.load(Ordering::Relaxed);
                if until == 0 || chrono::Utc::now().timestamp_millis() < until {
                    continue;
                }
                monitor.paused.store(false, Ordering::Relaxed);
                monitor.paused_until.store(0, Ordering::Relaxed);
            }

            // Password managers and similar apps mark what they copy as
            // private; it is never recorded (see `is_private_clipboard`).
            if is_private_clipboard() {
                continue;
            }

            // Real file references (Finder / Explorer copies) come first:
            // those clipboards also carry the file name as text and often an
            // icon image, which must not be recorded instead of the files.
            // Clipet's own image pastes publish file URLs into its image
            // store; those fall through to the image check as before.
            if let Some(paths) = clipboard_file_paths(&mut clipboard) {
                let images_dir = data_dir.join("images");
                if !paths.is_empty() && !paths.iter().all(|p| p.starts_with(&images_dir)) {
                    let h = hash_file_list(&paths);
                    if Some(&h) != last_hash.as_ref() && !is_self_write(&monitor, &h) {
                        ingest_files(&conn, &app, &paths, &h);
                        last_hash = Some(h);
                    }
                    continue;
                }
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

/// Every file reference on the clipboard, in copy order. On macOS each
/// pasteboard item's `public.file-url` is read directly, and names are
/// normalized (see below) so the same files always produce the same entry.
#[cfg(target_os = "macos")]
pub(crate) fn clipboard_file_paths(_clipboard: &mut arboard::Clipboard) -> Option<Vec<PathBuf>> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    use objc2_foundation::{NSString, NSURL};
    objc2::rc::autoreleasepool(|_| {
        let items = NSPasteboard::generalPasteboard().pasteboardItems()?;
        let paths: Vec<PathBuf> = items
            .iter()
            .filter_map(|item| {
                let url = unsafe { item.stringForType(NSPasteboardTypeFileURL) }?;
                let url = NSURL::URLWithString(&NSString::from_str(&url.to_string()))?;
                if !url.isFileURL() {
                    return None;
                }
                // NSURL returns decomposed Unicode (NFD) for names such as
                // Korean; store the composed form (NFC) so the same files
                // always compare equal. macOS file systems accept both.
                use unicode_normalization::UnicodeNormalization;
                url.path().map(|p| PathBuf::from(p.to_string().nfc().collect::<String>()))
            })
            .collect();
        (!paths.is_empty()).then_some(paths)
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn clipboard_file_paths(clipboard: &mut arboard::Clipboard) -> Option<Vec<PathBuf>> {
    clipboard.get().file_list().ok().filter(|paths| !paths.is_empty())
}

/// The hash a file list is recorded and compared under.
pub fn hash_file_list(paths: &[PathBuf]) -> String {
    let joined: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    hash_bytes(format!("files\n{}", joined.join("\n")).as_bytes())
}

/// Display text for a file list: its names, e.g. "report.pdf, data.xlsx +2".
pub fn file_list_preview(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned()))
        .collect();
    let shown = names.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
    match names.len() {
        0..=2 => shown,
        n => format!("{shown} +{}", n - 2),
    }
}

/// Records copied files and folders by reference: their paths, never their
/// contents. Copying the same files again moves that entry to the top.
fn ingest_files(conn: &Connection, app: &AppHandle, paths: &[PathBuf], hash: &str) {
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(existing) = db::find_item_by_hash(conn, "file", hash) {
        db::touch_item(conn, &existing, now);
        let _ = app.emit("clipflow://new-item", &existing);
        return;
    }
    let content: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let Ok(content) = serde_json::to_string(&content) else { return };
    let id = uuid_like();
    db::insert_item(conn, &id, "file", &content, &file_list_preview(paths), "", now, hash);
    let _ = app.emit("clipflow://new-item", &id);
}

/// Whether the app that wrote the clipboard asked clipboard managers not to
/// record it. This honors the conventions apps actually use; it does not try
/// to guess passwords from the text itself.
#[cfg(target_os = "macos")]
fn is_private_clipboard() -> bool {
    use objc2_app_kit::NSPasteboard;
    // nspasteboard.org: concealed (passwords) and transient (temporary) data.
    const PRIVATE: [&str; 2] = ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType"];
    objc2::rc::autoreleasepool(|_| {
        NSPasteboard::generalPasteboard()
            .types()
            .is_some_and(|types| types.iter().any(|t| PRIVATE.contains(&t.to_string().as_str())))
    })
}

/// Windows: the formats that ask clipboard monitors and Clipboard History to
/// skip the content (set by password managers and by apps for secrets).
#[cfg(target_os = "windows")]
fn is_private_clipboard() -> bool {
    use windows::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
    use windows::core::w;
    unsafe {
        let exclude = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        exclude != 0 && IsClipboardFormatAvailable(exclude).is_ok()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_private_clipboard() -> bool {
    false
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
    crate::sync::journal::put(conn, &id);
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
        crate::sync::journal::put(conn, &existing);
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
    crate::sync::journal::put(conn, &id);
    let _ = app.emit("clipflow://new-item", &id);
    crate::ocr::wake(app);
}

fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with("http://") || s.starts_with("https://")) && !s.contains(' ') && !s.contains('\n')
}

pub fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos)
}
