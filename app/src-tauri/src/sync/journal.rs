//! The hook capture and history commands call after changing an item. While
//! sync is on it logs the change (see `store::append_event`) and wakes the
//! live sessions; while sync is off it does nothing.

use super::store;
use rusqlite::Connection;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

static ENABLED: AtomicBool = AtomicBool::new(false);
static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();

fn changes() -> &'static watch::Sender<u64> {
    CHANGES.get_or_init(|| watch::channel(0).0)
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

/// Wakes every session serving this device's log.
pub fn notify() {
    changes().send_modify(|n| *n = n.wrapping_add(1));
}

pub fn subscribe() -> watch::Receiver<u64> {
    changes().subscribe()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// An item was created or changed (copied, re-copied, pinned, edited).
pub fn put(conn: &Connection, item_id: &str) {
    record(conn, item_id, store::PUT);
}

/// An item was deleted by the user (not by Clear or retention: those stay
/// local, see docs/SYNC.md §5).
pub fn del(conn: &Connection, item_id: &str) {
    record(conn, item_id, store::DEL);
}

fn record(conn: &Connection, item_id: &str, op: &str) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    match store::append_event(conn, item_id, op, now_ms()) {
        Ok(_) => notify(),
        Err(e) => eprintln!("[clipet] could not record a sync change: {e}"),
    }
}
