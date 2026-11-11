//! A stand-in OS layer for tests: records what would be injected and lets a
//! test feed what the hooks would capture.

use super::geometry::Screen;
use super::protocol::Os;
use super::session::Inject;
use super::{Captured, Permission, Platform, Sink};
use std::sync::Mutex;

pub struct MockPlatform {
    os: Os,
    screens: Vec<Screen>,
    sink: Mutex<Option<Sink>>,
    pub injected: Mutex<Vec<Inject>>,
    /// Every `set_remote` call: (remote, place).
    pub remote: Mutex<Vec<(bool, Option<(f64, f64)>)>>,
    pub granted: Mutex<bool>,
}

impl MockPlatform {
    pub fn new(os: Os, screens: Vec<Screen>) -> Self {
        Self {
            os,
            screens,
            sink: Mutex::new(None),
            injected: Mutex::new(Vec::new()),
            remote: Mutex::new(Vec::new()),
            granted: Mutex::new(true),
        }
    }

    pub fn feed(&self, event: Captured) {
        if let Some(sink) = self.sink.lock().unwrap().as_ref() {
            sink(event);
        }
    }

    pub fn injected(&self) -> Vec<Inject> {
        self.injected.lock().unwrap().clone()
    }

    pub fn is_remote(&self) -> bool {
        self.remote.lock().unwrap().last().is_some_and(|r| r.0)
    }
}

impl Platform for MockPlatform {
    fn os(&self) -> Os {
        self.os
    }
    fn screens(&self) -> Vec<Screen> {
        self.screens.clone()
    }
    fn start(&self, sink: Sink) -> Result<(), String> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn set_remote(&self, remote: bool, place: Option<(f64, f64)>) {
        self.remote.lock().unwrap().push((remote, place));
    }
    fn set_active(&self, _active: bool) {}
    fn inject(&self, events: &[Inject]) {
        self.injected.lock().unwrap().extend_from_slice(events);
    }
    fn permission(&self) -> Permission {
        Permission { granted: *self.granted.lock().unwrap(), hint: String::new() }
    }
}
