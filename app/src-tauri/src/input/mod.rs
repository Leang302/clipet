//! Mouse and keyboard sharing between paired devices (docs/CONNECT.md §3).
//!
//! - `protocol` — messages on the input stream and their framing.
//! - `keymap` — physical keys as USB HID codes; Windows/macOS tables.
//! - `geometry` — screen edges, multi-monitor bands, the virtual cursor.
//! - `session` — controlled-side state, release-all, hotkeys (pure, tested).
//! - `windows` / `macos` — capture (hooks / event tap) and injection.
//!
//! This service runs both roles: controller (forwards local input while a
//! session is active) and controlled (injects input from a device the user
//! allowed to control this computer). Keystrokes are never stored or logged.

pub mod geometry;
pub mod keymap;
pub mod protocol;
pub mod session;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

use crate::db;
use crate::sync::net::Node;
use geometry::{EdgeDetector, Screen, Side, band, point_at};
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use protocol::{Button, Os, PROTOCOL_VERSION, ToControlled, ToController, read_msg, write_msg};
use serde::{Deserialize, Serialize};
use session::{ControlledSession, HEARTBEAT_MS, Hotkey, Inject, Outcome};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

/// What a platform hook reports. Positions are in the OS's desktop
/// coordinates; deltas in device-independent pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Captured {
    /// Local pointer position (only while no session is active).
    Position { x: f64, y: f64, button_held: bool },
    Delta { dx: f64, dy: f64 },
    Button { b: Button, down: bool },
    /// Wheel units, 120 = one notch.
    Scroll { dx: f32, dy: f32 },
    Key { hid: u16, down: bool, repeat: bool },
    Hotkey(Hotkey),
}

pub type Sink = Arc<dyn Fn(Captured) + Send + Sync>;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    /// Capture and injection are allowed by the OS.
    pub granted: bool,
    /// What to do about it, in plain language (empty when fine).
    pub hint: String,
}

/// The OS side of input sharing.
pub trait Platform: Send + Sync + 'static {
    fn os(&self) -> Os;
    fn screens(&self) -> Vec<Screen>;
    /// Starts the hooks once; events go to `sink`.
    fn start(&self, sink: Sink) -> Result<(), String>;
    /// Remote: swallow local input and park/hide the pointer. Local: restore,
    /// putting the pointer at `place` if given.
    fn set_remote(&self, remote: bool, place: Option<(f64, f64)>);
    /// Whether Clipet's hotkeys and edge tracking are wanted at all.
    fn set_active(&self, active: bool);
    fn inject(&self, events: &[Inject]);
    fn permission(&self) -> Permission;
}

pub fn native_platform() -> Option<Arc<dyn Platform>> {
    #[cfg(target_os = "windows")]
    return Some(Arc::new(windows::WindowsPlatform::new()));
    #[cfg(target_os = "macos")]
    return Some(Arc::new(macos::MacPlatform::new()));
    #[allow(unreachable_code)]
    None
}

const SETTINGS_KEY: &str = "connect.input";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct InputSettings {
    /// Master switch; off by default.
    pub enabled: bool,
    pub mouse: bool,
    pub keyboard: bool,
    pub edge_switch: bool,
    pub hotkey_switch: bool,
    pub edge_delay_ms: u32,
    /// Ask on this computer before another device takes control.
    pub require_confirmation: bool,
    /// Ctrl acts as ⌘ when a PC keyboard controls a Mac (and back).
    pub swap_ctrl_cmd: bool,
    pub invert_scroll: bool,
    /// Allow sessions over a relayed (slow) connection.
    pub allow_relayed: bool,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mouse: true,
            keyboard: true,
            edge_switch: true,
            hotkey_switch: true,
            edge_delay_ms: 0,
            require_confirmation: true,
            swap_ctrl_cmd: true,
            invert_scroll: false,
            allow_relayed: false,
        }
    }
}

pub fn load_settings(conn: &rusqlite::Connection) -> InputSettings {
    db::get_setting(conn, SETTINGS_KEY)
        .and_then(|j| serde_json::from_str::<InputSettings>(&j).ok())
        .map(|s| InputSettings { edge_delay_ms: s.edge_delay_ms.min(2000), ..s })
        .unwrap_or_default()
}

pub fn save_settings(conn: &rusqlite::Connection, s: &InputSettings) {
    if let Ok(j) = serde_json::to_string(s) {
        db::set_setting(conn, SETTINGS_KEY, &j);
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum InputMode {
    Idle,
    /// Waiting for the other device to accept (it may be asking its user).
    Connecting { peer: String },
    Controlling { peer: String },
    ControlledBy { peer: String },
}

#[derive(Serialize, Clone, Debug)]
pub struct InputStatus {
    pub settings: InputSettings,
    pub permission: Permission,
    pub supported: bool,
    pub mode: InputMode,
    /// A device asking to control this computer, waiting for an answer.
    pub request: Option<String>,
    pub last_error: Option<String>,
}

enum Mode {
    Idle,
    Connecting { peer: EndpointId },
    Controlling { peer: EndpointId, side: Side, entry: f64, tx: mpsc::UnboundedSender<Captured>, stop: Arc<Notify> },
    Controlled { peer: EndpointId, stop: Arc<Notify> },
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

const CLOSE_OK: u32 = 0;
/// How long the display list is reused before asking the OS again.
const SCREENS_TTL: Duration = Duration::from_secs(2);

/// What the pointer path reads on every local mouse move, kept in memory.
struct Cache {
    settings: InputSettings,
    arrangement: Vec<(EndpointId, Side)>,
    screens: Vec<Screen>,
    screens_at: Option<std::time::Instant>,
}

pub struct InputService {
    platform: Option<Arc<dyn Platform>>,
    data_dir: PathBuf,
    node: RwLock<Option<Arc<Node>>>,
    mode: Mutex<Mode>,
    detectors: Mutex<HashMap<Side, EdgeDetector>>,
    request: Mutex<Option<(String, oneshot::Sender<bool>)>>,
    last_error: Mutex<Option<String>>,
    connections: Mutex<HashMap<EndpointId, Connection>>,
    observer: Arc<dyn Fn() + Send + Sync>,
    events: mpsc::UnboundedSender<Captured>,
    cache: RwLock<Cache>,
}

impl InputService {
    pub fn new(platform: Option<Arc<dyn Platform>>, data_dir: PathBuf, observer: Arc<dyn Fn() + Send + Sync>) -> Arc<Self> {
        let (events, rx) = mpsc::unbounded_channel();
        let service = Arc::new(Self {
            platform,
            data_dir,
            node: RwLock::new(None),
            mode: Mutex::new(Mode::Idle),
            detectors: Mutex::new(HashMap::new()),
            request: Mutex::new(None),
            last_error: Mutex::new(None),
            connections: Mutex::new(HashMap::new()),
            observer,
            events,
            cache: RwLock::new(Cache { settings: InputSettings::default(), arrangement: Vec::new(), screens: Vec::new(), screens_at: None }),
        });
        service.refresh();
        let dispatcher = service.clone();
        // Created during app setup, before any async context exists.
        tauri::async_runtime::spawn(async move { dispatcher.dispatch(rx).await });
        service
    }

    fn changed(&self) {
        (self.observer)();
    }

    fn settings(&self) -> InputSettings {
        self.cache.read().unwrap().settings.clone()
    }

    /// Re-reads settings and the screen arrangement (after they change).
    pub fn refresh(&self) {
        let conn = db::open(&self.data_dir);
        let settings = load_settings(&conn);
        let arrangement = crate::sync::store::list_peers(&conn)
            .into_iter()
            .filter_map(|p| Some((p.id.parse().ok()?, p.side?)))
            .collect();
        let mut cache = self.cache.write().unwrap();
        cache.settings = settings;
        cache.arrangement = arrangement;
        cache.screens_at = None;
    }

    fn screens(&self) -> Vec<Screen> {
        let Some(platform) = &self.platform else { return Vec::new() };
        {
            let cache = self.cache.read().unwrap();
            if cache.screens_at.is_some_and(|at| at.elapsed() < SCREENS_TTL) {
                return cache.screens.clone();
            }
        }
        let screens = platform.screens();
        let mut cache = self.cache.write().unwrap();
        cache.screens = screens.clone();
        cache.screens_at = Some(std::time::Instant::now());
        screens
    }

    fn set_error(&self, error: Option<String>) {
        *self.last_error.lock().unwrap() = error;
        self.changed();
    }

    /// Called when the device connection layer starts or stops.
    pub fn attach(self: &Arc<Self>, node: Option<Arc<Node>>) {
        if let Some(node) = &node {
            let service = self.clone();
            node.set_input_handler(Arc::new(move |peer, conn| {
                let service = service.clone();
                tokio::spawn(async move { service.serve_connection(peer, conn).await });
            }));
        } else {
            self.stop();
            self.connections.lock().unwrap().clear();
        }
        *self.node.write().unwrap() = node;
        self.apply_settings();
    }

    /// Starts or quiets the hooks to match the settings.
    pub fn apply_settings(self: &Arc<Self>) {
        self.refresh();
        let Some(platform) = &self.platform else { return };
        let settings = self.settings();
        let active = settings.enabled && self.node.read().unwrap().is_some();
        if active {
            let events = self.events.clone();
            if let Err(e) = platform.start(Arc::new(move |c| {
                let _ = events.send(c);
            })) {
                self.set_error(Some(e));
            }
        } else {
            self.stop();
        }
        platform.set_active(active);
        self.changed();
    }

    pub fn status(&self) -> InputStatus {
        let peer_name = |id: &EndpointId| {
            crate::sync::store::list_peers(&db::open(&self.data_dir))
                .into_iter()
                .find(|p| p.id == id.to_string())
                .map(|p| p.name)
                .unwrap_or_else(|| id.fmt_short().to_string())
        };
        let mode = match &*self.mode.lock().unwrap() {
            Mode::Idle => InputMode::Idle,
            Mode::Connecting { peer } => InputMode::Connecting { peer: peer_name(peer) },
            Mode::Controlling { peer, .. } => InputMode::Controlling { peer: peer_name(peer) },
            Mode::Controlled { peer, .. } => InputMode::ControlledBy { peer: peer_name(peer) },
        };
        InputStatus {
            settings: self.settings(),
            permission: self.platform.as_ref().map(|p| p.permission()).unwrap_or(Permission {
                granted: false,
                hint: "Input sharing isn't available on this system.".to_string(),
            }),
            supported: self.platform.is_some(),
            mode,
            request: self.request.lock().unwrap().as_ref().map(|(name, _)| name.clone()),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }

    /// Stop Sharing / emergency: ends whatever session is active, either role.
    pub fn stop(&self) {
        let mode = std::mem::replace(&mut *self.mode.lock().unwrap(), Mode::Idle);
        match mode {
            Mode::Controlling { stop, .. } | Mode::Controlled { stop, .. } => stop.notify_one(),
            Mode::Connecting { .. } | Mode::Idle => {}
        }
        if let Some((_, answer)) = self.request.lock().unwrap().take() {
            let _ = answer.send(false);
        }
        if let Some(p) = &self.platform {
            p.set_remote(false, None);
        }
        self.changed();
    }

    pub fn answer_request(&self, approve: bool) -> Result<(), String> {
        let (_, answer) = self.request.lock().unwrap().take().ok_or("No device is asking right now.")?;
        let _ = answer.send(approve);
        self.changed();
        Ok(())
    }

    // -------------------------------------------------------------- controller

    async fn dispatch(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Captured>) {
        while let Some(event) = rx.recv().await {
            let forward = {
                let mode = self.mode.lock().unwrap();
                match &*mode {
                    Mode::Controlling { tx, side, .. } => Some((tx.clone(), *side)),
                    _ => None,
                }
            };
            match (forward, event) {
                (Some(_), Captured::Hotkey(Hotkey::Emergency)) => self.leave(),
                (Some((_, side)), Captured::Hotkey(Hotkey::Switch(s))) if s == side.opposite() => self.leave(),
                (Some((tx, _)), event @ (Captured::Delta { .. } | Captured::Button { .. } | Captured::Scroll { .. } | Captured::Key { .. })) => {
                    let _ = tx.send(event);
                }
                (Some(_), _) => {}
                (None, Captured::Hotkey(Hotkey::Emergency)) => self.stop(),
                (None, Captured::Hotkey(Hotkey::Switch(side))) => {
                    if self.settings().hotkey_switch {
                        if let Some(peer) = self.peer_at(side) {
                            self.clone().spawn_enter(peer, side, 0.5);
                        }
                    }
                }
                (None, Captured::Position { x, y, button_held }) => self.on_position(x, y, button_held),
                (None, _) => {}
            }
        }
    }

    /// Paired devices placed next to this one, with input sharing allowed
    /// by the arrangement.
    fn arrangement(&self) -> Vec<(EndpointId, Side)> {
        self.cache.read().unwrap().arrangement.clone()
    }

    fn peer_at(&self, side: Side) -> Option<EndpointId> {
        self.arrangement().into_iter().find(|(_, s)| *s == side).map(|(id, _)| id)
    }

    fn on_position(self: &Arc<Self>, x: f64, y: f64, button_held: bool) {
        if !matches!(*self.mode.lock().unwrap(), Mode::Idle) {
            return;
        }
        let settings = self.settings();
        if !settings.edge_switch || !settings.mouse {
            return;
        }
        if self.platform.is_none() {
            return;
        }
        let screens = self.screens();
        let now = now_ms();
        for (peer, side) in self.arrangement() {
            let fired = {
                let mut detectors = self.detectors.lock().unwrap();
                let detector = detectors.entry(side).or_insert_with(|| EdgeDetector::new(side, settings.edge_delay_ms.into()));
                detector.delay_ms = settings.edge_delay_ms.into();
                detector.on_position(&band(&screens, side), x, y, now, button_held)
            };
            if let Some(fraction) = fired {
                self.clone().spawn_enter(peer, side, fraction);
                return;
            }
        }
    }

    fn cooldown(&self) {
        let now = now_ms();
        for d in self.detectors.lock().unwrap().values_mut() {
            d.cooldown(now);
        }
    }

    /// Manual switch from the UI.
    pub fn switch_to(self: &Arc<Self>, peer: &str) -> Result<(), String> {
        let id: EndpointId = peer.parse().map_err(|_| "unknown device".to_string())?;
        let side = self.arrangement().into_iter().find(|(p, _)| *p == id).map(|(_, s)| s).unwrap_or(Side::Right);
        self.clone().spawn_enter(id, side, 0.5);
        Ok(())
    }

    fn spawn_enter(self: Arc<Self>, peer: EndpointId, side: Side, fraction: f64) {
        {
            let mut mode = self.mode.lock().unwrap();
            if !matches!(*mode, Mode::Idle) {
                return;
            }
            *mode = Mode::Connecting { peer };
        }
        self.changed();
        tokio::spawn(async move {
            if let Err(e) = self.clone().enter(peer, side, fraction).await {
                let mut mode = self.mode.lock().unwrap();
                if matches!(*mode, Mode::Connecting { .. }) {
                    *mode = Mode::Idle;
                }
                drop(mode);
                self.cooldown();
                self.set_error(Some(e));
            }
        });
    }

    async fn input_connection(&self, peer: EndpointId) -> Result<Connection, String> {
        if let Some(conn) = self.connections.lock().unwrap().get(&peer).filter(|c| c.close_reason().is_none()).cloned() {
            return Ok(conn);
        }
        let node = self.node.read().unwrap().clone().ok_or("Clipet Connect is off.")?;
        let conn = node.connect_input(peer).await?;
        self.connections.lock().unwrap().insert(peer, conn.clone());
        Ok(conn)
    }

    async fn enter(self: Arc<Self>, peer: EndpointId, side: Side, fraction: f64) -> Result<(), String> {
        let settings = self.settings();
        if !settings.enabled {
            return Err("Input sharing is off.".to_string());
        }
        let platform = self.platform.clone().ok_or("Input sharing isn't available on this system.")?;
        let conn = self.input_connection(peer).await?;
        if !settings.allow_relayed && !wait_for_direct(&conn).await {
            return Err("Only a relayed (internet) connection is available, and input sharing over a relay is off.".to_string());
        }
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("Couldn't start sharing ({e})."))?;
        let session = u64::from_le_bytes(crate::sync::pairing::new_secret()?[..8].try_into().unwrap());
        let enter = ToControlled::Enter { v: PROTOCOL_VERSION, session, side, fraction, os: platform.os() };
        write_msg(&mut send, &enter).await.map_err(|e| e.to_string())?;
        // The other side may be asking its user; give them time.
        let reply = tokio::time::timeout(Duration::from_secs(40), read_msg::<_, ToController>(&mut recv))
            .await
            .map_err(|_| "The other device didn't answer.".to_string())?
            .map_err(|e| e.to_string())?;
        let peer_os = match reply {
            Some(ToController::Accepted { os }) => os,
            Some(ToController::Denied { reason }) => return Err(reason),
            _ => return Err("The other device closed the connection.".to_string()),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(Notify::new());
        {
            let mut mode = self.mode.lock().unwrap();
            if !matches!(*mode, Mode::Connecting { peer: p } if p == peer) {
                // Cancelled while waiting.
                let _ = send.finish();
                return Ok(());
            }
            *mode = Mode::Controlling { peer, side, entry: fraction, tx, stop: stop.clone() };
        }
        platform.set_remote(true, None);
        self.set_error(None);
        let service = self.clone();
        tokio::spawn(async move {
            let returned = service.clone().run_controller(send, recv, rx, stop, peer_os, settings).await;
            service.finish_controlling(peer, returned);
        });
        Ok(())
    }

    /// Streams local input until the session ends. Returns the edge fraction
    /// to put the local pointer back at, when the other side reported one.
    async fn run_controller(
        self: Arc<Self>,
        mut send: SendStream,
        mut recv: RecvStream,
        mut rx: mpsc::UnboundedReceiver<Captured>,
        stop: Arc<Notify>,
        peer_os: Os,
        settings: InputSettings,
    ) -> Option<f64> {
        let my_os = self.platform.as_ref().map_or(Os::Other, |p| p.os());
        let mut seq = 0u64;
        let mut next = || {
            seq += 1;
            seq
        };
        let (returned_tx, mut returned_rx) = oneshot::channel();
        let reader = tokio::spawn(async move {
            loop {
                match read_msg::<_, ToController>(&mut recv).await {
                    Ok(Some(ToController::Return { fraction })) => {
                        let _ = returned_tx.send(Some(fraction));
                        return;
                    }
                    Ok(Some(ToController::Ended { .. })) | Ok(None) | Err(_) => {
                        let _ = returned_tx.send(None);
                        return;
                    }
                    Ok(Some(_)) => continue,
                    #[allow(unreachable_patterns)]
                    _ => {
                        let _ = returned_tx.send(None);
                        return;
                    }
                }
            }
        });
        let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_MS));
        let result = loop {
            let event = tokio::select! {
                e = rx.recv() => match e { Some(e) => e, None => break None },
                _ = heartbeat.tick() => {
                    if write_msg(&mut send, &ToControlled::Heartbeat { s: next() }).await.is_err() { break None; }
                    continue;
                }
                r = &mut returned_rx => break r.unwrap_or(None),
                _ = stop.notified() => {
                    let _ = write_msg(&mut send, &ToControlled::Leave { s: next() }).await;
                    break None;
                }
            };
            // Coalesce pointer motion that queued up behind a slow write.
            let mut pending = vec![event];
            while let Ok(more) = rx.try_recv() {
                pending.push(more);
            }
            let mut msgs = Vec::new();
            let mut acc: Option<(f64, f64)> = None;
            for e in pending {
                match e {
                    Captured::Delta { dx, dy } if settings.mouse => {
                        let a = acc.get_or_insert((0.0, 0.0));
                        a.0 += dx;
                        a.1 += dy;
                    }
                    other => {
                        if let Some((dx, dy)) = acc.take() {
                            msgs.push(ToControlled::Move { s: next(), dx: dx as f32, dy: dy as f32 });
                        }
                        match other {
                            Captured::Button { b, down } if settings.mouse => msgs.push(ToControlled::Button { s: next(), b, down }),
                            Captured::Scroll { dx, dy } if settings.mouse => msgs.push(ToControlled::Scroll { s: next(), dx, dy }),
                            Captured::Key { hid, down, repeat } if settings.keyboard => {
                                let k = keymap::map_modifier(hid, my_os, peer_os, settings.swap_ctrl_cmd);
                                msgs.push(ToControlled::Key { s: next(), k, down, rep: repeat });
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some((dx, dy)) = acc {
                msgs.push(ToControlled::Move { s: next(), dx: dx as f32, dy: dy as f32 });
            }
            let mut failed = false;
            for m in &msgs {
                if write_msg(&mut send, m).await.is_err() {
                    failed = true;
                    break;
                }
            }
            if failed {
                break None;
            }
        };
        let _ = send.finish();
        reader.abort();
        result
    }

    /// Takes local control back, putting the pointer at the shared edge.
    fn leave(&self) {
        let stop = match &*self.mode.lock().unwrap() {
            Mode::Controlling { stop, .. } => Some(stop.clone()),
            _ => None,
        };
        if let Some(stop) = stop {
            stop.notify_one();
        }
    }

    fn finish_controlling(&self, peer: EndpointId, returned: Option<f64>) {
        let (side, entry) = {
            let mut mode = self.mode.lock().unwrap();
            match &*mode {
                Mode::Controlling { peer: p, side, entry, .. } if *p == peer => {
                    let found = (*side, *entry);
                    *mode = Mode::Idle;
                    found
                }
                // Already stopped (Stop Sharing): just make sure input is local.
                _ => {
                    drop(mode);
                    if let Some(p) = &self.platform {
                        p.set_remote(false, None);
                    }
                    self.cooldown();
                    self.changed();
                    return;
                }
            }
        };
        if let Some(platform) = &self.platform {
            let screens = platform.screens();
            let place = point_at(&band(&screens, side), side, returned.unwrap_or(entry));
            platform.set_remote(false, place);
        }
        self.cache.write().unwrap().screens_at = None;
        self.cooldown();
        self.changed();
    }

    // -------------------------------------------------------------- controlled

    async fn serve_connection(self: Arc<Self>, peer: EndpointId, conn: Connection) {
        loop {
            let Ok((send, recv)) = conn.accept_bi().await else { return };
            self.clone().serve_session(peer, send, recv).await;
        }
    }

    async fn serve_session(self: Arc<Self>, peer: EndpointId, mut send: SendStream, mut recv: RecvStream) {
        let first = tokio::time::timeout(Duration::from_secs(5), read_msg::<_, ToControlled>(&mut recv)).await;
        let Ok(Ok(Some(ToControlled::Enter { v, side, fraction, .. }))) = first else { return };
        match self.clone().admit(peer, v).await {
            Ok(()) => {}
            Err(reason) => {
                let _ = write_msg(&mut send, &ToController::Denied { reason }).await;
                let _ = send.finish();
                return;
            }
        }
        let Some(platform) = self.platform.clone() else { return };
        let settings = self.settings();
        let Some((mut session, first)) = ControlledSession::new(platform.screens(), side, fraction, now_ms(), settings.invert_scroll) else {
            let _ = write_msg(&mut send, &ToController::Denied { reason: "No display found.".into() }).await;
            return;
        };
        let stop = Arc::new(Notify::new());
        let cancelled = {
            let mut mode = self.mode.lock().unwrap();
            if matches!(*mode, Mode::Connecting { peer: p } if p == peer) {
                *mode = Mode::Controlled { peer, stop: stop.clone() };
                false
            } else {
                true
            }
        };
        if cancelled {
            let _ = write_msg(&mut send, &ToController::Denied { reason: "Cancelled.".into() }).await;
            return;
        }
        if write_msg(&mut send, &ToController::Accepted { os: platform.os() }).await.is_err() {
            self.end_controlled(peer);
            return;
        }
        self.changed();
        platform.inject(&[first]);
        let mut watchdog = tokio::time::interval(Duration::from_millis(HEARTBEAT_MS));
        loop {
            tokio::select! {
                msg = read_msg::<_, ToControlled>(&mut recv) => match msg {
                    Ok(Some(msg)) => match session.handle(&msg, now_ms()) {
                        Outcome::Inject(events) => platform.inject(&events),
                        Outcome::Return { fraction, release } => {
                            platform.inject(&release);
                            let _ = write_msg(&mut send, &ToController::Return { fraction }).await;
                            break;
                        }
                        Outcome::Leave(release) => {
                            platform.inject(&release);
                            break;
                        }
                        Outcome::Ignored => {}
                    },
                    _ => {
                        platform.inject(&session.release_all());
                        break;
                    }
                },
                _ = watchdog.tick() => {
                    if session.timed_out(now_ms()) {
                        platform.inject(&session.release_all());
                        break;
                    }
                }
                _ = stop.notified() => {
                    platform.inject(&session.release_all());
                    let _ = write_msg(&mut send, &ToController::Ended { reason: "Stopped on the other computer.".into() }).await;
                    break;
                }
            }
        }
        let _ = send.finish();
        self.end_controlled(peer);
    }

    /// Decides whether `peer` may control this computer now.
    async fn admit(self: Arc<Self>, peer: EndpointId, version: u32) -> Result<(), String> {
        if version != PROTOCOL_VERSION {
            return Err("The devices run incompatible Clipet versions.".to_string());
        }
        let settings = self.settings();
        let name = {
            let conn = db::open(&self.data_dir);
            let row = crate::sync::store::list_peers(&conn).into_iter().find(|p| p.id == peer.to_string());
            match row {
                Some(p) if p.allow_control => p.name,
                Some(_) => {
                    let me = crate::sync::store::load_device_name(&conn).unwrap_or_else(crate::sync::identity::default_device_name);
                    return Err(format!(
                        "{me} doesn't allow control from this device yet. On {me}, turn on \"Can control this computer\" for this device."
                    ));
                }
                None => return Err("This device isn't paired.".to_string()),
            }
        };
        if !settings.enabled {
            return Err("Input sharing is off on that computer.".to_string());
        }
        if self.platform.as_ref().is_none_or(|p| !p.permission().granted) {
            return Err("That computer needs a permission before it can be controlled.".to_string());
        }
        {
            let mut mode = self.mode.lock().unwrap();
            if !matches!(*mode, Mode::Idle) {
                return Err("That computer is already sharing input.".to_string());
            }
            *mode = Mode::Connecting { peer };
        }
        if settings.require_confirmation {
            let (tx, rx) = oneshot::channel();
            *self.request.lock().unwrap() = Some((name, tx));
            self.changed();
            let approved = tokio::time::timeout(Duration::from_secs(30), rx).await.ok().and_then(|r| r.ok()).unwrap_or(false);
            self.request.lock().unwrap().take();
            if !approved {
                let mut mode = self.mode.lock().unwrap();
                if matches!(*mode, Mode::Connecting { peer: p } if p == peer) {
                    *mode = Mode::Idle;
                }
                drop(mode);
                self.changed();
                return Err("Control wasn't approved on that computer.".to_string());
            }
        }
        Ok(())
    }

    fn end_controlled(&self, peer: EndpointId) {
        let mut mode = self.mode.lock().unwrap();
        if matches!(*mode, Mode::Controlled { peer: p, .. } | Mode::Connecting { peer: p } if p == peer) {
            *mode = Mode::Idle;
        }
        drop(mode);
        self.changed();
    }

    /// A device lost trust: end anything involving it.
    pub fn forget(&self, peer: &str) {
        let Ok(id) = peer.parse::<EndpointId>() else { return };
        let involved = matches!(&*self.mode.lock().unwrap(),
            Mode::Controlling { peer, .. } | Mode::Controlled { peer, .. } | Mode::Connecting { peer } if *peer == id);
        if involved {
            self.stop();
        }
        if let Some(conn) = self.connections.lock().unwrap().remove(&id) {
            conn.close(VarInt::from_u32(CLOSE_OK), b"removed");
        }
    }
}

/// Whether the connection's active path is direct (not through a relay),
/// waiting briefly for hole punching to finish.
async fn wait_for_direct(conn: &Connection) -> bool {
    for _ in 0..30 {
        if conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
