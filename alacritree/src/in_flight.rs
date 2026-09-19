//! Jobs keyed by what they work on, with IPC replies parked until each result
//! has been applied.
//!
//! A client that asks for work to act on its outcome would race its own
//! request if it were answered the moment the job started.  Every parked
//! reply is answered once, and a reply dropped unanswered, as a panicked job
//! leaves it, is answered with an error.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::mpsc::Sender;

use crate::ipc::protocol::IpcResult;
use crate::jobs::Job;

const ABANDONED: &str = "the work this request waited on was abandoned";

pub struct InFlight<K, T> {
    running: HashMap<K, Running<T>>,
}

struct Running<T> {
    job: Job<T>,
    waiters: Waiters,
}

/// A job that ended, with whoever was parked on it.  The entry is gone from
/// the map by the time this exists, so the waiters it carries are the only
/// handle left on them.
pub struct Finished<K, T> {
    pub key: K,
    /// `None` when the job's closure panicked.
    pub outcome: Option<T>,
    pub waiters: Waiters,
}

/// Replies parked on one job.  Dropping them unanswered still answers each
/// one, so no client waits on a reply that never comes.
#[derive(Default)]
pub struct Waiters(Vec<Sender<IpcResult>>);

impl Waiters {
    pub fn answer(mut self, reply: IpcResult) {
        for waiter in self.0.drain(..) {
            // A send error means the client gave up waiting.
            let _ = waiter.send(reply.clone());
        }
    }
}

impl Drop for Waiters {
    fn drop(&mut self) {
        for waiter in self.0.drain(..) {
            let _ = waiter.send(Err(ABANDONED.to_string()));
        }
    }
}

impl<K, T> Default for InFlight<K, T> {
    fn default() -> Self {
        Self { running: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone, T> InFlight<K, T> {
    /// Start the job `spawn` makes for `key`, unless one is already running
    /// for it, which a second request joins instead of doubling the work.
    /// Returns whether `spawn` ran.
    pub fn start(&mut self, key: K, spawn: impl FnOnce() -> Job<T>) -> bool {
        if self.running.contains_key(&key) {
            return false;
        }
        self.running.insert(key, Running { job: spawn(), waiters: Waiters::default() });
        true
    }

    /// Park `reply_tx` until the job running for `key` has finished.  Hands
    /// the channel back when nothing is running for it, leaving the caller to
    /// answer it however it sees fit.
    pub fn watch(
        &mut self,
        key: impl Borrow<K>,
        reply_tx: Sender<IpcResult>,
    ) -> Option<Sender<IpcResult>> {
        match self.running.get_mut(key.borrow()) {
            Some(running) => {
                running.waiters.0.push(reply_tx);
                None
            },
            None => Some(reply_tx),
        }
    }

    /// Take every job that has ended.  Runs every frame, so it allocates only
    /// when something did end.
    pub fn take_finished(&mut self) -> Vec<Finished<K, T>> {
        // `poll` before `failed`: a panic latches only on the `poll` that
        // drains it off the channel.  Deciding and removing are two passes
        // because the first one only borrows the map.
        let mut ended = Vec::new();
        for (key, running) in &self.running {
            match running.job.poll() {
                Some(value) => ended.push((key.clone(), Some(value))),
                None if running.job.failed() => ended.push((key.clone(), None)),
                None => {},
            }
        }
        ended
            .into_iter()
            .map(|(key, outcome)| {
                let running = self.running.remove(&key).expect("just observed above");
                Finished { key, outcome, waiters: running.waiters }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;
    use crate::jobs::{Pool, Priority};

    /// Poll until something ends, since a pool job lands on its own schedule.
    fn finished_within<K: Eq + Hash + Clone, T>(
        in_flight: &mut InFlight<K, T>,
        timeout: Duration,
    ) -> Vec<Finished<K, T>> {
        let deadline = Instant::now() + timeout;
        loop {
            let finished = in_flight.take_finished();
            if !finished.is_empty() || Instant::now() > deadline {
                return finished;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_waiter_is_answered_only_with_the_reply_made_from_the_result() {
        let pool = Pool::new(2);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (reply_tx, reply_rx) = mpsc::channel();
        let mut in_flight = InFlight::default();

        in_flight.start("/a", || {
            pool.spawn(Priority::Interactive, move |_| {
                let _ = release_rx.recv();
                7
            })
        });
        assert!(in_flight.watch("/a", reply_tx).is_none(), "a running job takes the waiter");

        assert!(in_flight.take_finished().is_empty(), "nothing has ended yet");
        assert!(
            matches!(reply_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "answering before the result is applied would let the caller act on stale data"
        );

        release_tx.send(()).unwrap();
        let mut finished = finished_within(&mut in_flight, Duration::from_secs(5));
        let Finished { key, outcome, waiters } = finished.remove(0);
        assert_eq!((key, outcome), ("/a", Some(7)));
        waiters.answer(Ok(json!({ "value": 7 })));

        assert_eq!(reply_rx.try_recv().unwrap(), Ok(json!({ "value": 7 })));
        let (late_tx, _late_rx) = mpsc::channel();
        assert!(in_flight.watch("/a", late_tx).is_some(), "a finished job is forgotten");
    }

    #[test]
    fn starting_a_key_already_running_joins_the_running_job() {
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let mut in_flight = InFlight::default();

        assert!(in_flight.start(1, || Job::ready("done")));
        assert!(in_flight.watch(1, first_tx).is_none());
        let started = in_flight.start(1, || panic!("a second job must not be spawned"));
        assert!(!started);
        assert!(in_flight.watch(1, second_tx).is_none());

        let mut finished = in_flight.take_finished();
        assert_eq!(finished.len(), 1, "one job ran for both requests");
        finished.remove(0).waiters.answer(Ok(json!("done")));

        assert_eq!(first_rx.try_recv().unwrap(), Ok(json!("done")));
        assert_eq!(second_rx.try_recv().unwrap(), Ok(json!("done")));
    }

    #[test]
    fn a_job_that_panics_answers_every_waiter_with_an_error() {
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let mut in_flight = InFlight::<u64, ()>::default();
        in_flight.start(1, Job::panicked);
        in_flight.watch(1, first_tx);
        in_flight.watch(1, second_tx);

        let finished = in_flight.take_finished();

        assert_eq!(finished.len(), 1, "a panicked job must not stay pending");
        assert!(finished[0].outcome.is_none());
        drop(finished);
        assert!(first_rx.try_recv().unwrap().is_err(), "a caller must never block forever");
        assert!(second_rx.try_recv().unwrap().is_err());
    }

    #[test]
    fn dropping_a_job_before_it_is_taken_answers_its_waiters() {
        let (reply_tx, reply_rx) = mpsc::channel();
        let mut in_flight = InFlight::default();
        in_flight.start(1, || Job::ready(()));
        in_flight.watch(1, reply_tx);

        drop(in_flight);

        assert_eq!(reply_rx.try_recv().unwrap(), Err(ABANDONED.to_string()));
    }

    #[test]
    fn watching_a_key_with_nothing_running_hands_the_channel_back() {
        let (reply_tx, _reply_rx) = mpsc::channel();
        let mut in_flight = InFlight::<u64, ()>::default();

        assert!(in_flight.watch(1, reply_tx).is_some());
    }
}
