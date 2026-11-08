//! Windows keyboard hook and text injection for Smart Snippets.
//!
//! A low-level keyboard hook (`WH_KEYBOARD_LL`) runs on its own thread and
//! sees each key before the focused app does. It keeps only the current word
//! (see `engine.rs`) and nothing is logged. When a word must be replaced, the
//! hook swallows the Space/Tab and hands the job to a worker; until the job
//! ends, further key events are held in order and replayed afterwards, so fast
//! typing can't land between the erase and the insert.
//!
//! The erase and the insert go out in one `SendInput` call, which Windows
//! never interleaves with other input. Unlike macOS, no main-thread hop is
//! needed: `SendInput` and the clipboard work from any thread.
//!
//! The Win32 calls are declared here directly (as `macos.rs` does for Carbon
//! and Accessibility) so this file needs no extra `windows` crate features.

use super::engine::{Decision, Engine, KeyInput, TriggerKey};
use super::{ListenerState, SnippetService};
use crate::clipboard::{self, MonitorState};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Stamped (as `dwExtraInfo`) on every event we send, so the hook never
/// treats our own erase / insert keystrokes as typing.
const SYNTHETIC_MARK: usize = 0x434C_4950; // "CLIP"
/// Stamped on held key events we replay: the hook tracks them as typing but
/// never starts a new replacement from one.
const REPLAY_MARK: usize = SYNTHETIC_MARK + 1;

/// Text that is short and single-line is typed directly, which leaves the
/// clipboard alone. Anything else is pasted, which apps insert faithfully
/// (newlines, tabs) as one edit.
const MAX_TYPED_CHARS: usize = 200;

const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(600);
/// Apps often read a pasted image after the paste event (to decode it or open
/// an upload sheet), so the previous clipboard comes back later.
const IMAGE_RESTORE_DELAY: Duration = Duration::from_millis(1500);

type Handle = *mut c_void;
type HookProc = Option<unsafe extern "system" fn(i32, usize, isize) -> isize>;

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_UP: u32 = 0x80;
const WM_LBUTTONDOWN: usize = 0x0201;
const WM_RBUTTONDOWN: usize = 0x0204;
const WM_MBUTTONDOWN: usize = 0x0207;
const WM_XBUTTONDOWN: usize = 0x020B;

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
/// `ToUnicodeEx` flag (Windows 10 1607+): translate without touching the
/// keyboard's dead-key state, so accents still compose in the focused app.
const TOUNICODE_KEEP_STATE: u32 = 0x4;

const WM_IME_CONTROL: u32 = 0x0283;
const IMC_GETCONVERSIONMODE: usize = 0x0001;
const IMC_GETOPENSTATUS: usize = 0x0005;
const IME_CMODE_NATIVE: usize = 0x0001;
const SMTO_ABORTIFHUNG: u32 = 0x0002;

const GMEM_MOVEABLE: u32 = 0x0002;

mod vk {
    pub const BACK: u32 = 0x08;
    pub const TAB: u32 = 0x09;
    pub const RETURN: u32 = 0x0D;
    pub const SHIFT: u32 = 0x10;
    pub const CONTROL: u32 = 0x11;
    pub const MENU: u32 = 0x12;
    pub const CAPITAL: u32 = 0x14;
    pub const ESCAPE: u32 = 0x1B;
    pub const SPACE: u32 = 0x20;
    pub const PRIOR: u32 = 0x21;
    pub const DOWN: u32 = 0x28;
    pub const INSERT: u32 = 0x2D;
    pub const DELETE: u32 = 0x2E;
    pub const V: u16 = 0x56;
    pub const LWIN: u32 = 0x5B;
    pub const RWIN: u32 = 0x5C;
    pub const NUMLOCK: u32 = 0x90;
    pub const SCROLL: u32 = 0x91;
    pub const LSHIFT: u32 = 0xA0;
    pub const RMENU: u32 = 0xA5;
    pub const PACKET: u32 = 0xE7;
}

#[repr(C)]
#[allow(dead_code)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

/// Only here so `Input` has the size Windows expects (`MOUSEINPUT` is the
/// largest member of the `INPUT` union).
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputUnion {
    ki: KeybdInput,
    _mi: MouseInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    u: InputUnion,
}

#[repr(C)]
#[allow(dead_code)]
struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    private: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(id: i32, proc_: HookProc, module: Handle, thread_id: u32) -> Handle;
    fn UnhookWindowsHookEx(hook: Handle) -> i32;
    fn CallNextHookEx(hook: Handle, code: i32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
    fn GetForegroundWindow() -> Handle;
    fn GetWindowThreadProcessId(hwnd: Handle, pid: *mut u32) -> u32;
    fn GetKeyboardLayout(thread_id: u32) -> Handle;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn GetKeyState(key: i32) -> i16;
    fn ToUnicodeEx(
        vk: u32,
        scan: u32,
        state: *const u8,
        buffer: *mut u16,
        buffer_len: i32,
        flags: u32,
        layout: Handle,
    ) -> i32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn SendMessageTimeoutW(
        hwnd: Handle,
        msg: u32,
        wparam: usize,
        lparam: isize,
        flags: u32,
        timeout_ms: u32,
        result: *mut usize,
    ) -> isize;
    fn OpenClipboard(owner: Handle) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn EnumClipboardFormats(format: u32) -> u32;
    fn GetClipboardData(format: u32) -> Handle;
    fn SetClipboardData(format: u32, data: Handle) -> Handle;
    fn GetClipboardSequenceNumber() -> u32;
    fn SetForegroundWindow(hwnd: Handle) -> i32;
    fn IsWindow(hwnd: Handle) -> i32;
    fn GetGUIThreadInfo(thread_id: u32, info: *mut GuiThreadInfo) -> i32;
}

#[repr(C)]
#[allow(dead_code)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[allow(dead_code)]
struct GuiThreadInfo {
    size: u32,
    flags: u32,
    active: Handle,
    focus: Handle,
    capture: Handle,
    menu_owner: Handle,
    move_size: Handle,
    caret: Handle,
    caret_rect: Rect,
}

/// The window a popup request came from: the foreground top-level window and
/// the control inside it with keyboard focus (`0` when unknown). Stored as
/// integers — window handles are plain identifiers, valid from any thread.
#[derive(Clone, Copy)]
pub struct WinTarget {
    hwnd: isize,
    focus: isize,
}

/// The control with keyboard focus in the thread that owns `hwnd`.
fn focused_control(hwnd: Handle) -> isize {
    unsafe {
        let thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let mut info: GuiThreadInfo = std::mem::zeroed();
        info.size = std::mem::size_of::<GuiThreadInfo>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return 0;
        }
        info.focus as isize
    }
}

fn capture_target() -> WinTarget {
    let hwnd = unsafe { GetForegroundWindow() };
    WinTarget {
        hwnd: hwnd as isize,
        focus: focused_control(hwnd),
    }
}

/// Makes the captured window the foreground window again and waits up to
/// 500 ms for Windows to confirm. Clipet just received the user's Enter/Esc,
/// so Windows lets it hand the foreground away.
pub(super) fn restore_focus(target: &WinTarget) -> Result<(), String> {
    let hwnd = target.hwnd as Handle;
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        return Err("The window you were typing in was closed, so nothing was inserted.".to_string());
    }
    unsafe { SetForegroundWindow(hwnd) };
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if unsafe { GetForegroundWindow() } == hwnd {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err("Couldn't return to the window you were typing in, so nothing was inserted.".to_string())
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalLock(mem: Handle) -> *mut c_void;
    fn GlobalUnlock(mem: Handle) -> i32;
    fn GlobalSize(mem: Handle) -> usize;
    fn GlobalFree(mem: Handle) -> Handle;
}

#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetDefaultIMEWnd(hwnd: Handle) -> Handle;
}

/// The hook procedures are plain functions, so they reach the service here.
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
/// Virtual key of the trigger the hook swallowed; its key-up is swallowed too
/// so the focused app never sees a release without a press.
static SWALLOW_UP: AtomicU32 = AtomicU32::new(0);

/// One key event as the hook saw it, held while a replacement is in flight.
#[derive(Clone, Copy)]
struct KeyEvent {
    vk: u32,
    scan: u32,
    flags: u32,
}

struct Job {
    decision: Decision,
    trigger: KeyEvent,
    verify_boundary: bool,
}

struct Shared {
    service: Arc<SnippetService>,
    engine: Mutex<Engine>,
    seen_generation: AtomicU64,
    last_target_pid: AtomicU32,
    /// `Some` while a replacement is in flight; holds key events to replay.
    held: Mutex<Option<Vec<KeyEvent>>>,
    jobs: mpsc::Sender<Job>,
    own_pid: u32,
}

pub fn start(service: Arc<SnippetService>, monitor: Arc<MonitorState>) {
    let (jobs, job_rx) = mpsc::channel::<Job>();
    let shared = Arc::new(Shared {
        service: service.clone(),
        engine: Mutex::new(Engine::new()),
        seen_generation: AtomicU64::new(service.generation()),
        last_target_pid: AtomicU32::new(0),
        held: Mutex::new(None),
        jobs,
        own_pid: std::process::id(),
    });
    if SHARED.set(shared.clone()).is_err() {
        return;
    }

    let worker_shared = shared.clone();
    std::thread::spawn(move || {
        for job in job_rx {
            run_job(&worker_shared, &monitor, job);
        }
    });

    std::thread::spawn(move || run_hooks(shared));
}

/// Installs the hooks and pumps this thread's messages, which is what makes
/// Windows call them. No permission is needed; installing only fails when the
/// system is out of resources, so retry a few seconds later.
fn run_hooks(shared: Arc<Shared>) {
    loop {
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
        if keyboard.is_null() {
            super::diag(&format!(
                "could not install the keyboard hook: {}",
                std::io::Error::last_os_error()
            ));
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        // Without the mouse hook a click can't reset the word, which only
        // makes a mid-text expansion possible; keyboard expansion still works.
        let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) };
        if mouse.is_null() {
            super::diag("could not install the mouse hook; clicks won't reset the word");
        }
        shared.service.set_listener(ListenerState::Active);
        let mut msg = Msg {
            hwnd: std::ptr::null_mut(),
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            pt_x: 0,
            pt_y: 0,
            private: 0,
        };
        // Runs for the life of the app.
        while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {}
        unsafe {
            UnhookWindowsHookEx(keyboard);
            if !mouse.is_null() {
                UnhookWindowsHookEx(mouse);
            }
        }
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION {
        if let Some(shared) = SHARED.get() {
            let info = unsafe { &*(lparam as *const KbdLlHookStruct) };
            if on_key(shared, info) {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION
        && matches!(wparam, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN)
    {
        // A click can move the caret anywhere.
        if let Some(shared) = SHARED.get() {
            shared.engine.lock().unwrap().reset();
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Returns true to swallow the event.
fn on_key(shared: &Shared, info: &KbdLlHookStruct) -> bool {
    if info.extra_info == SYNTHETIC_MARK {
        return false;
    }
    let replay = info.extra_info == REPLAY_MARK;
    let up = info.flags & LLKHF_UP != 0;
    let event = KeyEvent {
        vk: info.vk_code,
        scan: info.scan_code,
        flags: info.flags,
    };

    if !replay && up && info.vk_code != 0 {
        if SWALLOW_UP
            .compare_exchange(info.vk_code, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }

    // The hook runs serially on one thread, so checking and later setting
    // `held` below can't race with another key event; only the worker clears
    // it.
    let mut held = shared.held.lock().unwrap();
    if !replay {
        if let Some(queue) = held.as_mut() {
            queue.push(event);
            return true;
        }
    }
    if up {
        return false;
    }

    let mut engine = shared.engine.lock().unwrap();
    let generation = shared.service.generation();
    if shared.seen_generation.swap(generation, Ordering::SeqCst) != generation {
        engine.reset();
    }
    let (target_pid, layout) = foreground();
    let previous_pid = shared.last_target_pid.swap(target_pid, Ordering::SeqCst);
    if previous_pid != target_pid {
        engine.reset();
    }
    let input_text = classify(&event, layout);
    let is_trigger = matches!(input_text, Some(Classified::Trigger(_)));
    if target_pid == shared.own_pid || !shared.service.is_expanding() {
        // Clipet's own windows (e.g. the snippet editor) and paused state
        // never expand, and no word is tracked meanwhile.
        if is_trigger {
            super::diag(&format!(
                "trigger ignored: target pid {target_pid} (Clipet is {}), expanding={}",
                shared.own_pid,
                shared.service.is_expanding()
            ));
        }
        engine.reset();
        return false;
    }

    let Some(input_text) = input_text else {
        return false;
    };
    let word_chars = engine.word_chars();
    let verify_boundary = engine.word_starts_line();
    let decision = shared.service.with_live(|config, index| {
        engine.feed(input_text.as_input(), config, index)
    });
    if is_trigger {
        // Only the decision and the word's length are logged, never its text.
        super::diag(&format!(
            "trigger: word of {word_chars} chars -> {}{} (target pid {target_pid}{})",
            match &decision {
                Decision::Pass => "pass",
                Decision::Expand { .. } => "expand",
                Decision::ExpandImage { .. } => "expand image",
                Decision::Escape { .. } => "escape",
                Decision::Prompt { .. } => "prompt",
            },
            if replay { ", replayed" } else { "" },
            if previous_pid != target_pid { ", target changed" } else { "" },
        ));
    }
    if replay || !decision.swallows_trigger() {
        return false;
    }

    *held = Some(Vec::new());
    let job = Job {
        decision,
        trigger: event,
        verify_boundary,
    };
    if shared.jobs.send(job).is_err() {
        *held = None;
        return false;
    }
    SWALLOW_UP.store(event.vk, Ordering::SeqCst);
    true
}

/// The process that owns the foreground window and its keyboard layout.
fn foreground() -> (u32, Handle) {
    unsafe {
        let window = GetForegroundWindow();
        let mut pid = 0;
        let thread = GetWindowThreadProcessId(window, &mut pid);
        (pid, GetKeyboardLayout(thread))
    }
}

fn key_down(key: u32) -> bool {
    (unsafe { GetAsyncKeyState(key as i32) } as u16) & 0x8000 != 0
}

fn key_toggled(key: u32) -> bool {
    (unsafe { GetKeyState(key as i32) }) & 1 != 0
}

/// Owned form of `KeyInput` (the typed text is translated from the key).
enum Classified {
    Text(String),
    Backspace,
    Reset,
    Trigger(TriggerKey),
}

impl Classified {
    fn as_input(&self) -> KeyInput<'_> {
        match self {
            Classified::Text(text) => KeyInput::Text(text),
            Classified::Backspace => KeyInput::Backspace,
            Classified::Reset => KeyInput::Reset,
            Classified::Trigger(key) => KeyInput::Trigger(*key),
        }
    }
}

fn classify(event: &KeyEvent, layout: Handle) -> Option<Classified> {
    let code = event.vk;
    // Modifiers and lock keys type nothing on their own.
    if matches!(
        code,
        vk::SHIFT | vk::CONTROL | vk::MENU | vk::CAPITAL | vk::LWIN | vk::RWIN | vk::NUMLOCK | vk::SCROLL
    ) || (vk::LSHIFT..=vk::RMENU).contains(&code)
    {
        return None;
    }
    if code == vk::PACKET {
        // Text sent by another program as a Unicode "key"; the scan code
        // holds the UTF-16 unit.
        return Some(Classified::Text(String::from_utf16_lossy(&[event.scan as u16])));
    }
    let ctrl = key_down(vk::CONTROL);
    let alt = key_down(vk::MENU);
    let shift = key_down(vk::SHIFT);
    if key_down(vk::LWIN) || key_down(vk::RWIN) || ctrl != alt {
        // Shortcuts (select all, paste, undo, Alt menus…) change text
        // unpredictably. Ctrl+Alt together is AltGr, which types characters.
        return Some(Classified::Reset);
    }
    let alt_gr = ctrl && alt;
    match code {
        vk::SPACE if !alt_gr => Some(Classified::Trigger(TriggerKey::Space)),
        vk::TAB if !alt_gr && !shift => Some(Classified::Trigger(TriggerKey::Tab)),
        vk::BACK if !alt_gr => Some(Classified::Backspace),
        vk::SPACE | vk::TAB | vk::BACK => Some(Classified::Reset),
        vk::RETURN
        | vk::ESCAPE
        | vk::INSERT
        | vk::DELETE
        | vk::PRIOR..=vk::DOWN => Some(Classified::Reset),
        _ => {
            let text = typed_text(event, layout, shift, alt_gr);
            // Empty for dead keys and function keys: nothing was typed yet.
            (!text.is_empty()).then_some(Classified::Text(text))
        }
    }
}

fn typed_text(event: &KeyEvent, layout: Handle, shift: bool, alt_gr: bool) -> String {
    let mut state = [0u8; 256];
    if shift {
        state[vk::SHIFT as usize] = 0x80;
    }
    if alt_gr {
        state[vk::CONTROL as usize] = 0x80;
        state[vk::MENU as usize] = 0x80;
    }
    if key_toggled(vk::CAPITAL) {
        state[vk::CAPITAL as usize] = 0x01;
    }
    if key_toggled(vk::NUMLOCK) {
        state[vk::NUMLOCK as usize] = 0x01;
    }
    let mut buffer = [0u16; 8];
    let len = unsafe {
        ToUnicodeEx(
            event.vk,
            event.scan,
            state.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            TOUNICODE_KEEP_STATE,
            layout,
        )
    };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..(len as usize).min(buffer.len())])
}

/// What replaces the typed shortcut.
enum Insert {
    Text(String),
    /// PNG bytes, already checked to decode.
    Image(Vec<u8>),
}

enum Outcome {
    /// Replaced; `restore` holds the clipboard to put back after a paste.
    Done { restore: Option<ClipboardRestore> },
    /// Not safe to replace right now; the trigger key is delivered unchanged.
    Skipped(&'static str),
}

fn run_job(shared: &Shared, monitor: &Arc<MonitorState>, job: Job) {
    let Job {
        decision,
        trigger,
        verify_boundary,
    } = job;
    let decision = match decision {
        Decision::Prompt {
            snippet_id,
            delete_chars,
            prefill,
            typed,
        } => return open_prompt(shared, snippet_id, delete_chars, typed, prefill, trigger),
        other => other,
    };
    let (delete_chars, insert, snippet_id) = match decision {
        Decision::Expand {
            snippet_id,
            delete_chars,
            insert,
        } => (delete_chars, Some(Insert::Text(insert)), Some(snippet_id)),
        Decision::ExpandImage {
            snippet_id,
            delete_chars,
        } => {
            // A missing or damaged file skips the expansion and leaves the
            // typed shortcut as it is.
            let png = shared
                .service
                .snippet(&snippet_id)
                .and_then(|snippet| shared.service.image_png(&snippet));
            match png {
                Some(Ok(png)) => (delete_chars, Some(Insert::Image(png)), Some(snippet_id)),
                Some(Err(e)) => {
                    super::diag(&format!("image snippet unavailable: {e}"));
                    (delete_chars, None, None)
                }
                None => (delete_chars, None, None),
            }
        }
        Decision::Escape {
            delete_chars,
            insert,
        } => (delete_chars, Some(Insert::Text(insert)), None),
        Decision::Pass => (0, Some(Insert::Text(String::new())), None),
        Decision::Prompt { .. } => unreachable!("handled above"),
    };
    let Some(insert) = insert else {
        shared.engine.lock().unwrap().reset();
        replay_trigger(trigger);
        release_held(shared);
        return;
    };
    let is_image = matches!(insert, Insert::Image(_));

    let started = std::time::Instant::now();
    let outcome = replace(monitor, verify_boundary, delete_chars, &insert);
    let elapsed = started.elapsed().as_millis();
    match &outcome {
        Outcome::Done { restore } => super::diag(&format!(
            "replaced {delete_chars} chars in {elapsed}ms via {}",
            if is_image { "image paste" } else if restore.is_some() { "paste" } else { "typing" }
        )),
        Outcome::Skipped(reason) => super::diag(&format!("skipped after {elapsed}ms: {reason}")),
    }

    match outcome {
        Outcome::Done { restore } => {
            if let Some(id) = snippet_id {
                shared.service.record_use(&id);
            }
            release_held(shared);
            if let Some(restore) = restore {
                std::thread::sleep(if is_image { IMAGE_RESTORE_DELAY } else { CLIPBOARD_RESTORE_DELAY });
                restore.apply();
            }
        }
        Outcome::Skipped(_) => {
            // The engine assumed the replacement happened; it didn't.
            shared.engine.lock().unwrap().reset();
            replay_trigger(trigger);
            release_held(shared);
        }
    }
}

/// A parameterized snippet still needs values: remember the window and the
/// control it was typed in and open the popup. The invocation stays untouched
/// until the user confirms (`complete_prompt`); the Tab is consumed.
fn open_prompt(
    shared: &Shared,
    snippet_id: String,
    delete_chars: usize,
    typed: String,
    prefill: std::collections::HashMap<String, String>,
    trigger: KeyEvent,
) {
    let target = capture_target();
    super::diag(&format!(
        "prompt: opening for {delete_chars} typed chars (focused control {})",
        if target.focus != 0 { "known" } else { "unknown" }
    ));
    shared.engine.lock().unwrap().reset();
    // Keys typed right after Tab still belong to that window; deliver them
    // before the popup takes focus.
    release_held(shared);
    if !shared.service.open_prompt(snippet_id, delete_chars, typed, prefill, target) {
        super::diag("prompt: window unavailable; delivering Tab");
        replay_trigger(trigger);
    }
}

/// Finishes a confirmed popup request: brings back the window the shortcut
/// was typed in and, only if the same control still has focus, replaces the
/// invocation with `text`. Windows offers no general way to read the text
/// before the caret, so the window and focused control are what's checked.
pub(super) fn complete_prompt(
    monitor: &Arc<MonitorState>,
    pending: super::prompt::PendingPrompt,
    text: &str,
) -> Result<(), String> {
    let shared = SHARED
        .get()
        .ok_or_else(|| "the keyboard listener isn't running; nothing was inserted".to_string())?;
    restore_focus(&pending.target)?;
    if pending.target.focus != 0 && focused_control(pending.target.hwnd as Handle) != pending.target.focus {
        return Err("The text field you were typing in no longer has focus, so nothing was inserted.".to_string());
    }
    *shared.held.lock().unwrap() = Some(Vec::new());
    let outcome = replace(monitor, false, pending.delete_chars, &Insert::Text(text.to_string()));
    // Whatever happened, the engine no longer knows the text at the caret.
    shared.engine.lock().unwrap().reset();
    release_held(shared);
    match outcome {
        Outcome::Done { restore } => {
            if let Some(restore) = restore {
                std::thread::sleep(CLIPBOARD_RESTORE_DELAY);
                restore.apply();
            }
            Ok(())
        }
        Outcome::Skipped(reason) => Err(format!("Nothing was inserted: {reason}.")),
    }
}

/// Replays held key events in order. New events that arrive while replaying
/// keep queueing behind them; the queue closes only once it is empty.
fn release_held(shared: &Shared) {
    loop {
        let batch = {
            let mut held = shared.held.lock().unwrap();
            match held.as_mut() {
                Some(queue) if !queue.is_empty() => std::mem::take(queue),
                _ => {
                    *held = None;
                    return;
                }
            }
        };
        let inputs: Vec<Input> = batch.iter().map(replay_input).collect();
        send(&inputs);
    }
}

/// Delivers the swallowed trigger as a full press. Its physical key-up is
/// still swallowed by the hook (see `SWALLOW_UP`).
fn replay_trigger(trigger: KeyEvent) {
    let up = KeyEvent {
        flags: trigger.flags | LLKHF_UP,
        ..trigger
    };
    send(&[replay_input(&trigger), replay_input(&up)]);
}

fn replay_input(event: &KeyEvent) -> Input {
    let up = if event.flags & LLKHF_UP != 0 { KEYEVENTF_KEYUP } else { 0 };
    if event.vk == vk::PACKET {
        return key_input(0, event.scan as u16, KEYEVENTF_UNICODE | up, REPLAY_MARK);
    }
    let extended = if event.flags & LLKHF_EXTENDED != 0 { KEYEVENTF_EXTENDEDKEY } else { 0 };
    key_input(event.vk as u16, event.scan as u16, extended | up, REPLAY_MARK)
}

fn key_input(vk: u16, scan: u16, flags: u32, extra_info: usize) -> Input {
    Input {
        kind: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KeybdInput {
                vk,
                scan,
                flags,
                time: 0,
                extra_info,
            },
        },
    }
}

fn send(inputs: &[Input]) -> u32 {
    if inputs.is_empty() {
        return 0;
    }
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<Input>() as i32,
        )
    }
}

fn replace(
    monitor: &Arc<MonitorState>,
    verify_boundary: bool,
    delete_chars: usize,
    insert: &Insert,
) -> Outcome {
    // IMEs (Korean, Japanese, Chinese…) compose text the hook can't see.
    // Never rewrite text while one is composing.
    if input_method_active() {
        return Outcome::Skipped("an input method is active");
    }
    if verify_boundary {
        // Windows has no cheap way to read the character before the caret in
        // every app. Like macOS when a field hides its text, assume the
        // shortcut starts a word rather than refuse to expand.
        super::diag("note: text before the shortcut is unknown, assuming word start");
    }

    let typed = match insert {
        Insert::Text(text)
            if text.chars().count() <= MAX_TYPED_CHARS && !text.contains(['\n', '\r', '\t']) =>
        {
            Some(text)
        }
        _ => None,
    };
    // Stage the clipboard before erasing anything, so a failure leaves the
    // user's text exactly as typed.
    let restore = if typed.is_none() {
        match stage_clipboard(monitor, insert) {
            Ok(restore) => Some(restore),
            Err(e) => {
                super::diag(&format!("clipboard staging failed: {e}"));
                return Outcome::Skipped("the clipboard could not be staged");
            }
        }
    } else {
        None
    };

    const VK_BACK: u16 = vk::BACK as u16;
    const VK_CONTROL: u16 = vk::CONTROL as u16;
    let mut inputs = Vec::new();
    for _ in 0..delete_chars {
        inputs.push(key_input(VK_BACK, 0, 0, SYNTHETIC_MARK));
        inputs.push(key_input(VK_BACK, 0, KEYEVENTF_KEYUP, SYNTHETIC_MARK));
    }
    match typed {
        Some(text) => {
            for unit in text.encode_utf16() {
                inputs.push(key_input(0, unit, KEYEVENTF_UNICODE, SYNTHETIC_MARK));
                inputs.push(key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP, SYNTHETIC_MARK));
            }
        }
        None => {
            inputs.push(key_input(VK_CONTROL, 0, 0, SYNTHETIC_MARK));
            inputs.push(key_input(vk::V, 0, 0, SYNTHETIC_MARK));
            inputs.push(key_input(vk::V, 0, KEYEVENTF_KEYUP, SYNTHETIC_MARK));
            inputs.push(key_input(VK_CONTROL, 0, KEYEVENTF_KEYUP, SYNTHETIC_MARK));
        }
    }
    // One call: Windows inserts the whole sequence without interleaving any
    // other input. Zero means it was blocked (e.g. by UIPI) and nothing ran.
    if !inputs.is_empty() && send(&inputs) == 0 {
        if let Some(restore) = restore {
            restore.apply();
        }
        return Outcome::Skipped(
            "Windows blocked the keystrokes (is the app running as administrator?)",
        );
    }
    Outcome::Done { restore }
}

fn input_method_active() -> bool {
    unsafe {
        let ime = ImmGetDefaultIMEWnd(GetForegroundWindow());
        if ime.is_null() {
            return false;
        }
        let query = |command: usize| {
            let mut result = 0usize;
            let ok = SendMessageTimeoutW(
                ime,
                WM_IME_CONTROL,
                command,
                0,
                SMTO_ABORTIFHUNG,
                100,
                &mut result,
            );
            (ok != 0).then_some(result)
        };
        // Open and in native mode (kana, hangul, pinyin…), not direct ASCII.
        query(IMC_GETOPENSTATUS).is_some_and(|open| open != 0)
            && query(IMC_GETCONVERSIONMODE).is_some_and(|mode| mode & IME_CMODE_NATIVE != 0)
    }
}

/// Opens the clipboard, retrying briefly while another app holds it.
fn open_clipboard() -> Result<(), String> {
    for _ in 0..10 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("the clipboard is in use by another app".to_string())
}

/// Formats whose data is a plain memory block that can be copied and put
/// back. GDI handles (bitmaps, metafiles, palettes) and private formats are
/// skipped; Windows re-creates the bitmap form from the DIB one.
fn restorable(format: u32) -> bool {
    const CF_BITMAP: u32 = 2;
    const CF_METAFILEPICT: u32 = 3;
    const CF_PALETTE: u32 = 9;
    const CF_ENHMETAFILE: u32 = 14;
    !matches!(format, CF_BITMAP | CF_METAFILEPICT | CF_PALETTE | CF_ENHMETAFILE)
        && !(0x0080..=0x008F).contains(&format)
        && !(0x0200..=0x03FF).contains(&format)
}

/// Everything that was on the clipboard before a paste-based expansion, put
/// back afterwards unless something else changed the clipboard meanwhile.
struct ClipboardRestore {
    formats: Vec<(u32, Vec<u8>)>,
    sequence: u32,
}

fn snapshot_clipboard() -> Result<Vec<(u32, Vec<u8>)>, String> {
    open_clipboard()?;
    let mut formats = Vec::new();
    let mut format = 0;
    unsafe {
        loop {
            format = EnumClipboardFormats(format);
            if format == 0 {
                break;
            }
            if !restorable(format) {
                continue;
            }
            let data = GetClipboardData(format);
            if data.is_null() {
                continue;
            }
            let size = GlobalSize(data);
            let ptr = GlobalLock(data);
            if ptr.is_null() {
                continue;
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            GlobalUnlock(data);
            formats.push((format, bytes));
        }
        CloseClipboard();
    }
    Ok(formats)
}

fn stage_clipboard(monitor: &Arc<MonitorState>, insert: &Insert) -> Result<ClipboardRestore, String> {
    let formats = snapshot_clipboard()?;
    let mut board = arboard::Clipboard::new().map_err(|e| e.to_string())?;

    // Mark the snippet as our own write so the history monitor skips it.
    let mut self_write = monitor
        .last_self_write_hash
        .lock()
        .map_err(|_| "clipboard monitor state is unavailable".to_string())?;
    match insert {
        Insert::Text(text) => {
            board
                .set_text(text.clone())
                .map_err(|e| format!("the clipboard refused the snippet text: {e}"))?;
            *self_write = Some(clipboard::hash_bytes(text.as_bytes()));
        }
        Insert::Image(png) => {
            let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)
                .map_err(|e| format!("the snippet's image is damaged: {e}"))?
                .to_rgba8();
            board
                .set_image(arboard::ImageData {
                    width: rgba.width() as usize,
                    height: rgba.height() as usize,
                    bytes: std::borrow::Cow::Owned(rgba.into_raw()),
                })
                .map_err(|e| format!("the clipboard refused the snippet image: {e}"))?;
            // Hash the image the way the history monitor reads it back, so the
            // snippet isn't recorded as a new copy.
            let hash = board
                .get_image()
                .map(|img| clipboard::hash_bytes(&img.bytes))
                .map_err(|e| format!("the clipboard image can't be read back: {e}"))?;
            *self_write = Some(hash);
        }
    }
    Ok(ClipboardRestore {
        formats,
        sequence: unsafe { GetClipboardSequenceNumber() },
    })
}

impl ClipboardRestore {
    fn apply(self) {
        if open_clipboard().is_err() {
            return;
        }
        unsafe {
            // The user (or another app) copied something since: keep that.
            if GetClipboardSequenceNumber() != self.sequence {
                CloseClipboard();
                return;
            }
            EmptyClipboard();
            for (format, bytes) in &self.formats {
                let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
                if memory.is_null() {
                    continue;
                }
                let ptr = GlobalLock(memory);
                if ptr.is_null() {
                    GlobalFree(memory);
                    continue;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                GlobalUnlock(memory);
                // On success the clipboard owns the memory.
                if SetClipboardData(*format, memory).is_null() {
                    GlobalFree(memory);
                }
            }
            CloseClipboard();
        }
    }
}
