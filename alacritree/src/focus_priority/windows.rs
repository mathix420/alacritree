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
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JobObjectBasicLimitInformation, SetInformationJobObject,
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
}

impl PriorityJob {
    /// Create a job and put `pid` in it, or `None` if either step is refused.
    ///
    /// Taking a pid rather than the caller's handle is safe here because the
    /// caller holds one: a process cannot have its number reused while any
    /// handle to it is open.
    pub fn adopt(pid: u32) -> Option<Self> {
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
        Some(Self { job, boosted: Cell::new(false) })
    }

    /// Raise every member one class above the load, or return them all to
    /// normal.  Focus asks for this every frame, so an unchanged state costs
    /// nothing.
    pub fn set_boosted(&self, boosted: bool) {
        if self.boosted.get() == boosted {
            return;
        }
        self.boosted.set(boosted);
        // The limit reaches members already running as well as ones yet to
        // start, in both directions, so releasing the boost is this same call
        // rather than a walk over the members.
        self.set_limit(JOB_OBJECT_LIMIT_PRIORITY_CLASS, class(boosted));
    }

    fn set_limit(&self, flags: u32, class: u32) {
        let limits = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: flags,
            PriorityClass: class,
            ..unsafe { std::mem::zeroed() }
        };
        let set = unsafe {
            SetInformationJobObject(
                self.job.0,
                JobObjectBasicLimitInformation,
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
    /// keeps running.  Both steps matter: setting the class to normal is what
    /// lowers those survivors, and clearing the limit is what stops a job
    /// nobody holds any more from pinning them there for good.
    fn drop(&mut self) {
        self.set_boosted(false);
        self.set_limit(0, NORMAL_PRIORITY_CLASS);
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    use windows_sys::Win32::System::Threading::{GetPriorityClass, PROCESS_QUERY_INFORMATION};

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
    #[test]
    fn a_vanished_pid_is_ignored() {
        let pid = {
            let subject = subject();
            subject.pid()
        };

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
        let job = PriorityJob::adopt(pid).expect("put the subject in a job");

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
        let job = PriorityJob::adopt(subject.pid()).expect("put the subject in a job");
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
        let job = PriorityJob::adopt(pid).expect("put the subject in a job");
        job.set_boosted(true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS);

        drop(job);
        assert_eq!(class_of(pid), NORMAL_PRIORITY_CLASS);

        set_boosted(pid, true);
        assert_eq!(class_of(pid), ABOVE_NORMAL_PRIORITY_CLASS, "the dropped job still pins it");
    }
}
