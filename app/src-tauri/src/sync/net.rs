//! The iroh side of sync: one endpoint, a gate that drops unknown devices
//! right after the TLS handshake, one task per trusted device that keeps a
//! connection open, sync sessions, and pairing (docs/SYNC.md §2, §4).

use super::engine::{self, Applied, Ctx, Outgoing, Prefs};
use super::identity::clean_name;
use super::pairing::{self, Invite};
use super::store;
use super::wire::{self, Frame, PROTOCOL_VERSION};
use super::journal;
use crate::db;
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, RecvStream, Side, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, oneshot, watch};

pub const SYNC_ALPN: &[u8] = b"clipet/sync/1";
pub const PAIR_ALPN: &[u8] = b"clipet/pair/1";
/// Mouse and keyboard sharing; a connection of its own so clipboard
/// transfers never delay input.
pub const INPUT_ALPN: &[u8] = b"clipet/input/1";

pub type InputHandler = Arc<dyn Fn(EndpointId, Connection) + Send + Sync>;

const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_secs(20);
/// A session with no frame for this long is considered dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
const BATCH: i64 = 64;

const CLOSE_OK: u32 = 0;
const CLOSE_DUPLICATE: u32 = 1;
const CLOSE_REMOVED: u32 = 2;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// How the endpoint reaches the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// n0 relays, hole punching and address lookup (the app).
    Internet,
    /// Loopback only, no relays or lookup services (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    LocalOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Connection, pairing or device list changed.
    Status,
    /// Synced items were added, changed or removed.
    Items,
}

pub type Observer = Arc<dyn Fn(Change) + Send + Sync>;

/// Rejects connections the device never agreed to, before any of Clipet's
/// code reads from them: sync from untrusted devices, and pairing while no
/// invitation is open. Connections this device dials are always allowed.
#[derive(Debug)]
struct Gate {
    trusted: Arc<RwLock<HashSet<EndpointId>>>,
    pairing_open: Arc<AtomicBool>,
    /// While paused, peers are turned away instead of connecting and being
    /// dropped (which would make them redial in a loop).
    paused: Arc<AtomicBool>,
}

impl EndpointHooks for Gate {
    fn after_handshake<'a>(&'a self, conn: &'a Connection) -> impl Future<Output = AfterHandshakeOutcome> + Send + 'a {
        let allowed = conn.side() == Side::Client
            || match conn.alpn() {
                SYNC_ALPN => {
                    !self.paused.load(Ordering::SeqCst) && self.trusted.read().unwrap().contains(&conn.remote_id())
                }
                PAIR_ALPN => self.pairing_open.load(Ordering::SeqCst),
                // The input service checks the per-device grant itself, so
                // it can tell the other side why control was refused.
                INPUT_ALPN => self.trusted.read().unwrap().contains(&conn.remote_id()),
                _ => false,
            };
        async move {
            if allowed {
                AfterHandshakeOutcome::accept()
            } else {
                AfterHandshakeOutcome::Reject { error_code: VarInt::from_u32(CLOSE_REMOVED), reason: b"not paired".to_vec() }
            }
        }
    }
}

enum Pairing {
    Idle,
    /// Add Device is open: an invitation and a code for nearby devices.
    Hosting { secret: [u8; 32], code: String, exp: i64, invite: String },
    /// A device proved the invitation or the code; this user approves it.
    /// `code` is the verification code to compare (invitations only).
    HostConfirm { name: String, code: Option<String>, decide: Option<oneshot::Sender<bool>> },
    /// `confirm` is set while this user must confirm the verification code.
    Joining { host_name: String, code: Option<String>, confirm: Option<oneshot::Sender<bool>> },
    Done { name: String },
    Failed { reason: String },
}

/// What the Device Sync page shows about pairing.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum PairingView {
    Idle,
    Hosting { invite: String, code: String, expires_at: i64 },
    Confirm { name: String, code: Option<String> },
    Joining { host_name: String, code: Option<String>, needs_confirm: bool },
    Done { name: String },
    Failed { reason: String },
}

#[derive(Default)]
struct PeerRuntime {
    /// The live connection and whether it is the preferred one (dialed by
    /// the device with the smaller id; both sides keep that one).
    conn: Option<(Connection, bool)>,
    /// `stable_id` of the connection the peer has actually answered on. A
    /// connection the other side turns away right after the handshake never
    /// gets here, so it never shows as online.
    established: Option<usize>,
    last_error: Option<String>,
    wake: Arc<Notify>,
    running: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct PeerStatus {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub online: bool,
    pub added_at: i64,
    pub last_seen: Option<i64>,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    pub allow_clipboard: bool,
    pub allow_control: bool,
    pub side: Option<crate::input::geometry::Side>,
}

#[derive(Serialize, Clone, Debug)]
pub struct NodeStatus {
    pub device_id: String,
    pub short_id: String,
    /// Connected to a relay (reachable from other networks).
    pub relay_connected: bool,
    pub peers: Vec<PeerStatus>,
    pub pairing: PairingView,
    /// Unpaired Clipet devices on this network with Add Device open.
    pub nearby: Vec<NearbyDevice>,
}

#[derive(Serialize, Clone, Debug)]
pub struct NearbyDevice {
    pub id: String,
    pub short_id: String,
    pub name: String,
}

pub struct Node {
    data_dir: PathBuf,
    me: EndpointId,
    endpoint: Endpoint,
    network: Network,
    lookup: MemoryLookup,
    trusted: Arc<RwLock<HashSet<EndpointId>>>,
    pairing_open: Arc<AtomicBool>,
    peers: Mutex<HashMap<EndpointId, PeerRuntime>>,
    pairing: Mutex<Pairing>,
    /// The running join attempt, so Cancel can stop it.
    join_task: Mutex<Option<tokio::task::AbortHandle>>,
    paused: Arc<AtomicBool>,
    device_name: Mutex<String>,
    observer: Observer,
    shutdown: watch::Sender<bool>,
    /// Serializes applying received changes across sessions.
    apply_lock: Mutex<()>,
    input_handler: Mutex<Option<InputHandler>>,
    discovery: Option<super::discovery::Discovery>,
    /// Devices announcing "available to pair" on this network.
    nearby: Mutex<HashMap<EndpointId, String>>,
}

fn addr_json(addr: &EndpointAddr) -> Option<String> {
    serde_json::to_string(addr).ok()
}

fn short(id: &EndpointId) -> String {
    id.fmt_short().to_string()
}

impl Node {
    pub async fn start(
        data_dir: PathBuf,
        secret_key: SecretKey,
        network: Network,
        device_name: String,
        paused: bool,
        observer: Observer,
    ) -> Result<Arc<Self>, String> {
        let trusted = Arc::new(RwLock::new(HashSet::new()));
        let pairing_open = Arc::new(AtomicBool::new(false));
        let lookup = MemoryLookup::new();
        let paused = Arc::new(AtomicBool::new(paused));
        let gate = Gate { trusted: trusted.clone(), pairing_open: pairing_open.clone(), paused: paused.clone() };
        let alpns = vec![SYNC_ALPN.to_vec(), PAIR_ALPN.to_vec(), INPUT_ALPN.to_vec()];
        // Local-network discovery (not in loopback tests).
        let discovery = match network {
            Network::Internet => super::discovery::Discovery::new(secret_key.public()),
            Network::LocalOnly => None,
        };
        let builder = match network {
            Network::Internet => Endpoint::builder(presets::N0),
            Network::LocalOnly => Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .map_err(|e| e.to_string())?,
        };
        let builder = match &discovery {
            Some(d) => builder.address_lookup(d.clone()),
            None => builder,
        };
        let endpoint = builder
            .secret_key(secret_key)
            .alpns(alpns)
            .address_lookup(lookup.clone())
            .hooks(gate)
            .bind()
            .await
            .map_err(|e| format!("could not start the sync network: {e}"))?;

        let node = Arc::new(Self {
            data_dir,
            me: endpoint.id(),
            endpoint,
            network,
            lookup,
            trusted,
            pairing_open,
            peers: Mutex::new(HashMap::new()),
            pairing: Mutex::new(Pairing::Idle),
            join_task: Mutex::new(None),
            paused,
            device_name: Mutex::new(device_name),
            observer,
            shutdown: watch::channel(false).0,
            apply_lock: Mutex::new(()),
            input_handler: Mutex::new(None),
            discovery,
            nearby: Mutex::new(HashMap::new()),
        });
        if let Some(d) = node.discovery.clone() {
            let weak = Arc::downgrade(&node);
            tokio::spawn(super::discovery::watch(d, move |id, name| {
                let Some(node) = weak.upgrade() else { return };
                let mut nearby = node.nearby.lock().unwrap();
                let changed = match name {
                    Some(name) => nearby.insert(id, name.clone()).as_ref() != Some(&name),
                    None => nearby.remove(&id).is_some(),
                };
                drop(nearby);
                if changed {
                    node.changed(Change::Status);
                }
            }));
        }

        let conn = db::open(&node.data_dir);
        for peer in store::list_peers(&conn) {
            let Ok(id) = peer.id.parse::<EndpointId>() else { continue };
            if let Some(addr) = peer.addr.as_deref().and_then(|a| serde_json::from_str::<EndpointAddr>(a).ok()) {
                node.lookup.add_endpoint_info(addr);
            }
            node.trusted.write().unwrap().insert(id);
            node.spawn_peer_task(id);
        }
        drop(conn);

        let accept = node.clone();
        tokio::spawn(async move { accept.accept_loop().await });
        Ok(node)
    }

    #[cfg(test)]
    pub fn id(&self) -> EndpointId {
        self.me
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        for (_, rt) in self.peers.lock().unwrap().iter_mut() {
            if let Some((conn, _)) = rt.conn.take() {
                conn.close(VarInt::from_u32(CLOSE_OK), b"bye");
            }
        }
        self.endpoint.close().await;
    }

    fn changed(&self, change: Change) {
        (self.observer)(change);
    }

    fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub fn set_device_name(&self, name: String) {
        *self.device_name.lock().unwrap() = name;
    }

    /// This device's address as shared in invitations and with peers.
    async fn my_addr(&self) -> EndpointAddr {
        if self.network == Network::Internet {
            // Give the relay connection a moment so the invitation carries it.
            let _ = tokio::time::timeout(Duration::from_secs(5), self.endpoint.online()).await;
        }
        let mut addr = self.endpoint.addr();
        if addr.ip_addrs().next().is_none() {
            for socket in self.endpoint.bound_sockets() {
                addr = addr.with_ip_addr(socket);
            }
        }
        addr
    }

    // ------------------------------------------------------------ status

    pub fn status(&self) -> NodeStatus {
        self.expire_invite_if_due();
        let conn = db::open(&self.data_dir);
        let runtime = self.peers.lock().unwrap();
        let peers = store::list_peers(&conn)
            .into_iter()
            .map(|p| {
                let rt = p.id.parse::<EndpointId>().ok().and_then(|id| runtime.get(&id));
                PeerStatus {
                    short_id: p.id.parse::<EndpointId>().map(|id| short(&id)).unwrap_or_default(),
                    online: rt.is_some_and(|rt| {
                        rt.conn.as_ref().is_some_and(|(c, _)| {
                            c.close_reason().is_none() && rt.established == Some(c.stable_id())
                        })
                    }),
                    last_error: rt.and_then(|rt| rt.last_error.clone()),
                    allow_clipboard: p.allow_clipboard,
                    allow_control: p.allow_control,
                    side: p.side,
                    id: p.id,
                    name: p.name,
                    added_at: p.added_at,
                    last_seen: p.last_seen,
                    last_sync: p.last_sync,
                }
            })
            .collect();
        drop(runtime);
        let pairing = match &*self.pairing.lock().unwrap() {
            Pairing::Idle => PairingView::Idle,
            Pairing::Hosting { exp, invite, code, .. } => PairingView::Hosting { invite: invite.clone(), code: code.clone(), expires_at: *exp },
            Pairing::HostConfirm { name, code, .. } => PairingView::Confirm { name: name.clone(), code: code.clone() },
            Pairing::Joining { host_name, code, confirm } => {
                PairingView::Joining { host_name: host_name.clone(), code: code.clone(), needs_confirm: confirm.is_some() }
            }
            Pairing::Done { name } => PairingView::Done { name: name.clone() },
            Pairing::Failed { reason } => PairingView::Failed { reason: reason.clone() },
        };
        let nearby = self
            .nearby
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| **id != self.me && !self.is_trusted(**id))
            .map(|(id, name)| NearbyDevice { id: id.to_string(), short_id: short(id), name: name.clone() })
            .collect();
        NodeStatus {
            nearby,
            device_id: self.me.to_string(),
            short_id: short(&self.me),
            relay_connected: self.endpoint.addr().relay_urls().next().is_some(),
            peers,
            pairing,
        }
    }

    // ------------------------------------------------------------ peers

    fn spawn_peer_task(self: &Arc<Self>, peer: EndpointId) {
        {
            let mut peers = self.peers.lock().unwrap();
            let rt = peers.entry(peer).or_default();
            if rt.running {
                rt.wake.notify_one();
                return;
            }
            rt.running = true;
        }
        let node = self.clone();
        tokio::spawn(async move {
            node.clone().peer_task(peer).await;
            if let Some(rt) = node.peers.lock().unwrap().get_mut(&peer) {
                rt.running = false;
            }
        });
    }

    fn is_trusted(&self, peer: EndpointId) -> bool {
        self.trusted.read().unwrap().contains(&peer)
    }

    fn wake_handle(&self, peer: EndpointId) -> Arc<Notify> {
        self.peers.lock().unwrap().entry(peer).or_default().wake.clone()
    }

    fn live_conn(&self, peer: EndpointId) -> Option<Connection> {
        let peers = self.peers.lock().unwrap();
        peers
            .get(&peer)
            .and_then(|rt| rt.conn.as_ref())
            .filter(|(c, _)| c.close_reason().is_none())
            .map(|(c, _)| c.clone())
    }

    fn set_error(&self, peer: EndpointId, error: Option<String>) {
        if let Some(rt) = self.peers.lock().unwrap().get_mut(&peer) {
            rt.last_error = error;
        }
        self.changed(Change::Status);
    }

    /// Keeps one connection to `peer` open while it is trusted and sync runs.
    async fn peer_task(self: Arc<Self>, peer: EndpointId) {
        let wake = self.wake_handle(peer);
        let mut shutdown = self.shutdown.subscribe();
        let mut backoff = BACKOFF_MIN;
        loop {
            if self.is_shutdown() || !self.trusted.read().unwrap().contains(&peer) {
                return;
            }
            if self.paused.load(Ordering::SeqCst) {
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            }
            if let Some(conn) = self.live_conn(peer) {
                let connected = std::time::Instant::now();
                tokio::select! {
                    _ = conn.closed() => {}
                    _ = wake.notified() => continue,
                    _ = shutdown.changed() => continue,
                }
                // A session that keeps dying right away (the other side is
                // paused, removed us, or runs another version) must not turn
                // into a tight reconnect loop.
                if connected.elapsed() < Duration::from_secs(30) {
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = wake.notified() => {}
                        _ = shutdown.changed() => {}
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                } else {
                    backoff = BACKOFF_MIN;
                }
                continue;
            }
            match tokio::time::timeout(DIAL_TIMEOUT, self.endpoint.connect(peer, SYNC_ALPN)).await {
                Ok(Ok(conn)) => {
                    self.register(peer, conn, true);
                    continue;
                }
                Ok(Err(e)) => self.set_error(peer, Some(format!("Can't reach this device ({e})"))),
                Err(_) => self.set_error(peer, Some("Can't reach this device (timed out)".to_string())),
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = wake.notified() => {}
                _ = shutdown.changed() => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Adopts a new connection to `peer`, keeping exactly one: the one
    /// dialed by the device with the smaller id wins on both sides.
    fn register(self: &Arc<Self>, peer: EndpointId, conn: Connection, outgoing: bool) {
        // A dial that was already under way when the device was removed
        // completes after the removal; it must not become a session.
        if !self.is_trusted(peer) {
            conn.close(VarInt::from_u32(CLOSE_REMOVED), b"not paired");
            return;
        }
        let preferred = if outgoing { self.me < peer } else { peer < self.me };
        {
            let mut peers = self.peers.lock().unwrap();
            let rt = peers.entry(peer).or_default();
            if let Some((existing, existing_preferred)) = &rt.conn {
                if existing.close_reason().is_none() {
                    if preferred && !existing_preferred {
                        existing.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
                    } else {
                        conn.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
                        return;
                    }
                }
            }
            rt.conn = Some((conn.clone(), preferred));
            rt.last_error = None;
        }
        store::mark_seen(&db::open(&self.data_dir), &peer.to_string(), now_ms());
        self.changed(Change::Status);
        let node = self.clone();
        tokio::spawn(async move { node.session(peer, conn).await });
    }

    async fn session(self: Arc<Self>, peer: EndpointId, conn: Connection) {
        let mut shutdown = self.shutdown.subscribe();
        let result = tokio::select! {
            r = self.clone().serve(peer, conn.clone()) => r,
            r = self.clone().pull(peer, conn.clone()) => r,
            _ = conn.closed() => Ok(()),
            _ = shutdown.changed() => Ok(()),
        };
        conn.close(VarInt::from_u32(CLOSE_OK), b"bye");
        let mut peers = self.peers.lock().unwrap();
        if let Some(rt) = peers.get_mut(&peer) {
            if rt.conn.as_ref().is_some_and(|(c, _)| c.stable_id() == conn.stable_id()) {
                rt.conn = None;
            }
            if let Err(e) = &result {
                // Only counts and short ids reach logs, never content.
                eprintln!("[clipet] sync with {} ended: {e}", short(&peer));
                rt.last_error = Some(e.clone());
            }
            rt.wake.notify_one();
        }
        drop(peers);
        self.changed(Change::Status);
    }

    /// Sends this device's log to `peer`, after the cursor it asked for, then
    /// live changes as they happen.
    async fn serve(self: Arc<Self>, peer: EndpointId, conn: Connection) -> Result<(), String> {
        let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
        let request = tokio::time::timeout(SILENCE_LIMIT, wire::read_frame(&mut recv))
            .await
            .map_err(|_| "no request".to_string())?
            .map_err(|e| e.to_string())?;
        let Some((Frame::Request { version, cursor, prefs: theirs, addr, items }, _)) = request else {
            return Err("unexpected first frame".to_string());
        };
        if version != PROTOCOL_VERSION {
            return Err("the other device runs an incompatible Clipet version".to_string());
        }
        let db = db::open(&self.data_dir);
        if let Some(addr) = addr {
            self.remember_addr(&db, peer, &addr);
        }
        let mut changes = journal::subscribe();
        let mut pos = cursor;
        let mut presence_only_announced = false;
        loop {
            if self.paused.load(Ordering::SeqCst) {
                return Ok(());
            }
            if !self.is_trusted(peer) {
                return Err("device removed".to_string());
            }
            // Clipboard sync off on either side: only keep the device
            // "online" (heartbeats), and never move its cursor.
            if !items || !self.clipboard_allowed(&db, peer) {
                if !presence_only_announced {
                    presence_only_announced = true;
                    wire::write_frame(&mut send, &Frame::Heartbeat, None).await.map_err(|e| e.to_string())?;
                }
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() { return Ok(()) },
                    _ = tokio::time::sleep(HEARTBEAT) => {
                        wire::write_frame(&mut send, &Frame::Heartbeat, None).await.map_err(|e| e.to_string())?;
                    }
                }
                continue;
            }
            let mine = Prefs::from_settings(&store::load_settings(&db));
            let prefs = mine.intersect(theirs);
            let batch = store::events_after(&db, pos, BATCH);
            for event in &batch {
                let out = engine::outgoing(&db, &self.data_dir, event, &prefs);
                let (frame, blob) = match out {
                    Outgoing::Put(state, blob) => (
                        Frame::Event {
                            seq: event.seq,
                            op: store::PUT.to_string(),
                            item_id: event.item_id.clone(),
                            at: event.at,
                            blob_len: blob.as_ref().map_or(0, |b| b.len() as u64),
                            item: Some(state),
                        },
                        blob,
                    ),
                    Outgoing::Del => (
                        Frame::Event {
                            seq: event.seq,
                            op: store::DEL.to_string(),
                            item_id: event.item_id.clone(),
                            at: event.at,
                            item: None,
                            blob_len: 0,
                        },
                        None,
                    ),
                    Outgoing::Skip => {
                        pos = event.seq;
                        continue;
                    }
                };
                wire::write_frame(&mut send, &frame, blob.as_deref()).await.map_err(|e| e.to_string())?;
                pos = event.seq;
            }
            if (batch.len() as i64) < BATCH {
                wire::write_frame(&mut send, &Frame::CaughtUp { seq: pos }, None)
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::select! {
                    changed = changes.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                    _ = tokio::time::sleep(HEARTBEAT) => {
                        wire::write_frame(&mut send, &Frame::Heartbeat, None).await.map_err(|e| e.to_string())?;
                    }
                }
            }
        }
    }

    /// Asks `peer` for its changes after our cursor and applies them, each
    /// in one transaction with the cursor, so an interruption resumes at the
    /// first change not yet applied.
    async fn pull(self: Arc<Self>, peer: EndpointId, conn: Connection) -> Result<(), String> {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let peer_id = peer.to_string();
        let (cursor, prefs, items) = {
            let db = db::open(&self.data_dir);
            (store::cursor(&db, &peer_id), Prefs::from_settings(&store::load_settings(&db)), self.clipboard_allowed(&db, peer))
        };
        let addr = addr_json(&self.my_addr().await);
        let request = Frame::Request { version: PROTOCOL_VERSION, cursor, prefs, addr, items };
        wire::write_frame(&mut send, &request, None).await.map_err(|e| e.to_string())?;
        let mut established = false;
        loop {
            let frame = tokio::time::timeout(SILENCE_LIMIT, wire::read_frame(&mut recv))
                .await
                .map_err(|_| "the other device stopped responding".to_string())?
                .map_err(|e| format!("transfer interrupted ({e})"))?;
            let Some((frame, blob)) = frame else { return Ok(()) };
            if !self.is_trusted(peer) {
                return Err("device removed".to_string());
            }
            if !established {
                established = true;
                if let Some(rt) = self.peers.lock().unwrap().get_mut(&peer) {
                    rt.established = Some(conn.stable_id());
                }
                self.changed(Change::Status);
            }
            match frame {
                Frame::Event { seq, op, item_id, at, item, .. } => {
                    let applied = self.apply(&peer_id, seq, &op, &item_id, at, item, blob)?;
                    if applied.changed() {
                        self.changed(Change::Items);
                    }
                }
                Frame::CaughtUp { seq } => {
                    let db = db::open(&self.data_dir);
                    store::advance_cursor(&db, &peer_id, seq).map_err(|e| e.to_string())?;
                    store::mark_synced(&db, &peer_id, now_ms());
                    self.changed(Change::Status);
                }
                Frame::Heartbeat => {}
                _ => return Err("unexpected frame".to_string()),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        peer: &str,
        seq: i64,
        op: &str,
        item_id: &str,
        at: i64,
        item: Option<engine::ItemState>,
        blob: Option<Vec<u8>>,
    ) -> Result<Applied, String> {
        let me = self.me.to_string();
        let data_dir = self.data_dir.clone();
        let work = || {
            let _guard = self.apply_lock.lock().unwrap();
            let db = db::open(&data_dir);
            let prefs = Prefs::from_settings(&store::load_settings(&db));
            let ctx = Ctx { me: &me, peer, data_dir: &data_dir, prefs: &prefs, now: now_ms() };
            let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
            let still_paired = tx
                .query_row("SELECT 1 FROM sync_peers WHERE id = ?1", [peer], |_| Ok(()))
                .is_ok();
            if !still_paired {
                return Err("device removed".to_string());
            }
            let applied = match (op, item) {
                (store::PUT, Some(state)) => engine::apply_put(&tx, &ctx, &state, blob.as_deref())?,
                (store::DEL, _) => engine::apply_del(&tx, &ctx, item_id, at)?,
                _ => Applied::Ignored("unknown change"),
            };
            store::advance_cursor(&tx, peer, seq).map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(applied)
        };
        // Image decoding and SQLite are blocking work.
        tokio::task::block_in_place(work)
    }

    /// Clipboard sync with `peer` is on here: globally and for that device.
    fn clipboard_allowed(&self, db: &rusqlite::Connection, peer: EndpointId) -> bool {
        store::load_settings(db).clipboard
            && store::list_peers(db).iter().any(|p| p.id == peer.to_string() && p.allow_clipboard)
    }

    fn remember_addr(&self, db: &rusqlite::Connection, peer: EndpointId, addr: &str) {
        if let Ok(parsed) = serde_json::from_str::<EndpointAddr>(addr) {
            if parsed.id == peer {
                self.lookup.add_endpoint_info(parsed);
                store::set_peer_addr(db, &peer.to_string(), addr);
            }
        }
    }

    // ------------------------------------------------------------ control

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        let peers = self.peers.lock().unwrap();
        for rt in peers.values() {
            if paused {
                if let Some((conn, _)) = &rt.conn {
                    conn.close(VarInt::from_u32(CLOSE_OK), b"paused");
                }
            }
            rt.wake.notify_one();
        }
        drop(peers);
        journal::notify();
        self.changed(Change::Status);
    }

    /// "Sync Now": retry every offline device immediately.
    pub async fn sync_now(&self) {
        self.endpoint.network_change().await;
        for rt in self.peers.lock().unwrap().values() {
            rt.wake.notify_one();
        }
        journal::notify();
    }

    /// Closes every sync connection so they reconnect with current
    /// settings (e.g. clipboard sync turned on or off for a device).
    pub fn restart_sessions(&self) {
        for rt in self.peers.lock().unwrap().values() {
            if let Some((conn, _)) = &rt.conn {
                conn.close(VarInt::from_u32(CLOSE_OK), b"settings changed");
            }
            rt.wake.notify_one();
        }
    }

    pub fn set_input_handler(&self, handler: InputHandler) {
        *self.input_handler.lock().unwrap() = Some(handler);
    }

    /// Opens an input connection to a trusted device.
    pub async fn connect_input(&self, peer: EndpointId) -> Result<Connection, String> {
        if !self.is_trusted(peer) {
            return Err("That device isn't paired.".to_string());
        }
        match tokio::time::timeout(Duration::from_secs(10), self.endpoint.connect(peer, INPUT_ALPN)).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(e)) => Err(format!("Can't reach that device ({e}).")),
            Err(_) => Err("Can't reach that device (timed out).".to_string()),
        }
    }

    pub fn remove_peer(&self, id: &str) -> Result<(), String> {
        let peer: EndpointId = id.parse().map_err(|_| "unknown device".to_string())?;
        self.trusted.write().unwrap().remove(&peer);
        store::remove_peer(&db::open(&self.data_dir), id);
        self.lookup.remove_endpoint_info(peer);
        if let Some(rt) = self.peers.lock().unwrap().remove(&peer) {
            if let Some((conn, _)) = rt.conn {
                conn.close(VarInt::from_u32(CLOSE_REMOVED), b"removed");
            }
            rt.wake.notify_one();
        }
        self.changed(Change::Status);
        Ok(())
    }

    // ------------------------------------------------------------ pairing (host)

    /// Opens Add Device: an invitation (any network) and a 6-digit code for a
    /// nearby device. Either works once; the window closes after 10 minutes.
    pub async fn create_invite(&self) -> Result<String, String> {
        let secret = pairing::new_secret()?;
        let code = pairing::new_code()?;
        let exp = now_ms() + pairing::INVITE_TTL_MS;
        let name = self.device_name.lock().unwrap().clone();
        let invite = Invite { v: 1, addr: self.my_addr().await, name: name.clone(), secret: pairing::to_hex(&secret), exp };
        let text = pairing::encode(&invite);
        self.cancel_pairing();
        *self.pairing.lock().unwrap() = Pairing::Hosting { secret, code, exp, invite: text.clone() };
        self.pairing_open.store(true, Ordering::SeqCst);
        if let Some(d) = &self.discovery {
            d.set_pairing_name(Some(&name));
        }
        self.changed(Change::Status);
        Ok(text)
    }

    /// No more pairing attempts until Add Device is opened again.
    fn close_pairing_window(&self) {
        self.pairing_open.store(false, Ordering::SeqCst);
        if let Some(d) = &self.discovery {
            d.set_pairing_name(None);
        }
    }

    /// Clears an open invitation or a finished/failed pairing; declines a
    /// pending approval on either side.
    pub fn cancel_pairing(&self) {
        let mut pairing = self.pairing.lock().unwrap();
        match &mut *pairing {
            Pairing::HostConfirm { decide, .. } | Pairing::Joining { confirm: decide, .. } => {
                if let Some(decide) = decide.take() {
                    let _ = decide.send(false);
                }
            }
            _ => {}
        }
        *pairing = Pairing::Idle;
        if let Some(task) = self.join_task.lock().unwrap().take() {
            task.abort();
        }
        drop(pairing);
        self.close_pairing_window();
        self.changed(Change::Status);
    }

    /// The user's answer: approving a joining device (host), or confirming
    /// the verification code (joiner).
    pub fn decide_pairing(&self, approve: bool) -> Result<(), String> {
        let answer = match &mut *self.pairing.lock().unwrap() {
            Pairing::HostConfirm { decide, .. } => decide.take(),
            Pairing::Joining { confirm, .. } => confirm.take(),
            _ => None,
        };
        let answer = answer.ok_or("Nothing is waiting for an answer.")?;
        let _ = answer.send(approve);
        self.changed(Change::Status);
        Ok(())
    }

    fn set_pairing(&self, state: Pairing) {
        *self.pairing.lock().unwrap() = state;
        self.changed(Change::Status);
    }

    fn expire_invite_if_due(&self) {
        let mut pairing = self.pairing.lock().unwrap();
        if let Pairing::Hosting { exp, .. } = &*pairing {
            if now_ms() > *exp {
                *pairing = Pairing::Failed { reason: "The invitation expired. Create a new one.".to_string() };
                drop(pairing);
                self.close_pairing_window();
            }
        }
    }

    async fn accept_loop(self: Arc<Self>) {
        let mut shutdown = self.shutdown.subscribe();
        loop {
            let incoming = tokio::select! {
                incoming = self.endpoint.accept() => incoming,
                _ = shutdown.changed() => return,
            };
            let Some(incoming) = incoming else { return };
            let node = self.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let remote = conn.remote_id();
                match conn.alpn() {
                    SYNC_ALPN if node.trusted.read().unwrap().contains(&remote) => node.register(remote, conn, false),
                    INPUT_ALPN if node.is_trusted(remote) => {
                        let handler = node.input_handler.lock().unwrap().clone();
                        match handler {
                            Some(handle) => handle(remote, conn),
                            None => conn.close(VarInt::from_u32(CLOSE_OK), b"input sharing is off"),
                        }
                    }
                    PAIR_ALPN => node.host_pairing(conn).await,
                    _ => conn.close(VarInt::from_u32(CLOSE_REMOVED), b"not paired"),
                }
            });
        }
    }

    async fn host_pairing(self: Arc<Self>, conn: Connection) {
        self.expire_invite_if_due();
        let result = self.clone().host_pairing_inner(&conn).await;
        if let Err(reason) = result {
            self.close_pairing_window();
            let mut pairing = self.pairing.lock().unwrap();
            if !matches!(*pairing, Pairing::Done { .. }) {
                *pairing = Pairing::Failed { reason };
            }
            drop(pairing);
            self.changed(Change::Status);
        }
        // Let the last frame reach the other side before closing.
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        conn.close(VarInt::from_u32(CLOSE_OK), b"bye");
    }

    async fn host_pairing_inner(self: Arc<Self>, conn: &Connection) -> Result<(), String> {
        let joiner = conn.remote_id();
        let (mut send, mut recv) = tokio::time::timeout(DIAL_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| "The other device didn't respond.".to_string())?
            .map_err(|e| e.to_string())?;
        let Frame::PairHello { version, secret, name, addr, spake } = read(&mut recv, DIAL_TIMEOUT).await? else {
            return Err("Pairing failed: unexpected message.".to_string());
        };
        // One attempt per Add Device, whatever happens next.
        let window = {
            let pairing = self.pairing.lock().unwrap();
            match &*pairing {
                Pairing::Hosting { secret, code, exp, .. } if now_ms() <= *exp => Some((*secret, code.clone())),
                _ => None,
            }
        };
        self.close_pairing_window();
        let reject = |reason: &str| Frame::PairRejected { reason: reason.to_string() };
        let Some((expected_secret, code)) = window else {
            let _ = wire::write_frame(&mut send, &reject("not accepting devices right now"), None).await;
            return Err("A device tried to pair, but Add Device wasn't open.".to_string());
        };
        if version != PROTOCOL_VERSION {
            let _ = wire::write_frame(&mut send, &reject("incompatible Clipet version"), None).await;
            return Err("The other device runs an incompatible Clipet version.".to_string());
        }
        if joiner == self.me {
            return Err("A device can't pair with itself.".to_string());
        }
        let name = match clean_name(&name) {
            n if n.is_empty() => "Unnamed device".to_string(),
            n => n,
        };

        let shown_code = match spake {
            // Nearby: prove both sides know the code, without sending it.
            Some(their_msg) => {
                let their_msg = pairing::from_hex(&their_msg).ok_or("Pairing failed: malformed message.")?;
                let (state, my_msg) = pairing::spake_host(&code, &self.me, &joiner);
                wire::write_frame(&mut send, &Frame::PairSpake { msg: pairing::to_hex(&my_msg) }, None)
                    .await
                    .map_err(|e| e.to_string())?;
                let key = state.finish(&their_msg).ok();
                let proof_ok = match read(&mut recv, DIAL_TIMEOUT).await? {
                    Frame::PairProof { mac } => key
                        .as_ref()
                        .zip(pairing::from_hex(&mac))
                        .is_some_and(|(k, mac)| pairing::ct_eq(&mac, &pairing::key_proof(k, "joiner"))),
                    _ => false,
                };
                let Some(key) = key.filter(|_| proof_ok) else {
                    let _ = wire::write_frame(&mut send, &reject("wrong code"), None).await;
                    return Err("A device entered a wrong pairing code. Use Add Device again to get a new code.".to_string());
                };
                wire::write_frame(&mut send, &Frame::PairProof { mac: pairing::to_hex(&pairing::key_proof(&key, "host")) }, None)
                    .await
                    .map_err(|e| e.to_string())?;
                None
            }
            // Invitation: the 256-bit secret, then codes compared on both screens.
            None => {
                if !pairing::from_hex32(&secret).is_some_and(|s| pairing::ct_eq(&s, &expected_secret)) {
                    let _ = wire::write_frame(&mut send, &reject("invalid or expired invitation"), None).await;
                    return Err("A device tried to pair with an invalid or expired invitation. Create a new one.".to_string());
                }
                let code = pairing::verification_code(&self.me, &joiner, &expected_secret);
                wire::write_frame(&mut send, &Frame::PairPending { code: code.clone() }, None)
                    .await
                    .map_err(|e| e.to_string())?;
                Some(code)
            }
        };

        let (decide, decision) = oneshot::channel();
        self.set_pairing(Pairing::HostConfirm { name: name.clone(), code: shown_code.clone(), decide: Some(decide) });
        let approved = tokio::select! {
            d = decision => d.unwrap_or(false),
            _ = tokio::time::sleep(APPROVAL_TIMEOUT) => false,
            _ = conn.closed() => return Err("The other device cancelled pairing.".to_string()),
        };
        if !approved {
            let _ = wire::write_frame(&mut send, &reject("declined"), None).await;
            self.set_pairing(Pairing::Failed { reason: "Pairing was declined.".to_string() });
            return Ok(());
        }
        if shown_code.is_some() {
            // Approval on both devices: the joiner confirms the code too.
            match read(&mut recv, APPROVAL_TIMEOUT).await? {
                Frame::PairConfirm => {}
                _ => return Err(format!("{name} didn't confirm the code. Pairing stopped.")),
            }
        }

        let joiner_id = joiner.to_string();
        let addr = addr.filter(|a| serde_json::from_str::<EndpointAddr>(a).is_ok_and(|p| p.id == joiner));
        store::add_peer(&db::open(&self.data_dir), &joiner_id, &name, addr.as_deref(), now_ms())
            .map_err(|e| e.to_string())?;
        // Trusted before the joiner hears "accepted", so its first sync
        // connection isn't turned away.
        self.trusted.write().unwrap().insert(joiner);
        let my_name = self.device_name.lock().unwrap().clone();
        let confirmed = async {
            wire::write_frame(&mut send, &Frame::PairAccepted { name: my_name }, None).await.ok()?;
            match read(&mut recv, DIAL_TIMEOUT).await.ok()? {
                Frame::PairAck => Some(()),
                _ => None,
            }
        }
        .await;
        if confirmed.is_none() {
            // The other side never stored us; don't keep a one-sided trust.
            self.trusted.write().unwrap().remove(&joiner);
            store::remove_peer(&db::open(&self.data_dir), &joiner_id);
            return Err("The other device didn't finish pairing.".to_string());
        }
        if let Some(addr) = addr.and_then(|a| serde_json::from_str::<EndpointAddr>(&a).ok()) {
            self.lookup.add_endpoint_info(addr);
        }
        self.set_pairing(Pairing::Done { name });
        let _ = send.finish();
        self.spawn_peer_task(joiner);
        Ok(())
    }

    // ------------------------------------------------------------ pairing (joiner)

    /// Pairs using an invitation from the other device.
    pub fn join(self: &Arc<Self>, invite_text: &str) -> Result<(), String> {
        let invite = pairing::decode(invite_text)?;
        if now_ms() > invite.exp {
            return Err("This invitation has expired. Create a new one on the other device.".to_string());
        }
        if invite.addr.id == self.me {
            return Err("That invitation was created on this device.".to_string());
        }
        let host_name = clean_name(&invite.name);
        let secret = pairing::from_hex32(&invite.secret).ok_or("This invitation is damaged.")?;
        self.start_join(invite.addr.clone(), host_name, JoinWith::Invite { secret, addr: invite.addr });
        Ok(())
    }

    /// Pairs with a nearby device using the code it shows.
    pub fn join_nearby(self: &Arc<Self>, id: &str, code: &str) -> Result<(), String> {
        let id: EndpointId = id.parse().map_err(|_| "That device is no longer nearby.".to_string())?;
        let host_name = self.nearby.lock().unwrap().get(&id).cloned().unwrap_or_else(|| short(&id));
        self.join_with_code(EndpointAddr::new(id), host_name, code)
    }

    pub(crate) fn join_with_code(self: &Arc<Self>, addr: EndpointAddr, host_name: String, code: &str) -> Result<(), String> {
        let code = pairing::normalize_code(code).ok_or("Type the 6-digit code shown on the other device.")?;
        if addr.id == self.me {
            return Err("That's this device.".to_string());
        }
        self.start_join(addr, host_name, JoinWith::Code(code));
        Ok(())
    }

    fn start_join(self: &Arc<Self>, addr: EndpointAddr, host_name: String, with: JoinWith) {
        self.set_pairing(Pairing::Joining { host_name: host_name.clone(), code: None, confirm: None });
        let node = self.clone();
        let task = tokio::spawn(async move {
            if let Err(reason) = node.clone().join_inner(addr, host_name, with).await {
                node.set_pairing(Pairing::Failed { reason });
            }
        });
        if let Some(old) = self.join_task.lock().unwrap().replace(task.abort_handle()) {
            old.abort();
        }
    }

    async fn join_inner(self: Arc<Self>, addr: EndpointAddr, host_name: String, with: JoinWith) -> Result<(), String> {
        self.lookup.add_endpoint_info(addr.clone());
        let conn = tokio::time::timeout(DIAL_TIMEOUT, self.endpoint.connect(addr.clone(), PAIR_ALPN))
            .await
            .map_err(|_| format!("Couldn't reach {host_name}. Make sure Clipet is running there with Clipet Connect on."))?
            .map_err(|e| format!("Couldn't reach {host_name} ({e})."))?;
        let result = self.join_handshake(&conn, addr.id, &host_name, with).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        conn.close(VarInt::from_u32(CLOSE_OK), b"bye");
        result
    }

    async fn join_handshake(self: &Arc<Self>, conn: &Connection, host: EndpointId, host_name: &str, with: JoinWith) -> Result<(), String> {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let name = self.device_name.lock().unwrap().clone();
        let my_addr = addr_json(&self.my_addr().await);
        let refused = |what: &str| format!("{host_name} refused: {what}.");
        let stored_addr = match &with {
            JoinWith::Invite { addr, .. } => addr_json(addr),
            JoinWith::Code(_) => None,
        };
        match with {
            JoinWith::Invite { secret, .. } => {
                let hello = Frame::PairHello { version: PROTOCOL_VERSION, secret: pairing::to_hex(&secret), name, addr: my_addr, spake: None };
                wire::write_frame(&mut send, &hello, None).await.map_err(|e| e.to_string())?;
                let expected = pairing::verification_code(&host, &self.me, &secret);
                match read(&mut recv, DIAL_TIMEOUT).await? {
                    Frame::PairPending { code } if code == expected => {}
                    Frame::PairPending { .. } => return Err("The verification codes don't match. Pairing stopped.".to_string()),
                    Frame::PairRejected { reason } => return Err(refused(&reason)),
                    _ => return Err("Pairing failed: unexpected message.".to_string()),
                }
                // Approval on this device too: the user confirms the code.
                let (confirm, confirmed) = oneshot::channel();
                self.set_pairing(Pairing::Joining { host_name: host_name.to_string(), code: Some(expected), confirm: Some(confirm) });
                // Meanwhile the host may decline; nothing else arrives before
                // this side confirms.
                let ok = tokio::select! {
                    r = confirmed => r.unwrap_or(false),
                    frame = read(&mut recv, APPROVAL_TIMEOUT) => return Err(match frame {
                        Ok(Frame::PairRejected { .. }) => format!("{host_name} declined pairing."),
                        Ok(_) => "Pairing failed: unexpected message.".to_string(),
                        Err(e) => e,
                    }),
                };
                if !ok {
                    let _ = wire::write_frame(&mut send, &Frame::PairRejected { reason: "cancelled".into() }, None).await;
                    return Err("Pairing cancelled.".to_string());
                }
                wire::write_frame(&mut send, &Frame::PairConfirm, None).await.map_err(|e| e.to_string())?;
                self.set_pairing(Pairing::Joining { host_name: host_name.to_string(), code: None, confirm: None });
            }
            JoinWith::Code(code) => {
                let (state, my_msg) = pairing::spake_joiner(&code, &host, &self.me);
                let hello = Frame::PairHello { version: PROTOCOL_VERSION, secret: String::new(), name, addr: my_addr, spake: Some(pairing::to_hex(&my_msg)) };
                wire::write_frame(&mut send, &hello, None).await.map_err(|e| e.to_string())?;
                let their_msg = match read(&mut recv, DIAL_TIMEOUT).await? {
                    Frame::PairSpake { msg } => pairing::from_hex(&msg).ok_or("Pairing failed: malformed message.")?,
                    Frame::PairRejected { reason } => return Err(refused(&reason)),
                    _ => return Err("Pairing failed: unexpected message.".to_string()),
                };
                let key = state.finish(&their_msg).map_err(|_| "That code is wrong. Ask the other device for a new one.".to_string())?;
                wire::write_frame(&mut send, &Frame::PairProof { mac: pairing::to_hex(&pairing::key_proof(&key, "joiner")) }, None)
                    .await
                    .map_err(|e| e.to_string())?;
                match read(&mut recv, DIAL_TIMEOUT).await? {
                    Frame::PairProof { mac } if pairing::from_hex(&mac).is_some_and(|m| pairing::ct_eq(&m, &pairing::key_proof(&key, "host"))) => {}
                    Frame::PairRejected { .. } => return Err("That code is wrong. Use Add Device on the other computer again for a new code.".to_string()),
                    _ => return Err("The other device couldn't prove it knows the code. Pairing stopped.".to_string()),
                }
            }
        }
        match read(&mut recv, APPROVAL_TIMEOUT + Duration::from_secs(10)).await? {
            Frame::PairAccepted { name } => {
                let name = match clean_name(&name) {
                    n if n.is_empty() => host_name.to_string(),
                    n => n,
                };
                store::add_peer(&db::open(&self.data_dir), &host.to_string(), &name, stored_addr.as_deref(), now_ms())
                    .map_err(|e| e.to_string())?;
                self.trusted.write().unwrap().insert(host);
                wire::write_frame(&mut send, &Frame::PairAck, None).await.map_err(|e| e.to_string())?;
                let _ = send.finish();
                self.set_pairing(Pairing::Done { name });
                self.spawn_peer_task(host);
                Ok(())
            }
            Frame::PairRejected { .. } => Err(format!("{host_name} declined pairing.")),
            _ => Err("Pairing failed: unexpected message.".to_string()),
        }
    }
}

enum JoinWith {
    Invite { secret: [u8; 32], addr: EndpointAddr },
    Code(String),
}

async fn read(recv: &mut RecvStream, limit: Duration) -> Result<Frame, String> {
    match tokio::time::timeout(limit, wire::read_frame(recv)).await {
        Err(_) => Err("The other device stopped responding.".to_string()),
        Ok(Err(e)) => Err(format!("Connection problem ({e}).")),
        Ok(Ok(None)) => Err("The other device closed the connection.".to_string()),
        Ok(Ok(Some((frame, _)))) => Ok(frame),
    }
}
