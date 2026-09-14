//! The waker a background task holds to tell the UI it has something new.
//!
//! Workers only ever need to say "look again", so they take this trait rather
//! than the GUI framework's context, and tests can count the wakes instead of
//! standing up a context nothing ever paints.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) trait Repaint: Clone + Send + Sync + 'static {
    /// Ask for a frame as soon as possible.
    fn wake(&self);

    /// Ask for a frame no later than `delay` from now.
    fn wake_after(&self, delay: Duration);
}

impl Repaint for egui::Context {
    fn wake(&self) {
        self.request_repaint();
    }

    fn wake_after(&self, delay: Duration) {
        self.request_repaint_after(delay);
    }
}

/// Records every wake so a test can assert that a worker woke the UI.
/// Clones share one record, as clones of a context share one window.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Recorder {
    wakes: Arc<AtomicUsize>,
    delayed: Arc<Mutex<Vec<Duration>>>,
}

#[cfg(test)]
impl Recorder {
    /// Immediate wakes so far.
    pub(crate) fn wakes(&self) -> usize {
        self.wakes.load(Ordering::SeqCst)
    }

    /// The delay of each deferred wake so far, in call order.
    pub(crate) fn delayed_wakes(&self) -> Vec<Duration> {
        self.delayed.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Repaint for Recorder {
    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_after(&self, delay: Duration) {
        self.delayed.lock().unwrap().push(delay);
    }
}
