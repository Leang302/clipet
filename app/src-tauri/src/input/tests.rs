//! Input sharing end to end: two paired devices over real iroh endpoints on
//! loopback, with mock OS layers standing in for hooks and injection.

use super::geometry::{Screen, Side};
use super::keymap::*;
use super::mock::MockPlatform;
use super::protocol::{Button, Os};
use super::session::{Hotkey, Inject};
use super::{Captured, InputMode, InputService, InputSettings};
use crate::db;
use crate::sync::store;
use crate::sync::tests::{Dev, device, pair, wait_for};
use std::sync::Arc;
use std::time::Duration;

struct Side2 {
    dev: Dev,
    platform: Arc<MockPlatform>,
    input: Arc<InputService>,
}

fn screens_pc() -> Vec<Screen> {
    vec![Screen { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0, scale: 1.0 }]
}

fn screens_mac() -> Vec<Screen> {
    vec![Screen { x: 0.0, y: 0.0, w: 1512.0, h: 982.0, scale: 1.0 }]
}

fn attach(dev: Dev, os: Os, screens: Vec<Screen>, settings: InputSettings) -> Side2 {
    db::open(&dev.dir);
    super::save_settings(&db::open(&dev.dir), &settings);
    let platform = Arc::new(MockPlatform::new(os, screens));
    let input = InputService::new(Some(platform.clone()), dev.dir.clone(), Arc::new(|| {}));
    input.attach(Some(dev.node.clone()));
    Side2 { dev, platform, input }
}

/// A Windows PC with a Mac to its right; the Mac allows control from the PC.
async fn desk(confirm: bool, allow: bool) -> (Side2, Side2) {
    let pc = device(&format!("in-pc-{confirm}-{allow}"), false).await;
    let mac = device(&format!("in-mac-{confirm}-{allow}"), false).await;
    pair(&pc, &mac).await;
    let pc_id = pc.node.status().device_id;
    let mac_id = mac.node.status().device_id;
    store::set_side(&db::open(&pc.dir), &mac_id, Some(Side::Right));
    store::set_allow_control(&db::open(&mac.dir), &pc_id, allow);
    let on = InputSettings { enabled: true, require_confirmation: confirm, ..Default::default() };
    let pc = attach(pc, Os::Windows, screens_pc(), on.clone());
    let mac = attach(mac, Os::MacOs, screens_mac(), on);
    (pc, mac)
}

fn mode(s: &Side2) -> InputMode {
    s.input.status().mode
}

fn push_right_edge(pc: &Side2) {
    for _ in 0..2 {
        pc.platform.feed(Captured::Position { x: 1919.0, y: 270.0, button_held: false });
    }
}

async fn start_controlling(pc: &Side2, mac: &Side2) {
    push_right_edge(pc);
    wait_for("PC controls the Mac", || matches!(mode(pc), InputMode::Controlling { .. }) && pc.platform.is_remote()).await;
    wait_for("the Mac is controlled", || matches!(mode(mac), InputMode::ControlledBy { .. })).await;
}

fn keys(mac: &Side2) -> Vec<(u16, bool)> {
    mac.platform
        .injected()
        .into_iter()
        .filter_map(|i| match i {
            Inject::Key { hid, down, .. } => Some((hid, down)),
            _ => None,
        })
        .collect()
}

async fn shutdown(a: Side2, b: Side2) {
    a.dev.node.shutdown().await;
    b.dev.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edge_switch_forwards_mouse_and_keys_then_returns_at_the_same_height() {
    let (pc, mac) = desk(false, true).await;
    start_controlling(&pc, &mac).await;

    // Entered at the Mac's left edge, a quarter of the way down.
    let first = mac.platform.injected()[0].clone();
    assert_eq!(first, Inject::MoveTo { x: 2.0, y: 982.0 * 0.25, buttons: vec![] });

    pc.platform.feed(Captured::Delta { dx: 10.0, dy: 5.0 });
    wait_for("the move arrives", || mac.platform.injected().iter().any(|i| *i == Inject::MoveTo { x: 12.0, y: 250.5, buttons: vec![] })).await;

    // Ctrl+C on the PC keyboard is ⌘C on the Mac.
    for (hid, down) in [(HID_LCTRL, true), (0x06, true), (0x06, false), (HID_LCTRL, false)] {
        pc.platform.feed(Captured::Key { hid, down, repeat: false });
    }
    wait_for("keys arrive", || keys(&mac).len() == 4).await;
    assert_eq!(keys(&mac), vec![(HID_LGUI, true), (0x06, true), (0x06, false), (HID_LGUI, false)]);

    pc.platform.feed(Captured::Button { b: Button::Left, down: true });
    pc.platform.feed(Captured::Button { b: Button::Left, down: false });
    wait_for("a click arrives", || {
        mac.platform.injected().iter().filter(|i| matches!(i, Inject::Button { b: Button::Left, .. })).count() == 2
    })
    .await;
    pc.platform.feed(Captured::Scroll { dx: 0.0, dy: 120.0 });
    wait_for("a scroll arrives", || mac.platform.injected().contains(&Inject::Scroll { dx: 0.0, dy: 120.0 })).await;

    // Back across the Mac's left edge: control returns to the PC.
    tokio::time::sleep(Duration::from_millis(350)).await;
    pc.platform.feed(Captured::Delta { dx: -5000.0, dy: 0.0 });
    wait_for("control returns", || matches!(mode(&pc), InputMode::Idle) && !pc.platform.is_remote()).await;
    let (remote, place) = *pc.platform.remote.lock().unwrap().last().unwrap();
    assert!(!remote);
    let (x, y) = place.expect("pointer placed at the edge");
    assert_eq!(x, 1918.0);
    assert!((y - 1080.0 * (250.5 / 982.0)).abs() < 2.0, "same relative height, got {y}");
    wait_for("the Mac is idle", || matches!(mode(&mac), InputMode::Idle)).await;
    shutdown(pc, mac).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_without_permission_cannot_take_control() {
    let (pc, mac) = desk(false, false).await;
    push_right_edge(&pc);
    wait_for("refused", || pc.input.status().last_error.is_some()).await;
    let error = pc.input.status().last_error.unwrap();
    assert!(error.contains("doesn't allow control"), "{error}");
    assert!(!pc.platform.is_remote());
    assert!(mac.platform.injected().is_empty(), "nothing injected");
    assert!(matches!(mode(&pc), InputMode::Idle));
    shutdown(pc, mac).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emergency_shortcut_returns_control_and_releases_held_keys() {
    let (pc, mac) = desk(false, true).await;
    start_controlling(&pc, &mac).await;
    pc.platform.feed(Captured::Key { hid: HID_LSHIFT, down: true, repeat: false });
    pc.platform.feed(Captured::Key { hid: 0x04, down: true, repeat: false });
    wait_for("keys held on the Mac", || keys(&mac).len() == 2).await;
    pc.platform.feed(Captured::Hotkey(Hotkey::Emergency));
    wait_for("local again", || !pc.platform.is_remote() && matches!(mode(&pc), InputMode::Idle)).await;
    wait_for("released on the Mac", || keys(&mac).len() == 4).await;
    // The letter before its modifier.
    assert_eq!(&keys(&mac)[2..], &[(0x04, false), (HID_LSHIFT, false)]);
    shutdown(pc, mac).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_controller_never_leaves_keys_stuck() {
    let (pc, mac) = desk(false, true).await;
    start_controlling(&pc, &mac).await;
    pc.platform.feed(Captured::Key { hid: HID_LCTRL, down: true, repeat: false });
    pc.platform.feed(Captured::Button { b: Button::Left, down: true });
    wait_for("held on the Mac", || keys(&mac).len() == 1).await;
    // The PC disappears mid-session (crash, Wi-Fi drop).
    pc.dev.node.shutdown().await;
    wait_for("released after the silence timeout", || {
        keys(&mac).contains(&(HID_LGUI, false))
            && mac.platform.injected().iter().any(|i| matches!(i, Inject::Button { b: Button::Left, down: false, .. }))
    })
    .await;
    wait_for("the Mac is idle", || matches!(mode(&mac), InputMode::Idle)).await;
    mac.dev.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmation_is_asked_on_the_controlled_computer() {
    let (pc, mac) = desk(true, true).await;
    push_right_edge(&pc);
    wait_for("the Mac asks", || mac.input.status().request.is_some()).await;
    assert_eq!(mac.input.status().request.as_deref(), Some("in-pc-true-true"), "asks by device name");
    assert!(!pc.platform.is_remote(), "nothing forwarded before approval");
    mac.input.answer_request(true).unwrap();
    wait_for("PC controls the Mac", || pc.platform.is_remote()).await;
    // Stop Sharing on the Mac ends it from that side.
    mac.input.stop();
    wait_for("PC is local again", || !pc.platform.is_remote() && matches!(mode(&pc), InputMode::Idle)).await;

    // A declined request forwards nothing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    push_right_edge(&pc);
    wait_for("asked again", || mac.input.status().request.is_some()).await;
    mac.input.answer_request(false).unwrap();
    wait_for("declined", || pc.input.status().last_error.is_some()).await;
    assert!(!pc.platform.is_remote());
    shutdown(pc, mac).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_control_ends_the_session() {
    let (pc, mac) = desk(false, true).await;
    start_controlling(&pc, &mac).await;
    let pc_id = pc.dev.node.status().device_id;
    store::set_allow_control(&db::open(&mac.dev.dir), &pc_id, false);
    mac.input.forget(&pc_id);
    wait_for("PC is local again", || !pc.platform.is_remote()).await;
    // And a new attempt is refused.
    tokio::time::sleep(Duration::from_millis(500)).await;
    pc.input.refresh();
    push_right_edge(&pc);
    wait_for("refused", || pc.input.status().last_error.is_some()).await;
    assert!(!pc.platform.is_remote());
    shutdown(pc, mac).await;
}
