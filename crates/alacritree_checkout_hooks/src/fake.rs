//! A hook that records what it receives, for tests on either side of the
//! trait. Clones share one log, so a test keeps a clone to read after
//! handing the hook away.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alacritree_common::jobs::Blocking;

use crate::{CheckoutEvent, CheckoutHook, HookError, Outcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Created { main: PathBuf, checkout: PathBuf },
    Opened { main: PathBuf, checkout: PathBuf },
    Removed { main: PathBuf, checkout: PathBuf },
}

#[derive(Debug, Clone, Default)]
pub struct FakeHook {
    events: Arc<Mutex<Vec<Event>>>,
    line: Option<String>,
    fails: bool,
}

impl FakeHook {
    pub fn silent() -> Self {
        Self::default()
    }

    pub fn reporting(line: &str) -> Self {
        Self { line: Some(line.to_string()), ..Self::default() }
    }

    pub fn failing() -> Self {
        Self { fails: true, ..Self::default() }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, event: Event) -> Outcome {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
        if self.fails {
            return Err(HookError::Spawn {
                hook: "fake".into(),
                source: std::io::Error::other("scripted failure"),
            });
        }
        Ok(self.line.clone())
    }
}

impl CheckoutHook for FakeHook {
    fn on_created(&self, e: &CheckoutEvent<'_>, _: &Blocking) -> Outcome {
        self.record(Event::Created { main: e.main.into(), checkout: e.checkout.into() })
    }

    fn on_opened(&self, e: &CheckoutEvent<'_>, _: &Blocking) -> Outcome {
        self.record(Event::Opened { main: e.main.into(), checkout: e.checkout.into() })
    }

    fn on_removed(&self, e: &CheckoutEvent<'_>, _: &Blocking) -> Outcome {
        self.record(Event::Removed { main: e.main.into(), checkout: e.checkout.into() })
    }
}
