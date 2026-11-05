//! Offline OCR for image history, so screenshots can be found by the text in
//! them. Uses the OS's built-in engine (Apple Vision on macOS, Windows.Media.Ocr
//! on Windows): no network, no API keys, nothing bundled.
//!
//! One background worker processes images newest-first, one at a time, with a
//! short pause between images so it never competes with capture or the UI.
//! Progress lives in the database (`clip_items.ocr_status`), so a restart
//! resumes where it left off and existing history gets indexed too.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(all(test, target_os = "macos"))]
mod bench;

use crate::db;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const UPDATED_EVENT: &str = "clipflow://ocr-updated";
const ENABLED_KEY: &str = "ocr.enabled";
/// Breathing room between images so a large backlog stays a background task.
const PAUSE_BETWEEN_IMAGES: Duration = Duration::from_millis(250);
/// Coalesces UI refreshes while a backlog is being indexed.
const NOTIFY_EVERY: usize = 5;

/// One recognized line. Box coordinates are fractions of the image size with
/// the origin at the top-left, so the UI can overlay them at any zoom.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Which languages to recognize. "Auto" lets the engine pick per image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OcrLanguage {
    Auto,
    KoreanEnglish,
    English,
    Korean,
}

/// Engine settings (Vision's names; Windows maps languages only).
#[derive(Clone, Debug)]
pub struct OcrOptions {
    pub languages: Vec<String>,
    pub auto_detect: bool,
    pub language_correction: bool,
    pub accurate: bool,
}

impl OcrOptions {
    pub fn for_language(language: OcrLanguage) -> Self {
        let languages: &[&str] = match language {
            OcrLanguage::Auto => &[],
            OcrLanguage::KoreanEnglish => &["ko-KR", "en-US"],
            OcrLanguage::English => &["en-US"],
            OcrLanguage::Korean => &["ko-KR"],
        };
        Self {
            languages: languages.iter().map(|l| l.to_string()).collect(),
            auto_detect: language == OcrLanguage::Auto,
            language_correction: true,
            accurate: true,
        }
    }
}

/// Image preparation before recognition. Always applied to an in-memory
/// copy; the stored screenshot is never modified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // the other variants exist for the benchmark
pub enum Preprocess {
    None,
    /// 2× Catmull-Rom upscale.
    Upscale2x,
    /// Grayscale, contrast stretched to the full range, dark backgrounds inverted.
    GrayContrast,
    /// The production policy (see `prepare`).
    Auto,
}

/// Images whose longer side is below this get upscaled under `Auto`.
const UPSCALE_BELOW_PX: u32 = 1600;
const MAX_PREPARED_SIDE: u32 = 4096;

pub(crate) fn prepare(original: &[u8], preprocess: Preprocess) -> Result<Vec<u8>, String> {
    use image::{DynamicImage, GenericImageView, imageops::FilterType};
    let decode = || image::load_from_memory(original).map_err(|e| format!("not a readable image: {e}"));
    let encode = |img: &DynamicImage| -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(out)
    };
    let upscale = |img: &DynamicImage| {
        let (w, h) = img.dimensions();
        let factor = 2.0f64.min(f64::from(MAX_PREPARED_SIDE) / f64::from(w.max(h).max(1)));
        img.resize(
            (f64::from(w) * factor) as u32,
            (f64::from(h) * factor) as u32,
            FilterType::CatmullRom,
        )
    };
    match preprocess {
        Preprocess::None => Ok(original.to_vec()),
        Preprocess::Upscale2x => encode(&upscale(&decode()?)),
        Preprocess::GrayContrast => {
            let mut gray = decode()?.to_luma8();
            let (lo, hi) = gray.pixels().fold((255u8, 0u8), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
            let mean = gray.pixels().map(|p| u64::from(p[0])).sum::<u64>() / u64::from(gray.width() * gray.height()).max(1);
            let span = f32::from(hi.saturating_sub(lo)).max(1.0);
            for p in gray.pixels_mut() {
                let v = (f32::from(p[0] - lo) * 255.0 / span) as u8;
                p[0] = if mean < 128 { 255 - v } else { v };
            }
            encode(&DynamicImage::ImageLuma8(gray))
        }
        Preprocess::Auto => {
            let img = decode()?;
            let (w, h) = img.dimensions();
            if w.max(h) < UPSCALE_BELOW_PX { encode(&upscale(&img)) } else { Ok(original.to_vec()) }
        }
    }
}

/// Lower-cases for matching. Text from both engines is already NFC.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn normalize_for_search(text: &str) -> String {
    text.to_lowercase()
}

pub(crate) fn recognize_prepared(image: &[u8], options: &OcrOptions) -> Result<Vec<OcrLine>, String> {
    #[cfg(target_os = "macos")]
    return macos::recognize_data(image, options);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (image, options);
        Err("in-memory recognition is only implemented on macOS".to_string())
    }
}

pub(crate) fn recognize(path: &Path, language: OcrLanguage) -> Result<Vec<OcrLine>, String> {
    if !path.is_file() {
        return Err("the image file is missing".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        let original = std::fs::read(path).map_err(|e| format!("could not read the image: {e}"))?;
        let prepared = prepare(&original, Preprocess::Auto)?;
        return recognize_prepared(&prepared, &OcrOptions::for_language(language));
    }
    #[cfg(windows)]
    return windows::recognize(path, language);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = language;
        Err("OCR is not available on this platform".to_string())
    }
}

pub fn supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// Bumped whenever recognition settings or preprocessing change in a way that
/// improves results. Images indexed by an older version are re-scanned in the
/// background (after new images), keeping their old text searchable until
/// the new result replaces it.
///   1: Vision auto-detect, original image.
///   2: Korean + English by default, small images upscaled 2× (measured with
///      the benchmark in `bench.rs`).
pub const OCR_VERSION: i64 = 2;

const LANGUAGE_KEY: &str = "ocr.language";
const DEFAULT_LANGUAGE: OcrLanguage = OcrLanguage::KoreanEnglish;

#[derive(Serialize)]
pub struct OcrStatus {
    pub supported: bool,
    pub enabled: bool,
    pub language: OcrLanguage,
    /// New images waiting for their first scan.
    pub pending: i64,
    /// Already-indexed images queued for a re-scan with improved settings.
    pub updating: i64,
    pub done: i64,
    pub failed: i64,
}

pub struct OcrService {
    app: AppHandle,
    data_dir: PathBuf,
    enabled: AtomicBool,
    language: Mutex<OcrLanguage>,
    wake_flag: Mutex<bool>,
    wake_signal: Condvar,
}

impl OcrService {
    pub fn new(app: AppHandle, data_dir: PathBuf) -> Arc<Self> {
        let conn = db::open(&data_dir);
        db::fail_interrupted_ocr(&conn);
        let enabled = db::get_setting(&conn, ENABLED_KEY).as_deref() != Some("false");
        let language = db::get_setting(&conn, LANGUAGE_KEY)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(DEFAULT_LANGUAGE);
        Arc::new(Self {
            app,
            data_dir,
            enabled: AtomicBool::new(enabled),
            language: Mutex::new(language),
            wake_flag: Mutex::new(false),
            wake_signal: Condvar::new(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        if !supported() {
            return;
        }
        let service = self.clone();
        std::thread::spawn(move || service.run());
    }

    pub fn wake(&self) {
        *self.wake_flag.lock().unwrap() = true;
        self.wake_signal.notify_all();
    }

    fn conn(&self) -> rusqlite::Connection {
        db::open(&self.data_dir)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        db::set_setting(&self.conn(), ENABLED_KEY, if enabled { "true" } else { "false" });
        self.wake();
    }

    /// Changing the language re-scans every image in the background; the
    /// current text stays searchable until each new result arrives.
    pub fn set_language(&self, language: OcrLanguage) {
        let mut current = self.language.lock().unwrap();
        if *current == language {
            return;
        }
        *current = language;
        drop(current);
        let conn = self.conn();
        db::set_setting(&conn, LANGUAGE_KEY, &serde_json::to_string(&language).unwrap());
        db::mark_all_ocr_stale(&conn);
        self.wake();
    }

    pub fn status(&self) -> OcrStatus {
        let counts = db::ocr_counts(&self.conn(), OCR_VERSION);
        OcrStatus {
            supported: supported(),
            enabled: self.enabled.load(Ordering::SeqCst),
            language: *self.language.lock().unwrap(),
            pending: counts.pending,
            updating: counts.stale,
            done: counts.done,
            failed: counts.failed,
        }
    }

    fn wait(&self, timeout: Duration) {
        let mut flag = self.wake_flag.lock().unwrap();
        if !*flag {
            flag = self.wake_signal.wait_timeout(flag, timeout).unwrap().0;
        }
        *flag = false;
    }

    fn run(&self) {
        let conn = self.conn();
        let images_dir = self.data_dir.join("images");
        let mut since_notify = 0usize;
        loop {
            if !self.enabled.load(Ordering::SeqCst) {
                self.wait(Duration::from_secs(3600));
                continue;
            }
            let Some(job) = db::next_ocr_job(&conn, OCR_VERSION) else {
                if since_notify > 0 {
                    since_notify = 0;
                    let _ = self.app.emit(UPDATED_EVENT, ());
                }
                self.wait(Duration::from_secs(600));
                continue;
            };

            // Crash safety: a first scan is marked running (an interrupted one
            // becomes "failed" at the next start); a re-scan is stamped with
            // the new version up front, so an image that crashes the engine
            // is never retried in a loop. Either way the old text survives.
            if job.rescan {
                db::set_ocr_version(&conn, &job.id, OCR_VERSION);
            } else {
                db::set_ocr_status(&conn, &job.id, db::OCR_RUNNING);
            }
            let language = *self.language.lock().unwrap();
            match recognize(&images_dir.join(&job.file), language) {
                Ok(lines) => {
                    let text = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
                    let json = serde_json::to_string(&lines).unwrap_or_else(|_| "[]".into());
                    db::set_ocr_result(&conn, &job.id, &text, &json, OCR_VERSION);
                }
                Err(error) => {
                    // Logged without any recognized text. A deleted item's
                    // row is gone, so this update is a no-op.
                    eprintln!("[clipet] OCR failed for {}: {error}", job.id);
                    db::set_ocr_failed(&conn, &job.id, &error, OCR_VERSION);
                }
            }
            since_notify += 1;
            if since_notify >= NOTIFY_EVERY {
                since_notify = 0;
                let _ = self.app.emit(UPDATED_EVENT, ());
            }
            std::thread::sleep(PAUSE_BETWEEN_IMAGES);
        }
    }
}

/// Nudges the worker after a new image is saved or an image is edited.
pub fn wake(app: &AppHandle) {
    if let Some(service) = app.try_state::<Arc<OcrService>>() {
        service.wake();
    }
}

type Service<'a> = tauri::State<'a, Arc<OcrService>>;

#[tauri::command]
pub fn ocr_status(service: Service) -> OcrStatus {
    service.status()
}

#[tauri::command]
pub fn ocr_set_enabled(service: Service, enabled: bool) -> OcrStatus {
    service.set_enabled(enabled);
    service.status()
}

#[tauri::command]
pub fn ocr_set_language(service: Service, language: OcrLanguage) -> OcrStatus {
    service.set_language(language);
    service.status()
}

/// Re-scans specific images now (ahead of the background backlog).
#[tauri::command]
pub fn ocr_rescan(service: Service, ids: Vec<String>) -> OcrStatus {
    let conn = service.conn();
    for id in &ids {
        db::queue_ocr(&conn, id);
    }
    service.wake();
    let _ = service.app.emit(UPDATED_EVENT, ());
    service.status()
}

/// Queues failed images for another attempt (e.g. after installing an OCR
/// language on Windows).
#[tauri::command]
pub fn ocr_retry_failed(service: Service) -> OcrStatus {
    db::requeue_failed_ocr(&service.conn());
    service.wake();
    service.status()
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("clipet-ocr-test-{}-{n}", std::process::id()))
    }

    #[test]
    fn queue_states_survive_restart_and_deletion() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        db::insert_item(&conn, "old", "image", "old.png", "1x1", "", 1, "h-old");
        db::insert_item(&conn, "new", "image", "new.png", "1x1", "", 2, "h-new");
        db::insert_item(&conn, "txt", "text", "hello", "hello", "", 3, "h-txt");

        // Newest image first; text items are never queued.
        assert_eq!(db::next_ocr_job(&conn, OCR_VERSION).unwrap().id, "new");
        assert_eq!(db::ocr_counts(&conn, OCR_VERSION).pending, 2);

        // Simulate a crash mid-OCR: on the next start it's marked failed, not retried.
        db::set_ocr_status(&conn, "new", db::OCR_RUNNING);
        db::fail_interrupted_ocr(&db::open(&dir));
        assert_eq!(db::next_ocr_job(&conn, OCR_VERSION).unwrap().id, "old");
        let item = db::get_item(&conn, "new").unwrap();
        assert_eq!(item.ocr_status, db::OCR_FAILED);
        assert!(item.ocr_error.unwrap().contains("interrupted"));

        // Deleted while recognizing: storing the result is a harmless no-op.
        db::delete_item(&conn, &dir, "old");
        assert!(!db::set_ocr_result(&conn, "old", "x", "[]", OCR_VERSION));
        assert!(db::next_ocr_job(&conn, OCR_VERSION).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn older_results_are_rescanned_after_new_images_and_keep_their_text() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        db::insert_item(&conn, "indexed", "image", "a.png", "1x1", "", 2, "a");
        db::set_ocr_result(&conn, "indexed", "매출관리내역 기", "[]", 1);
        db::insert_item(&conn, "fresh", "image", "b.png", "1x1", "", 1, "b");

        // New images come before re-scans, even when older.
        let job = db::next_ocr_job(&conn, OCR_VERSION).unwrap();
        assert_eq!((job.id.as_str(), job.rescan), ("fresh", false));
        db::set_ocr_result(&conn, "fresh", "", "[]", OCR_VERSION);

        let job = db::next_ocr_job(&conn, OCR_VERSION).unwrap();
        assert_eq!((job.id.as_str(), job.rescan), ("indexed", true));
        assert_eq!(db::ocr_counts(&conn, OCR_VERSION).stale, 1);
        // The re-scan is stamped first, so a crash never loops on it, and the
        // old text stays searchable until the new result arrives.
        db::set_ocr_version(&conn, "indexed", OCR_VERSION);
        assert!(db::next_ocr_job(&conn, OCR_VERSION).is_none());
        assert_eq!(db::get_item(&conn, "indexed").unwrap().ocr_text.as_deref(), Some("매출관리내역 기"));

        // A failed re-scan keeps the earlier text too.
        db::set_ocr_failed(&conn, "indexed", "boom", OCR_VERSION);
        assert_eq!(db::get_item(&conn, "indexed").unwrap().ocr_text.as_deref(), Some("매출관리내역 기"));

        // Settings change: everything goes back through the queue.
        db::mark_all_ocr_stale(&conn);
        assert_eq!(db::ocr_counts(&conn, OCR_VERSION), db::OcrCounts { pending: 1, stale: 1, done: 1, failed: 0 });

        // Manual re-scan of one image puts it at the front.
        db::queue_ocr(&conn, "fresh");
        assert_eq!(db::next_ocr_job(&conn, OCR_VERSION).unwrap().rescan, false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn results_are_stored_and_editing_requeues() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        db::insert_item(&conn, "a", "image", "a.png", "1x1", "", 1, "h");
        assert!(db::set_ocr_result(&conn, "a", "NullPointerException", "[]", OCR_VERSION));
        let item = db::get_item(&conn, "a").unwrap();
        assert_eq!(item.ocr_status, db::OCR_DONE);
        assert_eq!(item.ocr_text.as_deref(), Some("NullPointerException"));

        db::mark_edited(&conn, "a");
        let item = db::get_item(&conn, "a").unwrap();
        assert_eq!(item.ocr_status, db::OCR_PENDING);
        assert_eq!(item.ocr_text, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enabled_setting_persists() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        assert_eq!(db::get_setting(&conn, ENABLED_KEY), None);
        db::set_setting(&conn, ENABLED_KEY, "false");
        assert_eq!(db::get_setting(&db::open(&dir), ENABLED_KEY).as_deref(), Some("false"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_keeps_favorites_and_pinned_and_removes_derived_files() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        let images = dir.join("images");
        for (id, fav, pin) in [("fav", true, false), ("pin", false, true), ("gone", false, false)] {
            let file = format!("{id}.png");
            std::fs::write(images.join(&file), b"x").unwrap();
            for derived in db::derived_image_files(id) {
                std::fs::write(images.join(derived), b"x").unwrap();
            }
            db::insert_item(&conn, id, "image", &file, "1x1", "", 1, id);
            db::set_favorite(&conn, id, fav);
            db::set_pinned(&conn, id, pin);
        }
        db::clear_non_favorites(&conn, &dir);
        let left: Vec<_> = db::list_items(&conn, 10).into_iter().map(|i| i.id).collect();
        assert!(left.contains(&"fav".to_string()) && left.contains(&"pin".to_string()));
        assert_eq!(left.len(), 2);
        assert!(!images.join("gone.png").exists());
        for derived in db::derived_image_files("gone") {
            assert!(!images.join(derived).exists(), "derived files of cleared items are removed");
        }
        assert!(images.join(db::thumbnail_file("pin")).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repeated_image_is_found_by_hash() {
        let dir = temp_dir();
        let conn = db::open(&dir);
        db::insert_item(&conn, "a", "image", "a.png", "1x1", "", 1, "same");
        db::insert_item(&conn, "t", "text", "same", "same", "", 2, "same");
        assert_eq!(db::find_image_by_hash(&conn, "same").unwrap().0, "a");
        db::touch_item(&conn, "a", 99);
        assert_eq!(db::list_items(&conn, 1)[0].id, "a");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Real Vision OCR on a rendered image. macOS only; runs in `cargo test`.
    #[cfg(target_os = "macos")]
    #[test]
    fn recognizes_text_in_an_image() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("text.png");
        macos::render_text_image(&path, &["NullPointerException at line 42"], 40.0, false);
        let lines = recognize(&path, OcrLanguage::Auto).expect("vision should run");
        let text = lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join(" ");
        assert!(text.contains("NullPointerException"), "recognized: {text:?}");
        let line = &lines[0];
        assert!(line.x >= 0.0 && line.y >= 0.0 && line.x + line.w <= 1.01 && line.y + line.h <= 1.01);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Korean UI labels at small sizes, light and dark, must come back exactly.
    #[cfg(target_os = "macos")]
    #[test]
    fn recognizes_korean_and_english_ui_text() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let phrases = ["매출관리내역", "청구서 발행관리", "세금계산서", "계약 등록", "NullPointerException", "SCR-019"];
        for (size, dark) in [(13.0, false), (13.0, true)] {
            let path = dir.join(format!("ko-{size}-{dark}.png"));
            macos::render_text_image(&path, &phrases, size, dark);
            let text: String = recognize(&path, DEFAULT_LANGUAGE).unwrap().into_iter().map(|l| l.text + "\n").collect();
            // Vision sometimes inserts a space inside long Latin words
            // ("Null PointerException"); search ignores spaces in OCR text for
            // exactly that reason, so English is checked the same way.
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for phrase in phrases {
                if phrase.is_ascii() {
                    assert!(compact.contains(&phrase.replace(' ', "")), "{phrase} missing at {size}px dark={dark}: {text:?}");
                } else {
                    assert!(text.contains(phrase), "{phrase} not exact at {size}px dark={dark}: {text:?}");
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn image_without_text_yields_no_lines() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blank.png");
        image::RgbaImage::from_pixel(200, 120, image::Rgba([240, 240, 240, 255]))
            .save(&path)
            .unwrap();
        assert!(recognize(&path, OcrLanguage::Auto).unwrap().is_empty());
        assert!(recognize(&dir.join("missing.png"), OcrLanguage::Auto).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
