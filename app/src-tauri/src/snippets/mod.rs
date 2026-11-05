//! Smart Snippets: `;keyword` + Space/Tab expands to saved text (or pastes a
//! saved image) anywhere.
//!
//! - `engine` — pure word tracking and match/escape decisions (unit-tested).
//! - `store` — SQLite persistence for snippets and their settings.
//! - `macos` — the native keyboard hook and text injection. Other platforms
//!   report the listener as unsupported until their hook is implemented.

pub mod engine;
#[cfg(target_os = "macos")]
mod macos;
pub mod store;

use engine::{EngineConfig, IndexedSnippet, SnippetIndex};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Duration;
use store::{PauseState, Snippet, SnippetSettings};
use tauri::{AppHandle, Emitter, Manager};

use crate::clipboard::MonitorState;

pub const STATUS_EVENT: &str = "clipet://snippets-status";
pub const CHANGED_EVENT: &str = "clipet://snippets-changed";

const MAX_CONTENT_CHARS: usize = 100_000;
/// Longest side of the preview returned to the snippet list and editor.
const PREVIEW_PX: u32 = 480;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListenerState {
    Starting,
    Active,
    NeedsPermission,
    Unsupported,
}

impl ListenerState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Active,
            2 => Self::NeedsPermission,
            3 => Self::Unsupported,
            _ => Self::Starting,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct SnippetStatus {
    pub enabled: bool,
    pub paused: bool,
    /// Epoch milliseconds when a timed pause ends; `None` while active or
    /// paused until manually resumed.
    pub paused_until: Option<i64>,
    pub listener: ListenerState,
}

/// What the keyboard hook reads on every key press, swapped atomically as a
/// whole whenever snippets or settings change.
struct Live {
    settings: SnippetSettings,
    config: EngineConfig,
    index: SnippetIndex,
}

pub struct SnippetService {
    app: AppHandle,
    data_dir: PathBuf,
    live: RwLock<Live>,
    /// 0 = active, -1 = paused until resumed, otherwise the epoch-ms deadline.
    /// An atomic so the hook can check it on every key without locking; the
    /// deadline is wall-clock time, so a pause that ends while the Mac sleeps
    /// is already over at the first key press after wake.
    pause: AtomicI64,
    pause_lock: Mutex<()>,
    pause_changed: Condvar,
    /// Bumped on every pause/settings/snippet change. The hook forgets the
    /// word in progress when it sees a new generation, so nothing typed
    /// before a pause can expand after it.
    generation: AtomicU64,
    listener: AtomicU8,
    on_status: OnceLock<Box<dyn Fn(&SnippetStatus) + Send + Sync>>,
}

static DIAG: OnceLock<Mutex<std::sync::mpsc::Sender<String>>> = OnceLock::new();

/// Appends one line to `~/Library/Logs/com.clipflow.app/snippets.log`.
/// Records listener events and expansion outcomes only — never typed text.
/// Writes happen on a background thread so the keyboard hook never waits on
/// disk I/O.
pub(crate) fn diag(line: &str) {
    if let Some(sender) = DIAG.get() {
        let stamped = format!("{} {line}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
        let _ = sender.lock().map(|s| s.send(stamped));
    }
}

fn start_diag_log(app: &AppHandle) {
    let Ok(dir) = app.path().app_log_dir() else {
        return;
    };
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    if DIAG.set(Mutex::new(sender)).is_err() {
        return;
    }
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("snippets.log");
        // Keep the log small: start over once it passes 512 KB.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
            let _ = std::fs::remove_file(&path);
        }
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        for line in receiver {
            let _ = file.write_all(line.as_bytes());
        }
    });
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn new_id() -> String {
    format!("snip-{:x}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default())
}

fn build_live(conn: &rusqlite::Connection) -> Live {
    let settings = store::load_settings(conn);
    let prefix = engine::validate_prefix(&settings.prefix).unwrap_or(';');
    let snippets = store::list_snippets(conn);
    let index = SnippetIndex::new(
        snippets.iter().filter(|s| s.enabled).map(|s| {
            (
                s.keyword.as_str(),
                IndexedSnippet {
                    id: s.id.clone(),
                    content: s.content.clone(),
                    image: s.content_type == store::IMAGE,
                },
            )
        }),
        settings.case_sensitive,
    );
    Live {
        config: EngineConfig {
            prefix,
            mode: settings.trigger_mode,
            escape_enabled: settings.escape_enabled,
        },
        settings,
        index,
    }
}

impl SnippetService {
    pub fn new(app: AppHandle, data_dir: PathBuf) -> Arc<Self> {
        start_diag_log(&app);
        let conn = store::open(&data_dir);
        let pause = match store::load_pause(&conn) {
            PauseState::Active => 0,
            PauseState::Indefinite => -1,
            PauseState::Until(t) if t > now_ms() => t,
            PauseState::Until(_) => 0,
        };
        Arc::new(Self {
            app,
            live: RwLock::new(build_live(&conn)),
            data_dir,
            pause: AtomicI64::new(pause),
            pause_lock: Mutex::new(()),
            pause_changed: Condvar::new(),
            generation: AtomicU64::new(0),
            listener: AtomicU8::new(0),
            on_status: OnceLock::new(),
        })
    }

    /// Starts the keyboard listener and the pause-expiry timer.
    pub fn start(self: &Arc<Self>, monitor: Arc<MonitorState>) {
        self.spawn_pause_timer();
        #[cfg(target_os = "macos")]
        macos::start(self.clone(), monitor);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = monitor;
            self.set_listener(ListenerState::Unsupported);
        }
    }

    /// Registers the tray updater. Called once during setup.
    pub fn set_status_listener(&self, f: impl Fn(&SnippetStatus) + Send + Sync + 'static) {
        let _ = self.on_status.set(Box::new(f));
        self.notify();
    }

    fn conn(&self) -> rusqlite::Connection {
        store::open(&self.data_dir)
    }

    fn reload(&self) {
        let live = build_live(&self.conn());
        *self.live.write().unwrap() = live;
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.app.emit(CHANGED_EVENT, ());
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn set_listener(&self, state: ListenerState) {
        let value = match state {
            ListenerState::Starting => 0,
            ListenerState::Active => 1,
            ListenerState::NeedsPermission => 2,
            ListenerState::Unsupported => 3,
        };
        if self.listener.swap(value, Ordering::SeqCst) != value {
            diag(&format!("listener: {state:?}"));
            self.notify();
        }
    }

    fn is_paused(&self) -> bool {
        match self.pause.load(Ordering::SeqCst) {
            0 => false,
            -1 => true,
            until => now_ms() < until,
        }
    }

    /// Whether a typed shortcut may expand right now.
    pub(crate) fn is_expanding(&self) -> bool {
        !self.is_paused() && self.live.read().unwrap().settings.enabled
    }

    /// Runs `f` against the current config and index under one read lock.
    pub(crate) fn with_live<R>(&self, f: impl FnOnce(&EngineConfig, &SnippetIndex) -> R) -> R {
        let live = self.live.read().unwrap();
        f(&live.config, &live.index)
    }

    pub fn status(&self) -> SnippetStatus {
        let raw = self.pause.load(Ordering::SeqCst);
        let paused = self.is_paused();
        SnippetStatus {
            enabled: self.live.read().unwrap().settings.enabled,
            paused,
            paused_until: (paused && raw > 0).then_some(raw),
            listener: ListenerState::from_u8(self.listener.load(Ordering::SeqCst)),
        }
    }

    fn notify(&self) {
        let status = self.status();
        if let Some(f) = self.on_status.get() {
            f(&status);
        }
        let _ = self.app.emit(STATUS_EVENT, &status);
    }

    /// `None` pauses until manually resumed.
    pub fn pause_for(&self, minutes: Option<u32>) {
        let state = match minutes {
            Some(m) => PauseState::Until(now_ms() + i64::from(m.max(1)) * 60_000),
            None => PauseState::Indefinite,
        };
        self.set_pause(state);
    }

    pub fn resume(&self) {
        self.set_pause(PauseState::Active);
    }

    /// Tray "Pause/Resume Text Expansion" item: resumes when paused, otherwise
    /// pauses for the configured default duration.
    pub fn toggle_pause(&self) {
        if self.is_paused() {
            self.resume();
        } else {
            let minutes = self.live.read().unwrap().settings.default_pause_minutes;
            self.pause_for((minutes > 0).then_some(minutes));
        }
    }

    fn set_pause(&self, state: PauseState) {
        {
            let _guard = self.pause_lock.lock().unwrap();
            let raw = match state {
                PauseState::Active => 0,
                PauseState::Indefinite => -1,
                PauseState::Until(t) => t,
            };
            self.pause.store(raw, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.pause_changed.notify_all();
        }
        if let Err(e) = store::save_pause(&self.conn(), state) {
            eprintln!("[clipet] {e}");
        }
        self.notify();
    }

    /// Ends timed pauses on schedule so the tray and UI update even when no
    /// key is pressed. Waits are capped because the monotonic clock behind
    /// `Condvar::wait_timeout` doesn't advance while the Mac sleeps.
    fn spawn_pause_timer(self: &Arc<Self>) {
        let service = self.clone();
        std::thread::spawn(move || {
            let mut guard = service.pause_lock.lock().unwrap();
            loop {
                let raw = service.pause.load(Ordering::SeqCst);
                let wait = if raw > 0 {
                    let remaining = raw - now_ms();
                    if remaining <= 0 {
                        drop(guard);
                        service.resume();
                        guard = service.pause_lock.lock().unwrap();
                        continue;
                    }
                    Duration::from_millis(remaining.min(30_000) as u64)
                } else {
                    Duration::from_secs(3600)
                };
                guard = service.pause_changed.wait_timeout(guard, wait).unwrap().0;
            }
        });
    }

    pub(crate) fn record_use(&self, snippet_id: &str) {
        store::bump_snippet_usage(&self.conn(), snippet_id, now_ms());
        let _ = self.app.emit(CHANGED_EVENT, ());
    }

    pub fn snippet(&self, id: &str) -> Option<Snippet> {
        store::get_snippet(&self.conn(), id)
    }

    /// The PNG of an image snippet, read and checked to decode. `None` for a
    /// text snippet.
    pub fn image_png(&self, snippet: &Snippet) -> Option<Result<Vec<u8>, String>> {
        let path = store::image_path(&self.data_dir, snippet)?;
        Some(
            std::fs::read(&path)
                .map_err(|e| format!("the snippet's image can't be read: {e}"))
                .and_then(|bytes| {
                    image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                        .map(|_| bytes)
                        .map_err(|e| format!("the snippet's image is damaged: {e}"))
                }),
        )
    }

    fn images_dir(&self) -> PathBuf {
        self.data_dir.join(store::SNIPPET_IMAGES_DIR)
    }

    /// Copies a history image into the snippet image folder under a new name,
    /// so later history cleanup or edits never touch the snippet's copy.
    fn import_history_image(&self, item_id: &str) -> Result<String, String> {
        let history = crate::db::open(&self.data_dir);
        let item = crate::db::get_item(&history, item_id)
            .filter(|i| i.kind == "image" || i.kind == "screenshot")
            .ok_or_else(|| "that image is no longer in your history".to_string())?;
        let source = self.data_dir.join("images").join(&item.content);
        let bytes = std::fs::read(&source).map_err(|e| format!("the image can't be read: {e}"))?;
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|e| format!("the image is damaged: {e}"))?;
        self.write_image(&bytes)
    }

    fn write_image(&self, bytes: &[u8]) -> Result<String, String> {
        let dir = self.images_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not save the image: {e}"))?;
        let file = format!("{}.png", new_id());
        let temp = dir.join(format!("{file}.saving"));
        std::fs::write(&temp, bytes)
            .and_then(|_| std::fs::rename(&temp, dir.join(&file)))
            .map_err(|e| {
                let _ = std::fs::remove_file(&temp);
                format!("could not save the image: {e}")
            })?;
        Ok(file)
    }

    fn remove_image(&self, snippet: &Snippet) {
        if let Some(path) = store::image_path(&self.data_dir, snippet) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Deserialize)]
pub struct SnippetInput {
    pub id: Option<String>,
    pub name: String,
    pub keyword: String,
    #[serde(default)]
    pub content: String,
    /// `"text"` (default) or `"image"`.
    #[serde(default)]
    pub content_type: Option<String>,
    /// For an image snippet: the history image to use. Omitted when editing
    /// keeps the snippet's current image.
    #[serde(default)]
    pub image_item: Option<String>,
    pub enabled: bool,
}

impl SnippetInput {
    fn is_image(&self) -> bool {
        self.content_type.as_deref() == Some(store::IMAGE)
    }
}

/// Accepts the keyword with or without the prefix the user sees (`;name` or
/// `name`), then validates and checks for a clash with another snippet.
fn normalize_input(
    conn: &rusqlite::Connection,
    input: &SnippetInput,
    prefix: char,
) -> Result<(String, String), String> {
    let keyword = input.keyword.trim();
    let keyword = keyword.strip_prefix(prefix).unwrap_or(keyword).to_string();
    engine::validate_keyword(&keyword, prefix)?;
    if input.is_image() {
        // The image itself is checked when it is imported.
    } else if input.content.is_empty() {
        return Err("Snippet content can't be empty".to_string());
    }
    if input.content.chars().count() > MAX_CONTENT_CHARS {
        return Err(format!(
            "Snippets can be at most {MAX_CONTENT_CHARS} characters"
        ));
    }
    if let Some(owner) = store::keyword_owner(conn, &keyword, input.id.as_deref()) {
        return Err(format!(
            "{prefix}{} is already used by \"{}\"",
            owner.keyword, owner.name
        ));
    }
    let name = match input.name.trim() {
        "" => keyword.clone(),
        name => name.to_string(),
    };
    Ok((name, keyword))
}

type Service<'a> = tauri::State<'a, Arc<SnippetService>>;

#[tauri::command]
pub fn snippets_list(service: Service) -> Vec<Snippet> {
    store::list_snippets(&service.conn())
}

#[tauri::command]
pub fn snippet_save(service: Service, input: SnippetInput) -> Result<Snippet, String> {
    let conn = service.conn();
    let prefix = service.live.read().unwrap().config.prefix;
    let (name, keyword) = normalize_input(&conn, &input, prefix)?;
    let existing = match &input.id {
        Some(id) => Some(
            store::get_snippet(&conn, id).ok_or_else(|| "that snippet no longer exists".to_string())?,
        ),
        None => None,
    };
    let (content_type, content, imported) = if input.is_image() {
        match (&input.image_item, &existing) {
            (Some(item), _) => {
                let file = service.import_history_image(item)?;
                (store::IMAGE, file, true)
            }
            (None, Some(old)) if old.content_type == store::IMAGE => {
                (store::IMAGE, old.content.clone(), false)
            }
            _ => return Err("Choose an image for this snippet".to_string()),
        }
    } else {
        (store::TEXT, input.content.clone(), false)
    };

    let now = now_ms();
    let saved = match &existing {
        Some(old) => store::update_snippet(
            &conn, &old.id, &name, &keyword, &content, content_type, input.enabled, now,
        )
        .map(|_| old.id.clone()),
        None => {
            let id = new_id();
            store::insert_snippet(
                &conn,
                &Snippet {
                    id: id.clone(),
                    name,
                    keyword,
                    content: content.clone(),
                    content_type: content_type.to_string(),
                    category: None,
                    enabled: input.enabled,
                    favorite: false,
                    use_count: 0,
                    created_at: now,
                    updated_at: now,
                    last_used_at: None,
                },
            )
            .map(|_| id)
        }
    };
    let id = match saved {
        Ok(id) => id,
        Err(e) => {
            if imported {
                let _ = std::fs::remove_file(service.images_dir().join(&content));
            }
            return Err(e);
        }
    };
    // The previous image is no longer referenced once replaced or turned
    // into text.
    if let Some(old) = existing.filter(|old| old.content_type == store::IMAGE && old.content != content) {
        service.remove_image(&old);
    }
    service.reload();
    store::get_snippet(&conn, &id).ok_or_else(|| "the snippet was not saved".to_string())
}

#[tauri::command]
pub fn snippet_delete(service: Service, id: String) {
    let conn = service.conn();
    let snippet = store::get_snippet(&conn, &id);
    store::delete_snippet(&conn, &id);
    if let Some(snippet) = snippet {
        service.remove_image(&snippet);
    }
    service.reload();
}

#[tauri::command]
pub fn snippet_set_enabled(service: Service, id: String, enabled: bool) {
    store::set_snippet_enabled(&service.conn(), &id, enabled, now_ms());
    service.reload();
}

/// Copies a snippet under a free keyword (`name-copy`, `name-copy2`, …),
/// disabled so it can't shadow the original until the user edits it.
#[tauri::command]
pub fn snippet_duplicate(service: Service, id: String) -> Result<Snippet, String> {
    let conn = service.conn();
    let original =
        store::get_snippet(&conn, &id).ok_or_else(|| "that snippet no longer exists".to_string())?;
    let base: String = original
        .keyword
        .chars()
        .take(engine::MAX_KEYWORD_CHARS - 7)
        .collect();
    let keyword = (1..1000)
        .map(|n| match n {
            1 => format!("{base}-copy"),
            n => format!("{base}-copy{n}"),
        })
        .find(|k| store::keyword_owner(&conn, k, None).is_none())
        .ok_or_else(|| "could not find a free shortcut for the copy".to_string())?;
    // Each image snippet owns its file, so deleting one never breaks another.
    let content = match service.image_png(&original) {
        Some(png) => service.write_image(&png?)?,
        None => original.content.clone(),
    };
    let now = now_ms();
    let copy = Snippet {
        id: new_id(),
        content,
        name: format!("{} (copy)", original.name),
        keyword,
        enabled: false,
        use_count: 0,
        created_at: now,
        updated_at: now,
        last_used_at: None,
        ..original
    };
    store::insert_snippet(&conn, &copy)?;
    service.reload();
    Ok(copy)
}

/// A small PNG preview of an image snippet, as a data URL.
#[tauri::command]
pub fn snippet_image_preview(service: Service, id: String) -> Result<String, String> {
    use base64::Engine;
    let snippet = service.snippet(&id).ok_or_else(|| "that snippet no longer exists".to_string())?;
    let png = service.image_png(&snippet).ok_or_else(|| "not an image snippet".to_string())??;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let mut preview = Vec::new();
    image
        .thumbnail(PREVIEW_PX, PREVIEW_PX)
        .write_to(&mut std::io::Cursor::new(&mut preview), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(preview)
    ))
}

#[tauri::command]
pub fn snippet_settings_get(service: Service) -> SnippetSettings {
    service.live.read().unwrap().settings.clone()
}

#[tauri::command]
pub fn snippet_settings_set(service: Service, settings: SnippetSettings) -> Result<SnippetSettings, String> {
    let prefix = engine::validate_prefix(settings.prefix.trim())?;
    let conn = service.conn();
    if let Some(clash) = store::list_snippets(&conn)
        .iter()
        .find(|s| s.keyword.contains(prefix))
    {
        return Err(format!(
            "\"{}\" uses {prefix} in its shortcut, so {prefix} can't be the prefix",
            clash.name
        ));
    }
    let settings = SnippetSettings {
        prefix: prefix.to_string(),
        default_pause_minutes: settings.default_pause_minutes.clamp(1, 24 * 60),
        ..settings
    };
    store::save_settings(&conn, &settings)?;
    service.reload();
    service.notify();
    Ok(settings)
}

#[tauri::command]
pub fn snippets_status(service: Service) -> SnippetStatus {
    service.status()
}

#[tauri::command]
pub fn snippets_pause(service: Service, minutes: Option<u32>) {
    service.pause_for(minutes);
}

#[tauri::command]
pub fn snippets_resume(service: Service) {
    service.resume();
}

pub fn service(app: &AppHandle) -> Arc<SnippetService> {
    app.state::<Arc<SnippetService>>().inner().clone()
}
