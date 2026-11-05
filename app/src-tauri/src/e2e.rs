//! Live end-to-end paste check, compiled only with `--features e2e` and never
//! part of a shipped build. It drives the real popup → paste path inside the
//! signed app, so it runs with Clipet's own Accessibility permission.
//!
//! Write a command to `<app data>/e2e/cmd`; the result appears in
//! `<app data>/e2e/result`:
//! Every command starts with the title prefix of the test window that must be
//! in front (`paste <prefix> <text>`); otherwise nothing is pressed or pasted,
//! so a person's own windows are never typed into.
//!
//!   `paste <text>` — opens the popup over the frontmost window exactly like
//!   ⌘⇧V, waits like a user choosing an item, pastes `<text>` like Enter, then
//!   reports the target window's title (the test page mirrors its field there).
//!   `flow <text>` — the whole user flow at the keyboard level: copies
//!   `<text>`, presses ⌘⇧V and then Enter as real key events, reports the
//!   title, and removes `<text>` from history again.

use super::*;

pub fn start(app: tauri::AppHandle, monitor: Arc<MonitorState>, coordinator: Arc<PasteCoordinator>) {
    let Ok(dir) = app.path().app_data_dir().map(|d| d.join("e2e")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(command) = std::fs::read_to_string(dir.join("cmd")) else {
            continue;
        };
        let _ = std::fs::remove_file(dir.join("cmd"));
        let result = run(&app, &monitor, &coordinator, command.trim());
        let _ = std::fs::write(dir.join("result"), result);
    });
}

fn run(
    app: &tauri::AppHandle,
    monitor: &Arc<MonitorState>,
    coordinator: &Arc<PasteCoordinator>,
    command: &str,
) -> String {
    let mut words = command.splitn(3, ' ');
    let (Some(verb), Some(prefix), Some(text)) = (words.next(), words.next(), words.next()) else {
        return format!("expected <verb> <window-title-prefix> <text>: {command}");
    };
    let front = frontmost_application_pid()
        .and_then(paste_target::focused_window)
        .and_then(|w| paste_target::window_title(&w))
        .unwrap_or_default();
    if !front.starts_with(prefix) {
        return format!("refused: the window in front is not the test window ({prefix})");
    }
    if verb == "flow" {
        return flow(app, coordinator, text);
    }
    if verb == "type" {
        // A key press in the test page (e.g. to let it open a pop-up window).
        let typed = text.to_string();
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        let _ = app.run_on_main_thread(move || {
            use enigo::{Enigo, Keyboard, Settings};
            if let Ok(mut enigo) = Enigo::new(&Settings::default()) {
                let _ = enigo.text(&typed);
            }
            let _ = sent.send(());
        });
        let _ = received.recv_timeout(Duration::from_secs(3));
        return "typed".to_string();
    }
    if verb != "paste" {
        return format!("unknown command: {command}");
    }
    let mut report = Vec::new();

    let handle = app.clone();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let _ = app.run_on_main_thread(move || {
        toggle_history_window(&handle);
        let _ = sent.send(());
    });
    let _ = received.recv_timeout(Duration::from_secs(2));
    let target = snapshot_previous_paste_target(coordinator).unwrap_or_default();
    let title = |target: &PasteTarget| {
        target
            .window
            .as_ref()
            .and_then(paste_target::window_title)
            .unwrap_or_else(|| "<none>".into())
    };
    report.push(format!("target pid {:?}, window title before: {}", target.pid, title(&target)));

    // A person reading the list and pressing Enter.
    std::thread::sleep(Duration::from_millis(700));
    let content = text.to_string();
    let pasted = run_paste(app.clone(), monitor.clone(), coordinator.clone(), move |_, _| {
        Ok(PastePlan::Sequential {
            items: vec![PreparedSequentialItem {
                id: "e2e".into(),
                payload: PreparedClipboardPayload::Text {
                    hash: clipboard::hash_bytes(content.as_bytes()),
                    content,
                },
            }],
        })
    });
    report.push(format!("paste result: {pasted:?}"));
    std::thread::sleep(Duration::from_millis(800));
    report.push(format!("window title after: {}", title(&target)));
    report.join("\n")
}

/// Posts key events from the main thread (enigo's macOS requirement), with
/// raw key codes so no keyboard-layout lookup is involved.
fn keys(app: &tauri::AppHandle, steps: Vec<(enigo::Key, enigo::Direction)>) {
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let _ = app.run_on_main_thread(move || {
        use enigo::{Enigo, Keyboard, Settings};
        if let Ok(mut enigo) = Enigo::new(&Settings::default()) {
            for (key, direction) in steps {
                let _ = enigo.key(key, direction);
            }
        }
        let _ = sent.send(());
    });
    let _ = received.recv_timeout(Duration::from_secs(2));
}

fn flow(app: &tauri::AppHandle, coordinator: &Arc<PasteCoordinator>, text: &str) -> String {
    use enigo::{Direction::*, Key};
    const V: u32 = 9; // kVK_ANSI_V
    let mut report = Vec::new();
    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
        return format!("could not copy the marker: {e}");
    }
    // Let the history monitor (400 ms poll) record it as the newest item.
    std::thread::sleep(Duration::from_millis(1200));

    keys(app, vec![(Key::Meta, Press), (Key::Shift, Press), (Key::Other(V), Click), (Key::Shift, Release), (Key::Meta, Release)]);
    std::thread::sleep(Duration::from_millis(900));
    let popup_visible = app
        .get_webview_window("history")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    report.push(format!("popup visible after ⌘⇧V: {popup_visible}"));
    let target = snapshot_previous_paste_target(coordinator).unwrap_or_default();
    let title = |target: &PasteTarget| {
        target
            .window
            .as_ref()
            .and_then(paste_target::window_title)
            .unwrap_or_else(|| "<none>".into())
    };
    report.push(format!("target pid {:?}, window title before: {}", target.pid, title(&target)));
    if let Some(pid) = target.pid {
        report.push(format!("all windows before: {:?}", paste_target::window_titles(pid)));
    }

    // Find the item like a person would: replace the search box's text.
    keys(app, vec![(Key::Meta, Press), (Key::Other(0), Click), (Key::Meta, Release)]); // ⌘A
    let typed = text.to_string();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let _ = app.run_on_main_thread(move || {
        use enigo::{Enigo, Keyboard, Settings};
        if let Ok(mut enigo) = Enigo::new(&Settings::default()) {
            let _ = enigo.text(&typed);
        }
        let _ = sent.send(());
    });
    let _ = received.recv_timeout(Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(400));
    keys(app, vec![(Key::Return, Click)]);
    std::thread::sleep(Duration::from_millis(1500));
    report.push(format!("window title after Enter: {}", title(&target)));
    if let Some(pid) = target.pid {
        report.push(format!("all windows after: {:?}", paste_target::window_titles(pid)));
    }

    if let Ok(data_dir) = app.path().app_data_dir() {
        let conn = db::open(&data_dir);
        for item in db::list_items(&conn, 50).into_iter().filter(|i| i.content == text) {
            db::delete_item(&conn, &data_dir, &item.id);
        }
        let _ = app.emit("clipflow://new-item", ());
    }
    report.join("\n")
}
