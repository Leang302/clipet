//! Device Sync: pairs trusted devices and keeps their clipboard history in
//! step over iroh (peer-to-peer QUIC, relay fallback). Design, conflict
//! policy and third-party services: docs/SYNC.md.
//!
//! - `store` — sync tables and settings in `clipflow.sqlite3`.
//! - `journal` — the hook capture and history commands call after a change.
//! - `engine` — what is sent, how received changes merge (pure, tested).
//! - `wire` — framing over any byte stream.
//! - `pairing` — invitations and verification codes.
//! - `identity` — the device key (OS credential store) and name.
//! - `net` — the iroh endpoint, connections, sessions and pairing flows.

pub mod discovery;
pub mod engine;
pub mod identity;
pub mod journal;
pub mod net;
pub mod pairing;
pub mod store;
pub mod wire;
#[cfg(test)]
pub(crate) mod tests;

use identity::KeyStorage;
use net::{Change, Network, Node, NodeStatus};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::SyncSettings;
use tauri::{AppHandle, Emitter, Manager};

pub const STATUS_EVENT: &str = "clipet://sync-changed";
pub const INPUT_EVENT: &str = "clipet://connect-input";
/// The small always-on-top banner shown while input sharing is active or a
/// device is asking for control (see tauri.conf.json).
pub const BANNER_WINDOW: &str = "connect";

#[derive(Serialize, Clone, Debug)]
pub struct SyncStatus {
    pub settings: SyncSettings,
    pub device_name: String,
    pub key_storage: Option<KeyStorage>,
    /// The network side is up.
    pub running: bool,
    /// Why sync couldn't start, in plain language.
    pub error: Option<String>,
    pub node: Option<NodeStatus>,
    pub input: crate::input::InputStatus,
}

pub struct SyncService {
    app: AppHandle,
    data_dir: PathBuf,
    node: tokio::sync::Mutex<Option<Arc<Node>>>,
    key_storage: Mutex<Option<KeyStorage>>,
    error: Mutex<Option<String>>,
    items_pending: Arc<AtomicBool>,
    pub input: Arc<crate::input::InputService>,
}

impl SyncService {
    pub fn new(app: AppHandle, data_dir: PathBuf) -> Arc<Self> {
        let observer_app = app.clone();
        let input = crate::input::InputService::new(
            crate::input::native_platform(),
            data_dir.clone(),
            Arc::new(move || input_changed(&observer_app)),
        );
        Arc::new(Self {
            app,
            data_dir,
            node: tokio::sync::Mutex::new(None),
            key_storage: Mutex::new(None),
            error: Mutex::new(None),
            items_pending: Arc::new(AtomicBool::new(false)),
            input,
        })
    }

    fn conn(&self) -> rusqlite::Connection {
        crate::db::open(&self.data_dir)
    }

    fn settings(&self) -> SyncSettings {
        store::load_settings(&self.conn())
    }

    fn device_name(&self) -> String {
        store::load_device_name(&self.conn()).unwrap_or_else(identity::default_device_name)
    }

    /// Called once at startup.
    pub fn start(self: &Arc<Self>) {
        let settings = self.settings();
        if !settings.enabled {
            return;
        }
        journal::set_enabled(settings.clipboard);
        store::prune(&self.conn(), chrono::Utc::now().timestamp_millis());
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            service.start_node().await;
        });
    }

    fn observer(&self) -> net::Observer {
        let app = self.app.clone();
        let pending = self.items_pending.clone();
        Arc::new(move |change| match change {
            Change::Status => {
                let _ = app.emit(STATUS_EVENT, ());
            }
            Change::Items => {
                // Coalesce bursts (initial sync) into one refresh.
                if !pending.swap(true, Ordering::SeqCst) {
                    let app = app.clone();
                    let pending = pending.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        pending.store(false, Ordering::SeqCst);
                        let _ = app.emit("clipflow://new-item", ());
                        crate::ocr::wake(&app);
                    });
                }
            }
        })
    }

    async fn start_node(self: &Arc<Self>) {
        let mut slot = self.node.lock().await;
        if slot.is_some() {
            return;
        }
        let data_dir = self.data_dir.clone();
        let identity = tauri::async_runtime::spawn_blocking(move || identity::load_or_create(&data_dir))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        let (key, storage) = match identity {
            Ok(found) => found,
            Err(e) => {
                *self.error.lock().unwrap() = Some(format!("The device key couldn't be loaded: {e}"));
                let _ = self.app.emit(STATUS_EVENT, ());
                return;
            }
        };
        *self.key_storage.lock().unwrap() = Some(storage);
        let settings = self.settings();
        match Node::start(self.data_dir.clone(), key, Network::Internet, self.device_name(), settings.paused, self.observer()).await {
            Ok(node) => {
                self.input.attach(Some(node.clone()));
                *slot = Some(node);
                *self.error.lock().unwrap() = None;
            }
            Err(e) => *self.error.lock().unwrap() = Some(e),
        }
        drop(slot);
        let _ = self.app.emit(STATUS_EVENT, ());
    }

    async fn stop_node(&self) {
        self.input.attach(None);
        if let Some(node) = self.node.lock().await.take() {
            node.shutdown().await;
        }
    }

    async fn node(&self) -> Result<Arc<Node>, String> {
        self.node
            .lock()
            .await
            .clone()
            .ok_or_else(|| self.error.lock().unwrap().clone().unwrap_or_else(|| "Sync is off.".to_string()))
    }

    pub async fn status(&self) -> SyncStatus {
        let node = self.node.lock().await.clone();
        let node_status = match node {
            Some(node) => tokio::task::spawn_blocking(move || node.status()).await.ok(),
            None => None,
        };
        SyncStatus {
            settings: self.settings(),
            device_name: self.device_name(),
            key_storage: *self.key_storage.lock().unwrap(),
            running: node_status.is_some(),
            error: self.error.lock().unwrap().clone(),
            node: node_status,
            input: self.input.status(),
        }
    }

    async fn set_enabled(self: &Arc<Self>, enabled: bool) {
        let conn = self.conn();
        let settings = SyncSettings { enabled, ..store::load_settings(&conn) };
        store::save_settings(&conn, &settings);
        journal::set_enabled(enabled && settings.clipboard);
        if enabled {
            // Existing history is offered to devices paired from now on.
            store::backfill(&conn);
            store::prune(&conn, chrono::Utc::now().timestamp_millis());
            drop(conn);
            self.start_node().await;
        } else {
            drop(conn);
            self.stop_node().await;
            *self.error.lock().unwrap() = None;
        }
    }
}

type Service<'a> = tauri::State<'a, Arc<SyncService>>;

#[tauri::command]
pub async fn sync_status(service: Service<'_>) -> Result<SyncStatus, String> {
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_set_enabled(service: Service<'_>, enabled: bool) -> Result<SyncStatus, String> {
    service.inner().set_enabled(enabled).await;
    Ok(service.status().await)
}

/// Saves the content options; on/off and pause have their own commands.
#[tauri::command]
pub async fn sync_save_settings(service: Service<'_>, settings: SyncSettings) -> Result<SyncStatus, String> {
    let conn = service.conn();
    let current = store::load_settings(&conn);
    let settings = SyncSettings { enabled: current.enabled, paused: current.paused, ..settings }.clamped();
    store::save_settings(&conn, &settings);
    if settings.clipboard && !current.clipboard {
        // History made while clipboard sync was off becomes available now.
        store::backfill(&conn);
    }
    drop(conn);
    journal::set_enabled(settings.enabled && settings.clipboard);
    if settings.clipboard != current.clipboard {
        if let Ok(node) = service.node().await {
            node.restart_sessions();
        }
    }
    // Filters apply at send time; wake sessions so they re-read them.
    journal::notify();
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_set_paused(service: Service<'_>, paused: bool) -> Result<SyncStatus, String> {
    let conn = service.conn();
    let settings = SyncSettings { paused, ..store::load_settings(&conn) };
    store::save_settings(&conn, &settings);
    drop(conn);
    if let Ok(node) = service.node().await {
        node.set_paused(paused);
    }
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_rename_this_device(service: Service<'_>, name: String) -> Result<SyncStatus, String> {
    let name = identity::clean_name(&name);
    if name.is_empty() {
        return Err("The name can't be empty.".to_string());
    }
    store::save_device_name(&service.conn(), &name);
    if let Ok(node) = service.node().await {
        node.set_device_name(name);
    }
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_create_invite(service: Service<'_>) -> Result<SyncStatus, String> {
    service.node().await?.create_invite().await?;
    Ok(service.status().await)
}

/// Copies the open invitation without recording it in history (an
/// invitation is a one-time credential).
#[tauri::command]
pub async fn sync_copy_invite(
    service: Service<'_>,
    monitor: tauri::State<'_, Arc<crate::clipboard::MonitorState>>,
) -> Result<(), String> {
    let node = service.node().await?;
    let status = tokio::task::spawn_blocking(move || node.status()).await.map_err(|e| e.to_string())?;
    let net::PairingView::Hosting { invite, .. } = status.pairing else {
        return Err("There is no open invitation.".to_string());
    };
    *monitor.last_self_write_hash.lock().unwrap() = Some(crate::clipboard::hash_bytes(invite.as_bytes()));
    arboard::Clipboard::new().and_then(|mut c| c.set_text(invite)).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn sync_join(service: Service<'_>, invite: String) -> Result<SyncStatus, String> {
    service.node().await?.join(&invite)?;
    Ok(service.status().await)
}

/// Pairs with a nearby device by typing the code it shows.
#[tauri::command]
pub async fn sync_join_nearby(service: Service<'_>, id: String, code: String) -> Result<SyncStatus, String> {
    service.node().await?.join_nearby(&id, &code)?;
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_decide_pairing(service: Service<'_>, approve: bool) -> Result<SyncStatus, String> {
    service.node().await?.decide_pairing(approve)?;
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_cancel_pairing(service: Service<'_>) -> Result<SyncStatus, String> {
    if let Ok(node) = service.node().await {
        node.cancel_pairing();
    }
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_remove_device(service: Service<'_>, id: String) -> Result<SyncStatus, String> {
    service.input.forget(&id);
    match service.node().await {
        Ok(node) => node.remove_peer(&id)?,
        // Sync off: just forget the device.
        Err(_) => store::remove_peer(&service.conn(), &id),
    }
    service.input.refresh();
    Ok(service.status().await)
}

#[tauri::command]
pub async fn sync_rename_device(service: Service<'_>, id: String, name: String) -> Result<SyncStatus, String> {
    let name = identity::clean_name(&name);
    if name.is_empty() {
        return Err("The name can't be empty.".to_string());
    }
    store::rename_peer(&service.conn(), &id, &name);
    Ok(service.status().await)
}

/// Per-device options: clipboard sync, control of this computer, and where
/// its screen sits.
#[tauri::command]
pub async fn connect_set_device(
    service: Service<'_>,
    id: String,
    allow_clipboard: Option<bool>,
    allow_control: Option<bool>,
    side: Option<Option<crate::input::geometry::Side>>,
) -> Result<SyncStatus, String> {
    let conn = service.conn();
    if store::list_peers(&conn).iter().all(|p| p.id != id) {
        return Err("unknown device".to_string());
    }
    if let Some(allow) = allow_clipboard {
        store::set_allow_clipboard(&conn, &id, allow);
    }
    if let Some(allow) = allow_control {
        store::set_allow_control(&conn, &id, allow);
        if !allow {
            // Revoking control ends a session that device is running now.
            service.input.forget(&id);
        }
    }
    if let Some(side) = side {
        store::set_side(&conn, &id, side);
    }
    drop(conn);
    service.input.refresh();
    if allow_clipboard.is_some() {
        if let Ok(node) = service.node().await {
            node.restart_sessions();
        }
    }
    Ok(service.status().await)
}

#[tauri::command]
pub async fn input_save_settings(service: Service<'_>, settings: crate::input::InputSettings) -> Result<SyncStatus, String> {
    let settings = crate::input::InputSettings { edge_delay_ms: settings.edge_delay_ms.min(2000), ..settings };
    crate::input::save_settings(&service.conn(), &settings);
    if !settings.enabled {
        service.input.stop();
    }
    service.input.apply_settings();
    Ok(service.status().await)
}

#[tauri::command]
pub async fn input_switch_to(service: Service<'_>, id: String) -> Result<SyncStatus, String> {
    service.input.switch_to(&id)?;
    Ok(service.status().await)
}

/// Stop Sharing: ends the active session, whichever side this is.
#[tauri::command]
pub async fn input_stop(service: Service<'_>) -> Result<SyncStatus, String> {
    service.input.stop();
    Ok(service.status().await)
}

#[tauri::command]
pub async fn input_answer(service: Service<'_>, approve: bool) -> Result<SyncStatus, String> {
    service.input.answer_request(approve)?;
    Ok(service.status().await)
}

#[tauri::command]
pub fn input_status(service: Service<'_>) -> crate::input::InputStatus {
    service.input.status()
}

/// Shows the banner while something about input sharing must be visible:
/// a device asking for control, or a session in either direction.
fn input_changed(app: &AppHandle) {
    let _ = app.emit(INPUT_EVENT, ());
    let Some(service) = app.try_state::<Arc<SyncService>>() else { return };
    let status = service.input.status();
    let visible = status.request.is_some() || !matches!(status.mode, crate::input::InputMode::Idle);
    if let Some(win) = app.get_webview_window(BANNER_WINDOW) {
        if visible {
            if let Ok(Some(monitor)) = win.primary_monitor() {
                let size = monitor.size();
                let scale = monitor.scale_factor();
                let width = (420.0 * scale) as i32;
                let x = monitor.position().x + (size.width as i32 - width) / 2;
                let y = monitor.position().y + (12.0 * scale) as i32;
                let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
            }
            let _ = win.show();
        } else {
            let _ = win.hide();
        }
    }
}

#[tauri::command]
pub async fn sync_now(service: Service<'_>) -> Result<SyncStatus, String> {
    service.node().await?.sync_now().await;
    Ok(service.status().await)
}

/// A fresh, unique directory for a test's database and images.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("clipet-sync-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
