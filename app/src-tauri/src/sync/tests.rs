//! End-to-end sync over real iroh endpoints on this machine (loopback, no
//! relays or lookup services). These exercise pairing, live sync, offline
//! catch-up, restarts and revocation through the same code the app runs.

use super::net::{Network, Node, PairingView};
use super::{journal, pairing, store};
use crate::db;
use iroh::SecretKey;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct Dev {
    pub(crate) dir: PathBuf,
    key: SecretKey,
    name: String,
    network: Network,
    pub(crate) node: Arc<Node>,
}

async fn start(dir: PathBuf, key: SecretKey, name: &str, images: bool, network: Network) -> Dev {
    {
        let conn = db::open(&dir);
        let settings = store::SyncSettings { enabled: true, sync_images: images, ..Default::default() };
        store::save_settings(&conn, &settings);
    }
    let node = Node::start(dir.clone(), key.clone(), network, name.to_string(), false, Arc::new(|_| {}))
        .await
        .unwrap();
    Dev { dir, key, name: name.to_string(), network, node }
}

pub(crate) async fn device(name: &str, images: bool) -> Dev {
    start(super::test_dir(name), SecretKey::generate(), name, images, Network::LocalOnly).await
}

async fn restart(dev: Dev) -> Dev {
    dev.node.shutdown().await;
    start(dev.dir, dev.key, &dev.name, true, dev.network).await
}

pub(crate) async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn copy_text(dev: &Dev, id: &str, text: &str) {
    let conn = db::open(&dev.dir);
    db::insert_item(&conn, id, "text", text, text, "", chrono::Utc::now().timestamp_millis(), &crate::clipboard::hash_bytes(text.as_bytes()));
    store::append_event(&conn, id, store::PUT, 1).unwrap();
    journal::notify();
}

fn has(dev: &Dev, id: &str) -> bool {
    db::get_item(&db::open(&dev.dir), id).is_some()
}

fn pairing_state(dev: &Dev) -> PairingView {
    dev.node.status().pairing
}

pub(crate) async fn pair(host: &Dev, joiner: &Dev) {
    let invite = host.node.create_invite().await.unwrap();
    joiner.node.join(&invite).unwrap();
    wait_for("host asks for approval", || matches!(pairing_state(host), PairingView::Confirm { .. })).await;
    wait_for("joiner shows the code", || matches!(pairing_state(joiner), PairingView::Joining { code: Some(_), needs_confirm: true, .. })).await;
    let PairingView::Confirm { name, code: Some(code) } = pairing_state(host) else { unreachable!() };
    let PairingView::Joining { code: Some(joiner_code), .. } = pairing_state(joiner) else { unreachable!() };
    assert_eq!(code, joiner_code, "both screens show the same code");
    assert_eq!(name, joiner.name);
    // Approval on both devices.
    host.node.decide_pairing(true).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!matches!(pairing_state(host), PairingView::Done { .. }), "not done until the joiner confirms");
    joiner.node.decide_pairing(true).unwrap();
    wait_for("both sides done", || {
        matches!(pairing_state(host), PairingView::Done { .. }) && matches!(pairing_state(joiner), PairingView::Done { .. })
    })
    .await;
}

fn online(dev: &Dev, other: &Dev) -> bool {
    let id = other.node.id().to_string();
    dev.node.status().peers.iter().any(|p| p.id == id && p.online)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_then_sync_both_ways_catch_up_and_revoke() {
    let a = device("e2e-a", true).await;
    let b = device("e2e-b", true).await;
    pair(&a, &b).await;
    assert_eq!(store::list_peers(&db::open(&a.dir))[0].name, "e2e-b");
    assert_eq!(store::list_peers(&db::open(&b.dir))[0].name, "e2e-a");
    wait_for("connected", || online(&a, &b) && online(&b, &a)).await;

    // Live, both directions.
    copy_text(&a, "from-a", "hello from A");
    wait_for("A's copy reaches B", || has(&b, "from-a")).await;
    copy_text(&b, "from-b", "hello from B");
    wait_for("B's copy reaches A", || has(&a, "from-b")).await;
    let item = db::get_item(&db::open(&b.dir), "from-a").unwrap();
    assert_eq!(item.origin, a.node.id().to_string());

    // Offline catch-up across a restart: B is down while A copies.
    b.node.shutdown().await;
    for n in 0..20 {
        copy_text(&a, &format!("offline-{n}"), &format!("offline item {n}"));
    }
    let b = restart(b).await;
    wait_for("20 offline items reach B", || (0..20).all(|n| has(&b, &format!("offline-{n}")))).await;

    // An image.
    let pixels = image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]));
    let file = "img-a.png";
    pixels.save(a.dir.join("images").join(file)).unwrap();
    {
        let conn = db::open(&a.dir);
        db::insert_item(&conn, "img-a", "image", file, "8x8", "", 5, &crate::clipboard::hash_bytes(pixels.as_raw()));
        store::append_event(&conn, "img-a", store::PUT, 5).unwrap();
        journal::notify();
    }
    wait_for("the image reaches B", || has(&b, "img-a")).await;

    // A deletion.
    {
        let conn = db::open(&a.dir);
        db::delete_item(&conn, &a.dir, "from-a");
        store::append_event(&conn, "from-a", store::DEL, 10).unwrap();
        journal::notify();
    }
    wait_for("the delete reaches B", || !has(&b, "from-a")).await;

    // Revoke B on A: B's later copies never arrive, and B is not let back in.
    a.node.remove_peer(&b.node.id().to_string()).unwrap();
    wait_for("B disconnected", || !online(&b, &a)).await;
    copy_text(&b, "after-revoke", "should not sync");
    b.node.sync_now().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!has(&a, "after-revoke"));
    assert!(!online(&b, &a));

    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_invitation_is_refused_and_closes_the_invite() {
    let a = device("bad-a", false).await;
    let b = device("bad-b", false).await;
    let invite = a.node.create_invite().await.unwrap();
    let mut decoded = pairing::decode(&invite).unwrap();
    decoded.secret = pairing::to_hex(&pairing::new_secret().unwrap());
    b.node.join(&pairing::encode(&decoded)).unwrap();
    wait_for("joiner fails", || matches!(pairing_state(&b), PairingView::Failed { .. })).await;
    wait_for("host closes the invite", || matches!(pairing_state(&a), PairingView::Failed { .. })).await;
    // The real invitation is no longer accepted either: one attempt only.
    let c = device("bad-c", false).await;
    c.node.join(&invite).unwrap();
    wait_for("second attempt fails", || matches!(pairing_state(&c), PairingView::Failed { .. })).await;
    assert!(store::list_peers(&db::open(&a.dir)).is_empty());
    for dev in [a, b, c] {
        dev.node.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declined_pairing_trusts_nobody() {
    let a = device("dec-a", false).await;
    let b = device("dec-b", false).await;
    let invite = a.node.create_invite().await.unwrap();
    b.node.join(&invite).unwrap();
    wait_for("approval asked", || matches!(pairing_state(&a), PairingView::Confirm { .. })).await;
    a.node.decide_pairing(false).unwrap();
    wait_for("joiner told", || matches!(pairing_state(&b), PairingView::Failed { .. })).await;
    assert!(store::list_peers(&db::open(&a.dir)).is_empty());
    assert!(store::list_peers(&db::open(&b.dir)).is_empty());
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpaired_device_cannot_open_a_sync_connection() {
    let a = device("gate-a", false).await;
    let stranger = device("gate-x", false).await;
    let invite = pairing::decode(&a.node.create_invite().await.unwrap()).unwrap();
    a.node.cancel_pairing();
    // Dial A's sync protocol directly, as a device that was never paired.
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![super::net::SYNC_ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let result = async {
        let conn = endpoint.connect(invite.addr.clone(), super::net::SYNC_ALPN).await.ok()?;
        let (mut send, mut recv) = conn.open_bi().await.ok()?;
        let request = super::wire::Frame::Request {
            version: 1,
            cursor: 0,
            prefs: super::engine::Prefs::from_settings(&Default::default()),
            addr: None,
            items: true,
        };
        super::wire::write_frame(&mut send, &request, None).await.ok()?;
        super::wire::read_frame(&mut recv).await.ok()?
    };
    let got = tokio::time::timeout(Duration::from_secs(10), result).await.unwrap_or(None);
    assert!(got.is_none(), "an unpaired device must not receive anything");
    endpoint.close().await;
    a.node.shutdown().await;
    stranger.node.shutdown().await;
}

/// The real network path: n0 relays and address lookup (`Network::Internet`).
/// Needs internet access, so it only runs on request:
/// `cargo test internet -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn internet_pairing_and_sync_through_n0_services() {
    let a = start(super::test_dir("net-a"), SecretKey::generate(), "net-a", false, Network::Internet).await;
    let b = start(super::test_dir("net-b"), SecretKey::generate(), "net-b", false, Network::Internet).await;
    wait_for("relay connection", || a.node.status().relay_connected && b.node.status().relay_connected).await;
    // Pair the nearby way: B finds A over mDNS while Add Device is open on A,
    // then types A's code (SPAKE2).
    a.node.create_invite().await.unwrap();
    let code = hosting_code(&a);
    let a_id = a.node.id().to_string();
    wait_for("B sees A nearby", || b.node.status().nearby.iter().any(|d| d.id == a_id && d.name == "net-a")).await;
    b.node.join_nearby(&a_id, &code).unwrap();
    wait_for("A asks for approval", || matches!(pairing_state(&a), PairingView::Confirm { code: None, .. })).await;
    a.node.decide_pairing(true).unwrap();
    wait_for("paired", || {
        matches!(pairing_state(&a), PairingView::Done { .. }) && matches!(pairing_state(&b), PairingView::Done { .. })
    })
    .await;
    wait_for("A stops announcing", || !b.node.status().nearby.iter().any(|d| d.id == a_id)).await;
    copy_text(&a, "net-1", "over the internet");
    wait_for("A's copy reaches B", || has(&b, "net-1")).await;
    copy_text(&b, "net-2", "and back");
    wait_for("B's copy reaches A", || has(&a, "net-2")).await;
    // Reconnect by key alone: forget the stored addresses, restart B.
    db::open(&b.dir).execute("UPDATE sync_peers SET addr = NULL", []).unwrap();
    b.node.shutdown().await;
    copy_text(&a, "net-3", "after restart");
    let b = restart(b).await;
    wait_for("catch-up after restart via address lookup", || has(&b, "net-3")).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_stops_traffic_and_resume_catches_up() {
    let a = device("pause-a", false).await;
    let b = device("pause-b", false).await;
    pair(&a, &b).await;
    wait_for("connected", || online(&a, &b) && online(&b, &a)).await;

    b.node.set_paused(true);
    wait_for("A sees B go away", || !online(&a, &b)).await;
    copy_text(&a, "while-paused", "held back");
    a.node.sync_now().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!has(&b, "while-paused"), "nothing transfers while paused");
    assert!(!online(&a, &b), "a paused device turns connections away");

    b.node.set_paused(false);
    wait_for("caught up after resume", || has(&b, "while-paused")).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}

/// Drives the real, built Clipet app (run under a separate test identifier so
/// it gets its own empty profile) against a sync node standing in for a
/// second computer, over the real internet path. Opt-in, and only via
/// `CLIPET_APP_EXE` / `CLIPET_TEST_PROFILE`. Puts the user's clipboard back
/// and removes the test key and profile afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn live_app_syncs_with_another_device() {
    let exe = std::env::var("CLIPET_APP_EXE").expect("set CLIPET_APP_EXE");
    let profile = PathBuf::from(std::env::var("CLIPET_TEST_PROFILE").expect("set CLIPET_TEST_PROFILE"));
    let entry = keyring::Entry::new("com.clipflow.app", "sync-device-key").unwrap();
    assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)), "a real device key exists; not touching it");

    struct Cleanup {
        child: Option<std::process::Child>,
        clipboard: Option<String>,
        profile: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Ok(mut board) = arboard::Clipboard::new() {
                let _ = match self.clipboard.take() {
                    Some(text) => board.set_text(text),
                    None => board.clear(),
                };
            }
            if let Ok(entry) = keyring::Entry::new("com.clipflow.app", "sync-device-key") {
                let _ = entry.delete_credential();
            }
            std::thread::sleep(Duration::from_millis(500));
            let _ = std::fs::remove_dir_all(&self.profile);
        }
    }
    let launch = |exe: &str| std::process::Command::new(exe).spawn().expect("launch the app");
    async fn wait_long(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..1200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    let _ = std::fs::remove_dir_all(&profile);
    let mut cleanup = Cleanup { child: None, clipboard: arboard::Clipboard::new().ok().and_then(|mut b| b.get_text().ok()), profile: profile.clone() };

    // Identities: the app's key goes where the app looks for it.
    let app_key = SecretKey::generate();
    entry.set_password(&pairing::to_hex(&app_key.to_bytes())).unwrap();
    let app_id = app_key.public().to_string();
    let helper_key = SecretKey::generate();
    let helper_id = helper_key.public().to_string();
    let helper_dir = super::test_dir("live-helper");

    // Already paired both ways (pairing itself is covered by the other tests).
    store::add_peer(&db::open(&helper_dir), &app_id, "Clipet app", None, 1).unwrap();
    {
        let conn = db::open(&profile);
        store::save_settings(&conn, &store::SyncSettings { enabled: true, ..Default::default() });
        store::add_peer(&conn, &helper_id, "Other computer", None, 1).unwrap();
    }
    let helper = start(helper_dir, helper_key, "Other computer", false, Network::Internet).await;
    let app_online = |helper: &Dev| helper.node.status().peers.iter().any(|p| p.id == app_id && p.online);
    let app_has = |id: &str| db::get_item(&db::open(&profile), id);

    cleanup.child = Some(launch(&exe));
    wait_long("the app connects to the other device", || app_online(&helper)).await;
    println!("connected: app {} <-> other device {}", &app_id[..10], &helper_id[..10]);

    // 1. A real copy on this computer reaches the other device.
    let marker = format!("Clipet live sync test {}", pairing::to_hex(&pairing::new_secret().unwrap()[..4]));
    arboard::Clipboard::new().unwrap().set_text(marker.clone()).unwrap();
    let hash = crate::clipboard::hash_bytes(marker.as_bytes());
    wait_long("the copy reaches the other device", || db::find_item_by_hash(&db::open(&helper.dir), "text", &hash).is_some()).await;
    println!("PASS app -> other device: copied text arrived");

    // 2. The other device's copy appears in the app's history only.
    copy_text(&helper, "live-from-other", "Copied on the other computer");
    wait_long("the other device's copy reaches the app", || app_has("live-from-other").is_some()).await;
    assert_eq!(app_has("live-from-other").unwrap().origin, helper_id);
    assert_eq!(arboard::Clipboard::new().unwrap().get_text().unwrap(), marker, "the app's clipboard must not change");
    println!("PASS other device -> app: in history, clipboard untouched");

    // 3. Offline catch-up across an app restart.
    if let Some(mut child) = cleanup.child.take() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    wait_long("the other device sees the app go offline", || !app_online(&helper)).await;
    for n in 0..5 {
        copy_text(&helper, &format!("live-offline-{n}"), &format!("Copied while the app was closed {n}"));
    }
    cleanup.child = Some(launch(&exe));
    wait_long("5 items caught up after restart", || (0..5).all(|n| app_has(&format!("live-offline-{n}")).is_some())).await;
    println!("PASS offline catch-up: 5 items after restart");

    // 4. A delete on the other device removes the item in the app.
    {
        let conn = db::open(&helper.dir);
        db::delete_item(&conn, &helper.dir, "live-from-other");
        store::append_event(&conn, "live-from-other", store::DEL, chrono::Utc::now().timestamp_millis()).unwrap();
        journal::notify();
    }
    wait_long("the delete reaches the app", || app_has("live-from-other").is_none()).await;
    println!("PASS delete propagates");

    helper.node.shutdown().await;
    drop(cleanup);
    assert!(matches!(
        keyring::Entry::new("com.clipflow.app", "sync-device-key").unwrap().get_password(),
        Err(keyring::Error::NoEntry)
    ));
}


fn hosting_code(dev: &Dev) -> String {
    match pairing_state(dev) {
        PairingView::Hosting { code, .. } => code,
        other => panic!("not hosting: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nearby_pairing_with_the_right_code() {
    let a = device("code-a", false).await;
    let b = device("code-b", false).await;
    let invite = pairing::decode(&a.node.create_invite().await.unwrap()).unwrap();
    let code = hosting_code(&a);
    // Typed without the space, as people do.
    b.node.join_with_code(invite.addr, "code-a".into(), &code.replace(' ', "")).unwrap();
    wait_for("host asks for approval", || matches!(pairing_state(&a), PairingView::Confirm { code: None, .. })).await;
    let PairingView::Confirm { name, .. } = pairing_state(&a) else { unreachable!() };
    assert_eq!(name, "code-b");
    a.node.decide_pairing(true).unwrap();
    wait_for("both done", || {
        matches!(pairing_state(&a), PairingView::Done { .. }) && matches!(pairing_state(&b), PairingView::Done { .. })
    })
    .await;
    wait_for("connected", || online(&a, &b) && online(&b, &a)).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_code_fails_once_and_closes_add_device() {
    let a = device("bad-code-a", false).await;
    let b = device("bad-code-b", false).await;
    let invite = pairing::decode(&a.node.create_invite().await.unwrap()).unwrap();
    let right = hosting_code(&a);
    let wrong = if right == "000 000" { "111 111" } else { "000 000" };
    b.node.join_with_code(invite.addr.clone(), "bad-code-a".into(), wrong).unwrap();
    wait_for("joiner fails", || matches!(pairing_state(&b), PairingView::Failed { .. })).await;
    wait_for("host closes Add Device", || matches!(pairing_state(&a), PairingView::Failed { .. })).await;
    // The right code no longer works either: one guess per Add Device.
    let c = device("bad-code-c", false).await;
    c.node.join_with_code(invite.addr, "bad-code-a".into(), &right).unwrap();
    wait_for("second try refused", || matches!(pairing_state(&c), PairingView::Failed { .. })).await;
    assert!(store::list_peers(&db::open(&a.dir)).is_empty());
    for dev in [a, b, c] {
        dev.node.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_who_cancels_at_the_code_trusts_nobody() {
    let a = device("jc-a", false).await;
    let b = device("jc-b", false).await;
    let invite = a.node.create_invite().await.unwrap();
    b.node.join(&invite).unwrap();
    wait_for("joiner shows the code", || matches!(pairing_state(&b), PairingView::Joining { needs_confirm: true, .. })).await;
    a.node.decide_pairing(true).unwrap();
    b.node.decide_pairing(false).unwrap();
    wait_for("host told", || matches!(pairing_state(&a), PairingView::Failed { .. })).await;
    assert!(store::list_peers(&db::open(&a.dir)).is_empty());
    assert!(store::list_peers(&db::open(&b.dir)).is_empty());
    a.node.shutdown().await;
    b.node.shutdown().await;
}
