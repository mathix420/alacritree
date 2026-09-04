//! Bookkeeping for PTYs opened on a worker.
//!
//! A session's record exists from the frame that asked for it, but its PTY
//! arrives some frames later.  This holds the receivers in between, and the
//! IPC replies parked until a caller's session is actually live — a client
//! that creates a session in order to write to it would otherwise race its
//! own PTY.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use crate::ipc::IpcResult;
use crate::session::{Attachment, SessionId};

struct Pending {
    rx: Receiver<std::io::Result<Attachment>>,
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
    pub fn start(&mut self, id: SessionId, rx: Receiver<std::io::Result<Attachment>>) {
        self.pending.insert(id, Pending { rx, waiters: Vec::new() });
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
            match pending.rx.try_recv() {
                Ok(result) => resolved.push((*id, Some(result))),
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => resolved.push((*id, None)),
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
                        std::io::Error::other("the session's PTY worker stopped"),
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
    //! case here drives the `Failed` path instead: `take_finished` treats a
    //! `Err` from the worker and a dropped sender the same way it treats
    //! `Ok`, so `Failed` alone exercises the retain/remove/answer plumbing
    //! this module owns. `Finished::Opened` is covered app-side in Task 6,
    //! where a real session is already being spawned for other reasons.
    use super::*;
    use std::sync::mpsc;

    /// Nothing is opening for `id` any more: `watch` hands a waiter straight
    /// back rather than parking it on an entry that will never resolve.
    fn forgotten(spawns: &mut PendingSpawns, id: SessionId) -> bool {
        let (reply_tx, _reply_rx) = mpsc::channel();
        spawns.watch(id, reply_tx).is_some()
    }

    #[test]
    fn a_finished_open_comes_back_failed_with_its_id() {
        let (open_tx, open_rx) = mpsc::channel();
        let mut spawns = PendingSpawns::default();
        spawns.start(1, open_rx);

        open_tx.send(Err(std::io::Error::other("no such shell"))).unwrap();
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
    fn a_worker_that_drops_its_sender_comes_back_failed_not_stuck_pending() {
        let (open_tx, open_rx) = mpsc::channel::<std::io::Result<Attachment>>();
        let mut spawns = PendingSpawns::default();
        spawns.start(1, open_rx);
        drop(open_tx);

        let finished = spawns.take_finished();

        assert_eq!(finished.len(), 1);
        assert!(matches!(&finished[0], Finished::Failed(1, _, _)));
        assert!(
            forgotten(&mut spawns, 1),
            "a dropped sender must not leave a pending entry that never resolves"
        );
    }

    #[test]
    fn a_waiter_parked_with_watch_is_answered_once_the_open_resolves() {
        let (open_tx, open_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();

        let mut spawns = PendingSpawns::default();
        spawns.start(1, open_rx);
        assert!(spawns.watch(1, reply_tx).is_none(), "a pending open takes the waiter");

        open_tx.send(Err(std::io::Error::other("boom"))).unwrap();
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
    fn watching_an_id_nothing_is_opening_hands_the_channel_back() {
        let (reply_tx, _reply_rx) = mpsc::channel();
        let mut spawns = PendingSpawns::default();

        assert!(spawns.watch(1, reply_tx).is_some());
    }
}
