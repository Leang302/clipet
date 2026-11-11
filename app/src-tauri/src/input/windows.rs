//! Windows capture and injection for input sharing.
//!
//! Capture: low-level hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) on a thread of
//! their own. While no session is active they only report pointer positions
//! (for edge switching) and Clipet's hotkeys, and pass everything through.
//! During a session they swallow local input and report it; the pointer is
//! parked at the centre of its display so every move yields a delta.
//!
//! Injection: `SendInput` with absolute virtual-desktop coordinates and scan
//! codes (layout-independent). Injected events carry a mark the hooks skip.
//!
//! Limits: no capture or injection into elevated (administrator) windows
//! unless Clipet runs elevated (UIPI); the secure desktop (Ctrl+Alt+Del, UAC,
//! lock screen) is always local.

use super::geometry::Screen;
use super::keymap;
use super::protocol::{Button, Os};
use super::session::{HotkeyMatcher, Inject};
use super::{Captured, Permission, Platform, Sink};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// `dwExtraInfo` on events Clipet injects for input sharing.
const INPUT_MARK: usize = 0x434C_4960;

type Handle = *mut c_void;
type HookProc = Option<unsafe extern "system" fn(i32, usize, isize) -> isize>;
type MonitorEnumProc = Option<unsafe extern "system" fn(Handle, Handle, *mut Rect, isize) -> i32>;

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_UP: u32 = 0x80;

const WM_MOUSEMOVE: usize = 0x0200;
const WM_LBUTTONDOWN: usize = 0x0201;
const WM_LBUTTONUP: usize = 0x0202;
const WM_RBUTTONDOWN: usize = 0x0204;
const WM_RBUTTONUP: usize = 0x0205;
const WM_MBUTTONDOWN: usize = 0x0207;
const WM_MBUTTONUP: usize = 0x0208;
const WM_MOUSEWHEEL: usize = 0x020A;
const WM_XBUTTONDOWN: usize = 0x020B;
const WM_XBUTTONUP: usize = 0x020C;
const WM_MOUSEHWHEEL: usize = 0x020E;

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;
const VK_PAUSE: u16 = 0x13;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const MDT_EFFECTIVE_DPI: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[allow(dead_code)]
struct MsLlHookStruct {
    pt: Point,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
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
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputUnion {
    mi: MouseInput,
    ki: KeybdInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    u: InputUnion,
}

#[repr(C)]
#[allow(dead_code)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}

#[repr(C)]
#[allow(dead_code)]
struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: Point,
    private: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(id: i32, proc_: HookProc, module: Handle, thread_id: u32) -> Handle;
    fn CallNextHookEx(hook: Handle, code: i32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn EnumDisplayMonitors(hdc: Handle, clip: *const Rect, proc_: MonitorEnumProc, data: isize) -> i32;
    fn GetMonitorInfoW(monitor: Handle, info: *mut MonitorInfo) -> i32;
    fn MonitorFromPoint(point: Point, flags: u32) -> Handle;
    fn SetProcessDpiAwarenessContext(context: isize) -> i32;
}

/// Per-monitor DPI awareness v2: real pixels everywhere. Tauri already sets
/// it for the app; setting it again just fails harmlessly.
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
}

#[link(name = "shcore")]
unsafe extern "system" {
    fn GetDpiForMonitor(monitor: Handle, kind: i32, dpi_x: *mut u32, dpi_y: *mut u32) -> i32;
}

/// State the hook procedures read; they are plain functions, so it lives in
/// a static.
struct Shared {
    active: AtomicBool,
    remote: AtomicBool,
    sink: RwLock<Option<Sink>>,
    /// Where the pointer is parked during a session, and the scale there.
    park: Mutex<(Point, f64)>,
    hotkeys: Mutex<HotkeyMatcher>,
    /// Keys the local system saw go down; their key-ups stay local even
    /// during a session (no stuck Ctrl after a hotkey switch).
    local_keys: Mutex<HashSet<u16>>,
    /// Keys forwarded down during the session (to tell repeats apart).
    remote_keys: Mutex<HashSet<u16>>,
    /// Local mouse buttons held (bit per button), same purpose.
    local_buttons: AtomicU8,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            active: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            sink: RwLock::new(None),
            park: Mutex::new((Point::default(), 1.0)),
            hotkeys: Mutex::new(HotkeyMatcher::default()),
            local_keys: Mutex::new(HashSet::new()),
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

fn monitor_scale(monitor: Handle) -> f64 {
    let (mut x, mut y) = (96u32, 96u32);
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } != 0 || x == 0 {
        return 1.0;
    }
    f64::from(x) / 96.0
}

fn scale_at(point: Point) -> f64 {
    monitor_scale(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) })
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION && on_mouse(wparam, unsafe { &*(lparam as *const MsLlHookStruct) }) {
        return 1;
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION && on_key(unsafe { &*(lparam as *const KbdLlHookStruct) }) {
        return 1;
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn mouse_button(msg: usize, data: u32) -> Option<(Button, bool)> {
    let x = if (data >> 16) == 2 { Button::Forward } else { Button::Back };
    Some(match msg {
        WM_LBUTTONDOWN => (Button::Left, true),
        WM_LBUTTONUP => (Button::Left, false),
        WM_RBUTTONDOWN => (Button::Right, true),
        WM_RBUTTONUP => (Button::Right, false),
        WM_MBUTTONDOWN => (Button::Middle, true),
        WM_MBUTTONUP => (Button::Middle, false),
        WM_XBUTTONDOWN => (x, true),
        WM_XBUTTONUP => (x, false),
        _ => return None,
    })
}

/// Returns true to swallow the event.
fn on_mouse(msg: usize, info: &MsLlHookStruct) -> bool {
    let s = shared();
    if info.extra_info == INPUT_MARK || !s.active.load(Ordering::Relaxed) {
        return false;
    }
    let remote = s.remote.load(Ordering::Relaxed);
    if let Some((b, down)) = mouse_button(msg, info.mouse_data) {
        let bit = button_bit(b);
        if !remote {
            if down {
                s.local_buttons.fetch_or(bit, Ordering::Relaxed);
            } else {
                s.local_buttons.fetch_and(!bit, Ordering::Relaxed);
            }
            return false;
        }
        if !down && s.local_buttons.load(Ordering::Relaxed) & bit != 0 {
            // Pressed before the session started: release it locally.
            s.local_buttons.fetch_and(!bit, Ordering::Relaxed);
            return false;
        }
        emit(Captured::Button { b, down });
        return true;
    }
    match msg {
        WM_MOUSEMOVE if remote => {
            let (park, scale) = *s.park.lock().unwrap();
            let (dx, dy) = (info.pt.x - park.x, info.pt.y - park.y);
            if dx != 0 || dy != 0 {
                emit(Captured::Delta { dx: f64::from(dx) / scale, dy: f64::from(dy) / scale });
            }
            true
        }
        WM_MOUSEMOVE => {
            emit(Captured::Position {
                x: f64::from(info.pt.x),
                y: f64::from(info.pt.y),
                button_held: s.local_buttons.load(Ordering::Relaxed) != 0,
            });
            false
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL if remote => {
            let delta = f32::from((info.mouse_data >> 16) as u16 as i16);
            emit(if msg == WM_MOUSEWHEEL { Captured::Scroll { dx: 0.0, dy: delta } } else { Captured::Scroll { dx: delta, dy: 0.0 } });
            true
        }
        _ => false,
    }
}

fn on_key(info: &KbdLlHookStruct) -> bool {
    let s = shared();
    if info.extra_info == INPUT_MARK || !s.active.load(Ordering::Relaxed) {
        return false;
    }
    let down = info.flags & LLKHF_UP == 0;
    let extended = info.flags & LLKHF_EXTENDED != 0;
    let hid = if info.vk_code == u32::from(VK_PAUSE) {
        Some(keymap::HID_PAUSE)
    } else {
        keymap::hid_from_windows(info.scan_code as u16, extended)
    };
    let remote = s.remote.load(Ordering::Relaxed);
    let Some(hid) = hid else {
        // A key Clipet can't map: keep it local rather than lose it.
        return false;
    };
    let (hotkey, swallow) = s.hotkeys.lock().unwrap().on_key(hid, down);
    if let Some(hotkey) = hotkey {
        emit(Captured::Hotkey(hotkey));
    }
    if swallow {
        return true;
    }
    if !remote {
        let mut local = s.local_keys.lock().unwrap();
        if down {
            local.insert(hid);
        } else {
            local.remove(&hid);
        }
        return false;
    }
    if !down && s.local_keys.lock().unwrap().remove(&hid) {
        // Went down locally before the session: let the local system see it
        // come up too.
        return false;
    }
    let repeat = {
        let mut held = s.remote_keys.lock().unwrap();
        if down {
            !held.insert(hid)
        } else {
            held.remove(&hid);
            false
        }
    };
    emit(Captured::Key { hid, down, repeat });
    true
}

fn run_hooks() {
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        if kb.is_null() || mouse.is_null() {
            eprintln!("[clipet] input sharing hooks could not be installed: {}", std::io::Error::last_os_error());
        }
        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
    }
}

pub struct WindowsPlatform {
    started: AtomicBool,
    /// Fractional wheel movement not yet sent.
    scroll_rest: Mutex<(f32, f32)>,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        // Coordinates from hooks, GetCursorPos, monitor info and SendInput
        // must all be physical pixels, whatever the process was started with.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { started: AtomicBool::new(false), scroll_rest: Mutex::new((0.0, 0.0)) }
    }
}

unsafe extern "system" fn collect_monitor(monitor: Handle, _hdc: Handle, _rect: *mut Rect, data: isize) -> i32 {
    let screens = unsafe { &mut *(data as *mut Vec<Screen>) };
    let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let r = info.monitor;
        screens.push(Screen {
            x: f64::from(r.left),
            y: f64::from(r.top),
            w: f64::from(r.right - r.left),
            h: f64::from(r.bottom - r.top),
            scale: monitor_scale(monitor),
        });
    }
    1
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> Input {
    Input { kind: INPUT_MOUSE, u: InputUnion { mi: MouseInput { dx, dy, mouse_data: data, flags, time: 0, extra_info: INPUT_MARK } } }
}

fn key(vk: u16, scan: u16, flags: u32) -> Input {
    Input { kind: INPUT_KEYBOARD, u: InputUnion { ki: KeybdInput { vk, scan, flags, time: 0, extra_info: INPUT_MARK } } }
}

/// Absolute coordinates for `SendInput`: 0..=65535 across the virtual desktop.
fn absolute(x: f64, y: f64) -> (i32, i32) {
    unsafe {
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2));
        let nx = ((x - f64::from(vx)) * 65535.0 / f64::from(vw - 1)).round() as i32;
        let ny = ((y - f64::from(vy)) * 65535.0 / f64::from(vh - 1)).round() as i32;
        (nx.clamp(0, 65535), ny.clamp(0, 65535))
    }
}

fn move_to(x: f64, y: f64) -> Input {
    let (nx, ny) = absolute(x, y);
    mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
}

impl Platform for WindowsPlatform {
    fn os(&self) -> Os {
        Os::Windows
    }

    fn screens(&self) -> Vec<Screen> {
        let mut screens: Vec<Screen> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect_monitor), &mut screens as *mut _ as isize);
        }
        screens
    }

    fn start(&self, sink: Sink) -> Result<(), String> {
        *shared().sink.write().unwrap() = Some(sink);
        if !self.started.swap(true, Ordering::SeqCst) {
            std::thread::Builder::new()
                .name("clipet-input-hooks".into())
                .spawn(run_hooks)
                .map_err(|e| format!("could not start input hooks: {e}"))?;
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
        if remote {
            // Park at the centre of the current display so moves in every
            // direction show up as deltas.
            let mut cursor = Point::default();
            unsafe { GetCursorPos(&mut cursor) };
            let screens = self.screens();
            let screen = screens
                .iter()
                .find(|m| f64::from(cursor.x) >= m.x && f64::from(cursor.x) < m.x + m.w && f64::from(cursor.y) >= m.y && f64::from(cursor.y) < m.y + m.h)
                .or(screens.first())
                .copied();
            if let Some(m) = screen {
                let park = Point { x: (m.x + m.w / 2.0) as i32, y: (m.y + m.h / 2.0) as i32 };
                *s.park.lock().unwrap() = (park, scale_at(park));
                unsafe { SetCursorPos(park.x, park.y) };
            }
            s.remote_keys.lock().unwrap().clear();
            s.remote.store(true, Ordering::SeqCst);
        } else {
            let was_remote = s.remote.swap(false, Ordering::SeqCst);
            s.remote_keys.lock().unwrap().clear();
            if let Some((x, y)) = place {
                unsafe { SetCursorPos(x.round() as i32, y.round() as i32) };
            } else if was_remote {
                // Stopped without a target edge: leave the pointer where it
                // was parked (centre of the display).
            }
        }
    }

    fn inject(&self, events: &[Inject]) {
        let mut inputs = Vec::with_capacity(events.len() * 2);
        for event in events {
            match *event {
                Inject::MoveTo { x, y, .. } => inputs.push(move_to(x, y)),
                Inject::Button { b, down, x, y } => {
                    inputs.push(move_to(x, y));
                    let (flags, data) = match (b, down) {
                        (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                        (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                        (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                        (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                        (Button::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                        (Button::Back, false) => (MOUSEEVENTF_XUP, 1),
                        (Button::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                        (Button::Forward, false) => (MOUSEEVENTF_XUP, 2),
                    };
                    inputs.push(mouse(0, 0, data, flags));
                }
                Inject::Scroll { dx, dy } => {
                    let mut rest = self.scroll_rest.lock().unwrap();
                    rest.0 += dx;
                    rest.1 += dy;
                    let (wx, wy) = (rest.0.trunc(), rest.1.trunc());
                    rest.0 -= wx;
                    rest.1 -= wy;
                    if wy != 0.0 {
                        inputs.push(mouse(0, 0, wy as i32 as u32, MOUSEEVENTF_WHEEL));
                    }
                    if wx != 0.0 {
                        inputs.push(mouse(0, 0, wx as i32 as u32, MOUSEEVENTF_HWHEEL));
                    }
                }
                Inject::Key { hid, down, .. } => {
                    let up = if down { 0 } else { KEYEVENTF_KEYUP };
                    if keymap::is_pause(hid) {
                        inputs.push(key(VK_PAUSE, 0, up));
                    } else if let Some((scan, extended)) = keymap::windows_from_hid(hid) {
                        let ext = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
                        inputs.push(key(0, scan, KEYEVENTF_SCANCODE | ext | up));
                    }
                }
            }
        }
        if !inputs.is_empty() {
            // Zero means Windows blocked it (an elevated window is focused,
            // or the secure desktop is up); there is nothing more to do.
            unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<Input>() as i32) };
        }
    }

    fn permission(&self) -> Permission {
        Permission { granted: true, hint: String::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real OS, harmlessly: reads the displays, installs the hooks
    /// in pass-through mode, and "moves" the pointer to where it already is to
    /// check the absolute-coordinate math. Run with `--ignored`.
    #[test]
    #[ignore]
    fn real_displays_hooks_and_injection_math() {
        let platform = WindowsPlatform::new();
        let screens = platform.screens();
        assert!(!screens.is_empty(), "no displays found");
        for s in &screens {
            assert!(s.w > 0.0 && s.h > 0.0 && s.scale >= 1.0 && s.scale <= 4.0, "odd display {s:?}");
        }
        println!("displays: {screens:?}");

        platform.start(Arc::new(|_| {})).unwrap();
        platform.set_active(false);
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Retry a few times: if someone is moving the mouse, a sample is useless.
        let still = |a: Point, b: Point| (a.x - b.x).abs() <= 1 && (a.y - b.y).abs() <= 1;
        let mut checked = false;
        for attempt in 0..10 {
            let (mut before, mut steady) = (Point::default(), Point::default());
            unsafe { GetCursorPos(&mut before) };
            std::thread::sleep(std::time::Duration::from_millis(60));
            unsafe { GetCursorPos(&mut steady) };
            if !still(before, steady) {
                println!("attempt {attempt}: the mouse is moving, retrying");
                continue;
            }
            let sent = unsafe { SendInput(1, &move_to(f64::from(steady.x), f64::from(steady.y)), std::mem::size_of::<Input>() as i32) };
            std::thread::sleep(std::time::Duration::from_millis(60));
            let mut after = Point::default();
            unsafe { GetCursorPos(&mut after) };
            println!("SendInput accepted {sent}; pointer {}x{} -> {}x{}", steady.x, steady.y, after.x, after.y);
            assert_eq!(sent, 1);
            assert!(still(steady, after), "absolute mapping is off");
            checked = true;
            break;
        }
        assert!(checked, "the mouse never stayed still long enough to check");
    }
}
