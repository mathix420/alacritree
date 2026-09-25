//! Task lists shared between agents and humans. A backend owns the tasks;
//! this module decides which backend runs, reads where a checkout sits, and
//! shapes the lists for the tab and the agent hooks.

pub(crate) mod backend;
pub(crate) mod facts;
pub(crate) mod hook;
pub(crate) mod view;
