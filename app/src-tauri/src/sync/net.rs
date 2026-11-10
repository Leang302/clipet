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
    Hosting { secret: [u8; 32], exp: i64, invite: String },
    HostConfirm { name: String, code: String, decide: Option<oneshot::Sender<bool>> },
    Joining { host_name: String, code: Option<String> },
    Done { name: String },
    Failed { reason: String },
}

/// What the Device Sync page shows about pairing.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum PairingView {
    Idle,
    Hosting { invite: String, expires_at: i64 },
    Confirm { name: String, code: String },
    Joining { host_name: String, code: Option<String> },
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
}

#[derive(Serialize, Clone, Debug)]
pub struct NodeStatus {
    pub device_id: String,
    pub short_id: String,
    /// Connected to a relay (reachable from other networks).
    pub relay_connected: bool,
    pub peers: Vec<PeerStatus>,
    pub pairing: PairingView,
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
        let alpns = vec![SYNC_ALPN.to_vec(), PAIR_ALPN.to_vec()];
        let builder = match network {
            Network::Internet => Endpoint::builder(presets::N0),
            Network::LocalOnly => Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .map_err(|e| e.to_string())?,
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
        });

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
            Pairing::Hosting { exp, invite, .. } => PairingView::Hosting { invite: invite.clone(), expires_at: *exp },
            Pairing::HostConfirm { name, code, .. } => PairingView::Confirm { name: name.clone(), code: code.clone() },
            Pairing::Joining { host_name, code } => PairingView::Joining { host_name: host_name.clone(), code: code.clone() },
            Pairing::Done { name } => PairingView::Done { name: name.clone() },
            Pairing::Failed { reason } => PairingView::Failed { reason: reason.clone() },
        };
        NodeStatus {
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
        let Some((Frame::Request { version, cursor, prefs: theirs, addr }, _)) = request else {
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
        loop {
            if self.paused.load(Ordering::SeqCst) {
                return Ok(());
            }
            if !self.is_trusted(peer) {
                return Err("device removed".to_string());
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
        let (cursor, prefs) = {
            let db = db::open(&self.data_dir);
            (store::cursor(&db, &peer_id), Prefs::from_settings(&store::load_settings(&db)))
        };
        let addr = addr_json(&self.my_addr().await);
        let request = Frame::Request { version: PROTOCOL_VERSION, cursor, prefs, addr };
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

    pub async fn create_invite(&self) -> Result<String, String> {
        let secret = pairing::new_secret()?;
        let exp = now_ms() + pairing::INVITE_TTL_MS;
        let invite = Invite {
            v: 1,
            addr: self.my_addr().await,
            name: self.device_name.lock().unwrap().clone(),
            secret: pairing::to_hex(&secret),
            exp,
        };
        let text = pairing::encode(&invite);
        self.cancel_pairing();
        *self.pairing.lock().unwrap() = Pairing::Hosting { secret, exp, invite: text.clone() };
        self.pairing_open.store(true, Ordering::SeqCst);
        self.changed(Change::Status);
        Ok(text)
    }

    /// Clears an open invitation or a finished/failed pairing; declines a
    /// pending approval.
    pub fn cancel_pairing(&self) {
        let mut pairing = self.pairing.lock().unwrap();
        if let Pairing::HostConfirm { decide, .. } = &mut *pairing {
            if let Some(decide) = decide.take() {
                let _ = decide.send(false);
            }
        }
        *pairing = Pairing::Idle;
        if let Some(task) = self.join_task.lock().unwrap().take() {
            task.abort();
        }
        self.pairing_open.store(false, Ordering::SeqCst);
        drop(pairing);
        self.changed(Change::Status);
    }

    pub fn decide_pairing(&self, approve: bool) -> Result<(), String> {
        let mut pairing = self.pairing.lock().unwrap();
        match &mut *pairing {
            Pairing::HostConfirm { decide, .. } => {
                if let Some(decide) = decide.take() {
                    let _ = decide.send(approve);
                }
                Ok(())
            }
            _ => Err("There is no device waiting for approval.".to_string()),
        }
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
                self.pairing_open.store(false, Ordering::SeqCst);
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
            self.pairing_open.store(false, Ordering::SeqCst);
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
        let hello = read(&mut recv, DIAL_TIMEOUT).await?;
        let Frame::PairHello { version, secret, name, addr } = hello else {
            return Err("Pairing failed: unexpected message.".to_string());
        };
        // One attempt per invitation, whatever happens next.
        let secret_ok = {
            let mut pairing = self.pairing.lock().unwrap();
            self.pairing_open.store(false, Ordering::SeqCst);
            let ok = match &*pairing {
                Pairing::Hosting { secret: expected, exp, .. } if now_ms() <= *exp => {
                    pairing::from_hex32(&secret).is_some_and(|s| pairing::ct_eq(&s, expected)).then_some(*expected)
                }
                _ => None,
            };
            if ok.is_none() {
                *pairing = Pairing::Failed {
                    reason: "A device tried to pair with an invalid or expired invitation. Create a new one.".to_string(),
                };
            }
            ok
        };
        let Some(secret) = secret_ok else {
            let _ = wire::write_frame(&mut send, &Frame::PairRejected { reason: "invalid or expired invitation".into() }, None).await;
            self.changed(Change::Status);
            return Ok(());
        };
        if version != PROTOCOL_VERSION {
            let _ = wire::write_frame(&mut send, &Frame::PairRejected { reason: "incompatible Clipet version".into() }, None).await;
            return Err("The other device runs an incompatible Clipet version.".to_string());
        }
        if joiner == self.me {
            return Err("A device can't pair with itself.".to_string());
        }
        let name = match clean_name(&name) {
            n if n.is_empty() => "Unnamed device".to_string(),
            n => n,
        };
        let code = pairing::verification_code(&self.me, &joiner, &secret);
        wire::write_frame(&mut send, &Frame::PairPending { code: code.clone() }, None)
            .await
            .map_err(|e| e.to_string())?;
        let (decide, decision) = oneshot::channel();
        self.set_pairing(Pairing::HostConfirm { name: name.clone(), code, decide: Some(decide) });

        let approved = tokio::select! {
            d = decision => d.unwrap_or(false),
            _ = tokio::time::sleep(APPROVAL_TIMEOUT) => false,
            _ = conn.closed() => return Err("The other device cancelled pairing.".to_string()),
        };
        if !approved {
            let _ = wire::write_frame(&mut send, &Frame::PairRejected { reason: "declined".into() }, None).await;
            self.set_pairing(Pairing::Failed { reason: "Pairing was declined.".to_string() });
            return Ok(());
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

    /// Starts pairing with the device that created `invite_text`; progress
    /// is reported through the status.
    pub fn join(self: &Arc<Self>, invite_text: &str) -> Result<(), String> {
        let invite = pairing::decode(invite_text)?;
        if now_ms() > invite.exp {
            return Err("This invitation has expired. Create a new one on the other device.".to_string());
        }
        if invite.addr.id == self.me {
            return Err("That invitation was created on this device.".to_string());
        }
        let host_name = clean_name(&invite.name);
        self.set_pairing(Pairing::Joining { host_name: host_name.clone(), code: None });
        let node = self.clone();
        let task = tokio::spawn(async move {
            if let Err(reason) = node.clone().join_inner(invite, host_name).await {
                node.set_pairing(Pairing::Failed { reason });
            }
        });
        if let Some(old) = self.join_task.lock().unwrap().replace(task.abort_handle()) {
            old.abort();
        }
        Ok(())
    }

    async fn join_inner(self: Arc<Self>, invite: Invite, host_name: String) -> Result<(), String> {
        let host = invite.addr.id;
        let secret = pairing::from_hex32(&invite.secret).ok_or("This invitation is damaged.")?;
        self.lookup.add_endpoint_info(invite.addr.clone());
        let conn = tokio::time::timeout(DIAL_TIMEOUT, self.endpoint.connect(invite.addr.clone(), PAIR_ALPN))
            .await
            .map_err(|_| format!("Couldn't reach {host_name}. Make sure Clipet is running there with sync on."))?
            .map_err(|e| format!("Couldn't reach {host_name} ({e})."))?;
        let result = self.join_handshake(&conn, &invite, host, &secret, &host_name).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        conn.close(VarInt::from_u32(CLOSE_OK), b"bye");
        result
    }

    async fn join_handshake(
        self: &Arc<Self>,
        conn: &Connection,
        invite: &Invite,
        host: EndpointId,
        secret: &[u8; 32],
        host_name: &str,
    ) -> Result<(), String> {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        let name = self.device_name.lock().unwrap().clone();
        let addr = addr_json(&self.my_addr().await);
        let hello = Frame::PairHello { version: PROTOCOL_VERSION, secret: invite.secret.clone(), name, addr };
        wire::write_frame(&mut send, &hello, None).await.map_err(|e| e.to_string())?;
        let expected = pairing::verification_code(&host, &self.me, secret);
        match read(&mut recv, DIAL_TIMEOUT).await? {
            Frame::PairPending { code } if code == expected => {}
            Frame::PairPending { .. } => return Err("The verification codes don't match. Pairing stopped.".to_string()),
            Frame::PairRejected { .. } => {
                return Err(format!("{host_name} refused the invitation. It may have expired or been used already."));
            }
            _ => return Err("Pairing failed: unexpected message.".to_string()),
        }
        self.set_pairing(Pairing::Joining { host_name: host_name.to_string(), code: Some(expected) });
        match read(&mut recv, APPROVAL_TIMEOUT + Duration::from_secs(10)).await? {
            Frame::PairAccepted { name } => {
                let name = match clean_name(&name) {
                    n if n.is_empty() => host_name.to_string(),
                    n => n,
                };
                store::add_peer(&db::open(&self.data_dir), &host.to_string(), &name, addr_json(&invite.addr).as_deref(), now_ms())
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

async fn read(recv: &mut RecvStream, limit: Duration) -> Result<Frame, String> {
    match tokio::time::timeout(limit, wire::read_frame(recv)).await {
        Err(_) => Err("The other device stopped responding.".to_string()),
        Ok(Err(e)) => Err(format!("Connection problem ({e}).")),
        Ok(Ok(None)) => Err("The other device closed the connection.".to_string()),
        Ok(Ok(Some((frame, _)))) => Ok(frame),
    }
}
