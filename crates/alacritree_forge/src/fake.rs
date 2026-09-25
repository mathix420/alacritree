//! A forge that answers from a script and records what it was asked, for
//! tests of the code that schedules lookups. Clones share one record, so a
//! test keeps a clone to read after handing the forge away.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritree_common::jobs::Blocking;

use crate::{ForgeError, Head, PrInfo, PullRequests, RemoteForge};

#[derive(Debug, Clone, Default)]
pub struct FakeForge {
    prs: HashMap<String, PrInfo>,
    failing: Vec<String>,
    calls: Arc<Mutex<Vec<Vec<Head>>>>,
}

impl FakeForge {
    /// Answers `pr` for any checkout on `branch`, and no PR for the rest.
    pub fn with_pr(mut self, branch: &str, pr: PrInfo) -> Self {
        self.prs.insert(branch.to_string(), pr);
        self
    }

    /// Fails the lookup of any checkout on `branch`.
    pub fn failing_on(mut self, branch: &str) -> Self {
        self.failing.push(branch.to_string());
        self
    }

    /// The heads of each call so far, in call order.
    pub fn calls(&self) -> Vec<Vec<Head>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl RemoteForge for FakeForge {
    fn pull_requests(&self, heads: Vec<Head>, _: &Blocking) -> PullRequests {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(heads.clone());
        heads
            .into_iter()
            .map(|head| {
                let answer = if self.failing.contains(&head.branch) {
                    Err(ForgeError::Malformed { program: "fake" })
                } else {
                    Ok(self.prs.get(&head.branch).cloned())
                };
                (head.path, answer)
            })
            .collect()
    }
}
