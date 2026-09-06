//! Bookkeeping for PTYs opened on a worker.
//!
//! A session's record exists from the frame that asked for it, but its PTY
//! arrives some frames later.  This holds the open jobs in between, and the
//! IPC replies parked until a caller's session is actually live — a client
//! that creates a session in order to write to it would otherwise race its
//! own PTY.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use crate::ipc::IpcResult;
use crate::jobs::Job;
use crate::session::{Attachment, SessionId};

struct Pending {
    job: Job<std::io::Result<Attachment>>,
    waiters: Vec<Sender<IpcResult>>,
}

#[derive(Default)]
pub struct PendingSpawns {
    pending: HashMap<SessionId, Pending>,
}

/// An open that resolved, with whoever was parked on it.  `take_finished`
/// removes the `Pending` entry as soon as it decides an open is done,
/// waiters included, so this is the only place those waiters still exist —
/// answering them has to happen from what this carries out, not by looking
/// the id back up afterwards.
pub enum Finished {
    Opened(SessionId, Attachment, Vec<Sender<IpcResult>>),
    Failed(SessionId, std::io::Error, Vec<Sender<IpcResult>>),
}

impl PendingSpawns {
    pub fn start(&mut self, id: SessionId, job: Job<std::io::Result<Attachment>>) {
        self.pending.insert(id, Pending { job, waiters: Vec::new() });
    }

    /// Park `reply_tx` until the session's PTY is live.  Hands the channel
    /// back when nothing is opening for that id, leaving the caller to answer
    /// it however it sees fit.
    pub fn watch(
        &mut self,
        id: SessionId,
        reply_tx: Sender<IpcResult>,
    ) -> Option<Sender<IpcResult>> {
        match self.pending.get_mut(&id) {
            Some(pending) => {
                pending.waiters.push(reply_tx);
                None
            },
            None => Some(reply_tx),
        }
    }

    /// Take every open that has finished.  The workspace a session belongs to
    /// is deliberately not stored here: a pending session can be moved to
    /// another workspace, so the caller reads it off the record it finds.
    pub fn take_finished(&mut self) -> Vec<Finished> {
        // `retain` cannot move `waiters` out of the `Pending` it inspects, so
        // the receive and the removal are two passes: this one decides what
        // finished without touching the map, the next one takes those
        // entries out whole.
        let mut resolved = Vec::new();
        for (id, pending) in self.pending.iter_mut() {
            match pending.job.poll() {
                Some(result) => resolved.push((*id, Some(result))),
                // A worker that unwound reports through `failed`, and only
                // after the `poll` that drains the failure off the channel —
                // so this order is the one that sees it.
                None if pending.job.failed() => resolved.push((*id, None)),
                None => {},
            }
        }

        resolved
            .into_iter()
            .map(|(id, result)| {
                let waiters = self.pending.remove(&id).expect("just observed above").waiters;
                match result {
                    Some(Ok(attachment)) => Finished::Opened(id, attachment, waiters),
                    Some(Err(e)) => Finished::Failed(id, e, waiters),
                    None => Finished::Failed(
                        id,
                        std::io::Error::other("the session's PTY worker panicked"),
                        waiters,
                    ),
                }
            })
            .collect()
    }

    /// Answer `waiters` with `reply`.  `take_finished` hands them over with
    /// the result they were parked on, so there is no id to look up here.
    pub fn answer(waiters: Vec<Sender<IpcResult>>, reply: IpcResult) {
        for waiter in waiters {
            let _ = waiter.send(reply.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    //! `Attachment` only comes from `session::open`, which spawns a real PTY —
    //! too slow and too platform-dependent for this module's tests. Every
    //! case here drives the `Failed` path instead: `take_finished` treats an
    //! `Err` from the worker and a panicked worker the same way it treats
    //! `Ok`, so `Failed` alone exercises the retain/remove/answer plumbing
    //! this module owns. `Finished::Opened` is covered app-side, where a real
    //! session is already being spawned for other reasons.
    use super::*;
    use std::sync::mpsc;

    fn refused(reason: &str) -> Job<std::io::Result<Attachment>> {
        Job::ready(Err(std::io::Error::other(reason)))
    }

    /// Nothing is opening for `id` any more: `watch` hands a waiter straight
    /// back rather than parking it on an entry that will never resolve.
    fn forgotten(spawns: &mut PendingSpawns, id: SessionId) -> bool {
        let (reply_tx, _reply_rx) = mpsc::channel();
        spawns.watch(id, reply_tx).is_some()
    }

    #[test]
    fn a_finished_open_comes_back_failed_with_its_id() {
        let mut spawns = PendingSpawns::default();
        spawns.start(1, refused("no such shell"));

        let finished = spawns.take_finished();

        assert_eq!(finished.len(), 1);
        match &finished[0] {
            Finished::Failed(id, e, _) => {
                assert_eq!(*id, 1);
                assert_eq!(e.to_string(), "no such shell");
            },
            Finished::Opened(..) => panic!("expected Failed"),
        }
        assert!(forgotten(&mut spawns, 1), "a finished open is forgotten");
    }

    #[test]
    fn a_worker_that_panicked_comes_back_failed_not_stuck_pending() {
        let mut spawns = PendingSpawns::default();
        spawns.start(1, Job::panicked());

        let finished = spawns.take_finished();

        assert_eq!(finished.len(), 1);
        assert!(matches!(&finished[0], Finished::Failed(1, _, _)));
        assert!(
            forgotten(&mut spawns, 1),
            "a panicked open must not leave a pending entry that never resolves"
        );
    }

    #[test]
    fn a_waiter_parked_with_watch_is_answered_once_the_open_resolves() {
        let (reply_tx, reply_rx) = mpsc::channel();

        let mut spawns = PendingSpawns::default();
        spawns.start(1, refused("boom"));
        assert!(spawns.watch(1, reply_tx).is_none(), "a pending open takes the waiter");

        let mut finished = spawns.take_finished();
        assert_eq!(finished.len(), 1);
        let Finished::Failed(id, e, waiters) = finished.remove(0) else {
            panic!("expected Failed")
        };
        assert_eq!(id, 1);
        PendingSpawns::answer(waiters, Err(e.to_string()));

        assert_eq!(reply_rx.try_recv().unwrap(), Err("boom".to_string()));
    }

    #[test]
    fn a_second_waiter_joins_the_open_already_running() {
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();

        let mut spawns = PendingSpawns::default();
        spawns.start(1, refused("boom"));
        assert!(spawns.watch(1, first_tx).is_none());
        assert!(spawns.watch(1, second_tx).is_none());

        let mut finished = spawns.take_finished();
        assert_eq!(finished.len(), 1);
        let Finished::Failed(_, e, waiters) = finished.remove(0) else { panic!("expected Failed") };
        PendingSpawns::answer(waiters, Err(e.to_string()));

        assert_eq!(first_rx.try_recv().unwrap(), Err("boom".to_string()));
        assert_eq!(second_rx.try_recv().unwrap(), Err("boom".to_string()));
    }

    #[test]
    fn watching_an_id_nothing_is_opening_hands_the_channel_back() {
        let (reply_tx, _reply_rx) = mpsc::channel();
        let mut spawns = PendingSpawns::default();

        assert!(spawns.watch(1, reply_tx).is_some());
    }
}
