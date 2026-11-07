//! macOS keyboard hook and text injection for Smart Snippets.
//!
//! A session-level `CGEventTap` runs on its own thread and sees each key-down
//! before the focused app does. It keeps only the current word (see
//! `engine.rs`) and nothing is logged. When a word must be replaced, the tap
//! swallows the Space/Tab and hands the job to a worker; until the job ends,
//! further key presses are held in order and replayed afterwards, so fast
//! typing can't land between the erase and the insert.
//!
//! All injection runs on the main thread. enigo's layout-aware key lookup
//! (needed for Cmd+V) uses Text Input Source APIs that abort the process when
//! called from any other thread — the same crash fixed for normal paste.

use super::engine::{Decision, Engine, KeyInput, TriggerKey};
use super::{ListenerState, SnippetService};
use crate::clipboard::{self, MonitorState};
use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use foreign_types::ForeignType;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Stamped on every event we post, so the tap never treats our own erase /
/// insert keystrokes as typing (no recursive or duplicate expansion).
const SYNTHETIC_MARK: i64 = 0x434C_4950_4554; // "CLIPET"
/// Stamped on held key presses we replay: the tap tracks them as typing but
/// never starts a new replacement from one.
const REPLAY_MARK: i64 = SYNTHETIC_MARK + 1;

/// Text that is short and single-line is typed directly, which leaves the
/// clipboard alone. Anything else is pasted, which apps insert faithfully
/// (newlines, tabs) as one edit.
const MAX_TYPED_CHARS: usize = 200;

const MAIN_THREAD_TIMEOUT: Duration = Duration::from_millis(1500);
const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(600);
/// Apps often read a pasted image after the paste event (to decode it or open
/// an upload sheet), so the previous clipboard comes back later.
const IMAGE_RESTORE_DELAY: Duration = Duration::from_millis(1500);

mod keycode {
    pub const RETURN: i64 = 36;
    pub const TAB: i64 = 48;
    pub const SPACE: i64 = 49;
    pub const DELETE: i64 = 51;
    pub const ESCAPE: i64 = 53;
    pub const KEYPAD_ENTER: i64 = 76;
    pub const HOME: i64 = 115;
    pub const PAGE_UP: i64 = 116;
    pub const FORWARD_DELETE: i64 = 117;
    pub const END: i64 = 119;
    pub const PAGE_DOWN: i64 = 121;
    pub const ARROW_FIRST: i64 = 123;
    pub const ARROW_LAST: i64 = 126;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventKeyboardGetUnicodeString(
        event: *mut c_void,
        max_len: std::ffi::c_ulong,
        actual_len: *mut std::ffi::c_ulong,
        buffer: *mut u16,
    );
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
    fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
    static kTISPropertyInputSourceType: *const c_void;
    static kTISTypeKeyboardInputMode: *const c_void;
}

/// The tap's mach port, so the callback can re-enable the tap when macOS
/// disables it (after a slow callback or secure-input transitions).
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// A CGEvent moved to the worker or held in the replay queue. CGEvent is a
/// reference-counted CoreFoundation object that is safe to use from any thread.
struct SendEvent(CGEvent);
unsafe impl Send for SendEvent {}

struct Job {
    decision: Decision,
    trigger: SendEvent,
    target_pid: i64,
    /// Nothing typed before the word was seen; ask the field what precedes it.
    verify_boundary: bool,
}

struct Shared {
    service: Arc<SnippetService>,
    engine: Mutex<Engine>,
    seen_generation: AtomicU64,
    last_target_pid: AtomicI64,
    /// `Some` while a replacement is in flight; holds key presses to replay.
    held: Mutex<Option<Vec<SendEvent>>>,
    jobs: mpsc::Sender<Job>,
    own_pid: i64,
}

/// The app holding macOS secure input, if any. Secure input is session-wide:
/// while one app has it (normally while its password field is focused, but
/// apps can leave it stuck on), event taps receive no keystrokes at all.
fn secure_input_holder() -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> core_foundation::dictionary::CFDictionaryRef;
    }
    if unsafe { IsSecureEventInputEnabled() } == 0 {
        return None;
    }
    let pid = unsafe {
        let raw = CGSessionCopyCurrentDictionary();
        if raw.is_null() {
            None
        } else {
            let dict: CFDictionary<CFString, core_foundation::base::CFType> = CFDictionary::wrap_under_create_rule(raw);
            dict.find(CFString::from_static_string("kCGSSessionSecureInputPID"))
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i32())
        }
    };
    let name = pid
        .and_then(objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier)
        .and_then(|app| app.localizedName())
        .map(|n| n.to_string());
    Some(name.unwrap_or_else(|| "another app".to_string()))
}

/// Checks secure input every 2 s, so the warning appears (and clears) on its
/// own even though no keys reach the tap while it's on.
fn watch_secure_input(service: Arc<SnippetService>) {
    std::thread::spawn(move || loop {
        let holder = secure_input_holder().filter(|name| name != "Clipet");
        service.set_blocked_by(holder);
        std::thread::sleep(Duration::from_secs(2));
    });
}

pub fn start(service: Arc<SnippetService>, monitor: Arc<MonitorState>) {
    watch_secure_input(service.clone());
    let (jobs, job_rx) = mpsc::channel::<Job>();
    let shared = Arc::new(Shared {
        service: service.clone(),
        engine: Mutex::new(Engine::new()),
        seen_generation: AtomicU64::new(service.generation()),
        last_target_pid: AtomicI64::new(0),
        held: Mutex::new(None),
        jobs,
        own_pid: i64::from(std::process::id()),
    });

    let worker_shared = shared.clone();
    std::thread::spawn(move || {
        for job in job_rx {
            run_job(&worker_shared, &monitor, job);
        }
    });

    std::thread::spawn(move || run_tap(shared));
}

/// Installing a filtering tap requires Accessibility trust. Until it's
/// granted, retry every few seconds so expansion starts as soon as the user
/// enables Clipet in System Settings — no restart needed.
fn run_tap(shared: Arc<Shared>) {
    loop {
        if !crate::accessibility_trusted() {
            shared.service.set_listener(ListenerState::NeedsPermission);
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        let callback_shared = shared.clone();
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![
                CGEventType::KeyDown,
                CGEventType::LeftMouseDown,
                CGEventType::RightMouseDown,
                CGEventType::OtherMouseDown,
            ],
            move |_proxy: CGEventTapProxy, event_type, event| {
                on_event(&callback_shared, event_type, event)
            },
        );
        let Ok(tap) = tap else {
            shared.service.set_listener(ListenerState::NeedsPermission);
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let Ok(source) = tap.mach_port().create_runloop_source(0) else {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        TAP_PORT.store(
            tap.mach_port().as_concrete_TypeRef() as *mut c_void,
            Ordering::SeqCst,
        );
        CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
        tap.enable();
        shared.service.set_listener(ListenerState::Active);
        // Runs for the life of the app.
        CFRunLoop::run_current();
        TAP_PORT.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}

fn on_event(shared: &Shared, event_type: CGEventType, event: &CGEvent) -> CallbackResult {
    match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            super::diag(&format!("macOS disabled the listener ({event_type:?}); re-enabling"));
            let port = TAP_PORT.load(Ordering::SeqCst);
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, true) };
            }
            shared.engine.lock().unwrap().reset();
            CallbackResult::Keep
        }
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            // A click can move the caret anywhere.
            shared.engine.lock().unwrap().reset();
            CallbackResult::Keep
        }
        CGEventType::KeyDown => on_key_down(shared, event),
        _ => CallbackResult::Keep,
    }
}

fn on_key_down(shared: &Shared, event: &CGEvent) -> CallbackResult {
    let mark = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);
    if mark == SYNTHETIC_MARK {
        return CallbackResult::Keep;
    }
    let replay = mark == REPLAY_MARK;

    // The tap callback is serial, so checking and later setting `held` below
    // can't race with another key press; only the worker clears it.
    let mut held = shared.held.lock().unwrap();
    if !replay {
        if let Some(queue) = held.as_mut() {
            queue.push(SendEvent(event.clone()));
            return CallbackResult::Drop;
        }
    }

    let mut engine = shared.engine.lock().unwrap();
    let generation = shared.service.generation();
    if shared.seen_generation.swap(generation, Ordering::SeqCst) != generation {
        engine.reset();
    }
    let target_pid = event.get_integer_value_field(EventField::EVENT_TARGET_UNIX_PROCESS_ID);
    let previous_pid = shared.last_target_pid.swap(target_pid, Ordering::SeqCst);
    if previous_pid != target_pid {
        engine.reset();
    }
    let input_text = classify(event);
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
        return CallbackResult::Keep;
    }

    let Some(input_text) = input_text else {
        return CallbackResult::Keep;
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
            },
            if replay { ", replayed" } else { "" },
            if previous_pid != target_pid { ", target changed" } else { "" },
        ));
    }
    if replay || !decision.swallows_trigger() {
        return CallbackResult::Keep;
    }

    *held = Some(Vec::new());
    let job = Job {
        decision,
        trigger: SendEvent(event.clone()),
        target_pid,
        verify_boundary,
    };
    if shared.jobs.send(job).is_err() {
        *held = None;
        return CallbackResult::Keep;
    }
    CallbackResult::Drop
}

/// Owned form of `KeyInput` (the typed text is read out of the event).
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

fn classify(event: &CGEvent) -> Option<Classified> {
    let flags = event.get_flags();
    if flags.intersects(CGEventFlags::CGEventFlagCommand | CGEventFlags::CGEventFlagControl) {
        // Shortcuts (select all, paste, undo…) change text unpredictably.
        return Some(Classified::Reset);
    }
    let modified = flags.intersects(CGEventFlags::CGEventFlagAlternate);
    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
    match code {
        keycode::SPACE if !modified => Some(Classified::Trigger(TriggerKey::Space)),
        keycode::TAB
            if !modified && !flags.intersects(CGEventFlags::CGEventFlagShift) =>
        {
            Some(Classified::Trigger(TriggerKey::Tab))
        }
        keycode::DELETE if !modified => Some(Classified::Backspace),
        keycode::SPACE | keycode::TAB | keycode::DELETE => Some(Classified::Reset),
        keycode::RETURN
        | keycode::KEYPAD_ENTER
        | keycode::ESCAPE
        | keycode::FORWARD_DELETE
        | keycode::HOME
        | keycode::END
        | keycode::PAGE_UP
        | keycode::PAGE_DOWN
        | keycode::ARROW_FIRST..=keycode::ARROW_LAST => Some(Classified::Reset),
        _ => {
            let text = typed_text(event);
            // Empty for dead keys and function keys: nothing was typed yet.
            (!text.is_empty()).then_some(Classified::Text(text))
        }
    }
}

fn typed_text(event: &CGEvent) -> String {
    let mut buffer = [0u16; 8];
    let mut len: std::ffi::c_ulong = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(
            event.as_ptr() as *mut c_void,
            buffer.len() as std::ffi::c_ulong,
            &mut len,
            buffer.as_mut_ptr(),
        );
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
        target_pid,
        verify_boundary,
    } = job;
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
            // Read off the main thread; a missing or damaged file skips the
            // expansion and leaves the typed shortcut as it is.
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
    };
    let Some(insert) = insert else {
        shared.engine.lock().unwrap().reset();
        replay(&trigger.0);
        release_held(shared);
        return;
    };
    let is_image = matches!(insert, Insert::Image(_));

    let started = std::time::Instant::now();
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sent, received) = mpsc::sync_channel(1);
    let main_cancelled = cancelled.clone();
    let main_monitor = monitor.clone();
    let dispatched = shared.service.app.run_on_main_thread(move || {
        // A late run after the worker gave up must not edit text twice.
        if main_cancelled.load(Ordering::SeqCst) {
            return;
        }
        let _ = sent.send(replace_on_main(
            &main_monitor,
            target_pid,
            verify_boundary,
            delete_chars,
            &insert,
        ));
    });
    let outcome = if dispatched.is_ok() {
        received.recv_timeout(MAIN_THREAD_TIMEOUT).ok()
    } else {
        None
    };
    cancelled.store(true, Ordering::SeqCst);

    let elapsed = started.elapsed().as_millis();
    match &outcome {
        Some(Outcome::Done { restore }) => super::diag(&format!(
            "replaced {delete_chars} chars in {elapsed}ms via {}",
            if is_image { "image paste" } else if restore.is_some() { "paste" } else { "typing" }
        )),
        Some(Outcome::Skipped(reason)) => {
            super::diag(&format!("skipped after {elapsed}ms: {reason}"))
        }
        None if dispatched.is_err() => super::diag("skipped: main thread unavailable"),
        None => super::diag(&format!("skipped: main thread busy for {elapsed}ms")),
    }

    match outcome {
        Some(Outcome::Done { restore }) => {
            if let Some(id) = snippet_id {
                shared.service.record_use(&id);
            }
            release_held(shared);
            if let Some(restore) = restore {
                std::thread::sleep(if is_image { IMAGE_RESTORE_DELAY } else { CLIPBOARD_RESTORE_DELAY });
                restore.apply();
            }
        }
        Some(Outcome::Skipped(_)) | None => {
            // The engine assumed the replacement happened; it didn't.
            shared.engine.lock().unwrap().reset();
            replay(&trigger.0);
            release_held(shared);
        }
    }
}

/// Replays held key presses in order. New presses that arrive while replaying
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
        for event in batch {
            replay(&event.0);
        }
    }
}

fn replay(event: &CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, REPLAY_MARK);
    event.post(CGEventTapLocation::HID);
}

fn replace_on_main(
    monitor: &Arc<MonitorState>,
    target_pid: i64,
    verify_boundary: bool,
    delete_chars: usize,
    insert: &Insert,
) -> Outcome {
    // Password fields turn on secure input; IMEs (Korean, Japanese, Chinese…)
    // compose text the tap can't see. Never rewrite text in either case.
    if unsafe { IsSecureEventInputEnabled() } != 0 {
        return Outcome::Skipped("secure input is on");
    }
    if input_method_active() {
        return Outcome::Skipped("an input method is active");
    }
    // Diagnostic only: a key event's target can be a helper process of the
    // frontmost app, so a mismatch is not a reason to skip.
    if let Some(front) = frontmost_pid().filter(|pid| target_pid > 0 && i64::from(*pid) != target_pid) {
        super::diag(&format!("note: key target pid {target_pid}, frontmost app pid {front}"));
    }
    if verify_boundary {
        match text_before_word_is_boundary(delete_chars) {
            Some(false) => return Outcome::Skipped("the shortcut is attached to earlier text"),
            Some(true) => {}
            // The field doesn't expose its text (some Electron apps): assume
            // the shortcut starts a word rather than refuse to expand.
            None => super::diag("note: field text unavailable, assuming word start"),
        }
    }

    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let Ok(mut enigo) = Enigo::new(&Settings {
        mac_delay: 1,
        event_source_user_data: Some(SYNTHETIC_MARK),
        ..Settings::default()
    }) else {
        return Outcome::Skipped("could not create a keyboard event source");
    };

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

    for _ in 0..delete_chars {
        let _ = enigo.key(Key::Backspace, Direction::Click);
    }
    match typed {
        Some(text) => {
            let _ = enigo.text(text);
        }
        None => {
            // Explicit ⌘ flags and a layout-independent V key; see
            // `paste_target::post_paste_chord`.
            if let Err(e) = crate::paste_target::post_paste_chord(SYNTHETIC_MARK) {
                super::diag(&format!("paste keystroke failed: {e}"));
            }
        }
    }
    Outcome::Done { restore }
}

#[repr(C)]
struct CFRange {
    location: isize,
    length: isize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> *const c_void;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        parameter: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *const c_void;
    fn AXValueGetValue(value: *const c_void, kind: u32, out: *mut c_void) -> bool;
}

const AX_VALUE_CF_RANGE: u32 = 4;

/// Releases a CoreFoundation object when dropped.
struct Owned(*const c_void);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRelease(self.0) };
        }
    }
}

/// Asks the focused text field (through Accessibility) whether the character
/// just before the `word_chars`-long word at the caret is whitespace or the
/// start of the field. Reads that single character only. `None` when the
/// field doesn't expose its text or caret.
fn text_before_word_is_boundary(word_chars: usize) -> Option<bool> {
    use core_foundation::string::{CFString, CFStringRef};

    unsafe fn copy_attribute(element: *const c_void, name: &str) -> Option<Owned> {
        let name = CFString::new(name);
        let mut value = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef() as _, &mut value)
        };
        (status == 0 && !value.is_null()).then(|| Owned(value))
    }

    unsafe {
        let system = Owned(AXUIElementCreateSystemWide());
        // Never let a slow app stall the main thread for long.
        AXUIElementSetMessagingTimeout(system.0, 0.25);
        let focused = copy_attribute(system.0, "AXFocusedUIElement")?;
        let selection = copy_attribute(focused.0, "AXSelectedTextRange")?;
        let mut caret = CFRange {
            location: 0,
            length: 0,
        };
        if !AXValueGetValue(selection.0, AX_VALUE_CF_RANGE, &mut caret as *mut _ as *mut c_void)
            || caret.length != 0
        {
            return None;
        }
        let word_start = caret.location - word_chars as isize;
        if word_start < 0 {
            return None;
        }
        if word_start == 0 {
            return Some(true);
        }
        let before = CFRange {
            location: word_start - 1,
            length: 1,
        };
        let parameter = Owned(AXValueCreate(
            AX_VALUE_CF_RANGE,
            &before as *const _ as *const c_void,
        ));
        let attribute = CFString::new("AXStringForRange");
        let mut text = std::ptr::null();
        let status = AXUIElementCopyParameterizedAttributeValue(
            focused.0,
            attribute.as_concrete_TypeRef() as _,
            parameter.0,
            &mut text,
        );
        if status != 0 || text.is_null() {
            return None;
        }
        let text = CFString::wrap_under_create_rule(text as CFStringRef).to_string();
        Some(text.chars().all(char::is_whitespace))
    }
}

fn input_method_active() -> bool {
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return false;
        }
        let kind = TISGetInputSourceProperty(source, kTISPropertyInputSourceType);
        let active = !kind.is_null()
            && core_foundation::base::CFEqual(kind, kTISTypeKeyboardInputMode) != 0;
        core_foundation::base::CFRelease(source);
        active
    }
}

fn frontmost_pid() -> Option<i32> {
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

/// Everything that was on the clipboard before a paste-based expansion, put
/// back afterwards unless something else changed the clipboard meanwhile.
struct ClipboardRestore {
    items: Vec<Vec<(String, Vec<u8>)>>,
    change_count: isize,
}

fn stage_clipboard(monitor: &Arc<MonitorState>, insert: &Insert) -> Result<ClipboardRestore, String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString};
    use objc2_foundation::{NSData, NSString};

    let pasteboard = NSPasteboard::generalPasteboard();
    let items = pasteboard
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.types()
                        .iter()
                        .filter_map(|kind| {
                            let data = item.dataForType(&kind)?;
                            Some((kind.to_string(), data.to_vec()))
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();

    // Mark the snippet as our own write so the history monitor skips it.
    let mut self_write = monitor
        .last_self_write_hash
        .lock()
        .map_err(|_| "clipboard monitor state is unavailable".to_string())?;
    pasteboard.clearContents();
    match insert {
        Insert::Text(text) => {
            if !pasteboard.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString }) {
                return Err("the clipboard refused the snippet text".to_string());
            }
            *self_write = Some(clipboard::hash_bytes(text.as_bytes()));
        }
        Insert::Image(png) => {
            if !pasteboard.setData_forType(Some(&NSData::with_bytes(png)), unsafe { NSPasteboardTypePNG }) {
                return Err("the clipboard refused the snippet image".to_string());
            }
            // Hash the image the way the history monitor reads it back, so the
            // snippet isn't recorded as a new copy.
            let hash = arboard::Clipboard::new()
                .and_then(|mut c| c.get_image())
                .map(|img| clipboard::hash_bytes(&img.bytes))
                .map_err(|e| format!("the clipboard image can't be read back: {e}"))?;
            *self_write = Some(hash);
        }
    }
    Ok(ClipboardRestore {
        items,
        change_count: pasteboard.changeCount(),
    })
}

impl ClipboardRestore {
    fn apply(self) {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
        use objc2_foundation::{NSArray, NSData, NSString};

        objc2::rc::autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            // The user (or another app) copied something since: keep that.
            if pasteboard.changeCount() != self.change_count {
                return;
            }
            pasteboard.clearContents();
            if self.items.is_empty() {
                return;
            }
            let objects: Vec<_> = self
                .items
                .iter()
                .map(|representations| {
                    let item = NSPasteboardItem::new();
                    for (kind, bytes) in representations {
                        item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(kind));
                    }
                    ProtocolObject::from_retained(item)
                })
                .collect();
            let objects =
                NSArray::<ProtocolObject<dyn NSPasteboardWriting>>::from_retained_slice(&objects);
            pasteboard.writeObjects(&objects);
        });
    }
}
