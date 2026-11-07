mod clipboard;
mod db;
#[cfg(all(feature = "e2e", target_os = "macos"))]
mod e2e;
mod ocr;
#[cfg(target_os = "macos")]
mod paste_target;
mod retention;
mod snippets;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WebviewWindow};

use clipboard::MonitorState;

/// Which clipboard item(s) the (single, OS-level) annotation editor window is
/// currently open for. One editor window is reused for both the single-image
/// and batch-annotation workflows (spec §9/§10) — only the target list differs.
///
/// `restore_history` records whether the history popup was open when editing
/// started. The popup is always-on-top, so leaving it visible would bury the
/// editor underneath it; instead we hide it while editing and bring it back
/// (webview state — selection, filters — intact) when the editor closes.
#[derive(Default)]
struct EditorSession {
    targets: Mutex<Vec<String>>,
    restore_history: AtomicBool,
}

/// Coordinates all automatic paste operations. The operation lock prevents two
/// frontend invocations from interleaving clipboard writes and keystrokes. On
/// macOS we also remember the app that was frontmost immediately before the
/// history popup took focus, so paste can return to that exact destination.
#[derive(Default)]
struct PasteCoordinator {
    operation: Mutex<()>,
    #[cfg(target_os = "macos")]
    previous_target: Mutex<PasteTarget>,
}

/// Where a paste goes: the app that was frontmost when the popup opened and,
/// on macOS, its focused window (see `paste_target`).
#[derive(Clone, Default)]
struct PasteTarget {
    pid: Option<i32>,
    #[cfg(target_os = "macos")]
    window: Option<paste_target::WindowRef>,
    /// The text field that had focus, when the app exposes it.
    #[cfg(target_os = "macos")]
    element: Option<paste_target::ElementRef>,
}

enum PreparedClipboardPayload {
    Text {
        content: String,
        hash: String,
    },
    Image {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
        hash: String,
    },
    /// Native file references (Finder / Explorer), never file contents.
    Files {
        paths: Vec<PathBuf>,
        hash: String,
    },
}

struct PreparedSequentialItem {
    id: String,
    payload: PreparedClipboardPayload,
}

struct PreparedBatchImage {
    id: String,
    path: PathBuf,
    png_bytes: Vec<u8>,
    rgba_hash: String,
}

struct ValidatedImage {
    path: PathBuf,
    png_bytes: Vec<u8>,
    width: usize,
    height: usize,
    rgba_bytes: Vec<u8>,
    rgba_hash: String,
}

enum PastePlan {
    /// One clipboard generation and one paste event. macOS publishes both PNG
    /// and file-URL representations per ordered item; other platforms use the
    /// native file-list representation.
    ImageBatch { images: Vec<PreparedBatchImage> },
    /// Used for a single item or a mixed/non-image selection. Every payload is
    /// fully loaded before the popup hides, preventing a later missing/corrupt
    /// image from causing a partial transfer.
    Sequential { items: Vec<PreparedSequentialItem> },
}

#[tauri::command]
fn get_history(app: tauri::AppHandle) -> Vec<db::ClipItem> {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::list_items(&conn, 500)
}

#[tauri::command]
fn get_items(app: tauri::AppHandle, ids: Vec<String>) -> Vec<db::ClipItem> {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    ids.iter()
        .filter_map(|id| db::get_item(&conn, id))
        .collect()
}

#[tauri::command]
fn get_image_data_url(app: tauri::AppHandle, filename: String) -> Result<String, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let path = data_dir.join("images").join(&filename);
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/png;base64,{encoded}"))
}

#[tauri::command]
fn get_paused(monitor: tauri::State<Arc<MonitorState>>) -> bool {
    monitor.paused.load(Ordering::Relaxed)
}

#[tauri::command]
fn set_paused(monitor: tauri::State<Arc<MonitorState>>, paused: bool) {
    monitor.paused_until.store(0, Ordering::Relaxed);
    monitor.paused.store(paused, Ordering::Relaxed);
}

#[derive(serde::Serialize)]
struct MonitoringStatus {
    paused: bool,
    /// Epoch ms when recording resumes on its own; None = until resumed.
    until: Option<i64>,
}

fn monitoring_status_of(monitor: &MonitorState) -> MonitoringStatus {
    let until = monitor.paused_until.load(Ordering::Relaxed);
    let paused = monitor.paused.load(Ordering::Relaxed)
        && (until == 0 || chrono::Utc::now().timestamp_millis() < until);
    MonitoringStatus { paused, until: (paused && until > 0).then_some(until) }
}

#[tauri::command]
fn monitoring_status(monitor: tauri::State<Arc<MonitorState>>) -> MonitoringStatus {
    monitoring_status_of(&monitor)
}

/// Stops recording copies for `minutes` (None = until resumed), e.g. while
/// handling passwords that don't mark themselves private.
#[tauri::command]
fn pause_monitoring(monitor: tauri::State<Arc<MonitorState>>, minutes: Option<u32>) -> MonitoringStatus {
    let until = minutes.map(|m| chrono::Utc::now().timestamp_millis() + i64::from(m.clamp(1, 24 * 60)) * 60_000);
    monitor.paused_until.store(until.unwrap_or(0), Ordering::Relaxed);
    monitor.paused.store(true, Ordering::Relaxed);
    monitoring_status_of(&monitor)
}

/// Saves edited text back into a text/code/link item (the full viewer's Edit).
#[tauri::command]
fn update_item_text(app: tauri::AppHandle, id: String, content: String) -> Result<(), String> {
    if content.is_empty() {
        return Err("the text can't be empty".to_string());
    }
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let item = db::get_item(&conn, &id).ok_or_else(|| "that item no longer exists".to_string())?;
    if !matches!(item.kind.as_str(), "text" | "code" | "link") {
        return Err("only text items can be edited".to_string());
    }
    let preview: String = content.chars().take(120).collect();
    db::update_text(&conn, &id, &content, &preview, &clipboard::hash_bytes(content.as_bytes()))?;
    let _ = app.emit("clipflow://item-updated", &id);
    Ok(())
}

#[tauri::command]
fn storage_usage_mb(app: tauri::AppHandle) -> f64 {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    db::storage_usage_bytes(&data_dir) as f64 / (1024.0 * 1024.0)
}

#[tauri::command]
fn hide_history(window: WebviewWindow) {
    let _ = window.hide();
}

#[tauri::command]
fn toggle_favorite(app: tauri::AppHandle, id: String, favorite: bool) {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::set_favorite(&conn, &id, favorite);
}

#[tauri::command]
fn toggle_pinned(app: tauri::AppHandle, id: String, pinned: bool) {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::set_pinned(&conn, &id, pinned);
}

#[tauri::command]
fn delete_item(app: tauri::AppHandle, id: String) {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::delete_item(&conn, &data_dir, &id);
}

/// The default "Clear" action: removes everything except favorited items.
#[tauri::command]
fn clear_non_favorites(app: tauri::AppHandle) {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::clear_non_favorites(&conn, &data_dir);
}

/// The "danger zone" action: removes all clipboard history, favorites included.
/// Snippets live in a separate database and are never touched by this.
#[tauri::command]
fn clear_history(app: tauri::AppHandle) {
    let data_dir = app.path().app_data_dir().expect("no app data dir");
    let conn = db::open(&data_dir);
    db::clear_all(&conn, &data_dir);
}

/// Writes one item's content back to the real OS clipboard, marks the hash as
/// "ours" so the monitor thread doesn't re-ingest it as a new user copy, and
/// returns the hash that was recorded.
fn write_item_to_clipboard(
    data_dir: &PathBuf,
    monitor: &Arc<MonitorState>,
    item: &db::ClipItem,
) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if item.kind == "image" || item.kind == "screenshot" {
        let path = data_dir.join("images").join(&item.content);
        let img = image::open(&path).map_err(|e| e.to_string())?.to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let hash = clipboard::hash_bytes(img.as_raw());
        clipboard
            .set_image(arboard::ImageData {
                width: w,
                height: h,
                bytes: img.into_raw().into(),
            })
            .map_err(|e| e.to_string())?;
        *monitor.last_self_write_hash.lock().unwrap() = Some(hash);
    } else {
        let hash = clipboard::hash_bytes(item.content.as_bytes());
        clipboard
            .set_text(&item.content)
            .map_err(|e| e.to_string())?;
        *monitor.last_self_write_hash.lock().unwrap() = Some(hash);
    }
    Ok(())
}

/// The paths a file item refers to, in the order they were copied.
fn file_item_paths(item: &db::ClipItem) -> Vec<PathBuf> {
    serde_json::from_str::<Vec<String>>(&item.content)
        .map(|paths| paths.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// A file item's paths, failing with a clear message when any of them is no
/// longer there — pasting only some of the files would be worse than none.
fn available_file_paths(item: &db::ClipItem) -> Result<Vec<PathBuf>, String> {
    let paths = file_item_paths(item);
    if paths.is_empty() {
        return Err("this entry has no file paths".to_string());
    }
    if let Some(missing) = paths.iter().find(|p| std::fs::symlink_metadata(p).is_err() || !p.exists()) {
        return Err(format!(
            "\"{}\" is no longer at {}. It may have been moved, renamed or deleted, or its drive is disconnected. Nothing was pasted.",
            missing.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            missing.display()
        ));
    }
    Ok(paths)
}

#[derive(serde::Serialize)]
struct FileEntry {
    name: String,
    path: String,
    /// `file://` URL with spaces and non-ASCII characters percent-encoded.
    url: Option<String>,
    exists: bool,
    /// Present but unreadable (permissions).
    accessible: bool,
    is_dir: bool,
    is_symlink: bool,
    size: Option<u64>,
    modified: Option<i64>,
    extension: String,
}

fn file_entry(path: &Path) -> FileEntry {
    let link = std::fs::symlink_metadata(path).ok();
    let target = std::fs::metadata(path);
    let accessible = match &target {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path).is_ok(),
        Ok(_) => std::fs::File::open(path).is_ok(),
        Err(_) => false,
    };
    let meta = target.ok();
    FileEntry {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned()),
        path: path.to_string_lossy().into_owned(),
        url: tauri::Url::from_file_path(path).ok().map(|u| u.to_string()),
        exists: meta.is_some(),
        accessible,
        is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
        is_symlink: link.is_some_and(|m| m.file_type().is_symlink()),
        size: meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
        modified: meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        extension: path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default(),
    }
}

fn file_item(app: &tauri::AppHandle, id: &str) -> Result<db::ClipItem, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    db::get_item(&db::open(&data_dir), id)
        .filter(|item| item.kind == "file")
        .ok_or_else(|| "that file entry no longer exists".to_string())
}

/// What a file entry points to now: each file's name, location and whether
/// it is still there. Looked up live, so a moved file shows as missing.
#[tauri::command]
fn file_entries(app: tauri::AppHandle, id: String) -> Result<Vec<FileEntry>, String> {
    Ok(file_item_paths(&file_item(&app, &id)?).iter().map(|p| file_entry(p)).collect())
}

/// Copies a file entry's paths (one per line) or its `file://` links as
/// plain text. Not recorded as a new history item: the entry already exists.
#[tauri::command]
fn copy_file_paths(
    app: tauri::AppHandle,
    monitor: tauri::State<Arc<MonitorState>>,
    id: String,
    as_url: bool,
) -> Result<(), String> {
    let paths = file_item_paths(&file_item(&app, &id)?);
    let lines: Vec<String> = paths
        .iter()
        .map(|p| {
            if as_url {
                tauri::Url::from_file_path(p).map(|u| u.to_string()).unwrap_or_else(|_| p.to_string_lossy().into_owned())
            } else {
                p.to_string_lossy().into_owned()
            }
        })
        .collect();
    let text = lines.join("\n");
    let mut system_clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    write_prepared_payload(
        &mut system_clipboard,
        &monitor,
        &PreparedClipboardPayload::Text { hash: clipboard::hash_bytes(text.as_bytes()), content: text },
    )
}

/// Reveals (or opens) file `index` of a file entry. Only paths stored in the
/// entry can be opened — never an arbitrary path from the page.
#[tauri::command]
fn open_file_entry(app: tauri::AppHandle, id: String, index: usize, reveal: bool) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let paths = file_item_paths(&file_item(&app, &id)?);
    let path = paths.get(index).ok_or_else(|| "that file is not part of this entry".to_string())?;
    if !path.exists() {
        return Err(format!("{} is no longer there", path.display()));
    }
    if reveal {
        app.opener().reveal_item_in_dir(path)
    } else {
        app.opener().open_path(path.to_string_lossy(), None::<&str>)
    }
    .map_err(|e| e.to_string())
}

fn is_image_item(item: &db::ClipItem) -> bool {
    item.kind == "image" || item.kind == "screenshot"
}

/// Resolve every database row and validate every backing image before any
/// clipboard mutation or focus change. Mixed selections additionally decode
/// all images up front because their fallback transfer is sequential; that way
/// a corrupt later image cannot fail after earlier items have already pasted.
fn prepare_paste_plan(
    conn: &rusqlite::Connection,
    data_dir: &PathBuf,
    ids: &[String],
) -> Result<PastePlan, String> {
    if ids.is_empty() {
        return Err("no items selected".to_string());
    }

    let items = ids
        .iter()
        .map(|id| db::get_item(conn, id).ok_or_else(|| format!("item {id} not found")))
        .collect::<Result<Vec<_>, _>>()?;

    let images_dir = data_dir
        .join("images")
        .canonicalize()
        .map_err(|e| format!("could not access the image store: {e}"))?;
    let mut validated_images = Vec::with_capacity(items.len());

    for item in &items {
        if !is_image_item(item) {
            validated_images.push(None);
            continue;
        }

        let path = data_dir.join("images").join(&item.content);
        let path = path
            .canonicalize()
            .map_err(|e| format!("image {} is unavailable: {e}", item.id))?;
        if !path.starts_with(&images_dir) {
            return Err(format!(
                "image {} is outside the managed image store",
                item.id
            ));
        }
        let metadata = path
            .metadata()
            .map_err(|e| format!("image {} is unavailable: {e}", item.id))?;
        if !metadata.is_file() {
            return Err(format!("image {} is not a regular file", item.id));
        }
        let png_bytes =
            std::fs::read(&path).map_err(|e| format!("image {} cannot be read: {e}", item.id))?;
        // Decode exactly once from the retained original PNG bytes. A truncated
        // PNG can expose valid dimensions yet fail once a destination reads its
        // pixel stream, which would make a native batch partial.
        let rgba = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
            .map_err(|e| format!("image {} cannot be read as PNG: {e}", item.id))?
            .to_rgba8();
        validated_images.push(Some(ValidatedImage {
            path,
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            rgba_hash: clipboard::hash_bytes(rgba.as_raw()),
            rgba_bytes: rgba.into_raw(),
            png_bytes,
        }));
    }

    if items.len() > 1 && items.iter().all(is_image_item) {
        let images = items
            .into_iter()
            .zip(validated_images)
            .map(|(item, image)| {
                let image = image.ok_or_else(|| format!("image {} was not prepared", item.id))?;
                Ok(PreparedBatchImage {
                    id: item.id,
                    path: image.path,
                    png_bytes: image.png_bytes,
                    rgba_hash: image.rgba_hash,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        return Ok(PastePlan::ImageBatch { images });
    }

    let mut prepared = Vec::with_capacity(items.len());
    for (item, image) in items.into_iter().zip(validated_images) {
        let payload = if item.kind == "file" {
            let paths = available_file_paths(&item)?;
            PreparedClipboardPayload::Files {
                hash: clipboard::hash_file_list(&paths),
                paths,
            }
        } else if let Some(image) = image {
            PreparedClipboardPayload::Image {
                width: image.width,
                height: image.height,
                hash: image.rgba_hash,
                bytes: image.rgba_bytes,
            }
        } else {
            PreparedClipboardPayload::Text {
                hash: clipboard::hash_bytes(item.content.as_bytes()),
                content: item.content,
            }
        };
        prepared.push(PreparedSequentialItem {
            id: item.id,
            payload,
        });
    }

    Ok(PastePlan::Sequential { items: prepared })
}

fn write_prepared_payload(
    clipboard: &mut arboard::Clipboard,
    monitor: &Arc<MonitorState>,
    payload: &PreparedClipboardPayload,
) -> Result<(), String> {
    let hash = match payload {
        PreparedClipboardPayload::Text { content, hash } => {
            clipboard.set_text(content).map_err(|e| e.to_string())?;
            hash
        }
        PreparedClipboardPayload::Files { paths, hash } => {
            clipboard
                .set()
                .file_list(paths)
                .map_err(|e| format!("the files could not be put on the clipboard: {e}"))?;
            hash
        }
        PreparedClipboardPayload::Image {
            width,
            height,
            bytes,
            hash,
        } => {
            clipboard
                .set_image(arboard::ImageData {
                    width: *width,
                    height: *height,
                    bytes: bytes.as_slice().into(),
                })
                .map_err(|e| e.to_string())?;
            hash
        }
    };
    *monitor
        .last_self_write_hash
        .lock()
        .map_err(|_| "clipboard monitor state is unavailable".to_string())? = Some(hash.clone());
    Ok(())
}

/// macOS destinations vary: editors commonly consume an image representation,
/// while chat/file fields consume file URLs. Publish both representations on
/// every ordered pasteboard item so one Cmd+V can serve either model without
/// racing successive clipboard generations.
#[cfg(target_os = "macos")]
fn write_mac_image_batch(
    images: &[PreparedBatchImage],
    monitor: &Arc<MonitorState>,
) -> Result<(), String> {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
        NSPasteboardWriting,
    };
    use objc2_foundation::{NSArray, NSData, NSString, NSURL};

    objc2::rc::autoreleasepool(|_| {
        let first_hash = images
            .first()
            .map(|image| image.rgba_hash.clone())
            .ok_or_else(|| "no images selected".to_string())?;
        let mut objects = Vec::with_capacity(images.len());
        let mut expected_urls = Vec::with_capacity(images.len());

        for image in images {
            let path = image.path.to_str().ok_or_else(|| {
                format!("image path is not valid UTF-8: {}", image.path.display())
            })?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let url_string = url
                .absoluteString()
                .ok_or_else(|| format!("could not create a file URL for {path}"))?;
            let item = NSPasteboardItem::new();
            let png = NSData::with_bytes(&image.png_bytes);

            let wrote_png = unsafe { item.setData_forType(&png, NSPasteboardTypePNG) };
            let wrote_url = unsafe { item.setString_forType(&url_string, NSPasteboardTypeFileURL) };
            if !wrote_png || !wrote_url {
                return Err(format!("could not prepare {}", image.path.display()));
            }

            expected_urls.push(url_string);
            objects.push(ProtocolObject::from_retained(item));
        }

        let objects =
            NSArray::<ProtocolObject<dyn NSPasteboardWriting>>::from_retained_slice(&objects);
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();

        // Hold the monitor-state lock across the actual write. A monitor tick
        // that decodes the first PNG cannot compare its RGBA hash until this
        // self-write marker is installed.
        let mut self_write_hash = monitor
            .last_self_write_hash
            .lock()
            .map_err(|_| "clipboard monitor state is unavailable".to_string())?;
        if !pasteboard.writeObjects(&objects) {
            return Err("NSPasteboard refused the image batch".to_string());
        }
        *self_write_hash = Some(first_hash);
        drop(self_write_hash);

        let written = pasteboard
            .pasteboardItems()
            .ok_or_else(|| "NSPasteboard did not retain the image batch".to_string())?;
        if written.len() != images.len() {
            return Err(format!(
                "the clipboard accepted {} of {} selected images; nothing was pasted",
                written.len(),
                images.len()
            ));
        }

        for ((written_item, expected_image), expected_url) in
            written.iter().zip(images).zip(expected_urls.iter())
        {
            let written_png = unsafe { written_item.dataForType(NSPasteboardTypePNG) }
                .ok_or_else(|| format!("clipboard item {} lost its PNG data", expected_image.id))?;
            let expected_png = NSData::with_bytes(&expected_image.png_bytes);
            if !written_png.isEqualToData(&expected_png) {
                return Err(format!(
                    "clipboard item {} changed its PNG data",
                    expected_image.id
                ));
            }

            let written_url = unsafe { written_item.stringForType(NSPasteboardTypeFileURL) }
                .ok_or_else(|| format!("clipboard item {} lost its file URL", expected_image.id))?;
            if !written_url.isEqualToString(expected_url) {
                return Err(format!(
                    "clipboard item {} changed its file URL",
                    expected_image.id
                ));
            }
        }

        Ok(())
    })
}

/// Sequential transfer remains a compatibility fallback for mixed selections.
/// Text is normally consumed immediately; raw image payloads need longer because
/// many destination apps decode them after receiving the paste event.
fn payload_settle_delay(payload: &PreparedClipboardPayload) -> Duration {
    match payload {
        PreparedClipboardPayload::Text { content, .. } => {
            let extra = (content.len() as u64 / (64 * 1024)) * 20;
            Duration::from_millis((80 + extra).min(260))
        }
        PreparedClipboardPayload::Files { .. } => Duration::from_millis(400),
        PreparedClipboardPayload::Image { bytes, .. } => {
            let extra = (bytes.len() as u64 / (4 * 1024 * 1024)) * 150;
            Duration::from_millis((700 + extra).min(1_800))
        }
    }
}

/// On macOS, synthesizing input events (what `synth_paste_keystroke` does)
/// requires the app to be trusted for Accessibility. Without it, `CGEventPost`
/// silently does nothing — no error, the keystroke just never arrives — which
/// is exactly the "I pressed Enter and nothing got pasted" symptom. Check this
/// up front so we can return a clear, actionable error instead of a false
/// "Pasted" success.
#[cfg(target_os = "macos")]
pub(crate) fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// Shows macOS's own "allow this app to control your computer" prompt (with a
/// button straight to the Accessibility settings pane) and registers the app
/// in that list. Only prompts once per app/binary — later calls are silent.
#[cfg(target_os = "macos")]
fn request_accessibility_prompt() {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    }
    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let opts =
        CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFBoolean::true_value().as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) };
}

const MANUAL_PASTE_READY_PREFIX: &str = "CLIPET_MANUAL_PASTE_READY:";
const AUTO_PASTE_BLOCKED_PREFIX: &str = "CLIPET_AUTO_PASTE_BLOCKED:";

/// Returns the enclosing `.app` bundle when `executable` is a packaged macOS
/// binary such as `/Applications/Clipet.app/Contents/MacOS/app`. A development
/// binary in `target/debug` deliberately returns `None` because macOS assigns
/// its Accessibility consent to the terminal/IDE that launched it.
fn app_bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|extension| extension == "app")
        })
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn accessibility_hint() -> String {
    if let Some(bundle) = std::env::current_exe()
        .ok()
        .and_then(|executable| app_bundle_from_executable(&executable))
    {
        return format!(
            "macOS does not trust this exact Clipet build. In Accessibility, select the existing Clipet row and click −, then click + and add {}. Turn it on, then quit and reopen Clipet.",
            bundle.display()
        );
    }

    "This is a development build. In Accessibility, enable the app that launched it (Visual Studio Code, Terminal, or iTerm), then fully restart that launcher and Clipet.".to_string()
}

/// Checked while the popup is still visible, so a failure can actually be shown.
fn ensure_can_synthesize_paste() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if !accessibility_trusted() {
        request_accessibility_prompt();
        return Err(accessibility_hint());
    }
    Ok(())
}

/// If auto-paste is blocked, retain as much of the user's action as possible:
/// a single item (or one native all-image batch) can still be placed on the
/// clipboard for a manual Cmd+V. Mixed multi-item selections cannot be staged
/// faithfully as one clipboard generation, so those remain untouched.
fn stage_manual_paste(plan: &PastePlan, monitor: &Arc<MonitorState>) -> Result<bool, String> {
    match plan {
        PastePlan::ImageBatch { images } => {
            #[cfg(target_os = "macos")]
            {
                write_mac_image_batch(images, monitor)?;
                Ok(true)
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (images, monitor);
                Ok(false)
            }
        }
        PastePlan::Sequential { items } if items.len() == 1 => {
            let mut system_clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            write_prepared_payload(&mut system_clipboard, monitor, &items[0].payload)?;
            Ok(true)
        }
        PastePlan::Sequential { .. } => Ok(false),
    }
}

#[tauri::command]
fn open_accessibility_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn()
            .map_err(|e| format!("could not open Accessibility settings: {e}"))?;
        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    Err("Accessibility settings are only available on macOS".to_string())
}

#[cfg(target_os = "macos")]
fn frontmost_application_pid() -> Option<i32> {
    use objc2_app_kit::NSWorkspace;

    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|application| application.processIdentifier())
        .filter(|pid| *pid > 0)
}

/// Called immediately before the history popup is shown. Capturing the exact
/// process here avoids relying on whichever app macOS happens to choose after
/// Clipet hides, and lets us verify the destination really regained focus.
#[cfg(target_os = "macos")]
fn capture_previous_paste_target(coordinator: &PasteCoordinator) {
    let current_pid = std::process::id() as i32;
    let Some(target_pid) = frontmost_application_pid().filter(|pid| *pid != current_pid) else {
        // Opening from Clipet itself (for example after Escape hid only the
        // window) must not erase the still-valid external destination.
        return;
    };
    let window = paste_target::focused_window(target_pid);
    let element = paste_target::focused_element(target_pid);
    snippets::diag(&format!(
        "paste: popup opened over pid {target_pid} ({})",
        paste_target::describe_focus(target_pid, window.as_ref())
    ));
    if let Ok(mut stored) = coordinator.previous_target.lock() {
        *stored = PasteTarget {
            pid: Some(target_pid),
            window,
            element,
        };
    }
}

fn snapshot_previous_paste_target(coordinator: &PasteCoordinator) -> Result<PasteTarget, String> {
    #[cfg(target_os = "macos")]
    {
        return coordinator
            .previous_target
            .lock()
            .map(|target| target.clone())
            .map_err(|_| "paste target state is unavailable".to_string());
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = coordinator;
        Ok(PasteTarget::default())
    }
}

#[cfg(target_os = "macos")]
fn wait_for_hide_dispatch(app: &tauri::AppHandle) -> Result<(), String> {
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = sent.send(());
    })
    .map_err(|e| format!("could not hand focus back: {e}"))?;
    received
        .recv_timeout(Duration::from_millis(500))
        .map_err(|_| "timed out while handing focus back".to_string())
}

/// Hiding only the popup leaves Clipet active on macOS. Hide the whole app,
/// reactivate the process captured before the popup opened, then poll actual
/// frontmost-application state. The common path proceeds as soon as the OS
/// confirms focus, instead of paying an unconditional delay on every text paste.
fn focus_handoff_complete(
    frontmost_pid: i32,
    current_pid: i32,
    effective_target_pid: Option<i32>,
) -> bool {
    effective_target_pid
        .map(|expected| frontmost_pid == expected)
        .unwrap_or(frontmost_pid != current_pid)
}

fn return_focus_to_previous_app(
    app: &tauri::AppHandle,
    paste_target: &PasteTarget,
) -> Result<(), String> {
    let target_pid = paste_target.pid;
    if let Some(win) = app.get_webview_window("history") {
        let _ = win.hide();
    }

    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

        let started = Instant::now();
        app.hide()
            .map_err(|e| format!("could not hide Clipet: {e}"))?;
        // AppHandle::hide queues work on Tauri's event loop. This barrier is
        // reached after that message without blocking the event-loop thread.
        wait_for_hide_dispatch(app)?;
        snippets::diag(&format!("paste: Clipet hidden after {}ms", started.elapsed().as_millis()));

        let current_pid = std::process::id() as i32;
        let target =
            target_pid.and_then(NSRunningApplication::runningApplicationWithProcessIdentifier);

        if let Some(target) = target.as_ref() {
            if !target.isTerminated() && !target.isActive() {
                target.activateWithOptions(NSApplicationActivationOptions::empty());
            }
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(frontmost_pid) = frontmost_application_pid() {
                // If the captured app quit while the popup was open, fall back
                // to whichever non-Clipet app macOS made frontmost after hide.
                let effective_target_pid = target
                    .as_ref()
                    .filter(|application| !application.isTerminated())
                    .map(|application| application.processIdentifier());
                if focus_handoff_complete(frontmost_pid, current_pid, effective_target_pid) {
                    snippets::diag(&format!(
                        "paste: pid {frontmost_pid} frontmost after {}ms",
                        started.elapsed().as_millis()
                    ));
                    // The app is frontmost; make sure the window the user
                    // came from (e.g. a Chrome pop-up) has focus too.
                    if let (Some(pid), Some(window)) = (effective_target_pid, &paste_target.window) {
                        let handoff =
                            paste_target::refocus(pid, window, Duration::from_millis(400));
                        snippets::diag(&format!("paste: window handoff {handoff:?}"));
                        if handoff == paste_target::Handoff::Raised {
                            // Let the app move keyboard focus into the window's
                            // text field before ⌘V arrives.
                            std::thread::sleep(Duration::from_millis(40));
                        }
                    }
                    if let (Some(pid), Some(element)) = (effective_target_pid, &paste_target.element) {
                        let handoff =
                            paste_target::refocus_element(pid, element, Duration::from_millis(300));
                        snippets::diag(&format!("paste: text field handoff {handoff:?}"));
                    }
                    snippets::diag(&format!(
                        "paste: before ⌘V: {}",
                        paste_target::describe_focus(frontmost_pid, paste_target.window.as_ref())
                    ));
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                snippets::diag("paste: previous app never became frontmost; nothing pasted");
                return Err("the previous app did not regain focus; nothing was pasted".to_string());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = target_pid;
        // There is no cross-platform foreground-process API. This fallback is
        // off the UI thread and only applies outside the macOS implementation.
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }
}

fn restore_history_after_paste_error(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.show();
    if let Some(win) = app.get_webview_window("history") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Synthesizes the paste keystroke from the right thread. Paste work runs on a
/// blocking worker, but on macOS the V key code is looked up through the Text
/// Input Sources API (`paste_target::paste_key_code`), which asserts it is on the main queue — calling it
/// from the worker aborts the whole process (EXC_BREAKPOINT in
/// `TSMGetInputSourceProperty`). Hop to the main thread and wait for the result.
fn synth_paste_keystroke(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        app.run_on_main_thread(move || {
            let result = synth_paste_keystroke_on_current_thread();
            snippets::diag(&match &result {
                Ok(()) => "paste: ⌘V sent".to_string(),
                Err(e) => format!("paste: ⌘V failed: {e}"),
            });
            let _ = sent.send(result);
        })
        .map_err(|e| format!("could not send the paste keystroke: {e}"))?;
        return received
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "timed out while sending the paste keystroke".to_string())?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        synth_paste_keystroke_on_current_thread()
    }
}

/// The V of ⌘V / Ctrl+V as a fixed key code, never looked up by character on
/// the current layout: with a non-Latin layout (e.g. Khmer) no key types "v"
/// and the lookup fell back to the A key, sending Select All instead of paste.
/// (macOS posts its own chord, see `paste_target::post_paste_chord`.)
#[cfg(not(target_os = "macos"))]
fn paste_key() -> enigo::Key {
    // Windows virtual-key codes for letters don't depend on the layout.
    #[cfg(target_os = "windows")]
    return enigo::Key::Other(0x56); // VK_V
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return enigo::Key::Unicode('v');
}

/// Synthesizes the platform paste keystroke (Cmd+V / Ctrl+V) into whatever
/// application currently has OS focus. On macOS this must run on the main thread.
#[cfg(target_os = "macos")]
fn synth_paste_keystroke_on_current_thread() -> Result<(), String> {
    paste_target::post_paste_chord(0)
}

#[cfg(not(target_os = "macos"))]
fn synth_paste_keystroke_on_current_thread() -> Result<(), String> {
    use enigo::{Enigo, Keyboard, Settings};
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    let modifier = enigo::Key::Control;

    let pressed = enigo.key(modifier, enigo::Direction::Press);
    if let Err(error) = pressed {
        // A backend can report an error after partially applying an event, so
        // still send key-up defensively rather than risk a stuck modifier.
        let _ = enigo.key(modifier, enigo::Direction::Release);
        return Err(error.to_string());
    }

    let pasted = enigo.key(paste_key(), enigo::Direction::Click);
    let released = enigo.key(modifier, enigo::Direction::Release);
    match (pasted, released) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(paste_error), Ok(())) => Err(paste_error.to_string()),
        (Ok(()), Err(release_error)) => Err(format!(
            "paste was sent but the modifier key could not be released: {release_error}"
        )),
        (Err(paste_error), Err(release_error)) => Err(format!(
            "paste keystroke failed: {paste_error}; modifier release also failed: {release_error}"
        )),
    }
}

fn execute_paste(
    app: tauri::AppHandle,
    monitor: Arc<MonitorState>,
    coordinator: Arc<PasteCoordinator>,
    ids: Vec<String>,
) -> Result<(), String> {
    run_paste(app, monitor, coordinator, |conn, data_dir| {
        prepare_paste_plan(conn, data_dir, &ids)
    })
}

fn run_paste(
    app: tauri::AppHandle,
    monitor: Arc<MonitorState>,
    coordinator: Arc<PasteCoordinator>,
    build_plan: impl FnOnce(&rusqlite::Connection, &PathBuf) -> Result<PastePlan, String>,
) -> Result<(), String> {
    let _operation_guard = coordinator
        .operation
        .lock()
        .map_err(|_| "another paste operation ended unexpectedly".to_string())?;
    // Freeze the destination for this operation. Opening/toggling another popup
    // while large images are being preloaded cannot redirect an in-flight paste.
    let target = snapshot_previous_paste_target(&coordinator)?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let plan = build_plan(&conn, &data_dir)?;
    // Permission preflight is intentionally before any clipboard mutation and
    // while the popup is still visible. When macOS denies it, stage a faithful
    // single-item/native-image payload so the action still has a manual Cmd+V
    // fallback, then return a tagged error the frontend can present as a
    // persistent recovery dialog instead of a transient generic toast.
    if let Err(permission_hint) = ensure_can_synthesize_paste() {
        return match stage_manual_paste(&plan, &monitor) {
            Ok(true) => Err(format!("{MANUAL_PASTE_READY_PREFIX}{permission_hint}")),
            Ok(false) => Err(format!("{AUTO_PASTE_BLOCKED_PREFIX}{permission_hint}")),
            Err(copy_error) => Err(format!(
                "{AUTO_PASTE_BLOCKED_PREFIX}{permission_hint} The clipboard fallback also failed: {copy_error}"
            )),
        };
    }

    match plan {
        PastePlan::ImageBatch { images } => {
            #[cfg(target_os = "macos")]
            write_mac_image_batch(&images, &monitor)?;

            #[cfg(not(target_os = "macos"))]
            {
                let mut system_clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
                let paths = images
                    .iter()
                    .map(|image| image.path.clone())
                    .collect::<Vec<_>>();
                system_clipboard
                    .set()
                    .file_list(&paths)
                    .map_err(|e| format!("could not prepare the selected images: {e}"))?;
                let written_paths = system_clipboard
                    .get()
                    .file_list()
                    .map_err(|e| format!("could not verify the selected images: {e}"))?
                    .into_iter()
                    .map(|path| {
                        path.canonicalize().map_err(|e| {
                            format!("could not verify clipboard path {}: {e}", path.display())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if written_paths != paths {
                    return Err(format!(
                        "the clipboard accepted {} of {} selected images; nothing was pasted",
                        written_paths.len(),
                        paths.len()
                    ));
                }
            }

            if let Err(error) = return_focus_to_previous_app(&app, &target) {
                restore_history_after_paste_error(&app);
                return Err(error);
            }
            if let Err(error) = synth_paste_keystroke(&app) {
                restore_history_after_paste_error(&app);
                return Err(error);
            }

            let used_at = chrono::Utc::now().timestamp_millis();
            for image in images {
                db::bump_usage(&conn, &image.id, used_at);
            }
            Ok(())
        }
        PastePlan::Sequential { items } => {
            let mut system_clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            let total = items.len();
            let mut items = items.into_iter();
            let first = items
                .next()
                .ok_or_else(|| "no items selected".to_string())?;
            write_prepared_payload(&mut system_clipboard, &monitor, &first.payload)?;
            if let Err(error) = return_focus_to_previous_app(&app, &target) {
                restore_history_after_paste_error(&app);
                return Err(error);
            }

            let mut current = first;
            let mut completed = 0usize;
            loop {
                if let Err(error) = synth_paste_keystroke(&app) {
                    restore_history_after_paste_error(&app);
                    return Err(if completed == 0 {
                        error
                    } else {
                        format!("Pasted {completed} of {total} items before paste stopped: {error}")
                    });
                }
                db::bump_usage(&conn, &current.id, chrono::Utc::now().timestamp_millis());
                completed += 1;

                let Some(next) = items.next() else {
                    break;
                };
                std::thread::sleep(payload_settle_delay(&current.payload));
                if let Err(error) =
                    write_prepared_payload(&mut system_clipboard, &monitor, &next.payload)
                {
                    restore_history_after_paste_error(&app);
                    return Err(format!(
                        "Pasted {completed} of {total} items before paste stopped: {error}"
                    ));
                }
                current = next;
            }
            Ok(())
        }
    }
}

#[tauri::command]
async fn paste_item(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, Arc<MonitorState>>,
    coordinator: tauri::State<'_, Arc<PasteCoordinator>>,
    id: String,
) -> Result<(), String> {
    let monitor = monitor.inner().clone();
    let coordinator = coordinator.inner().clone();
    tauri::async_runtime::spawn_blocking(move || execute_paste(app, monitor, coordinator, vec![id]))
        .await
        .map_err(|e| format!("paste worker failed: {e}"))?
}

/// The command name is retained for frontend compatibility. Pure image batches
/// now use one native file-list paste; only mixed/non-image selections take the
/// preloaded, payload-paced sequential fallback.
#[tauri::command]
async fn paste_items_sequential(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, Arc<MonitorState>>,
    coordinator: tauri::State<'_, Arc<PasteCoordinator>>,
    ids: Vec<String>,
) -> Result<(), String> {
    let monitor = monitor.inner().clone();
    let coordinator = coordinator.inner().clone();
    tauri::async_runtime::spawn_blocking(move || execute_paste(app, monitor, coordinator, ids))
        .await
        .map_err(|e| format!("paste worker failed: {e}"))?
}

/// Pastes a snippet's content into the previous app through the same
/// focus-handoff path as a history item.
#[tauri::command]
async fn paste_snippet(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, Arc<MonitorState>>,
    coordinator: tauri::State<'_, Arc<PasteCoordinator>>,
    id: String,
) -> Result<(), String> {
    let service = snippets::service(&app);
    let snippet = service
        .snippet(&id)
        .ok_or_else(|| "that snippet no longer exists".to_string())?;
    let payload = snippet_payload(&service, &snippet)?;
    let monitor = monitor.inner().clone();
    let coordinator = coordinator.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_paste(app, monitor, coordinator, |_, _| {
            Ok(PastePlan::Sequential {
                items: vec![PreparedSequentialItem {
                    id: snippet.id.clone(),
                    payload,
                }],
            })
        })?;
        service.record_use(&snippet.id);
        Ok(())
    })
    .await
    .map_err(|e| format!("paste worker failed: {e}"))?
}

/// Copies a snippet's content without adding a duplicate history entry.
#[tauri::command]
fn copy_snippet(
    app: tauri::AppHandle,
    monitor: tauri::State<Arc<MonitorState>>,
    id: String,
) -> Result<(), String> {
    let service = snippets::service(&app);
    let snippet = service
        .snippet(&id)
        .ok_or_else(|| "that snippet no longer exists".to_string())?;
    let payload = snippet_payload(&service, &snippet)?;
    let mut system_clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    write_prepared_payload(&mut system_clipboard, &monitor, &payload)
}

/// A snippet's text, or its image decoded for the clipboard.
fn snippet_payload(
    service: &snippets::SnippetService,
    snippet: &snippets::store::Snippet,
) -> Result<PreparedClipboardPayload, String> {
    let Some(png) = service.image_png(snippet) else {
        // Same text as typed expansion inserts, including its whitespace setting.
        let content = format!("{}{}", snippet.content, snippets::store::suffix_text(&snippet.suffix));
        return Ok(PreparedClipboardPayload::Text {
            hash: clipboard::hash_bytes(content.as_bytes()),
            content,
        });
    };
    let rgba = image::load_from_memory_with_format(&png?, image::ImageFormat::Png)
        .map_err(|e| format!("the snippet's image is damaged: {e}"))?
        .to_rgba8();
    Ok(PreparedClipboardPayload::Image {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        hash: clipboard::hash_bytes(rgba.as_raw()),
        bytes: rgba.into_raw(),
    })
}

#[tauri::command]
fn start_annotation(
    app: tauri::AppHandle,
    session: tauri::State<EditorSession>,
    ids: Vec<String>,
) -> Result<(), String> {
    if ids.is_empty() {
        return Err("no images selected".to_string());
    }
    *session.targets.lock().unwrap() = ids;
    if let Some(history) = app.get_webview_window("history") {
        let was_open = history.is_visible().unwrap_or(false);
        session.restore_history.store(was_open, Ordering::Relaxed);
        if was_open {
            let _ = history.hide();
        }
    }
    if let Some(win) = app.get_webview_window("editor") {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    }
    let _ = app.emit("clipflow://editor-start", ());
    Ok(())
}

#[tauri::command]
fn get_editor_targets(session: tauri::State<EditorSession>) -> Vec<String> {
    session.targets.lock().unwrap().clone()
}

#[tauri::command]
fn close_editor(app: tauri::AppHandle, session: tauri::State<EditorSession>) {
    if let Some(win) = app.get_webview_window("editor") {
        let _ = win.hide();
    }
    // Spec §10: "Return to the clipboard popup with the selection preserved."
    if session.restore_history.swap(false, Ordering::Relaxed) {
        if let Some(win) = app.get_webview_window("history") {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

fn decode_data_url_png(data_url: &str) -> Result<(Vec<u8>, image::RgbaImage), String> {
    let b64 = data_url
        .split(',')
        .nth(1)
        .ok_or_else(|| "malformed data URL".to_string())?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    let pixels = image::load_from_memory(&bytes)
        .map_err(|e| format!("the edited image is not valid: {e}"))?
        .to_rgba8();
    Ok((bytes, pixels))
}

/// Saves an edited image as a new history item next to the untouched
/// original (the editor's default). Returns the new item's id.
#[tauri::command]
fn save_annotated_image_as_new(app: tauri::AppHandle, id: String, data_url: String) -> Result<String, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let item = db::get_item(&conn, &id).ok_or_else(|| "the original image is no longer in history".to_string())?;
    let (bytes, pixels) = decode_data_url_png(&data_url)?;
    let new_id = clipboard::uuid_like();
    let file = format!("{new_id}.png");
    let images_dir = data_dir.join("images");
    let temp_path = images_dir.join(format!("{new_id}.saving.png"));
    std::fs::write(&temp_path, &bytes)
        .and_then(|_| std::fs::rename(&temp_path, images_dir.join(&file)))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_path);
            format!("could not save the edited image: {e}")
        })?;
    let kind = if item.kind == "screenshot" { "screenshot" } else { "image" };
    db::insert_item(
        &conn,
        &new_id,
        kind,
        &file,
        &format!("{}x{}", pixels.width(), pixels.height()),
        &item.source_app,
        chrono::Utc::now().timestamp_millis(),
        &clipboard::hash_bytes(pixels.as_raw()),
    );
    db::mark_edited(&conn, &new_id);
    let _ = app.emit("clipflow://new-item", &new_id);
    ocr::wake(&app);
    Ok(new_id)
}

/// "Save & Paste": closes the editor without bringing the popup back, then
/// pastes the saved images into the app the popup was opened over.
#[tauri::command]
async fn paste_after_edit(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, Arc<MonitorState>>,
    coordinator: tauri::State<'_, Arc<PasteCoordinator>>,
    session: tauri::State<'_, EditorSession>,
    ids: Vec<String>,
) -> Result<(), String> {
    session.restore_history.store(false, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window("editor") {
        let _ = win.hide();
    }
    let monitor = monitor.inner().clone();
    let coordinator = coordinator.inner().clone();
    tauri::async_runtime::spawn_blocking(move || execute_paste(app, monitor, coordinator, ids))
        .await
        .map_err(|e| format!("paste worker failed: {e}"))?
}

#[tauri::command]
fn save_annotated_image(app: tauri::AppHandle, id: String, data_url: String) -> Result<(), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let item = db::get_item(&conn, &id).ok_or_else(|| "item not found".to_string())?;

    let b64 = data_url
        .split(',')
        .nth(1)
        .ok_or_else(|| "malformed data URL".to_string())?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;

    let images_dir = data_dir.join("images");
    let current_path = images_dir.join(&item.content);
    // Preserve the original capture the first time an item is edited (spec §9:
    // "Preserve the original image for future editing where practical").
    let original_path = images_dir.join(format!("{id}.original.png"));
    if !original_path.exists() && current_path.exists() {
        std::fs::copy(&current_path, &original_path).map_err(|e| e.to_string())?;
    }
    // Write-then-rename, so a crash mid-save can't leave a truncated image.
    let pixels = image::load_from_memory(&bytes)
        .map_err(|e| format!("the edited image is not valid: {e}"))?
        .to_rgba8();
    let temp_path = images_dir.join(format!("{id}.saving.png"));
    std::fs::write(&temp_path, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&temp_path, &current_path).map_err(|e| e.to_string())?;
    std::fs::remove_file(images_dir.join(db::thumbnail_file(&id))).ok();
    db::mark_edited(&conn, &id);
    // Keep duplicate detection truthful: the item now holds the edited pixels,
    // so re-copying the original image later creates a separate entry.
    db::set_content_hash(&conn, &id, &clipboard::hash_bytes(pixels.as_raw()));
    let _ = app.emit("clipflow://item-updated", &id);
    ocr::wake(&app);
    Ok(())
}

/// A small cached preview for the history list, so the list never decodes
/// full-size screenshots. Regenerated when the source image is newer.
#[tauri::command]
fn get_thumbnail_data_url(app: tauri::AppHandle, id: String) -> Result<String, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let item = db::get_item(&conn, &id).ok_or_else(|| "item not found".to_string())?;
    let images_dir = data_dir.join("images");
    let source = images_dir.join(&item.content);
    let thumb = images_dir.join(db::thumbnail_file(&id));
    let modified = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
    if modified(&thumb).is_none() || modified(&thumb) < modified(&source) {
        let img = image::open(&source).map_err(|e| e.to_string())?;
        img.thumbnail(160, 160)
            .save(&thumb)
            .map_err(|e| e.to_string())?;
    }
    let bytes = std::fs::read(&thumb).map_err(|e| e.to_string())?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/png;base64,{encoded}"))
}

/// Copies plain text (e.g. text recognized in a screenshot). It becomes a
/// normal history entry, since the user is copying new content.
#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| e.to_string())
}

/// Copies one item straight to the system clipboard without hiding any window
/// or synthesizing a paste — used right after a single-image annotation is
/// saved (spec §9: "Copy the updated image to the system clipboard for
/// single-image editing"). Batch saves deliberately do NOT call this.
#[tauri::command]
fn copy_item_to_clipboard(
    app: tauri::AppHandle,
    monitor: tauri::State<Arc<MonitorState>>,
    id: String,
) -> Result<(), String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = db::open(&data_dir);
    let item = db::get_item(&conn, &id).ok_or_else(|| "item not found".to_string())?;
    write_item_to_clipboard(&data_dir, &monitor, &item)
}

/// One axis of `keep_on_screen`: the window's start moved just enough to fit
/// inside `[area_start, area_start + area_len)`, or pinned to the start when
/// the window is larger than the area.
fn clamp_axis(start: i32, len: u32, area_start: i32, area_len: u32) -> i32 {
    let max = area_start + area_len as i32 - len as i32;
    if max < area_start {
        area_start
    } else {
        start.clamp(area_start, max)
    }
}

/// Keeps the popup inside the visible area (minus menu bar and Dock / taskbar)
/// of the screen it is mostly on, or the primary screen when it is on none.
fn keep_on_screen(win: &tauri::WebviewWindow) {
    let (Ok(position), Ok(size)) = (win.outer_position(), win.outer_size()) else {
        return;
    };
    let monitor = match win.current_monitor() {
        Ok(Some(monitor)) => monitor,
        _ => match win.primary_monitor() {
            Ok(Some(monitor)) => monitor,
            _ => return,
        },
    };
    let area = monitor.work_area();
    let x = clamp_axis(position.x, size.width, area.position.x, area.size.width);
    let y = clamp_axis(position.y, size.height, area.position.y, area.size.height);
    if (x, y) != (position.x, position.y) {
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

#[cfg(target_os = "macos")]
fn mouse_button_down() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    }
    // Combined session state, left button.
    unsafe { CGEventSourceButtonState(0, 0) }
}

#[cfg(not(target_os = "macos"))]
fn mouse_button_down() -> bool {
    false
}

/// Called for every move event. Once the window has stopped moving and the
/// mouse is released, pulls it back on screen — never mid-drag, which would
/// fight the user's hand.
fn keep_on_screen_after_move(win: &tauri::WebviewWindow, last_move: &Arc<Mutex<Instant>>, settling: &Arc<AtomicBool>) {
    *last_move.lock().unwrap() = Instant::now();
    if settling.swap(true, Ordering::SeqCst) {
        return;
    }
    let (win, last_move, settling) = (win.clone(), last_move.clone(), settling.clone());
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if last_move.lock().unwrap().elapsed() >= Duration::from_millis(250) && !mouse_button_down() {
                break;
            }
        }
        settling.store(false, Ordering::SeqCst);
        keep_on_screen(&win);
    });
}

fn toggle_history_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("history") {
        let visible = win.is_visible().unwrap_or(false);
        if visible {
            let _ = win.hide();
        } else {
            #[cfg(target_os = "macos")]
            capture_previous_paste_target(&app.state::<Arc<PasteCoordinator>>());
            // Paste hides the whole app (see return_focus_to_previous_app);
            // a window of a hidden app won't come forward until the app is unhidden.
            #[cfg(target_os = "macos")]
            let _ = app.show();
            let _ = win.show();
            // Its screen may have been disconnected since it was moved there.
            keep_on_screen(&win);
            let _ = win.set_focus();
            let _ = app.emit("clipflow://history-opened", ());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let monitor = Arc::new(MonitorState::default());
    let paste_coordinator = Arc::new(PasteCoordinator::default());

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(monitor.clone())
        .manage(paste_coordinator)
        .manage(EditorSession::default())
        .invoke_handler(tauri::generate_handler![
            get_history,
            get_items,
            get_image_data_url,
            get_paused,
            set_paused,
            storage_usage_mb,
            hide_history,
            toggle_favorite,
            toggle_pinned,
            delete_item,
            clear_history,
            clear_non_favorites,
            paste_item,
            paste_items_sequential,
            start_annotation,
            get_editor_targets,
            close_editor,
            save_annotated_image,
            copy_item_to_clipboard,
            open_accessibility_settings,
            paste_snippet,
            copy_snippet,
            snippets::snippets_list,
            snippets::snippet_save,
            snippets::snippet_delete,
            snippets::snippet_set_enabled,
            snippets::snippet_duplicate,
            snippets::snippet_image_preview,
            snippets::snippet_settings_get,
            snippets::snippet_settings_set,
            snippets::snippets_status,
            snippets::snippets_pause,
            snippets::snippets_resume,
            get_thumbnail_data_url,
            copy_text,
            ocr::ocr_status,
            ocr::ocr_set_enabled,
            ocr::ocr_retry_failed,
            ocr::ocr_set_language,
            ocr::ocr_rescan,
            retention::history_retention_get,
            monitoring_status,
            pause_monitoring,
            update_item_text,
            save_annotated_image_as_new,
            paste_after_edit,
            file_entries,
            copy_file_paths,
            open_file_entry,
            retention::history_retention_preview,
            retention::history_retention_set,
            retention::history_clear_expired
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Background-first: no main window, no dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let data_dir = app.path().app_data_dir().expect("no app data dir");
            let snippet_service = snippets::SnippetService::new(handle.clone(), data_dir.clone());
            app.manage(snippet_service.clone());
            let ocr_service = ocr::OcrService::new(handle.clone(), data_dir.clone());
            app.manage(ocr_service.clone());
            ocr_service.start();
            retention::start(handle.clone());
            clipboard::spawn_monitor(handle.clone(), data_dir, monitor.clone());
            #[cfg(all(feature = "e2e", target_os = "macos"))]
            e2e::start(
                handle.clone(),
                monitor.clone(),
                app.state::<Arc<PasteCoordinator>>().inner().clone(),
            );

            // Tray icon + menu.
            let open_item = MenuItem::with_id(
                app,
                "open_history",
                "Open Clipboard History",
                true,
                Some("CmdOrCtrl+Shift+V"),
            )?;
            let capture_item =
                MenuItem::with_id(app, "capture", "Capture Screenshot", true, None::<&str>)?;
            let pause_item =
                MenuItem::with_id(app, "toggle_pause", "Pause Monitoring", true, None::<&str>)?;
            let storage_item = MenuItem::with_id(
                app,
                "storage",
                "Storage Usage: calculating…",
                false,
                None::<&str>,
            )?;
            let settings_item =
                MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Clipet", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;

            // Text expansion: a status line, a one-click pause/resume using the
            // default duration, and explicit durations.
            let expansion_status =
                MenuItem::with_id(app, "snippets_status", "Text Expansion: On", false, None::<&str>)?;
            let expansion_toggle = MenuItem::with_id(
                app,
                "snippets_toggle",
                "Pause Text Expansion",
                true,
                None::<&str>,
            )?;
            let pause_for = Submenu::with_items(
                app,
                "Pause Text Expansion For",
                true,
                &[
                    &MenuItem::with_id(app, "snippets_pause_5", "5 Minutes", true, None::<&str>)?,
                    &MenuItem::with_id(app, "snippets_pause_15", "15 Minutes", true, None::<&str>)?,
                    &MenuItem::with_id(app, "snippets_pause_30", "30 Minutes", true, None::<&str>)?,
                    &MenuItem::with_id(app, "snippets_pause_60", "1 Hour", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(
                        app,
                        "snippets_pause_indefinite",
                        "Until I Resume",
                        true,
                        None::<&str>,
                    )?,
                ],
            )?;

            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &capture_item,
                    &separator,
                    &pause_item,
                    &settings_item,
                    &storage_item,
                    &separator,
                    &expansion_status,
                    &expansion_toggle,
                    &pause_for,
                    &separator,
                    &quit_item,
                ],
            )?;

            let monitor_for_menu = monitor.clone();
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .tooltip("Clipet — running in the background")
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open_history" => toggle_history_window(app),
                    "capture" => {
                        // Native screenshot capture is implemented in a later phase (spec §7);
                        // this proves the tray → action wiring end-to-end for now.
                        let _ = app.emit("clipflow://capture-requested", ());
                    }
                    "toggle_pause" => {
                        let was_paused = monitor_for_menu.paused.load(Ordering::Relaxed);
                        monitor_for_menu
                            .paused
                            .store(!was_paused, Ordering::Relaxed);
                    }
                    "settings" => {
                        let _ = app.emit("clipflow://open-settings", ());
                        toggle_history_window(app);
                    }
                    "snippets_toggle" => snippets::service(app).toggle_pause(),
                    "snippets_pause_5" => snippets::service(app).pause_for(Some(5)),
                    "snippets_pause_15" => snippets::service(app).pause_for(Some(15)),
                    "snippets_pause_30" => snippets::service(app).pause_for(Some(30)),
                    "snippets_pause_60" => snippets::service(app).pause_for(Some(60)),
                    "snippets_pause_indefinite" => snippets::service(app).pause_for(None),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let handle_for_status = handle.clone();
            snippet_service.set_status_listener(move |status| {
                use snippets::ListenerState;
                let line = match (status.listener, status.enabled, status.paused, status.paused_until) {
                    (ListenerState::Unsupported, ..) => "Text Expansion: Not available on this OS".to_string(),
                    (ListenerState::NeedsPermission, ..) => "Text Expansion: Needs Accessibility".to_string(),
                    (_, false, ..) => "Text Expansion: Off".to_string(),
                    _ if status.blocked_by.is_some() && !status.paused => format!(
                        "Text Expansion: Blocked by {} (password field)",
                        status.blocked_by.as_deref().unwrap_or_default()
                    ),
                    (_, _, true, Some(until)) => {
                        let local = chrono::DateTime::from_timestamp_millis(until)
                            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                            .unwrap_or_default();
                        format!("Text Expansion: Paused until {local}")
                    }
                    (_, _, true, None) => "Text Expansion: Paused".to_string(),
                    _ => "Text Expansion: On".to_string(),
                };
                let toggle_text = if status.paused {
                    "Resume Text Expansion"
                } else {
                    "Pause Text Expansion"
                };
                let tooltip = if status.paused {
                    "Clipet — text expansion paused"
                } else {
                    "Clipet — running in the background"
                };
                // Menu/tray setters block until the main thread runs them.
                // Status changes can come from the keyboard-listener thread,
                // which must never wait on the main thread, so queue instead.
                let (status_item, toggle_item, tray) =
                    (expansion_status.clone(), expansion_toggle.clone(), tray.clone());
                let _ = handle_for_status.run_on_main_thread(move || {
                    let _ = status_item.set_text(&line);
                    let _ = toggle_item.set_text(toggle_text);
                    let _ = tray.set_tooltip(Some(tooltip));
                });
            });
            snippet_service.start(monitor.clone());

            // Global shortcut: ⌘⇧V on macOS, Ctrl+Shift+V elsewhere. (SUPER is
            // the Windows key on Windows — Win+Shift+V, next to the OS's own
            // Win+V clipboard history — so it can't be used cross-platform.)
            {
                use tauri_plugin_global_shortcut::{
                    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
                };
                #[cfg(target_os = "macos")]
                let primary = Modifiers::SUPER;
                #[cfg(not(target_os = "macos"))]
                let primary = Modifiers::CONTROL;
                let shortcut = Shortcut::new(Some(Modifiers::SHIFT | primary), Code::KeyV);
                let handle_for_shortcut = handle.clone();
                app.global_shortcut()
                    .on_shortcut(shortcut, move |_app, _shortcut, event| {
                        if event.state() == ShortcutState::Pressed {
                            toggle_history_window(&handle_for_shortcut);
                        }
                    })?;
            }

            // Click outside → popup hides. (Opening the editor hides it
            // explicitly and close_editor restores it, so no special case is
            // needed here for our own editor taking focus.)
            if let Some(win) = app.get_webview_window("history") {
                let win_clone = win.clone();
                let last_move = Arc::new(Mutex::new(Instant::now()));
                let settling = Arc::new(AtomicBool::new(false));
                win.on_window_event(move |event| match event {
                    tauri::WindowEvent::Focused(false) => {
                        let _ = win_clone.hide();
                    }
                    // Dragged by its chrome (see popupDrag.ts).
                    tauri::WindowEvent::Moved(_) => {
                        keep_on_screen_after_move(&win_clone, &last_move, &settling)
                    }
                    _ => {}
                });
            }

            // The editor is a real (decorated, resizable) window rather than a
            // borderless popup, so — unlike "history" — it does NOT hide on
            // focus loss; closing it is an explicit Done/Cancel action instead.
            // We do still intercept the OS close button to hide (not destroy)
            // it, since it's declared once in tauri.conf.json and reused.
            // The OS close button goes through the editor's own Cancel flow
            // (discard-confirm if dirty → close_editor → restore history)
            // rather than just hiding the window behind its back.
            if let Some(win) = app.get_webview_window("editor") {
                let win_clone = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = win_clone.emit("clipflow://editor-close-requested", ());
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDataDir(PathBuf);

    impl TestDataDir {
        fn new() -> Self {
            let serial = NEXT_TEST_DIR.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "clipet-paste-test-{}-{}-{serial}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &PathBuf {
            &self.0
        }

        fn save_image(&self, filename: &str, color: [u8; 4]) {
            let image = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
            image.save(self.0.join("images").join(filename)).unwrap();
        }
    }

    impl Drop for TestDataDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn insert(conn: &rusqlite::Connection, id: &str, kind: &str, content: &str, created_at: i64) {
        db::insert_item(conn, id, kind, content, content, "", created_at, id);
    }

    const DAY: i64 = 86_400_000;

    #[test]
    fn retention_off_by_default_and_protects_favorites_and_pinned() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        let now = 100 * DAY;
        assert_eq!(db::load_retention(&conn), db::Retention::default());
        insert(&conn, "old", "text", "old", now - 10 * DAY);
        insert(&conn, "new", "text", "new", now - DAY / 2);
        insert(&conn, "fav", "text", "fav", now - 10 * DAY);
        insert(&conn, "pin", "text", "pin", now - 10 * DAY);
        // Clock moved back: an item "from the future" never expires.
        insert(&conn, "future", "text", "future", now + 5 * DAY);
        db::set_favorite(&conn, "fav", true);
        db::set_pinned(&conn, "pin", true);

        // Off (the default for everyone): nothing is ever deleted.
        let off = db::Retention::default();
        assert_eq!(db::delete_expired(&conn, root.path(), &off, now).unwrap(), 0);

        let week = db::Retention { days: 7, ..off };
        assert_eq!(db::count_expired(&conn, &week, now), 1);
        assert_eq!(db::delete_expired(&conn, root.path(), &week, now).unwrap(), 1);
        assert!(db::get_item(&conn, "old").is_none());
        for kept in ["new", "fav", "pin", "future"] {
            assert!(db::get_item(&conn, kept).is_some(), "{kept} must be kept");
        }

        let everything = db::Retention { days: 7, include_favorites: true, include_pinned: true };
        assert_eq!(db::count_expired(&conn, &everything, now), 2);
        db::save_retention(&conn, &everything);
        assert_eq!(db::load_retention(&conn), everything);
    }

    #[test]
    fn retention_removes_managed_images_but_never_referenced_user_files() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        let now = 100 * DAY;
        root.save_image("shot.png", [1, 2, 3, 255]);
        root.save_image("shared.png", [4, 5, 6, 255]);
        insert(&conn, "img", "image", "shot.png", now - 40 * DAY);
        // Two entries using one file: deleting the old one must keep the file.
        insert(&conn, "shared-old", "image", "shared.png", now - 40 * DAY);
        insert(&conn, "shared-new", "image", "shared.png", now);
        let user_file = root.path().join("Final Python.zip");
        std::fs::write(&user_file, b"zip").unwrap();
        let paths = serde_json::to_string(&[user_file.to_string_lossy()]).unwrap();
        insert(&conn, "file", "file", &paths, now - 40 * DAY);

        let month = db::Retention { days: 30, ..db::Retention::default() };
        assert_eq!(db::delete_expired(&conn, root.path(), &month, now).unwrap(), 3);
        assert!(!root.path().join("images/shot.png").exists());
        assert!(root.path().join("images/shared.png").exists());
        assert!(user_file.exists(), "removing a file entry never deletes the file itself");
    }

    #[test]
    fn retention_handles_thousands_of_items_in_one_transaction() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        let now = 1000 * DAY;
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..5000 {
            insert(&conn, &format!("i{i}"), "text", "x", now - (i % 20) * DAY);
        }
        conn.execute_batch("COMMIT").unwrap();
        let retention = db::Retention { days: 10, ..db::Retention::default() };
        let started = std::time::Instant::now();
        let removed = db::delete_expired(&conn, root.path(), &retention, now).unwrap();
        assert_eq!(removed, 2250); // ages 11..19 days, 250 each
        assert_eq!(db::list_items(&conn, 10_000).len(), 2750);
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn file_entries_keep_real_paths_and_encode_links() {
        let root = TestDataDir::new();
        let names = ["Final Python.zip", "보고서 2026.xlsx", "ឯកសារ.pdf", "folder with space"];
        let paths: Vec<PathBuf> = names.iter().map(|n| root.path().join(n)).collect();
        for p in &paths[..3] {
            std::fs::write(p, b"data").unwrap();
        }
        std::fs::create_dir(&paths[3]).unwrap();

        let zip = file_entry(&paths[0]);
        assert!(zip.exists && zip.accessible && !zip.is_dir);
        assert_eq!(zip.extension, "zip");
        assert_eq!(zip.size, Some(4));
        let url = zip.url.unwrap();
        assert!(url.starts_with("file:///") && url.ends_with("/Final%20Python.zip"), "{url}");
        let korean = file_entry(&paths[1]).url.unwrap();
        assert!(!korean.contains(' ') && korean.contains("%EB%B3%B4"), "{korean}");
        assert!(file_entry(&paths[3]).is_dir);

        // Order is preserved; preview names the files.
        assert_eq!(clipboard::file_list_preview(&paths), "Final Python.zip, 보고서 2026.xlsx +2");
        assert_ne!(clipboard::hash_file_list(&paths), clipboard::hash_file_list(&paths[..1]));
    }

    #[test]
    fn pasting_a_moved_or_deleted_file_fails_before_touching_the_clipboard() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        let present = root.path().join("a.pdf");
        let gone = root.path().join("moved.pdf");
        std::fs::write(&present, b"pdf").unwrap();
        let content = serde_json::to_string(&[present.to_string_lossy(), gone.to_string_lossy()]).unwrap();
        insert(&conn, "files", "file", &content, 1);
        let error = match prepare_paste_plan(&conn, root.path(), &["files".into()]) {
            Err(e) => e,
            Ok(_) => panic!("a missing file must stop the paste"),
        };
        assert!(error.contains("moved.pdf") && error.contains("Nothing was pasted"), "{error}");

        std::fs::write(&gone, b"pdf").unwrap();
        match prepare_paste_plan(&conn, root.path(), &["files".into()]).unwrap() {
            PastePlan::Sequential { items } => match &items[0].payload {
                PreparedClipboardPayload::Files { paths, .. } => assert_eq!(paths, &vec![present, gone]),
                _ => panic!("file entries paste as files, never as their names"),
            },
            PastePlan::ImageBatch { .. } => panic!("file entries are not image batches"),
        }
    }

    /// Uses the real system clipboard (restored afterwards), so it only runs
    /// on request: `cargo test files_clipboard_roundtrip -- --ignored`.
    #[test]
    #[ignore]
    fn files_clipboard_roundtrip() {
        let root = TestDataDir::new();
        let paths: Vec<PathBuf> = ["Final Python.zip", "보고서 2026.xlsx", "ឯកសារ.pdf"]
            .iter()
            .map(|n| {
                let p = root.path().join(n);
                std::fs::write(&p, b"x").unwrap();
                p.canonicalize().unwrap()
            })
            .collect();
        let mut system = arboard::Clipboard::new().unwrap();
        let saved = system.get_text().ok();
        let monitor = Arc::new(MonitorState::default());
        let payload = PreparedClipboardPayload::Files { hash: clipboard::hash_file_list(&paths), paths: paths.clone() };
        write_prepared_payload(&mut system, &monitor, &payload).unwrap();
        let read = clipboard::clipboard_file_paths(&mut system);
        if let Some(text) = saved {
            system.set_text(text).unwrap();
        }
        assert_eq!(read.as_ref(), Some(&paths), "Paste File must publish every file, in order");
        // Clipet's own paste is recognized and not recorded again.
        assert_eq!(monitor.last_self_write_hash.lock().unwrap().as_deref(), Some(clipboard::hash_file_list(&paths).as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlinks_and_unreadable_files_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestDataDir::new();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(root.path().join("nowhere.txt"), &link).unwrap();
        let broken = file_entry(&link);
        assert!(broken.is_symlink && !broken.exists);

        let secret = root.path().join("secret.txt");
        std::fs::write(&secret, b"x").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        let locked = file_entry(&secret);
        assert!(locked.exists && !locked.accessible);
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn all_image_selection_plans_one_native_batch_in_requested_order() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        root.save_image("first.png", [255, 0, 0, 255]);
        root.save_image("second.png", [0, 255, 0, 255]);
        insert(&conn, "first", "image", "first.png", 1);
        insert(&conn, "second", "screenshot", "second.png", 2);

        let ids = vec!["second".to_string(), "first".to_string()];
        let plan = prepare_paste_plan(&conn, root.path(), &ids).unwrap();
        match plan {
            PastePlan::ImageBatch { images } => {
                let planned_ids = images
                    .iter()
                    .map(|image| image.id.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(planned_ids, vec!["second", "first"]);
                let filenames = images
                    .iter()
                    .map(|image| {
                        image
                            .path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(filenames, vec!["second.png", "first.png"]);
                assert_eq!(
                    images[0].png_bytes,
                    std::fs::read(root.path().join("images").join("second.png")).unwrap()
                );
                let expected_first_rgba = [0, 255, 0, 255].repeat(4);
                assert_eq!(
                    images[0].rgba_hash,
                    clipboard::hash_bytes(&expected_first_rgba),
                    "the first ordered image hash suppresses monitor re-ingestion"
                );
            }
            PastePlan::Sequential { .. } => panic!("multiple images must use one native batch"),
        }
    }

    #[test]
    fn mixed_selection_is_fully_preloaded_before_transfer() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        root.save_image("picture.png", [1, 2, 3, 255]);
        insert(&conn, "text", "text", "hello", 1);
        insert(&conn, "picture", "image", "picture.png", 2);

        let ids = vec!["text".to_string(), "picture".to_string()];
        let plan = prepare_paste_plan(&conn, root.path(), &ids).unwrap();
        match plan {
            PastePlan::Sequential { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].id, "text");
                assert_eq!(items[1].id, "picture");
                assert!(matches!(
                    &items[0].payload,
                    PreparedClipboardPayload::Text { content, .. } if content == "hello"
                ));
                assert!(matches!(
                    &items[1].payload,
                    PreparedClipboardPayload::Image { width: 2, height: 2, bytes, .. }
                        if bytes.len() == 16
                ));
            }
            PastePlan::ImageBatch { .. } => panic!("mixed content must use the fallback"),
        }
    }

    #[test]
    fn missing_image_fails_during_preflight() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        insert(&conn, "missing", "image", "missing.png", 1);

        let error = prepare_paste_plan(&conn, root.path(), &["missing".to_string()])
            .err()
            .expect("preflight should fail");
        assert!(error.contains("image missing is unavailable"), "{error}");
    }

    #[test]
    fn truncated_batch_image_fails_full_decode_preflight() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());
        root.save_image("valid.png", [10, 20, 30, 255]);
        root.save_image("truncated.png", [40, 50, 60, 255]);
        let truncated_path = root.path().join("images").join("truncated.png");
        let mut truncated = std::fs::read(&truncated_path).unwrap();
        truncated.truncate(truncated.len() / 2);
        std::fs::write(&truncated_path, truncated).unwrap();
        assert!(
            image::image_dimensions(&truncated_path).is_ok(),
            "fixture should retain a readable PNG header"
        );
        insert(&conn, "valid", "image", "valid.png", 1);
        insert(&conn, "truncated", "image", "truncated.png", 2);

        let error = prepare_paste_plan(
            &conn,
            root.path(),
            &["valid".to_string(), "truncated".to_string()],
        )
        .err()
        .expect("full decode preflight should fail");
        assert!(error.contains("image truncated cannot be read"), "{error}");
    }

    #[test]
    fn missing_database_item_fails_during_preflight() {
        let root = TestDataDir::new();
        let conn = db::open(root.path());

        let error = prepare_paste_plan(&conn, root.path(), &["unknown".to_string()])
            .err()
            .expect("preflight should fail");
        assert_eq!(error, "item unknown not found");
    }

    #[test]
    fn focus_handoff_uses_any_external_app_when_target_is_unavailable() {
        assert!(focus_handoff_complete(200, 100, None));
        // Popup placement: fits → unchanged; past an edge → pulled in;
        // larger than the screen → pinned to its start.
        assert_eq!(clamp_axis(100, 800, 0, 1512), 100);
        assert_eq!(clamp_axis(1200, 800, 0, 1512), 712);
        assert_eq!(clamp_axis(-300, 800, 0, 1512), 0);
        assert_eq!(clamp_axis(-1800, 800, -1920, 1920), -1800);
        assert_eq!(clamp_axis(-100, 800, -1920, 1920), -800);
        assert_eq!(clamp_axis(50, 2000, 25, 957), 25);
        assert!(!focus_handoff_complete(100, 100, None));
        assert!(focus_handoff_complete(300, 100, Some(300)));
        assert!(!focus_handoff_complete(200, 100, Some(300)));
    }

    #[test]
    fn accessibility_guidance_distinguishes_packaged_and_development_binaries() {
        let packaged = Path::new("/Applications/Clipet.app/Contents/MacOS/app");
        assert_eq!(
            app_bundle_from_executable(packaged),
            Some(PathBuf::from("/Applications/Clipet.app"))
        );

        let development = Path::new("/workspace/app/src-tauri/target/debug/app");
        assert_eq!(app_bundle_from_executable(development), None);
    }
}
