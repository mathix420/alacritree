//! Task lists shared between agents and humans, kept in a store alacritree
//! does not own. The tab and the agent hook ask a [`TaskBackend`] for the
//! tasks under some project nodes and hand it one [`Edit`] at a time, so no
//! store's own query or modifier syntax leaves its backend.

// The trait's signatures are copied verbatim into the app crate by
// ambassador's delegation macro, so they name types by absolute path, and
// this crate must answer to its own name for those paths to resolve here too.
extern crate self as alacritree_tasks;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod scope;
pub mod tree;

use serde::Deserialize;

use crate::scope::GLOBAL;

/// Where a task stands. A store's other states, such as deleted or waiting,
/// stay out of a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Completed,
}

/// One task as a store lists it.
#[derive(Clone, Debug, PartialEq, Deserialize, schemars::JsonSchema)]
pub struct Task {
    /// What every edit addresses the task by. Stable for the task's life.
    pub id: String,
    pub description: String,
    pub status: Status,
    /// Work on the task has begun.
    #[serde(default)]
    pub started: bool,
    /// The `id` of the task this one nests under. A parent the listing does
    /// not hold leaves the task at the top level.
    #[serde(default)]
    pub parent: Option<String>,
    /// Position among the task's siblings, lowest first. Tasks without one
    /// follow the ordered ones, oldest first.
    #[serde(default)]
    pub order: Option<i64>,
    /// The project node the task belongs to, such as `repo.branch`. Absent
    /// is the global list.
    #[serde(default)]
    pub project: Option<String>,
    /// When the task was created. Compared as text, so it needs a format
    /// that sorts by time, such as ISO 8601 in UTC.
    #[serde(default)]
    pub entry: Option<String>,
    /// When the task last changed, compared the same way as `entry`.
    #[serde(default)]
    pub modified: Option<String>,
}

impl Task {
    /// The node the task belongs to, the global list when it names none.
    pub fn node(&self) -> &str {
        self.project.as_deref().unwrap_or(GLOBAL)
    }
}

/// Which project nodes a listing covers. A task matching none of them stays
/// out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    pub nodes: Vec<NodeMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeMatch {
    /// Tasks on exactly this node.
    Exact(String),
    /// Tasks on this node or any node below it. `r` covers `r.main` and
    /// `r.main.codex-1`, never a sibling repository named `r-web`.
    Subtree(String),
}

impl Filter {
    pub fn matches(&self, task: &Task) -> bool {
        let node = task.node();
        self.nodes.iter().any(|m| match m {
            NodeMatch::Exact(n) => node == n,
            NodeMatch::Subtree(n) => node
                .strip_prefix(n.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.')),
        })
    }
}

/// One change to the store. Every edit but `Add` names an existing task.
#[derive(Clone, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Edit {
    Add {
        project: String,
        description: String,
        parent: Option<String>,
        order: i64,
    },
    /// Nests the task under `parent`, or at the top level when `None`, at
    /// `order` among its new siblings.
    Move {
        id: String,
        parent: Option<String>,
        order: i64,
    },
    Reorder {
        id: String,
        order: i64,
    },
    Describe {
        id: String,
        description: String,
    },
    Done(String),
    /// Back to pending after `Done`.
    Undone(String),
    Start(String),
    Stop(String),
    Delete(String),
}

/// `program` is the command the backend ran, as the user would recognise it.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{program} not found")]
    Missing { program: String },
    #[error("{program} failed: {}", stderr.trim())]
    Failed { program: String, stderr: String },
    #[error("{program} could not run: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} did not list tasks: {source}")]
    Malformed {
        program: String,
        #[source]
        source: serde_json::Error,
    },
}

#[ambassador::delegatable_trait]
pub trait TaskBackend {
    /// The tasks on `side` that `filter` covers, pending and completed only.
    /// Blocks, so it runs on a pool worker.
    fn list(
        &self,
        side: &::alacritree_common::side::Side,
        filter: &::alacritree_tasks::Filter,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<
        ::std::vec::Vec<::alacritree_tasks::Task>,
        ::alacritree_tasks::TaskError,
    >;

    /// Blocks, so it runs on a pool worker.
    fn apply(
        &self,
        side: &::alacritree_common::side::Side,
        edit: &::alacritree_tasks::Edit,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<(), ::alacritree_tasks::TaskError>;

    /// What an agent is told about writing its own tasks to `project`, ahead
    /// of the lists it is shown.
    fn agent_guide(&self, project: &str) -> ::std::string::String;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(project: Option<&str>) -> Task {
        Task {
            id: "t".into(),
            description: "t".into(),
            status: Status::Pending,
            started: false,
            parent: None,
            order: None,
            project: project.map(Into::into),
            entry: None,
            modified: None,
        }
    }

    #[test]
    fn a_subtree_keeps_other_repositories_out() {
        let filter = Filter { nodes: vec![NodeMatch::Subtree("r".into())] };
        assert!(filter.matches(&on(Some("r"))));
        assert!(filter.matches(&on(Some("r.main.codex-1"))));
        assert!(!filter.matches(&on(Some("r-web"))));
        assert!(!filter.matches(&on(Some("rr.main"))));
    }

    #[test]
    fn a_task_without_a_project_is_global() {
        let filter = Filter { nodes: vec![NodeMatch::Exact(GLOBAL.into())] };
        assert!(filter.matches(&on(None)));
        assert!(!filter.matches(&on(Some("r"))));
    }
}
