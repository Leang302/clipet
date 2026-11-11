//! Session logic without any OS calls: the controlled side's state (replay
//! and stale-message checks, the virtual cursor, held keys/buttons to release)
//! and the controller's local hotkeys.

use super::geometry::{CursorStep, Screen, Side, VirtualCursor};
use super::keymap::{self, HID_ESCAPE};
use super::protocol::{Button, ToControlled};
use std::collections::BTreeSet;

/// Something to inject on the controlled computer, in its own coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum Inject {
    MoveTo { x: f64, y: f64, buttons: Vec<Button> },
    Button { b: Button, down: bool, x: f64, y: f64 },
    /// Wheel units (120 = one notch).
    Scroll { dx: f32, dy: f32 },
    Key { hid: u16, down: bool, repeat: bool },
}

/// Ends the session if nothing (not even a heartbeat) arrives for this long.
pub const SILENCE_TIMEOUT_MS: u64 = 1500;
pub const HEARTBEAT_MS: u64 = 250;
/// Right after entering, don't bounce straight back across the edge.
const RETURN_GRACE_MS: u64 = 300;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Inject(Vec<Inject>),
    /// The pointer crossed back: tell the controller, after these releases.
    Return { fraction: f64, release: Vec<Inject> },
    /// The controller left: release everything.
    Leave(Vec<Inject>),
    /// Stale, repeated or foreign message.
    Ignored,
}

pub struct ControlledSession {
    last_seq: u64,
    cursor: VirtualCursor,
    keys: BTreeSet<u16>,
    buttons: Vec<Button>,
    started_ms: u64,
    last_heard_ms: u64,
    invert_scroll: bool,
}

impl ControlledSession {
    /// `side` is where this computer sits as seen from the controller.
    pub fn new(screens: Vec<Screen>, side: Side, fraction: f64, now_ms: u64, invert_scroll: bool) -> Option<(Self, Inject)> {
        let cursor = VirtualCursor::new(screens, side.opposite(), fraction)?;
        let first = Inject::MoveTo { x: cursor.x, y: cursor.y, buttons: Vec::new() };
        Some((
            Self {
                last_seq: 0,
                cursor,
                keys: BTreeSet::new(),
                buttons: Vec::new(),
                started_ms: now_ms,
                last_heard_ms: now_ms,
                invert_scroll,
            },
            first,
        ))
    }

    pub fn timed_out(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_heard_ms) > SILENCE_TIMEOUT_MS
    }

    pub fn handle(&mut self, msg: &ToControlled, now_ms: u64) -> Outcome {
        let Some(seq) = msg.seq() else { return Outcome::Ignored };
        if seq <= self.last_seq {
            return Outcome::Ignored;
        }
        self.last_seq = seq;
        self.last_heard_ms = now_ms;
        match *msg {
            ToControlled::Move { dx, dy, .. } => match self.cursor.step(f64::from(dx), f64::from(dy)) {
                CursorStep::MovedTo(x, y) => Outcome::Inject(vec![Inject::MoveTo { x, y, buttons: self.buttons.clone() }]),
                CursorStep::Return(fraction) if now_ms.saturating_sub(self.started_ms) >= RETURN_GRACE_MS && self.buttons.is_empty() => {
                    Outcome::Return { fraction, release: self.release_all() }
                }
                CursorStep::Return(_) => Outcome::Inject(vec![Inject::MoveTo { x: self.cursor.x, y: self.cursor.y, buttons: self.buttons.clone() }]),
            },
            ToControlled::Button { b, down, .. } => {
                let held = self.buttons.contains(&b);
                if down == held {
                    return Outcome::Inject(Vec::new());
                }
                if down {
                    self.buttons.push(b);
                } else {
                    self.buttons.retain(|x| *x != b);
                }
                Outcome::Inject(vec![Inject::Button { b, down, x: self.cursor.x, y: self.cursor.y }])
            }
            ToControlled::Scroll { dx, dy, .. } => {
                let sign = if self.invert_scroll { -1.0 } else { 1.0 };
                Outcome::Inject(vec![Inject::Scroll { dx: dx * sign, dy: dy * sign }])
            }
            ToControlled::Key { k, down, rep, .. } => {
                if down {
                    self.keys.insert(k);
                } else if !self.keys.remove(&k) {
                    // Never pressed here: nothing to release.
                    return Outcome::Inject(Vec::new());
                }
                Outcome::Inject(vec![Inject::Key { hid: k, down, repeat: rep && down }])
            }
            ToControlled::Heartbeat { .. } => Outcome::Inject(Vec::new()),
            ToControlled::Leave { .. } => Outcome::Leave(self.release_all()),
            ToControlled::Enter { .. } => Outcome::Ignored,
        }
    }

    /// Releases every key and button this session pressed: modifiers last,
    /// so a released letter never turns into a shortcut.
    pub fn release_all(&mut self) -> Vec<Inject> {
        let mut out: Vec<Inject> = self
            .buttons
            .drain(..)
            .map(|b| Inject::Button { b, down: false, x: self.cursor.x, y: self.cursor.y })
            .collect();
        let keys: Vec<u16> = std::mem::take(&mut self.keys).into_iter().collect();
        let (mods, others): (Vec<u16>, Vec<u16>) = keys.into_iter().partition(|k| keymap::is_modifier(*k));
        out.extend(others.into_iter().chain(mods).map(|hid| Inject::Key { hid, down: false, repeat: false }));
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Shift+Esc: take local control back now.
    Emergency,
    /// Ctrl+Alt+Shift+arrow: switch toward that side.
    Switch(Side),
}

/// Tracks held modifiers on the controller to spot Clipet's hotkeys. The
/// trigger key's down and up are both swallowed locally, never forwarded.
#[derive(Default)]
pub struct HotkeyMatcher {
    held: BTreeSet<u16>,
    swallowing: Option<u16>,
}

impl HotkeyMatcher {
    fn chord_held(&self) -> bool {
        let any = |a: u16, b: u16| self.held.contains(&a) || self.held.contains(&b);
        any(keymap::HID_LCTRL, keymap::HID_RCTRL)
            && any(keymap::HID_LALT, keymap::HID_RALT)
            && any(keymap::HID_LSHIFT, keymap::HID_RSHIFT)
    }

    /// Returns (hotkey fired, swallow this key event).
    pub fn on_key(&mut self, hid: u16, down: bool) -> (Option<Hotkey>, bool) {
        if keymap::is_modifier(hid) {
            if down {
                self.held.insert(hid);
            } else {
                self.held.remove(&hid);
            }
            return (None, false);
        }
        if !down {
            if self.swallowing == Some(hid) {
                self.swallowing = None;
                return (None, true);
            }
            return (None, false);
        }
        if !self.chord_held() {
            return (None, false);
        }
        let hotkey = match hid {
            HID_ESCAPE => Hotkey::Emergency,
            keymap::HID_LEFT => Hotkey::Switch(Side::Left),
            keymap::HID_RIGHT => Hotkey::Switch(Side::Right),
            keymap::HID_UP => Hotkey::Switch(Side::Top),
            keymap::HID_DOWN => Hotkey::Switch(Side::Bottom),
            _ => return (None, false),
        };
        let repeat = self.swallowing == Some(hid);
        self.swallowing = Some(hid);
        (if repeat { None } else { Some(hotkey) }, true)
    }

    pub fn reset(&mut self) {
        self.held.clear();
        self.swallowing = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keymap::*;

    fn session() -> ControlledSession {
        let screens = vec![Screen { x: 0.0, y: 0.0, w: 1000.0, h: 800.0, scale: 1.0 }];
        ControlledSession::new(screens, Side::Right, 0.5, 0, false).unwrap().0
    }

    #[test]
    fn enters_at_the_matching_edge_point() {
        let screens = vec![Screen { x: 0.0, y: 0.0, w: 1000.0, h: 800.0, scale: 1.0 }];
        let (_, first) = ControlledSession::new(screens, Side::Right, 0.25, 0, false).unwrap();
        assert_eq!(first, Inject::MoveTo { x: 2.0, y: 200.0, buttons: vec![] });
    }

    #[test]
    fn stale_and_repeated_messages_are_dropped() {
        let mut s = session();
        assert!(matches!(s.handle(&ToControlled::Move { s: 5, dx: 1.0, dy: 0.0 }, 10), Outcome::Inject(_)));
        assert_eq!(s.handle(&ToControlled::Move { s: 5, dx: 1.0, dy: 0.0 }, 11), Outcome::Ignored);
        assert_eq!(s.handle(&ToControlled::Key { s: 3, k: 0x04, down: true, rep: false }, 12), Outcome::Ignored);
    }

    #[test]
    fn silence_times_out() {
        let mut s = session();
        s.handle(&ToControlled::Heartbeat { s: 1 }, 1000);
        assert!(!s.timed_out(1000 + SILENCE_TIMEOUT_MS));
        assert!(s.timed_out(1001 + SILENCE_TIMEOUT_MS));
    }

    #[test]
    fn everything_held_is_released_modifiers_last() {
        let mut s = session();
        s.handle(&ToControlled::Key { s: 1, k: HID_LCTRL, down: true, rep: false }, 0);
        s.handle(&ToControlled::Key { s: 2, k: 0x06, down: true, rep: false }, 0);
        s.handle(&ToControlled::Button { s: 3, b: Button::Left, down: true }, 0);
        let Outcome::Leave(release) = s.handle(&ToControlled::Leave { s: 4 }, 0) else { panic!() };
        assert_eq!(release.len(), 3);
        assert!(matches!(release[0], Inject::Button { b: Button::Left, down: false, .. }));
        assert_eq!(release[1], Inject::Key { hid: 0x06, down: false, repeat: false });
        assert_eq!(release[2], Inject::Key { hid: HID_LCTRL, down: false, repeat: false });
        assert!(s.release_all().is_empty(), "released once");
    }

    #[test]
    fn releases_for_keys_never_pressed_are_dropped_and_repeats_pass() {
        let mut s = session();
        assert_eq!(s.handle(&ToControlled::Key { s: 1, k: 0x04, down: false, rep: false }, 0), Outcome::Inject(vec![]));
        s.handle(&ToControlled::Key { s: 2, k: 0x04, down: true, rep: false }, 0);
        assert_eq!(
            s.handle(&ToControlled::Key { s: 3, k: 0x04, down: true, rep: true }, 0),
            Outcome::Inject(vec![Inject::Key { hid: 0x04, down: true, repeat: true }])
        );
    }

    #[test]
    fn dragging_carries_held_buttons_and_never_returns_mid_drag() {
        let mut s = session();
        s.handle(&ToControlled::Button { s: 1, b: Button::Left, down: true }, 1000);
        let Outcome::Inject(moves) = s.handle(&ToControlled::Move { s: 2, dx: 5.0, dy: 0.0 }, 1000) else { panic!() };
        assert_eq!(moves, vec![Inject::MoveTo { x: 7.0, y: 400.0, buttons: vec![Button::Left] }]);
        // Pushing back across the edge with the button held stays here.
        assert!(matches!(s.handle(&ToControlled::Move { s: 3, dx: -500.0, dy: 0.0 }, 2000), Outcome::Inject(_)));
        s.handle(&ToControlled::Button { s: 4, b: Button::Left, down: false }, 2000);
        assert!(matches!(s.handle(&ToControlled::Move { s: 5, dx: -500.0, dy: 0.0 }, 2000), Outcome::Return { .. }));
    }

    #[test]
    fn no_bounce_right_after_entering() {
        let mut s = session();
        assert!(matches!(s.handle(&ToControlled::Move { s: 1, dx: -50.0, dy: 0.0 }, 100), Outcome::Inject(_)));
        assert!(matches!(s.handle(&ToControlled::Move { s: 2, dx: -50.0, dy: 0.0 }, 400), Outcome::Return { fraction, .. } if (fraction - 0.5).abs() < 0.01));
    }

    #[test]
    fn scroll_can_be_inverted() {
        let screens = vec![Screen { x: 0.0, y: 0.0, w: 100.0, h: 100.0, scale: 1.0 }];
        let (mut s, _) = ControlledSession::new(screens, Side::Left, 0.5, 0, true).unwrap();
        assert_eq!(s.handle(&ToControlled::Scroll { s: 1, dx: 0.0, dy: 120.0 }, 0), Outcome::Inject(vec![Inject::Scroll { dx: -0.0, dy: -120.0 }]));
    }

    #[test]
    fn hotkeys_need_the_full_chord_and_swallow_only_the_trigger() {
        let mut h = HotkeyMatcher::default();
        assert_eq!(h.on_key(HID_ESCAPE, true), (None, false), "plain Esc passes");
        assert_eq!(h.on_key(HID_ESCAPE, false), (None, false));
        h.on_key(HID_LCTRL, true);
        h.on_key(HID_LALT, true);
        assert_eq!(h.on_key(HID_ESCAPE, true), (None, false), "Ctrl+Alt+Esc isn't ours");
        h.on_key(HID_ESCAPE, false);
        h.on_key(HID_RSHIFT, true);
        assert_eq!(h.on_key(HID_ESCAPE, true), (Some(Hotkey::Emergency), true));
        assert_eq!(h.on_key(HID_ESCAPE, true), (None, true), "auto-repeat fires once");
        assert_eq!(h.on_key(HID_ESCAPE, false), (None, true));
        assert_eq!(h.on_key(HID_RIGHT, true), (Some(Hotkey::Switch(Side::Right)), true));
        h.on_key(HID_RIGHT, false);
        h.on_key(HID_LALT, false);
        assert_eq!(h.on_key(HID_LEFT, true), (None, false));
    }
}
