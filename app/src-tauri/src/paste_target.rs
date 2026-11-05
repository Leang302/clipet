//! The exact window a paste goes back to (macOS).
//!
//! Remembering only the app is not enough when it has several windows: with a
//! Chrome pop-up chat open beside the main browser window, Chrome becomes the
//! frontmost app again before it re-focuses the pop-up — or it brings its main
//! window forward instead — so ⌘V landed in the wrong window or nowhere. The
//! window focused when the popup opened is captured through Accessibility
//! (already required for paste) and confirmed focused before ⌘V is sent.
//!
//! The window alone is not enough either: when Clipet takes focus, the page
//! inside a Chrome window can lose track of its text field (chat composers
//! often live in nested frames), so ⌘V reached the right window and was
//! dropped. The focused element is captured too and focused again if needed.

use core_foundation::base::{CFEqual, CFRelease, CFRetain, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use std::ffi::c_void;
use std::time::{Duration, Instant};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *const c_void,
    ) -> i32;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
}

/// Never let an unresponsive app stall the popup or the paste for long.
const AX_TIMEOUT_SECONDS: f32 = 0.25;

/// A retained Accessibility element (a window or a text field).
pub struct WindowRef(*const c_void);

pub type ElementRef = WindowRef;

// AXUIElement references are immutable CF objects; the Accessibility API may
// be called with them from any thread.
unsafe impl Send for WindowRef {}
unsafe impl Sync for WindowRef {}

impl Clone for WindowRef {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0) };
        Self(self.0)
    }
}

impl Drop for WindowRef {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

impl PartialEq for WindowRef {
    fn eq(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0, other.0) != 0 }
    }
}

/// The window that currently has keyboard focus in app `pid`.
pub fn focused_window(pid: i32) -> Option<WindowRef> {
    app_attribute(pid, "AXFocusedWindow")
}

/// The element (text field, web area…) with keyboard focus in app `pid`.
pub fn focused_element(pid: i32) -> Option<ElementRef> {
    app_attribute(pid, "AXFocusedUIElement")
}

fn app_attribute(pid: i32, name: &str) -> Option<WindowRef> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, AX_TIMEOUT_SECONDS);
        let attribute = CFString::new(name);
        let mut value = std::ptr::null();
        let status =
            AXUIElementCopyAttributeValue(app, attribute.as_concrete_TypeRef() as _, &mut value);
        CFRelease(app);
        (status == 0 && !value.is_null()).then(|| WindowRef(value))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Handoff {
    /// The window already had focus once its app was frontmost.
    AlreadyFocused,
    /// Another window of the app came forward; the remembered one was raised.
    Raised,
    /// The remembered window is gone (closed) or never regained focus.
    Unconfirmed,
}

/// Makes `window` the focused window of app `pid` again, waiting up to
/// `timeout` for the app to confirm it.
pub fn refocus(pid: i32, window: &WindowRef, timeout: Duration) -> Handoff {
    let deadline = Instant::now() + timeout;
    let is_focused = || focused_window(pid).is_some_and(|current| current == *window);
    // The app usually restores its own key window right after activation.
    let settle = Instant::now() + Duration::from_millis(60);
    while Instant::now() < settle {
        if is_focused() {
            return Handoff::AlreadyFocused;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        AXUIElementSetMessagingTimeout(window.0, AX_TIMEOUT_SECONDS);
        let main = CFString::new("AXMain");
        AXUIElementSetAttributeValue(
            window.0,
            main.as_concrete_TypeRef() as _,
            CFBoolean::true_value().as_concrete_TypeRef() as _,
        );
        let raise = CFString::new("AXRaise");
        if AXUIElementPerformAction(window.0, raise.as_concrete_TypeRef() as _) != 0 {
            // The window no longer exists.
            return Handoff::Unconfirmed;
        }
    }
    while Instant::now() < deadline {
        if is_focused() {
            return Handoff::Raised;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Handoff::Unconfirmed
}

#[derive(Debug, PartialEq, Eq)]
pub enum FieldHandoff {
    /// The text field still had focus.
    AlreadyFocused,
    /// Focus had moved off it inside the window; it was focused again.
    Restored,
    /// It no longer exists or refused focus; ⌘V goes wherever focus is.
    Unconfirmed,
}

/// Puts keyboard focus back on `element` inside app `pid` when it moved,
/// waiting up to `timeout` for the app to confirm.
pub fn refocus_element(pid: i32, element: &ElementRef, timeout: Duration) -> FieldHandoff {
    let is_focused = || focused_element(pid).is_some_and(|current| current == *element);
    if is_focused() {
        return FieldHandoff::AlreadyFocused;
    }
    unsafe {
        AXUIElementSetMessagingTimeout(element.0, AX_TIMEOUT_SECONDS);
        let focused = CFString::new("AXFocused");
        let status = AXUIElementSetAttributeValue(
            element.0,
            focused.as_concrete_TypeRef() as _,
            CFBoolean::true_value().as_concrete_TypeRef() as _,
        );
        if status != 0 {
            return FieldHandoff::Unconfirmed;
        }
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_focused() {
            return FieldHandoff::Restored;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    FieldHandoff::Unconfirmed
}

/// Copies an attribute of an Accessibility element; the caller releases it.
unsafe fn copy_attribute(element: *const c_void, name: &str) -> Option<*const c_void> {
    let attribute = CFString::new(name);
    let mut value = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef() as _, &mut value)
    };
    (status == 0 && !value.is_null()).then_some(value)
}

unsafe fn string_attribute(element: *const c_void, name: &str) -> Option<String> {
    let value = unsafe { copy_attribute(element, name)? };
    let text = unsafe { CFString::wrap_under_create_rule(value as _) }.to_string();
    Some(text)
}

unsafe fn bool_attribute(element: *const c_void, name: &str) -> Option<bool> {
    let value = unsafe { copy_attribute(element, name)? };
    let flag = unsafe { CFBoolean::wrap_under_create_rule(value as _) };
    Some(flag == CFBoolean::true_value())
}

/// One diagnostic line about app `pid` right before ⌘V: whether `window` is
/// its main/key window and the role of the element with keyboard focus.
/// Roles only (e.g. AXTextArea, AXWebArea) — never any text.
pub fn describe_focus(pid: i32, window: Option<&WindowRef>) -> String {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return "no accessibility element for the app".to_string();
        }
        AXUIElementSetMessagingTimeout(app, AX_TIMEOUT_SECONDS);
        let role = copy_attribute(app, "AXFocusedUIElement").map(|element| {
            let role = string_attribute(element, "AXRole").unwrap_or_else(|| "?".into());
            CFRelease(element);
            role
        });
        CFRelease(app);
        let window_state = window.map(|w| {
            format!(
                "main={:?} focused={:?}",
                bool_attribute(w.0, "AXMain"),
                bool_attribute(w.0, "AXFocused")
            )
        });
        format!(
            "focused element {}, window {}",
            role.as_deref().unwrap_or("none"),
            window_state.as_deref().unwrap_or("not captured")
        )
    }
}

/// The window's title. Test builds only: titles can contain private text.
#[cfg(feature = "e2e")]
pub fn window_title(window: &WindowRef) -> Option<String> {
    unsafe { string_attribute(window.0, "AXTitle") }
}

/// Titles of all of app `pid`'s windows. Test builds only.
#[cfg(feature = "e2e")]
pub fn window_titles(pid: i32) -> Vec<String> {
    use core_foundation::array::{CFArray, CFArrayRef};
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        let Some(windows) = copy_attribute(app, "AXWindows") else {
            CFRelease(app);
            return Vec::new();
        };
        CFRelease(app);
        let windows: CFArray<*const c_void> = CFArray::wrap_under_create_rule(windows as CFArrayRef);
        windows
            .iter()
            .map(|w| string_attribute(*w, "AXTitle").unwrap_or_else(|| "?".into()))
            .collect()
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *const c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const u8,
        key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

/// kVK_ANSI_V: the V key on an ANSI/ISO keyboard.
const ANSI_V: u16 = 9;

/// The key code for ⌘V. Never looked up on the current layout: a non-Latin
/// layout (Khmer, Russian, Thai…) has no key that types "v", and the
/// keyboard library then fell back to key code 0 — sending ⌘A (Select All)
/// instead of a paste. macOS resolves ⌘ shortcuts through the ASCII-capable
/// layout in that case, so the V key of that layout is what a person would
/// press (key 9 on QWERTY, the Dvorak V key on Dvorak). Main thread only.
pub fn paste_key_code() -> u16 {
    unsafe {
        let source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource() as *mut c_void;
        if source.is_null() {
            return ANSI_V;
        }
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        let code = (!data.is_null())
            .then(|| {
                let layout = core_foundation::data::CFDataGetBytePtr(data as _);
                (0..128u16).find(|&key_code| {
                    let mut dead_keys = 0u32;
                    let mut length = 0usize;
                    let mut chars = [0u16; 4];
                    let status = UCKeyTranslate(
                        layout,
                        key_code,
                        3, // kUCKeyActionDisplay
                        0,
                        u32::from(LMGetKbdType()),
                        1, // kUCKeyTranslateNoDeadKeysMask
                        &mut dead_keys,
                        chars.len(),
                        &mut length,
                        chars.as_mut_ptr(),
                    );
                    status == 0 && length == 1 && chars[0] == u16::from(b'v')
                })
            })
            .flatten();
        CFRelease(source);
        code.unwrap_or(ANSI_V)
    }
}

/// Posts ⌘V with the Command flag set on every event, from a private event
/// source. Relying on the combined keyboard state (what the keyboard library
/// did) made the V inherit whatever the hardware state was at that instant —
/// a ⌘ press not yet registered, or the user still letting go of ⌘⇧V/Enter —
/// so it sometimes arrived as a plain "v". `user_data` marks the events (see
/// the snippet listener). Main thread only (`paste_key_code`).
pub fn post_paste_chord(user_data: i64) -> Result<(), String> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, EventField};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    const COMMAND: u16 = 55; // kVK_Command
    let v = paste_key_code();
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| "could not create a keyboard event source".to_string())?;
    let steps = [
        (COMMAND, true, CGEventFlags::CGEventFlagCommand),
        (v, true, CGEventFlags::CGEventFlagCommand),
        (v, false, CGEventFlags::CGEventFlagCommand),
        (COMMAND, false, CGEventFlags::CGEventFlagNull),
    ];
    for (key, down, flags) in steps {
        let event = CGEvent::new_keyboard_event(source.clone(), key, down)
            .map_err(|_| "could not create the paste key event".to_string())?;
        event.set_flags(flags);
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, user_data);
        event.post(CGEventTapLocation::HID);
        // Some apps (Electron, Java) track modifiers from the ⌘ event itself.
        std::thread::sleep(Duration::from_millis(8));
    }
    Ok(())
}
