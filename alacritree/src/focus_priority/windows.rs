//! The boost on Windows: a job object per session, the class following focus.
//!
//! The measurement behind it: against 64 spinning threads a nushell prompt
//! echoed a character in 128 ms at the median and 1.9 s at the 95th
//! percentile, while the same PTY stack hosting `cmd.exe` — which answers from
//! its own read loop and needs no CPU — stayed at 0.2 ms.
//!
//! Windows does not spread the class on its own: `CreateProcess` gives a new
//! process the normal class unless its creator is at *idle* or *below* normal,
//! so a raise only ever travels downward and nothing here can leak.  That also
//! means raising a shell reaches neither an agent running inside it nor the
//! command it has just started, and a boost that goes looking for those misses
//! everything living less than one scan — which is what a short command on a
//! saturated machine is.  A job object closes that gap: a process created by a
//! member joins the job and is *born* at the job's class.
//!
//! So a session owns a [`PriorityJob`] and focus moves the class between them.
//! [`set_self_boosted`] covers alacritree's own process, which needs raising
//! for the same reason: the job reaches every depth, so a focused tab running
//! `cargo build -j16` raises all sixteen compilers, and a GUI left at normal
//! would lose to the tree it is drawing.

use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, SetPriorityClass,
};

/// A handle this module opened and is responsible for closing.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The class a boost puts its subject at, or the one it returns it to.
fn class(boosted: bool) -> u32 {
    if boosted { ABOVE_NORMAL_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS }
}

/// Put `pid` one class above the load, or return it to normal.
///
/// Best effort by design.  A process that exits between being listed and being
/// opened, or one this user may not touch, is skipped: the cost of missing it
/// is the latency that was there anyway.
fn set_boosted(pid: u32, boosted: bool) {
    let handle = unsafe { OpenProcess(PROCESS_SET_INFORMATION, 0, pid) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        log::debug!("could not open {pid} to set its priority: {}", io::Error::last_os_error());
        return;
    }
    let handle = Owned(handle);

    if unsafe { SetPriorityClass(handle.0, class(boosted)) } == 0 {
        log::debug!("{pid} refused a priority class: {}", io::Error::last_os_error());
    }
}

/// Raise alacritree itself alongside whatever session holds the boost, or put
/// it back once nothing does.
///
/// Asked for every frame, so the state is remembered and an unchanged answer
/// costs no syscall.
pub fn set_self_boosted(boosted: bool) {
    static SELF_BOOSTED: AtomicBool = AtomicBool::new(false);
    if SELF_BOOSTED.swap(boosted, Ordering::Relaxed) != boosted {
        set_boosted(std::process::id(), boosted);
    }
}

/// A job object holding one session's shell, and through it everything the
/// shell goes on to start.
pub struct PriorityJob {
    job: Owned,
    boosted: Cell<bool>,
    /// Whether closing this job ends what it holds.  Fixed at creation, since
    /// the flag has to be in place before the members exist.
    reaping: bool,
}

/// The limits a job carries regardless of focus.
///
/// Kill-on-close is what makes the job answer for its members' lifetime: the
/// console only reaps its own clients, so a descendant that left the console —
/// an editor's search helper, anything started detached — outlives the session
/// unless the job ends it.  Breakaway is the way out for a process that means
/// to outlive the terminal: it must ask with `CREATE_BREAKAWAY_FROM_JOB`, and
/// without this flag that request fails rather than being granted.
fn lifetime_limits(reaping: bool) -> u32 {
    if reaping { JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK } else { 0 }
}

impl PriorityJob {
    /// Create a job and put `pid` in it, or `None` if either step is refused.
    ///
    /// Taking a pid rather than the caller's handle is safe here because the
    /// caller holds one: a process cannot have its number reused while any
    /// handle to it is open.
    pub fn adopt(pid: u32, reaping: bool) -> Option<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            log::debug!("could not create a job for {pid}: {}", io::Error::last_os_error());
            return None;
        }
        let job = Owned(job);

        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            log::debug!("could not open {pid} to job it: {}", io::Error::last_os_error());
            return None;
        }
        let handle = Owned(handle);

        // The shell is already in conhost's job; Windows accepts a nested one.
        if unsafe { AssignProcessToJobObject(job.0, handle.0) } == 0 {
            log::debug!("{pid} refused job assignment: {}", io::Error::last_os_error());
            return None;
        }
        let job = Self { job, boosted: Cell::new(false), reaping };
        job.set_limit(lifetime_limits(reaping), NORMAL_PRIORITY_CLASS);
        Some(job)
    }

    /// Raise every member one class above the load, or return them all to
    /// normal.  Focus asks for this every frame, so an unchanged state costs
    /// nothing.
    pub fn set_boosted(&self, boosted: bool) {
        if self.boosted.get() == boosted {
            return;
        }
        self.boosted.set(boosted);
        let base = lifetime_limits(self.reaping);
        // The limit reaches members already running as well as ones yet to
        // start, in both directions, so releasing the boost is this same call
        // rather than a walk over the members.
        self.set_limit(base | JOB_OBJECT_LIMIT_PRIORITY_CLASS, class(boosted));
        if !boosted {
            // A job's class is a ceiling, not a setting: while the limit
            // stands, every member is held at normal and cannot raise itself.
            // Naming normal is what lowers the ones already running, and
            // clearing the limit afterwards is what gives them back their own
            // class — an unfocused session must cost its processes nothing.
            self.set_limit(base, NORMAL_PRIORITY_CLASS);
        }
    }

    /// Kill-on-close lives in the extended structure, and every set replaces
    /// `LimitFlags` whole, so the lifetime limits have to travel with the
    /// priority one or a change of focus would quietly drop them.
    fn set_limit(&self, flags: u32, class: u32) {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        limits.BasicLimitInformation.PriorityClass = class;
        let set = unsafe {
            SetInformationJobObject(
                self.job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if set == 0 {
            log::debug!("the job refused a priority limit: {}", io::Error::last_os_error());
        }
    }
}

impl Drop for PriorityJob {
    /// A job outlives the last handle to it for as long as it still has
    /// members, and a session's tab can be closed while something it started
    /// keeps running.  A reaping job ends those survivors as the last handle
    /// closes, so there is nothing left to put back; one that carries only the
    /// boost has to release it, which both lowers them and leaves them free to
    /// set their own class.
    fn drop(&mut self) {
        self.set_boosted(false);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::num::NonZeroU32;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{
        GetPriorityClass, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use crate::command_ext::CommandExt as _;
    use super::*;

    /// A child that sits still for the length of a test without spinning a
    /// core: `pause` blocks on a piped stdin nobody writes to.
    struct Subject(Child);

    impl Drop for Subject {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Subject {
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    fn spawn(args: [&str; 2]) -> Subject {
        Subject(
            Command::new("cmd.exe")
                .args(args)
                .hide_console()
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn cmd.exe"),
        )
    }

    fn subject() -> Subject {
        spawn(["/c", "pause"])
    }

    /// A subject that starts a child of its own, so a test can ask what that
    /// child was born at.
    fn subject_with_a_child() -> Subject {
        spawn(["/c", "ping -n 30 127.0.0.1 > nul"])
    }

    /// The first child of `parent`, waited for: a shell takes a moment to get
    /// its command started.
    fn child_of(parent: u32) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sys = System::new();
        loop {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            let child = sys
                .processes()
                .iter()
                .find(|(_, p)| p.parent().map(|pp| pp.as_u32()) == Some(parent))
                .map(|(pid, _)| pid.as_u32());
            if child.is_some() || Instant::now() > deadline {
                return child;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn class_of(pid: u32) -> u32 {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
        assert!(!handle.is_null(), "open {pid} for query");
        let handle = Owned(handle);
        unsafe { GetPriorityClass(handle.0) }
    }

    #[test]
    fn a_boost_is_applied_and_taken_back() {
        let subject = subject();
        let pid = subject.pid();
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);

        set_boosted(pid, true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS);

        set_boosted(pid, false);
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);
    }

    /// Windows only spreads a priority class downward, so a child of a boosted
    /// process starts at normal.  This is what keeps a raise from leaking, and
    /// it is also why the job exists: without one, the command a boosted shell
    /// just started competes with the load on equal terms.
    #[test]
    fn a_child_does_not_inherit_the_boost() {
        // The boosted process has to be the one doing the spawning, so this
        // test raises itself rather than a stand-in, and lowers itself before
        // asserting so a failure cannot leave the runner elevated.
        let me = std::process::id();
        set_boosted(me, true);
        let child = subject();
        let inherited = class_of(child.pid());
        set_boosted(me, false);

        assert_eq!(inherited, NORMAL_PRIORITY_CLASS);
    }

    /// Nothing here may panic on a pid that has gone, because the set is taken
    /// from a process table that is stale the moment it is read.
    ///
    /// The number has to be one the kernel cannot hand to anything, not one
    /// this test reaped: a pid is free for reuse the moment its last handle
    /// closes, so boosting a reaped one reaches whichever process picked it
    /// up next.  Windows numbers processes in multiples of four, which the
    /// probe below confirms rather than assumes.
    #[test]
    fn a_vanished_pid_is_ignored() {
        let pid = u32::MAX;
        let live = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(live.is_null(), "{pid} is a live process; boosting it would reach a stranger");

        set_boosted(pid, true);
        set_boosted(pid, false);
    }

    /// A member's own class follows the job's limit in both directions, which
    /// is what lets releasing the boost be one call rather than a walk over
    /// the members.
    #[test]
    fn a_job_raises_and_lowers_the_process_it_holds() {
        let subject = subject();
        let pid = subject.pid();
        let job = PriorityJob::adopt(pid, false).expect("put the subject in a job");

        job.set_boosted(true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS);

        job.set_boosted(false);
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);
    }

    /// The whole reason for the job: a process born inside it comes up at the
    /// class with nothing having to notice it started.  A boost that had to
    /// find it first would miss anything shorter than one scan interval, which
    /// is exactly what a short command under load is.
    #[test]
    fn a_process_born_in_the_job_comes_up_boosted() {
        let subject = subject_with_a_child();
        let job = PriorityJob::adopt(subject.pid(), false).expect("put the subject in a job");
        job.set_boosted(true);

        let child = child_of(subject.pid()).expect("the subject started a child");
        assert_eq!(class_of(child), ABOVE_NORMAL_PRIORITY_CLASS);
    }

    /// A closing tab must leave nothing raised behind it, and nothing held
    /// down either: a process the job held can outlive the session, and a job
    /// whose last handle has gone still enforces whatever limit it was left
    /// with.  Lowering the survivor is only half the job; it also has to be
    /// free to set its own class again.
    #[test]
    fn dropping_the_job_lowers_what_it_raised_without_pinning_it() {
        let subject = subject();
        let pid = subject.pid();
        let job = PriorityJob::adopt(pid, false).expect("put the subject in a job");
        job.set_boosted(true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS);

        drop(job);
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);

        set_boosted(pid, true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS, "the dropped job still pins it");
    }

    /// Losing focus has to hand the members back their own class, not hold
    /// them at normal.  A session that keeps the limit standing caps
    /// everything it is running for as long as it is not the focused tab —
    /// including anything that raises itself, which is what a build or an
    /// agent under the shell does.
    #[test]
    fn releasing_the_boost_leaves_the_members_free_to_raise_themselves() {
        let subject = subject();
        let pid = subject.pid();
        let job = PriorityJob::adopt(pid, false).expect("put the subject in a job");
        job.set_boosted(true);
        job.set_boosted(false);
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);

        set_boosted(pid, true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS, "the unfocused job caps it");
    }

    /// One process of a session's tree, as it stood before the teardown.
    struct Member {
        pid: u32,
        name: String,
        /// Whether the kernel counted it as part of the session's job.  A
        /// survivor that was never a member means the model is wrong; one
        /// that was means kill-on-close is.
        in_job: Option<bool>,
    }

    impl fmt::Display for Member {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}({})", self.name, self.pid)?;
            match self.in_job {
                Some(true) => f.write_str(" in the job"),
                Some(false) => f.write_str(" outside the job"),
                None => Ok(()),
            }
        }
    }

    fn render(members: &[Member]) -> String {
        members.iter().map(Member::to_string).collect::<Vec<_>>().join(", ")
    }

    /// A real ConPTY session torn down, and what outlived it.
    struct Teardown {
        /// False when `ClosePseudoConsole` had not returned by the deadline.
        /// It blocks until the conout pipe drains, so a session whose shell
        /// never exits wedges the drop rather than reporting anything.
        closed: bool,
        /// Everything the session was running before the teardown.
        started: Vec<Member>,
        /// The subset of those still running afterwards.
        survivors: Vec<Member>,
    }

    /// Every pid descended from `root`, `root` included, from one snapshot.
    fn tree_of(sys: &System, root: u32) -> Vec<u32> {
        let parents: Vec<(u32, Option<u32>)> = sys
            .processes()
            .iter()
            .map(|(pid, p)| (pid.as_u32(), p.parent().map(|pp| pp.as_u32())))
            .collect();
        let mut tree = vec![root];
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for (pid, _) in parents.iter().filter(|(_, pp)| *pp == Some(parent)) {
                tree.push(*pid);
                frontier.push(*pid);
            }
        }
        tree
    }

    fn snapshot() -> System {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        sys
    }

    fn name_of(sys: &System, pid: u32) -> String {
        sys.process(Pid::from_u32(pid))
            .map(|p| p.name().to_string_lossy().into_owned())
            .unwrap_or_else(|| "gone".to_owned())
    }

    /// Ask the kernel whether `pid` is a member of `job`, so a failure can say
    /// which of the two halves — the model or the flag — is at fault.
    fn is_in_job(pid: u32, job: HANDLE) -> Option<bool> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let handle = Owned(handle);
        let mut member = 0;
        let asked = unsafe { IsProcessInJob(handle.0, job, &mut member) };
        (asked != 0).then_some(member != 0)
    }

    /// Wait for `root` to have grown a tree of at least `want` processes, so a
    /// teardown is never asserted against a session that had not started yet.
    fn tree_once_grown(root: u32, want: usize) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let tree = tree_of(&snapshot(), root);
            if tree.len() >= want || Instant::now() > deadline {
                return tree;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Which of `members` are still running once they have had `grace` to go.
    fn still_running_after(members: &[Member], grace: Duration) -> Vec<Member> {
        let deadline = Instant::now() + grace;
        loop {
            let sys = snapshot();
            let alive: Vec<Member> = members
                .iter()
                .filter(|m| sys.process(Pid::from_u32(m.pid)).is_some())
                .map(|m| Member { pid: m.pid, name: m.name.clone(), in_job: m.in_job })
                .collect();
            if alive.is_empty() || Instant::now() > deadline {
                return alive;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Run `shell` inside a real pseudoconsole, wait for its tree to come up,
    /// then close the console and report what survived.
    ///
    /// `job` is the only difference between the arms: `None` leaves the
    /// session unjobbed, `Some(reaping)` jobs it with or without kill-on-close.
    /// Everything runs on a worker thread because closing a console can block
    /// forever, and a teardown that never returns is a result here rather than
    /// a reason to hang the suite.
    fn tear_down_a_conpty_session(job: Option<bool>, shell: Shell, want: usize) -> Teardown {
        let (members_tx, members_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let options = PtyOptions {
                shell: Some(shell),
                working_directory: None,
                drain_on_exit: false,
                env: HashMap::new(),
                escape_args: false,
            };
            let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
            crate::harden_dll_search_path();
            let pty = tty::new(&options, size, 0).expect("open a pseudoconsole");
            let root = pty.child_watcher().pid().map(NonZeroU32::get).expect("the shell's pid");
            // Jobbed before the tree is waited for, as a session does it: a
            // process joins a job when it is created, so anything already
            // running when the job appears stays outside it for good.
            let job = job.map(|reaping| PriorityJob::adopt(root, reaping).expect("job the shell"));
            let started = tree_once_grown(root, want);

            let sys = snapshot();
            let members: Vec<Member> = started
                .into_iter()
                .map(|pid| Member {
                    pid,
                    name: name_of(&sys, pid),
                    in_job: job.as_ref().and_then(|j| is_in_job(pid, j.job.0)),
                })
                .collect();

            members_tx.send(members).expect("report the session's tree");
            // Session drops its fields, the job among them, before the event
            // loop gets to the PTY, so the job goes first here too.
            drop(job);
            drop(pty);
            let _ = closed_tx.send(());
        });

        let started =
            members_rx.recv_timeout(Duration::from_secs(45)).expect("the session came up");
        let closed = closed_rx.recv_timeout(Duration::from_secs(20)).is_ok();

        // Closing the console returns before the kernel has finished tearing
        // the clients down, so a survivor is one still there after a grace
        // period rather than one still there the instant the close returns.
        let survivors = still_running_after(&started, Duration::from_secs(5));
        // A survivor would outlive the whole suite, and the arm that expects
        // one still has to clean up after itself.
        for member in &survivors {
            let pid = member.pid.to_string();
            let _ =
                Command::new("taskkill").args(["/F", "/T", "/PID", &pid]).hide_console().output();
        }
        Teardown { closed, started, survivors }
    }

    /// A shell and the command it is running, both clients of the console.
    fn console_clients() -> Shell {
        Shell::new("cmd.exe".into(), vec!["/c".into(), "ping -n 60 127.0.0.1 > nul".into()])
    }

    /// A shell that starts a process with no console of its own, which is the
    /// shape of the leak: a helper an editor spawns for completions is a child
    /// of the session but not a client of its console.
    ///
    /// Only a process already inside the session can start one, so the session
    /// runs this test binary again and [`a_child_that_leaves_the_console`] does
    /// the spawning.
    fn a_shell_that_escapes_its_console() -> Shell {
        let exe = std::env::current_exe().expect("the test binary's own path");
        Shell::new(exe.display().to_string(), vec![
            "--exact".into(),
            "focus_priority::windows::tests::a_child_that_leaves_the_console".into(),
            "--ignored".into(),
        ])
    }

    /// Not a test on its own: the escaping child that
    /// [`a_shell_that_escapes_its_console`] runs as a session's shell.
    ///
    /// `DETACHED_PROCESS` is what does the escaping — the child gets no
    /// console at all, so it is nobody's console client and closing the
    /// session's console has no claim on it.  It also spawns no window and no
    /// console host, so a test run leaves the desktop alone.
    #[test]
    #[ignore = "the escaping child a reaping test runs as its session's shell"]
    fn a_child_that_leaves_the_console() {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;

        Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .creation_flags(DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a child outside the console");
        std::thread::sleep(Duration::from_secs(60));
    }

    /// The baseline the jobbed arms are measured against.  Closing a
    /// pseudoconsole ends the shell and the command it is running, because
    /// conhost keeps its clients in a job it kills on close.  A failure here
    /// is the harness, not the feature.
    #[test]
    fn closing_a_pseudoconsole_reaps_its_console_clients() {
        let torn = tear_down_a_conpty_session(None, console_clients(), 2);
        assert!(torn.closed, "closing the pseudoconsole never returned");
        assert!(torn.started.len() >= 2, "no command ran: {}", render(&torn.started));
        assert!(torn.survivors.is_empty(), "outlived the console: {}", render(&torn.survivors));
    }

    /// The session's job nests inside conhost's, and conhost's is what reaps
    /// the console's clients.  A job that cost the session that reaping would
    /// buy typing latency with processes that outlive the terminal.
    #[test]
    fn a_job_does_not_cost_the_session_its_reaping() {
        let torn = tear_down_a_conpty_session(Some(true), console_clients(), 2);
        assert!(torn.closed, "closing the jobbed pseudoconsole never returned");
        assert!(torn.started.len() >= 2, "no command ran: {}", render(&torn.started));
        assert!(
            torn.survivors.is_empty(),
            "outlived a jobbed console: {}",
            render(&torn.survivors)
        );
    }

    /// The leak the feature exists for.  A console reaps its clients and
    /// nothing else, so a descendant that left it is stranded by the teardown
    /// and runs until the machine is rebooted.  Without this the reaping arm
    /// below could pass on a session that never escaped anything.
    #[test]
    fn the_console_alone_strands_what_leaves_it() {
        let torn = tear_down_a_conpty_session(None, a_shell_that_escapes_its_console(), 2);
        assert!(torn.closed, "closing the pseudoconsole never returned");
        assert!(torn.started.len() >= 2, "nothing left the console: {}", render(&torn.started));
        assert!(
            !torn.survivors.is_empty(),
            "the console reaped what left it, so there is no leak to fix"
        );
    }

    /// What the console cannot reach, the job has to: it holds every
    /// descendant at every depth, console client or not, which makes it the
    /// only thing placed to end them.
    #[test]
    fn a_reaping_job_ends_what_leaves_the_console() {
        let torn = tear_down_a_conpty_session(Some(true), a_shell_that_escapes_its_console(), 2);
        assert!(torn.closed, "closing the jobbed pseudoconsole never returned");
        assert!(torn.started.len() >= 2, "nothing left the console: {}", render(&torn.started));
        assert!(torn.survivors.is_empty(), "stranded by the teardown: {}", render(&torn.survivors));
    }
}
