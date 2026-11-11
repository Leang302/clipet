//! macOS capture and injection for input sharing.
//!
//! Capture: an active session-level `CGEventTap` on its own run-loop thread
//! (needs Accessibility). With no session it reports pointer positions and
//! Clipet's hotkeys and passes everything through. During a session it drops
//! local input and reports it; the pointer is hidden and detached from the
//! mouse (`CGAssociateMouseAndMouseCursorPosition(false)`), and deltas come
//! from the event's delta fields.
//!
//! Injection: `CGEventPost` at the HID level with absolute positions, click
//! counts for double-clicks, `*MouseDragged` while a button is held, and the
//! current modifier flags on every event. Injected events carry a mark in
//! `EVENT_SOURCE_USER_DATA` the tap skips.
//!
//! Limits: Secure Input (password fields, some terminals) hides keystrokes
//! from every event tap, so a Mac can't forward typing while it is on.

use super::geometry::Screen;
use super::keymap;
use super::protocol::{Button, Os};
use super::session::{HotkeyMatcher, Inject};
use super::{Captured, Permission, Platform, Sink};
use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// `EVENT_SOURCE_USER_DATA` on events Clipet injects for input sharing.
const INPUT_MARK: i64 = 0x434C_4960;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

struct Shared {
    active: AtomicBool,
    remote: AtomicBool,
    sink: RwLock<Option<Sink>>,
    hotkeys: Mutex<HotkeyMatcher>,
    /// Key codes the local system saw go down (their key-ups stay local).
    local_keys: Mutex<HashSet<u16>>,
    /// Modifier key codes currently down, to tell press from release in
    /// `FlagsChanged` events.
    modifiers: Mutex<HashSet<u16>>,
    remote_keys: Mutex<HashSet<u16>>,
    local_buttons: AtomicU8,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            active: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            sink: RwLock::new(None),
            hotkeys: Mutex::new(HotkeyMatcher::default()),
            local_keys: Mutex::new(HashSet::new()),
            modifiers: Mutex::new(HashSet::new()),
            remote_keys: Mutex::new(HashSet::new()),
            local_buttons: AtomicU8::new(0),
        })
    })
}

fn emit(event: Captured) {
    if let Some(sink) = shared().sink.read().unwrap().as_ref() {
        sink(event);
    }
}

fn button_bit(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 4,
        Button::Back => 8,
        Button::Forward => 16,
    }
}

fn other_button(event: &CGEvent) -> Button {
    match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
        3 => Button::Back,
        4 => Button::Forward,
        _ => Button::Middle,
    }
}

const MODIFIER_KEYCODES: [u16; 9] = [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E, 0x39];

fn on_event(event_type: CGEventType, event: &CGEvent) -> CallbackResult {
    let s = shared();
    match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            let port = TAP_PORT.load(Ordering::SeqCst);
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, true) };
            }
            return CallbackResult::Keep;
        }
        _ => {}
    }
    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INPUT_MARK || !s.active.load(Ordering::Relaxed) {
        return CallbackResult::Keep;
    }
    let remote = s.remote.load(Ordering::Relaxed);
    let button = match event_type {
        CGEventType::LeftMouseDown => Some((Button::Left, true)),
        CGEventType::LeftMouseUp => Some((Button::Left, false)),
        CGEventType::RightMouseDown => Some((Button::Right, true)),
        CGEventType::RightMouseUp => Some((Button::Right, false)),
        CGEventType::OtherMouseDown => Some((other_button(event), true)),
        CGEventType::OtherMouseUp => Some((other_button(event), false)),
        _ => None,
    };
    if let Some((b, down)) = button {
        let bit = button_bit(b);
        if !remote {
            if down {
                s.local_buttons.fetch_or(bit, Ordering::Relaxed);
            } else {
                s.local_buttons.fetch_and(!bit, Ordering::Relaxed);
            }
            return CallbackResult::Keep;
        }
        if !down && s.local_buttons.load(Ordering::Relaxed) & bit != 0 {
            s.local_buttons.fetch_and(!bit, Ordering::Relaxed);
            return CallbackResult::Keep;
        }
        emit(Captured::Button { b, down });
        return CallbackResult::Drop;
    }
    match event_type {
        CGEventType::MouseMoved | CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged | CGEventType::OtherMouseDragged => {
            if remote {
                let dx = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                let dy = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                if dx != 0.0 || dy != 0.0 {
                    emit(Captured::Delta { dx, dy });
                }
                CallbackResult::Drop
            } else {
                let p = event.location();
                emit(Captured::Position { x: p.x, y: p.y, button_held: s.local_buttons.load(Ordering::Relaxed) != 0 });
                CallbackResult::Keep
            }
        }
        CGEventType::ScrollWheel if remote => {
            // Pixel deltas; 40 px = one wheel notch (120 units).
            let dy = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
            let dx = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
            emit(Captured::Scroll { dx: (-dx * 3.0) as f32, dy: (dy * 3.0) as f32 });
            CallbackResult::Drop
        }
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let down = match event_type {
                CGEventType::KeyDown => true,
                CGEventType::KeyUp => false,
                _ => {
                    if !MODIFIER_KEYCODES.contains(&code) {
                        return CallbackResult::Keep;
                    }
                    let mut mods = s.modifiers.lock().unwrap();
                    if mods.contains(&code) {
                        mods.remove(&code);
                        false
                    } else {
                        mods.insert(code);
                        true
                    }
                }
            };
            on_key(code, down, event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0, remote, event_type)
        }
        _ => CallbackResult::Keep,
    }
}

fn on_key(code: u16, down: bool, autorepeat: bool, remote: bool, event_type: CGEventType) -> CallbackResult {
    let s = shared();
    let Some(hid) = keymap::hid_from_mac(code) else { return CallbackResult::Keep };
    let (hotkey, swallow) = s.hotkeys.lock().unwrap().on_key(hid, down);
    if let Some(hotkey) = hotkey {
        emit(Captured::Hotkey(hotkey));
    }
    if swallow {
        return CallbackResult::Drop;
    }
    if !remote {
        let mut local = s.local_keys.lock().unwrap();
        if down {
            local.insert(hid);
        } else {
            local.remove(&hid);
        }
        return CallbackResult::Keep;
    }
    if !down && s.local_keys.lock().unwrap().remove(&hid) {
        return CallbackResult::Keep;
    }
    let repeat = {
        let mut held = s.remote_keys.lock().unwrap();
        if down {
            !held.insert(hid) || autorepeat
        } else {
            held.remove(&hid);
            false
        }
    };
    emit(Captured::Key { hid, down, repeat });
    // Caps Lock reports one FlagsChanged per press: send the release too.
    if matches!(event_type, CGEventType::FlagsChanged) && code == 0x39 {
        emit(Captured::Key { hid, down: false, repeat: false });
        s.remote_keys.lock().unwrap().remove(&hid);
    }
    CallbackResult::Drop
}

fn run_tap() {
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ],
        |_proxy: CGEventTapProxy, event_type, event| on_event(event_type, event),
    );
    let Ok(tap) = tap else {
        eprintln!("[clipet] input sharing: the event tap could not be created (Accessibility permission?)");
        return;
    };
    let Ok(source) = tap.mach_port().create_runloop_source(0) else { return };
    TAP_PORT.store(tap.mach_port().as_concrete_TypeRef() as *mut c_void, Ordering::SeqCst);
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    CFRunLoop::run_current();
}

/// Injection state: held buttons and modifiers (macOS needs them on every
/// event) and the last click, for double-click counts.
#[derive(Default)]
struct Injector {
    buttons: Vec<Button>,
    modifiers: HashSet<u16>,
    last_click: Option<(Button, Instant, f64, f64, i64)>,
    scroll_rest: (f64, f64),
}

pub struct MacPlatform {
    started: AtomicBool,
    injector: Mutex<Injector>,
}

impl MacPlatform {
    pub fn new() -> Self {
        Self { started: AtomicBool::new(false), injector: Mutex::new(Injector::default()) }
    }
}

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

fn post(event: &CGEvent, flags: CGEventFlags) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INPUT_MARK);
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
}

fn flags_for(modifiers: &HashSet<u16>) -> CGEventFlags {
    let mut bits: u64 = 0;
    for &m in modifiers {
        bits |= match m {
            keymap::HID_LSHIFT => 0x0002_0002,
            keymap::HID_RSHIFT => 0x0002_0004,
            keymap::HID_LCTRL => 0x0004_0001,
            keymap::HID_RCTRL => 0x0004_2000,
            keymap::HID_LALT => 0x0008_0020,
            keymap::HID_RALT => 0x0008_0040,
            keymap::HID_LGUI => 0x0010_0008,
            keymap::HID_RGUI => 0x0010_0010,
            _ => 0,
        };
    }
    CGEventFlags::from_bits_retain(bits)
}

fn mouse_kind(b: Button, down: bool) -> (CGEventType, CGMouseButton) {
    match (b, down) {
        (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
        (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
        (_, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        (_, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
    }
}

impl Platform for MacPlatform {
    fn os(&self) -> Os {
        Os::MacOs
    }

    fn screens(&self) -> Vec<Screen> {
        CGDisplay::active_displays()
            .unwrap_or_default()
            .into_iter()
            .map(|id| {
                let b = CGDisplay::new(id).bounds();
                // Quartz global coordinates are points already.
                Screen { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height, scale: 1.0 }
            })
            .collect()
    }

    fn start(&self, sink: Sink) -> Result<(), String> {
        *shared().sink.write().unwrap() = Some(sink);
        if !crate::accessibility_trusted() {
            return Err("Clipet needs Accessibility permission to share the mouse and keyboard.".to_string());
        }
        if !self.started.swap(true, Ordering::SeqCst) {
            std::thread::Builder::new()
                .name("clipet-input-tap".into())
                .spawn(run_tap)
                .map_err(|e| format!("could not start the input tap: {e}"))?;
        }
        Ok(())
    }

    fn set_active(&self, active: bool) {
        let s = shared();
        s.active.store(active, Ordering::SeqCst);
        if !active {
            self.set_remote(false, None);
            s.hotkeys.lock().unwrap().reset();
        }
    }

    fn set_remote(&self, remote: bool, place: Option<(f64, f64)>) {
        let s = shared();
        let main = CGDisplay::main();
        if remote {
            s.remote_keys.lock().unwrap().clear();
            s.remote.store(true, Ordering::SeqCst);
            let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
            let _ = main.hide_cursor();
        } else if s.remote.swap(false, Ordering::SeqCst) {
            s.remote_keys.lock().unwrap().clear();
            if let Some((x, y)) = place {
                let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
            }
            let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
            let _ = main.show_cursor();
        } else if let Some((x, y)) = place {
            let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
        }
    }

    fn inject(&self, events: &[Inject]) {
        let mut st = self.injector.lock().unwrap();
        for event in events {
            let Some(src) = source() else { return };
            match event {
                Inject::MoveTo { x, y, buttons } => {
                    let (kind, button) = match buttons.first() {
                        Some(Button::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
                        Some(Button::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
                        Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
                        None => (CGEventType::MouseMoved, CGMouseButton::Left),
                    };
                    if let Ok(e) = CGEvent::new_mouse_event(src, kind, CGPoint::new(*x, *y), button) {
                        post(&e, flags_for(&st.modifiers));
                    }
                }
                Inject::Button { b, down, x, y } => {
                    let (kind, button) = mouse_kind(*b, *down);
                    // Double-clicks: macOS needs the click count on the event.
                    let count = if *down {
                        let count = match st.last_click {
                            Some((lb, at, lx, ly, n))
                                if lb == *b && at.elapsed() < Duration::from_millis(500) && (lx - x).abs() < 5.0 && (ly - y).abs() < 5.0 =>
                            {
                                n + 1
                            }
                            _ => 1,
                        };
                        st.last_click = Some((*b, Instant::now(), *x, *y, count));
                        count
                    } else {
                        st.last_click.map_or(1, |c| c.4)
                    };
                    if *down {
                        st.buttons.push(*b);
                    } else {
                        st.buttons.retain(|h| h != b);
                    }
                    if let Ok(e) = CGEvent::new_mouse_event(src, kind, CGPoint::new(*x, *y), button) {
                        e.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, count);
                        if matches!(b, Button::Back | Button::Forward | Button::Middle) {
                            let number = match b {
                                Button::Back => 3,
                                Button::Forward => 4,
                                _ => 2,
                            };
                            e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
                        }
                        post(&e, flags_for(&st.modifiers));
                    }
                }
                Inject::Scroll { dx, dy } => {
                    // 120 units = one notch = 40 px.
                    st.scroll_rest.0 += f64::from(*dx) / 3.0;
                    st.scroll_rest.1 += f64::from(*dy) / 3.0;
                    let (px, py) = (st.scroll_rest.0.trunc(), st.scroll_rest.1.trunc());
                    st.scroll_rest.0 -= px;
                    st.scroll_rest.1 -= py;
                    if px != 0.0 || py != 0.0 {
                        if let Ok(e) = CGEvent::new_scroll_event(src, ScrollEventUnit::PIXEL, 2, py as i32, -(px as i32), 0) {
                            post(&e, flags_for(&st.modifiers));
                        }
                    }
                }
                Inject::Key { hid, down, repeat } => {
                    let Some(code) = keymap::mac_from_hid(*hid) else { continue };
                    let modifier = keymap::is_modifier(*hid);
                    if modifier {
                        if *down {
                            st.modifiers.insert(*hid);
                        } else {
                            st.modifiers.remove(hid);
                        }
                    }
                    if let Ok(e) = CGEvent::new_keyboard_event(src, code, *down) {
                        if modifier {
                            e.set_type(CGEventType::FlagsChanged);
                        }
                        if *repeat {
                            e.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
                        }
                        post(&e, flags_for(&st.modifiers));
                    }
                }
            }
        }
    }

    fn permission(&self) -> Permission {
        if !crate::accessibility_trusted() {
            return Permission {
                granted: false,
                hint: "Allow Clipet in System Settings → Privacy & Security → Accessibility, then turn input sharing off and on.".to_string(),
            };
        }
        if unsafe { IsSecureEventInputEnabled() } != 0 {
            return Permission {
                granted: true,
                hint: "Secure Input is on (a password field or a terminal has it), so this Mac can't forward typing right now.".to_string(),
            };
        }
        Permission { granted: true, hint: String::new() }
    }
}
