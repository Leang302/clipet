//! The missing-parameter popup (spec Part D2): lifecycle and commands.
//!
//! When Tab completes a parameterized snippet that still lacks a valid value,
//! the keyboard hook leaves the typed invocation exactly where it is and opens
//! a small Clipet window here. Nothing in the target app changes until the user
//! confirms; Esc or clicking away cancels and modifies nothing.
//!
//! Confirming returns focus through the same path a normal paste uses, then
//! the platform layer refuses to insert unless the same app is frontmost, the
//! same text field has focus (when the app exposes it) and the text before the
//! caret still ends with the invocation (when the field exposes its text). Only
//! then is the invocation erased and the rendered text inserted. Typed values
//! are never logged.

use super::{SnippetService, params, store};
use crate::clipboard::MonitorState;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

pub const WINDOW_LABEL: &str = "prompt";
pub const OPEN_EVENT: &str = "clipet://prompt-open";

/// macOS can briefly report focus loss while a window is coming forward; a
/// blur this soon after opening is not the user clicking away.
const BLUR_GRACE: Duration = Duration::from_millis(400);

/// Where the invocation was typed, captured when the popup opens.
#[cfg(target_os = "macos")]
pub(crate) type Target = crate::PasteTarget;
#[cfg(windows)]
pub(crate) type Target = super::windows::WinTarget;
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) type Target = ();

pub(crate) struct PendingPrompt {
    pub token: u64,
    pub snippet_id: String,
    pub delete_chars: usize,
    /// Checked against the field's text before replacing, where the OS can
    /// read it (macOS Accessibility; Windows has no general way).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub typed: String,
    pub prefill: HashMap<String, String>,
    pub target: Target,
    opened_at: Instant,
}

/// What the popup shows. Holds the template and the values the user already
/// typed inline (to prefill), never anything from the target app.
#[derive(Serialize)]
pub struct PromptInfo {
    pub token: u64,
    pub name: String,
    pub trigger: String,
    /// The template used on this OS (its variant, else the default).
    pub content: String,
    pub suffix: String,
    pub parameters: Vec<params::Parameter>,
    pub prefill: HashMap<String, String>,
}

impl SnippetService {
    /// Opens the popup for a parameterized snippet, replacing any earlier
    /// request. Returns false (and opens nothing) if it can't.
    pub(crate) fn open_prompt(
        &self,
        snippet_id: String,
        delete_chars: usize,
        typed: String,
        prefill: HashMap<String, String>,
        target: Target,
    ) -> bool {
        if self.snippet(&snippet_id).is_none() {
            return false;
        }
        let Some(win) = self.app.get_webview_window(WINDOW_LABEL) else {
            return false;
        };
        let token = self.prompt_seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.prompt.lock().unwrap() = Some(PendingPrompt {
            token,
            snippet_id,
            delete_chars,
            typed,
            prefill,
            target,
            opened_at: Instant::now(),
        });
        // A window of a hidden app won't come forward until the app is shown
        // (paste hides the whole app; see `return_focus_to_previous_app`).
        #[cfg(target_os = "macos")]
        let _ = self.app.show();
        let _ = win.center();
        let _ = win.show();
        crate::keep_on_screen(&win);
        let _ = win.set_focus();
        let _ = self.app.emit_to(WINDOW_LABEL, OPEN_EVENT, token);
        true
    }

    fn prompt_info(&self) -> Option<PromptInfo> {
        let guard = self.prompt.lock().unwrap();
        let pending = guard.as_ref()?;
        let snippet = self.snippet(&pending.snippet_id)?;
        let prefix = self.live.read().unwrap().config.prefix;
        Some(PromptInfo {
            token: pending.token,
            name: snippet.name.clone(),
            trigger: format!("{prefix}{}", snippet.keyword),
            content: store::resolve_template(&snippet.content, &snippet.variants, store::current_platform()),
            suffix: snippet.suffix.clone(),
            parameters: params::parse_parameters(&snippet.parameters),
            prefill: pending.prefill.clone(),
        })
    }

    /// Takes the request only if `token` is still current, so each request
    /// completes or cancels at most once (a repeated Enter can't insert twice).
    fn take_prompt(&self, token: u64) -> Option<PendingPrompt> {
        let mut guard = self.prompt.lock().unwrap();
        if guard.as_ref().is_some_and(|p| p.token == token) {
            guard.take()
        } else {
            None
        }
    }

    /// The user clicked somewhere else: cancel without pulling focus back.
    pub(crate) fn cancel_prompt_on_blur(&self) {
        let mut guard = self.prompt.lock().unwrap();
        if guard.as_ref().is_some_and(|p| p.opened_at.elapsed() >= BLUR_GRACE) {
            guard.take();
            if let Some(win) = self.app.get_webview_window(WINDOW_LABEL) {
                let _ = win.hide();
            }
            super::diag("prompt: cancelled (focus moved away)");
        }
    }
}

/// The text a confirmed request inserts: the OS's template plus its whitespace
/// setting, rendered with `values`. Errors keep the popup open for a fix.
fn render_request(service: &SnippetService, snippet_id: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let snippet = service
        .snippet(snippet_id)
        .ok_or_else(|| "that snippet no longer exists".to_string())?;
    let template = format!(
        "{}{}",
        store::resolve_template(&snippet.content, &snippet.variants, store::current_platform()),
        store::suffix_text(&snippet.suffix)
    );
    super::render_for_paste(&template, &snippet.parameters, Some(values))
}

type Service<'a> = tauri::State<'a, Arc<SnippetService>>;

#[tauri::command]
pub fn snippet_prompt_get(service: Service) -> Option<PromptInfo> {
    service.prompt_info()
}

/// Inserts the rendered snippet into the app it was typed in. Invalid values
/// return an error and keep the request open; anything that makes the
/// original field uncertain cancels the insertion instead of guessing.
#[tauri::command]
pub async fn snippet_prompt_submit(
    app: AppHandle,
    monitor: tauri::State<'_, Arc<MonitorState>>,
    token: u64,
    values: HashMap<String, String>,
) -> Result<(), String> {
    let service = super::service(&app);
    let snippet_id = service
        .prompt
        .lock()
        .unwrap()
        .as_ref()
        .filter(|p| p.token == token)
        .map(|p| p.snippet_id.clone())
        .ok_or_else(|| "This request is no longer active.".to_string())?;
    let text = render_request(&service, &snippet_id, &values)?;
    let pending = service
        .take_prompt(token)
        .ok_or_else(|| "This request is no longer active.".to_string())?;
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        let _ = win.hide();
    }
    let monitor = monitor.inner().clone();
    let worker_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || insert(&worker_app, &monitor, pending, &text))
        .await
        .map_err(|e| format!("insert worker failed: {e}"))?;
    match &result {
        Ok(()) => {
            super::diag("prompt: inserted");
            service.record_use(&snippet_id);
        }
        Err(reason) => {
            super::diag(&format!("prompt: not inserted: {reason}"));
            // Say so instead of failing silently; the popup shows the reason.
            if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
                #[cfg(target_os = "macos")]
                let _ = app.show();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    }
    result
}

/// Closes the popup. With `return_focus`, the app the shortcut was typed in
/// comes back to the front, untouched.
#[tauri::command]
pub async fn snippet_prompt_cancel(app: AppHandle, token: u64, return_focus: bool) -> Result<(), String> {
    let service = super::service(&app);
    let pending = service.take_prompt(token);
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        let _ = win.hide();
    }
    if let (true, Some(pending)) = (return_focus, pending) {
        super::diag("prompt: cancelled");
        let worker_app = app.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || {
            let _ = restore_focus(&worker_app, &pending.target);
        })
        .await;
    }
    Ok(())
}

/// Brings back the window the shortcut was typed in, without changing it.
fn restore_focus(app: &AppHandle, target: &Target) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        crate::return_focus_to_previous_app(app, target)
    }
    #[cfg(windows)]
    {
        let _ = app;
        super::windows::restore_focus(target)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (app, target);
        Ok(())
    }
}

fn insert(app: &AppHandle, monitor: &Arc<MonitorState>, pending: PendingPrompt, text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        super::macos::complete_prompt(app, monitor, pending, text)
    }
    #[cfg(windows)]
    {
        let _ = app;
        super::windows::complete_prompt(monitor, pending, text)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (app, monitor, pending, text);
        Err("Filling in values from a popup isn't available on this OS yet.".to_string())
    }
}
