//! One pool for every piece of work that must not run on the UI thread.
//!
//! A handler that gathers its content synchronously cannot draw until the
//! gathering returns, which under CPU load is seconds.  Work goes here
//! instead, and the `Blocking` token makes that structural: the helpers that
//! block take one, only a pool worker is handed one, so calling such a helper
//! from `update` does not compile.

use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

/// Whether anything on screen is waiting for the job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    /// A pending state is showing until this lands.
    Interactive,
    /// Housekeeping nobody is looking at: status polls, PR lookups, liveness.
    Background,
}

/// Proof that the holder runs on a pool worker.  The constructor is private to
/// this module, so a blocking helper that takes one cannot be called from the
/// UI thread.
pub struct Blocking(());

struct Task {
    cancelled: Arc<AtomicBool>,
    run: Box<dyn FnOnce(&Blocking) + Send>,
    /// Reports a caught panic to the `Job`'s channel.  Kept separate from
    /// `run` because `run` moves its sender into a closure that drops it,
    /// unsent, on unwind — this holds a clone that survives that unwind so
    /// the caller sees an explicit failure instead of a channel that just
    /// went quiet.
    on_failure: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
struct State {
    interactive: VecDeque<Task>,
    background: VecDeque<Task>,
    background_running: usize,
}

/// The next task this worker may run, and whether it occupies a background
/// slot.  Background work is capped one below the worker count so a click
/// never queues behind a pool full of git walks.
fn take(state: &mut State, workers: usize) -> Option<(Task, bool)> {
    if let Some(task) = state.interactive.pop_front() {
        return Some((task, false));
    }
    if state.background_running + 1 < workers {
        if let Some(task) = state.background.pop_front() {
            state.background_running += 1;
            return Some((task, true));
        }
    }
    None
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    workers: usize,
}

/// A pool of worker threads that run for the life of the process. There is
/// no shutdown path — a dropped `Pool` leaks its threads, parked forever on
/// the empty queue. Harmless for the process-wide singleton this crate uses;
/// don't construct one you intend to drop.
pub struct Pool {
    shared: Arc<Shared>,
}

/// A submitted job.  Dropping the handle cancels the work if it has not
/// started, so a status scan for a workspace the user has left stops costing
/// a core.
pub struct Job<T> {
    rx: mpsc::Receiver<Result<T, JobFailed>>,
    cancelled: Arc<AtomicBool>,
    /// Latched by `poll` the moment it drains a failure off the channel, so
    /// the signal survives every `poll` after that one too — `poll` itself
    /// can only report it on the one call that observes it, since its `T`
    /// return has no room for "failed".
    failed: Cell<bool>,
}

/// A job's closure unwound instead of returning.  Carries no data — the
/// panic itself is already logged from the pool worker that caught it, so
/// `Job::failed` is only ever asked "did it happen", not "with what".
pub struct JobFailed;

impl<T> Job<T> {
    /// The result if it has landed.  Never blocks.
    ///
    /// A job whose closure panicked reports through [`Job::failed`] instead
    /// of a value here — this returns `None` for it, same as "hasn't landed
    /// yet".  Poll every frame and check `failed` after: the failure only
    /// latches on the `poll` call that drains it off the channel.
    pub fn poll(&self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(Ok(value)) => Some(value),
            Ok(Err(JobFailed)) => {
                self.failed.set(true);
                None
            },
            Err(_) => None,
        }
    }

    /// Whether the job's closure panicked, as observed by a previous `poll`.
    /// `false` for a job still running, and for one that panicked but hasn't
    /// been polled since — call `poll` first on every frame, then check this.
    pub fn failed(&self) -> bool {
        self.failed.get()
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Pool {
    /// `workers` is clamped to at least two: the background reservation needs
    /// one worker beyond the one it holds free.
    pub fn new(workers: usize) -> Self {
        let workers = workers.max(2);
        let shared =
            Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new(), workers });
        for _ in 0..workers {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || worker(shared));
        }
        Self { shared }
    }

    #[must_use = "dropping the handle cancels the job"]
    pub fn spawn<T, F>(&self, priority: Priority, f: F) -> Job<T>
    where
        F: FnOnce(&Blocking) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let fail_tx = tx.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = Task {
            cancelled: Arc::clone(&cancelled),
            run: Box::new(move |blocking| {
                let _ = tx.send(Ok(f(blocking)));
            }),
            on_failure: Box::new(move || {
                let _ = fail_tx.send(Err(JobFailed));
            }),
        };
        let mut state = self.shared.state.lock().expect("the job queue is poisoned");
        match priority {
            Priority::Interactive => state.interactive.push_back(task),
            Priority::Background => state.background.push_back(task),
        }
        drop(state);
        self.shared.wake.notify_one();
        Job { rx, cancelled, failed: Cell::new(false) }
    }
}

/// Releases a worker's background slot on drop, whether the task returned
/// normally or unwound through a panic — a straight-line decrement after the
/// call would never run for a panicking job, permanently shrinking the pool.
struct BackgroundSlot<'a> {
    shared: &'a Shared,
}

impl Drop for BackgroundSlot<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("the job queue is poisoned");
        state.background_running -= 1;
        drop(state);
        // A freed slot may admit a task another worker is asleep on.
        self.shared.wake.notify_all();
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let mut state = shared.state.lock().expect("the job queue is poisoned");
        let (task, was_background) = loop {
            if let Some(taken) = take(&mut state, shared.workers) {
                break taken;
            }
            state = shared.wake.wait(state).expect("the job queue is poisoned");
        };
        drop(state);

        let _slot = was_background.then(|| BackgroundSlot { shared: &shared });

        if !task.cancelled.load(Ordering::Relaxed) {
            lower_this_thread(was_background);
            let outcome = catch_unwind(AssertUnwindSafe(|| (task.run)(&Blocking(()))));
            if let Err(panic) = outcome {
                log::error!("a job panicked: {}", panic_message(&panic));
                (task.on_failure)();
            }
        }
    }
}

/// A job's closure and its `Send` payload panicked; the only recovery that
/// keeps the pool alive is to log it here, since `Job::failed` reports only
/// that a panic happened, not what it said.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Housekeeping should yield to the UI thread when the CPU is contended, and a
/// worker outlives one job, so the class is set per job rather than at spawn.
#[cfg(windows)]
fn lower_this_thread(background: bool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
    };
    let level = if background { THREAD_PRIORITY_BELOW_NORMAL } else { THREAD_PRIORITY_NORMAL };
    unsafe { SetThreadPriority(GetCurrentThread(), level) };
}

#[cfg(not(windows))]
fn lower_this_thread(_background: bool) {}

/// Block on the calling thread, deliberately.  The CLI and the IPC connection
/// threads have no window and nothing to paint, so blocking is correct there.
/// A named entry point rather than a public constructor, so the exception is
/// one reviewable call instead of a habit.
pub fn on_this_thread<T>(f: impl FnOnce(&Blocking) -> T) -> T {
    f(&Blocking(()))
}

/// The process-wide pool.  Sized for work that waits on subprocesses and the
/// filesystem rather than work that saturates a core, so a wide pool would only
/// multiply concurrent git walks.
pub fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 4));
        Pool::new(workers)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn state_with(interactive: usize, background: usize) -> State {
        let mut state = State::default();
        for _ in 0..interactive {
            state.interactive.push_back(Task {
                cancelled: Arc::new(AtomicBool::new(false)),
                run: Box::new(|_| {}),
                on_failure: Box::new(|| {}),
            });
        }
        for _ in 0..background {
            state.background.push_back(Task {
                cancelled: Arc::new(AtomicBool::new(false)),
                run: Box::new(|_| {}),
                on_failure: Box::new(|| {}),
            });
        }
        state
    }

    #[test]
    fn interactive_work_goes_first() {
        let mut state = state_with(1, 1);
        let (_, was_background) = take(&mut state, 4).expect("a runnable task");
        assert!(!was_background);
        assert_eq!(state.background.len(), 1, "the background task is still queued");
    }

    #[test]
    fn a_worker_stays_free_for_interactive_work() {
        let mut state = state_with(0, 3);
        assert!(take(&mut state, 2).is_some(), "the first background task runs");
        assert!(take(&mut state, 2).is_none(), "the second would leave no worker for a click");
        assert_eq!(state.background.len(), 2);
    }

    #[test]
    fn a_finished_background_task_frees_its_slot() {
        let mut state = state_with(0, 2);
        take(&mut state, 2).expect("the first background task runs");
        state.background_running -= 1;
        assert!(take(&mut state, 2).is_some(), "the freed slot admits the next one");
    }

    #[test]
    fn a_result_reaches_the_handle() {
        let pool = Pool::new(2);
        let job = pool.spawn(Priority::Interactive, |_| 7_u32);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = job.poll() {
                assert_eq!(value, 7);
                assert!(!job.failed(), "a job that reported a value did not fail");
                return;
            }
            assert!(Instant::now() < deadline, "the job never reported");
            std::thread::yield_now();
        }
    }

    fn poll_until<T>(job: &Job<T>, timeout: Duration) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = job.poll() {
                return value;
            }
            assert!(Instant::now() < deadline, "the job never reported");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_second_background_job_runs_after_the_first_finishes() {
        let pool = Pool::new(2);
        let first = pool.spawn(Priority::Background, |_| 1_u32);
        assert_eq!(poll_until(&first, Duration::from_secs(5)), 1);
        let second = pool.spawn(Priority::Background, |_| 2_u32);
        assert_eq!(
            poll_until(&second, Duration::from_secs(5)),
            2,
            "the slot the first job held must be free for the second"
        );
    }

    #[test]
    fn a_panicking_background_job_frees_its_slot() {
        let pool = Pool::new(2);
        let panicking = pool.spawn(Priority::Background, |_: &Blocking| -> u32 { panic!("boom") });
        let next = pool.spawn(Priority::Background, |_| 5_u32);
        assert_eq!(
            poll_until(&next, Duration::from_secs(5)),
            5,
            "a panicking job must not permanently occupy its background slot"
        );
        drop(panicking);
    }

    /// The regression test for `Job::failed`: a panicked closure must not
    /// merely stop resolving, it must report through `failed` once `poll`
    /// has drained the failure off the channel.
    #[test]
    fn a_panicking_jobs_failure_is_observable_through_failed() {
        let pool = Pool::new(2);
        let job = pool.spawn(Priority::Background, |_: &Blocking| -> u32 { panic!("boom") });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.failed() {
            assert!(job.poll().is_none(), "a panicking job never reports a value");
            assert!(Instant::now() < deadline, "the failure was never observed");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_job_still_running_has_not_failed() {
        let pool = Pool::new(1);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let job = pool.spawn(Priority::Background, move |_| {
            let _ = release_rx.recv();
        });
        assert!(job.poll().is_none(), "the job has not landed yet");
        assert!(!job.failed(), "a job merely still running has not failed");
        let _ = release_tx.send(());
    }

    #[test]
    fn the_calling_thread_can_take_a_token_explicitly() {
        assert_eq!(on_this_thread(|_| 3_u8), 3);
    }

    #[test]
    fn dropping_the_handle_cancels_work_that_has_not_started() {
        // `Pool::new` floors the worker count at two, which leaves exactly one
        // background slot.  Holding that slot busy keeps the second submission
        // queued until after its handle drops.
        let pool = Pool::new(1);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocker = pool.spawn(Priority::Background, move |_| {
            let _ = release_rx.recv();
        });
        let (ran_tx, ran_rx) = mpsc::channel::<()>();
        drop(pool.spawn(Priority::Background, move |_| {
            let _ = ran_tx.send(());
        }));
        let _ = release_tx.send(());
        drop(blocker);
        assert!(
            ran_rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a cancelled task must not run"
        );
    }
}
