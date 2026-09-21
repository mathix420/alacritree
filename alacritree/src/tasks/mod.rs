//! Task lists kept in taskwarrior and shared between agents and humans.
//! Taskwarrior owns the tasks; this module names where one belongs, runs
//! `task`, and shapes the result for the tab and the agent hooks.

pub(crate) mod facts;
pub(crate) mod scope;
pub(crate) mod taskwarrior;
pub(crate) mod tree;
