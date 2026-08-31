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
    interactive_running: usize,
    background_running: usize,
}

/// Which class a running task occupies, so the guard that frees its slot
/// knows which counter to decrement.
#[derive(Clone, Copy)]
enum Slot {
    Interactive,
    Background,
}

/// The next task this worker may run, and the slot it occupies.  Each class
/// is capped one below the worker count, so neither can shut the other out:
/// a click never queues behind a pool full of git walks, and a burst of
/// creates never stops a status refresh.  Interactive keeps first refusal.
fn take(state: &mut State, workers: usize) -> Option<(Task, Slot)> {
    if state.interactive_running + 1 < workers {
        if let Some(task) = state.interactive.pop_front() {
            state.interactive_running += 1;
            return Some((task, Slot::Interactive));
        }
    }
    if state.background_running + 1 < workers {
        if let Some(task) = state.background.pop_front() {
            state.background_running += 1;
            return Some((task, Slot::Background));
        }
    }
    None
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    workers: usize,
    /// How this pool wakes whatever is watching a job.  A callback rather than
    /// an `egui::Context`, so the pool stays free of the UI toolkit; anything
    /// with no screen — the CLI, the tests that only want a result — leaves it
    /// unset and the wake is a no-op.
    wake_ui: OnceLock<Box<dyn Fn() + Send + Sync>>,
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
    /// `workers` is clamped to at least two: each class's ceiling needs one
    /// worker beyond the one it holds free.
    pub fn new(workers: usize) -> Self {
        let workers = workers.max(2);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            workers,
            wake_ui: OnceLock::new(),
        });
        for _ in 0..workers {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || worker(shared));
        }
        Self { shared }
    }

    /// Register the wake-up this pool runs after every job.  The first
    /// registration wins; later ones are ignored.
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.shared.wake_ui.set(Box::new(wake));
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

/// Releases a worker's slot on drop, whether the task returned normally or
/// unwound through a panic — a straight-line decrement after the call would
/// never run for a panicking job, permanently shrinking the pool.
struct SlotGuard<'a> {
    shared: &'a Shared,
    slot: Slot,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("the job queue is poisoned");
        match self.slot {
            Slot::Interactive => state.interactive_running -= 1,
            Slot::Background => state.background_running -= 1,
        }
        drop(state);
        // A freed slot may admit a task another worker is asleep on.
        self.shared.wake.notify_all();
    }
}

/// Wakes the UI when a job ends, whether it returned or unwound.  A job that
/// panics never reaches whatever repaint its own closure would have asked for,
/// and [`Job::failed`] is only ever read from a frame — without this, a modal
/// with nothing else driving repaints sits on its last state until the user
/// happens to move the mouse.
struct WakeOnEnd<'a> {
    shared: &'a Shared,
}

impl Drop for WakeOnEnd<'_> {
    fn drop(&mut self) {
        if let Some(wake) = self.shared.wake_ui.get() {
            wake();
        }
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let mut state = shared.state.lock().expect("the job queue is poisoned");
        let (task, slot) = loop {
            if let Some(taken) = take(&mut state, shared.workers) {
                break taken;
            }
            state = shared.wake.wait(state).expect("the job queue is poisoned");
        };
        drop(state);

        let _slot = SlotGuard { shared: &shared, slot };

        if !task.cancelled.load(Ordering::Relaxed) {
            lower_this_thread(matches!(slot, Slot::Background));
            let _wake = WakeOnEnd { shared: &shared };
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

/// Block on the calling thread, deliberately.  The CLI, the IPC connection
/// threads, and construction before the first frame all have nothing on
/// screen waiting on them, so blocking is correct there.  A named entry point
/// rather than a public constructor, so the exception is one reviewable call
/// instead of a habit.
pub fn on_this_thread<T>(f: impl FnOnce(&Blocking) -> T) -> T {
    f(&Blocking(()))
}

/// The process-wide pool.  Sized for IO-bound work — subprocesses and git
/// walks that spend their time waiting, not saturating a core — so the range
/// is chosen for concurrency headroom rather than derived from core count;
/// `available_parallelism` only seeds where in that range a given machine
/// starts.
pub fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 8));
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
        let (_, slot) = take(&mut state, 4).expect("a runnable task");
        assert!(matches!(slot, Slot::Interactive));
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

    /// The regression test for the wake: a closure that unwinds never reaches
    /// its own repaint, so without the pool's own wake-up nothing brings the
    /// frame that would read [`Job::failed`].  The wake is only worth anything
    /// if the failure is already on the channel when it fires, which is what
    /// the guard's position in `worker` buys — hence the second half.
    #[test]
    fn a_panicking_job_wakes_the_ui_with_its_failure_already_readable() {
        let pool = Pool::new(2);
        let (woken_tx, woken_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        // The waker holds its worker until the assertions below have run, so
        // "what the frame sees when the wake arrives" is exactly what the pool
        // had done by then — nothing the worker does afterwards can drift into
        // the window and make a wrong order look right.  `Sender` and
        // `Receiver` are `Send` but not `Sync`, hence the mutex.
        let gate = Mutex::new((woken_tx, release_rx));
        pool.set_waker(move || {
            let gate = gate.lock().expect("the wake gate is poisoned");
            let _ = gate.0.send(());
            let _ = gate.1.recv();
        });
        let job = pool.spawn(Priority::Background, |_: &Blocking| -> u32 { panic!("boom") });

        assert!(
            woken_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a panicked job must wake the loop that reads its failure"
        );
        assert!(job.poll().is_none(), "a panicking job never reports a value");
        assert!(job.failed(), "the failure must be readable on the frame the wake brings");
        let _ = release_tx.send(());
    }

    #[test]
    fn the_calling_thread_can_take_a_token_explicitly() {
        assert_eq!(on_this_thread(|_| 3_u8), 3);
    }

    /// Interactive work must not be able to take every worker.  A pool with all
    /// its workers on interactive jobs cannot refresh a git status, poll worktree
    /// liveness, or look up a PR, and nothing on screen says why.
    #[test]
    fn interactive_work_leaves_a_worker_for_background() {
        let pool = Pool::new(4);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let mut held = Vec::new();
        for _ in 0..4 {
            let rx = Arc::clone(&release_rx);
            held.push(pool.spawn(Priority::Interactive, move |_| {
                let _ = rx.lock().expect("the release channel is poisoned").recv();
            }));
        }

        let (ran_tx, ran_rx) = mpsc::channel();
        let background = pool.spawn(Priority::Background, move |_| {
            let _ = ran_tx.send(());
        });

        assert!(
            ran_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the background job never ran: interactive work took every worker"
        );

        for _ in 0..4 {
            let _ = release_tx.send(());
        }
        drop(held);
        drop(background);
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
