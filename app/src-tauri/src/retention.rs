//! Automatic history cleanup. Off until the user picks a period; then expired
//! items are removed at startup and every hour while Clipet runs.

use crate::db::{self, Retention};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

const INTERVAL: Duration = Duration::from_secs(60 * 60);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Runs cleanup once and tells the popup to reload if anything was removed.
fn run(app: &AppHandle, data_dir: &PathBuf) -> Result<usize, String> {
    let conn = db::open(data_dir);
    let retention = db::load_retention(&conn);
    let removed = db::delete_expired(&conn, data_dir, &retention, now_ms())?;
    if removed > 0 {
        let _ = app.emit("clipflow://new-item", ());
    }
    Ok(removed)
}

pub fn start(app: AppHandle) {
    let Ok(dir) = data_dir(&app) else { return };
    std::thread::spawn(move || loop {
        if let Err(e) = run(&app, &dir) {
            // Retried next round; a failed transaction leaves history intact.
            eprintln!("[clipet] history cleanup failed: {e}");
        }
        std::thread::sleep(INTERVAL);
    });
}

#[derive(Serialize)]
pub struct RetentionStatus {
    retention: Retention,
    /// Items the current setting would remove right now.
    eligible: i64,
}

#[tauri::command]
pub fn history_retention_get(app: AppHandle) -> Result<RetentionStatus, String> {
    let conn = db::open(&data_dir(&app)?);
    let retention = db::load_retention(&conn);
    Ok(RetentionStatus { eligible: db::count_expired(&conn, &retention, now_ms()), retention })
}

/// How many items `retention` would remove now, without changing anything.
#[tauri::command]
pub fn history_retention_preview(app: AppHandle, retention: Retention) -> Result<i64, String> {
    let conn = db::open(&data_dir(&app)?);
    Ok(db::count_expired(&conn, &retention, now_ms()))
}

/// Saves the setting and applies it immediately (the UI confirms first).
#[tauri::command]
pub fn history_retention_set(app: AppHandle, retention: Retention) -> Result<usize, String> {
    let dir = data_dir(&app)?;
    let retention = Retention { days: retention.days.min(36_500), ..retention };
    db::save_retention(&db::open(&dir), &retention);
    run(&app, &dir)
}

#[tauri::command]
pub fn history_clear_expired(app: AppHandle) -> Result<usize, String> {
    run(&app, &data_dir(&app)?)
}
