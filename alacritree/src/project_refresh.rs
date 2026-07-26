//! Bookkeeping for worktree re-discovery, which always runs on a worker.
//!
//! Discovery opens the repository with git2, enumerates worktrees and resolves
//! the default branch — tens of milliseconds on a project with many worktrees,
//! far too much for the UI thread.
//!
//! IPC callers still block until the result is live rather than being answered
//! the moment the worker starts: a client that refreshes a project in order to
//! act on the new worktree list would otherwise race its own request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use crate::ipc::IpcResult;
use crate::projects::Discovered;

struct Pending {
    rx: Receiver<Discovered>,
    waiters: Vec<Sender<IpcResult>>,
}

#[derive(Default)]
pub struct ProjectRefreshes {
    pending: HashMap<PathBuf, Pending>,
}

impl ProjectRefreshes {
    pub fn is_running(&self, root: &Path) -> bool {
        self.pending.contains_key(root)
    }

    pub fn start(&mut self, root: PathBuf, rx: Receiver<Discovered>) {
        self.pending.insert(root, Pending { rx, waiters: Vec::new() });
    }

    /// Park `reply_tx` until the refresh running for `root` has been applied.
    /// Hands the channel back when nothing is running, leaving the caller to
    /// answer it however it sees fit.
    pub fn watch(&mut self, root: &Path, reply_tx: Sender<IpcResult>) -> Option<Sender<IpcResult>> {
        match self.pending.get_mut(root) {
            Some(pending) => {
                pending.waiters.push(reply_tx);
                None
            },
            None => Some(reply_tx),
        }
    }

    /// Adopt every finished discovery through `apply`, then answer whoever was
    /// waiting on it with the reply `apply` produced.
    pub fn poll(&mut self, mut apply: impl FnMut(&Path, Discovered) -> IpcResult) {
        self.pending.retain(|root, pending| match pending.rx.try_recv() {
            Ok(found) => {
                let reply = apply(root, found);
                for waiter in pending.waiters.drain(..) {
                    let _ = waiter.send(reply.clone());
                }
                false
            },
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                for waiter in pending.waiters.drain(..) {
                    let _ = waiter.send(Err("project refresh worker stopped".to_string()));
                }
                false
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Project;
    use serde_json::json;
    use std::sync::mpsc;

    fn discovered(root: &str) -> Discovered {
        Discovered { project: Project::placeholder(PathBuf::from(root)), authoritative: true }
    }

    #[test]
    fn a_waiter_is_answered_only_once_the_discovery_has_been_applied() {
        let root = PathBuf::from("/a");
        let (found_tx, found_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();

        let mut refreshes = ProjectRefreshes::default();
        refreshes.start(root.clone(), found_rx);
        assert!(refreshes.watch(&root, reply_tx).is_none(), "a running refresh takes the waiter");

        let mut applied = Vec::new();
        refreshes.poll(|root, _| {
            applied.push(root.to_path_buf());
            Ok(json!({}))
        });
        assert!(applied.is_empty(), "nothing to apply until the worker reports");
        assert!(
            matches!(reply_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "replying before the worktree list is live would let the caller act on stale data"
        );

        found_tx.send(discovered("/a")).unwrap();
        refreshes.poll(|root, _| {
            applied.push(root.to_path_buf());
            Ok(json!({ "root": root }))
        });

        assert_eq!(applied, vec![root.clone()]);
        assert_eq!(reply_rx.try_recv().unwrap(), Ok(json!({ "root": root })));
        assert!(!refreshes.is_running(&root), "a finished refresh is forgotten");
    }

    #[test]
    fn a_second_request_joins_the_refresh_already_running() {
        let root = PathBuf::from("/a");
        let (found_tx, found_rx) = mpsc::channel();
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();

        let mut refreshes = ProjectRefreshes::default();
        refreshes.start(root.clone(), found_rx);
        assert!(refreshes.watch(&root, first_tx).is_none());
        assert!(refreshes.watch(&root, second_tx).is_none());

        found_tx.send(discovered("/a")).unwrap();
        refreshes.poll(|_, _| Ok(json!({ "ok": true })));

        assert_eq!(first_rx.try_recv().unwrap(), Ok(json!({ "ok": true })));
        assert_eq!(second_rx.try_recv().unwrap(), Ok(json!({ "ok": true })));
    }

    #[test]
    fn watching_a_project_with_no_refresh_running_hands_the_channel_back() {
        let (reply_tx, _reply_rx) = mpsc::channel();
        let mut refreshes = ProjectRefreshes::default();

        assert!(refreshes.watch(Path::new("/a"), reply_tx).is_some());
    }

    #[test]
    fn a_worker_that_dies_still_answers_its_waiters() {
        let root = PathBuf::from("/a");
        let (found_tx, found_rx) = mpsc::channel::<Discovered>();
        let (reply_tx, reply_rx) = mpsc::channel();

        let mut refreshes = ProjectRefreshes::default();
        refreshes.start(root.clone(), found_rx);
        refreshes.watch(&root, reply_tx);
        drop(found_tx);

        refreshes.poll(|_, _| Ok(json!({})));

        assert!(reply_rx.try_recv().unwrap().is_err(), "a caller must never block forever");
        assert!(!refreshes.is_running(&root));
    }
}
