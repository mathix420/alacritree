//! Why alacritree died.
//!
//! A release build is `windows_subsystem = "windows"`, so stderr goes nowhere
//! when it is launched from a shortcut and a panic leaves no trace at all.
//! This records one artifact per GUI process: single writer, never shared, so
//! no cross-process protocol is needed to keep it intact.

use std::backtrace::Backtrace;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::time::SystemTime;

use crate::logdir;

/// Whether a log directory has been chosen.  Read without the lock so the hook
/// can decline before contending for anything, and false until `install`, which
/// is what keeps the hook inert in unit tests that never opt in.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Defaults on so a panic during `config::load()` is still recorded; lowered
/// once the preference is known.
static ENABLED: AtomicBool = AtomicBool::new(true);
/// Latched by the first write failure so a broken disk is reported once, not
/// once per panic.
static BROKEN: AtomicBool = AtomicBool::new(false);
/// Panics the hook could not write because another thread held the lock.
static SKIPPED: AtomicUsize = AtomicUsize::new(0);
/// Latched by the first `record_reason` call so a later, less specific reason
/// (`window-closed` following an already-recorded `os-close-app`) cannot
/// overwrite it.
static REASON_RECORDED: AtomicBool = AtomicBool::new(false);

static STATE: Mutex<State> = Mutex::new(State::new());

/// How many panic records one process may write before it starts costing more
/// than it explains.
const MAX_PANIC_RECORDS: usize = 20;

struct State {
    version: &'static str,
    /// Where artifacts live.  Guarded rather than a `OnceLock` because every
    /// writer already holds this lock, and a directory that can only ever be
    /// set once is unreachable for a second test case.
    dir: Option<PathBuf>,
    /// The artifact this process has confirmed as its own, once `ensure_artifact`
    /// has created or reopened it.  Reused directly on every later call so a file
    /// that merely happens to already sit at our identity's path — debris from an
    /// unrelated writer — is never mistaken for ours; only a path we ourselves
    /// settled on through `create_new` is ever reopened for append.
    artifact: Option<PathBuf>,
    /// How many panic records this process has written, against the cap.
    panics: usize,
    /// Location of the previous panic, for collapsing a repeat that fires from
    /// the same place every frame.
    last: Option<String>,
    repeats: usize,
}

impl State {
    const fn new() -> Self {
        Self { version: "", dir: None, artifact: None, panics: 0, last: None, repeats: 0 }
    }
}

/// Arm the recorder.  Creates the directory but no file: an artifact is only
/// created once something is worth writing, so a launch with crash logging off
/// leaves nothing behind.
pub fn install(dir: &Path, version: &'static str) {
    if let Err(e) = logdir::prepare_log_dir(dir) {
        let _ = writeln!(std::io::stderr(), "alacritree: cannot secure {}: {e}", dir.display());
        BROKEN.store(true, Ordering::Relaxed);
        return;
    }
    {
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        state.version = version;
        state.dir = Some(dir.to_path_buf());
        state.artifact = None;
    }
    ARMED.store(true, Ordering::Relaxed);

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info);
        previous(info);
    }));
}

/// Point the recorder at `[debug] log_dir`, which is only known once the config
/// has been read.  Not a second `install`: `install` wraps the previous panic
/// hook, so a second call chains two hooks and records every panic twice.
///
/// Declines a directory it cannot secure, leaving the recorder on the one it
/// has.  Declines once an artifact exists, so one process cannot end up with
/// its records split across two directories.
pub fn set_dir(dir: &Path) {
    if let Err(e) = logdir::prepare_log_dir(dir) {
        let _ = writeln!(std::io::stderr(), "alacritree: cannot secure {}: {e}", dir.display());
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    if state.artifact.is_none() {
        state.dir = Some(dir.to_path_buf());
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Create the artifact for this session.  Called after the gate is known.
pub fn session_begin() {
    if !writable() {
        return;
    }
    match STATE.lock() {
        Ok(mut state) => {
            let _ = ensure_artifact(&mut state);
        },
        Err(poisoned) => {
            let _ = ensure_artifact(&mut poisoned.into_inner());
        },
    }
}

pub fn record_exit(result: &Result<(), eframe::Error>) {
    if !writable() {
        return;
    }
    let mut event = String::new();
    let skipped = SKIPPED.load(Ordering::Relaxed);
    if skipped > 0 {
        event.push_str(&line(&format!("{SKIPPED_MARKER} {skipped}")));
    }
    match result {
        Ok(()) => event.push_str(&line(EXIT_OK_MARKER)),
        Err(e) => event.push_str(&line(&format!("{EXIT_ERROR_MARKER} {e}"))),
    }

    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    flush_repeats(&mut guard);
    write_event(&mut guard, &event);
}

/// Record why the process is on its way out, the moment it becomes known —
/// not deferred to `record_exit` — so a process killed before `run_native`
/// returns still leaves the reason behind. First writer wins: the latch keeps
/// a later, less specific close (`window-closed` after an OS session end has
/// already recorded `os-close-app`) from overwriting the real reason.
pub fn record_reason(reason: ExitReason) {
    if !writable() {
        return;
    }
    if REASON_RECORDED.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }

    let event = line(&format!("{EXIT_REASON_MARKER} {}", reason.as_str()));
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    flush_repeats(&mut guard);
    write_event(&mut guard, &event);
}

fn writable() -> bool {
    ENABLED.load(Ordering::Relaxed)
        && !BROKEN.load(Ordering::Relaxed)
        && ARMED.load(Ordering::Relaxed)
}

fn timestamp() -> String {
    // Seconds since the epoch, rendered without a date crate: the artifact
    // name already carries the machine-readable start, so this only has to be
    // orderable and human-skimmable.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("t{secs}")
}

fn line(body: &str) -> String {
    format!("{} {body}\n", timestamp())
}

/// The vocabulary the writer emits and [`classify`] reads back.  Building both
/// sides from these constants, instead of each duplicating the literals, is
/// what keeps a renamed marker from silently breaking classification while
/// every hand-written test still passes.
pub const HEADER_MARKER: &str = "start ";
pub const PANIC_MARKER: &str = "PANIC thread=";
pub const SKIPPED_MARKER: &str = "panic records skipped:";
pub const EXIT_OK_MARKER: &str = "exit ok";
pub const EXIT_ERROR_MARKER: &str = "exit error:";
pub const EXIT_REASON_MARKER: &str = "exit reason:";

/// Why the process is on its way out, as far as alacritree could tell.
// The three OS reasons are only ever constructed by the Windows session-end
// hook, which does not compile elsewhere.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    UserQuit,
    WindowClosed,
    OsCloseApp,
    OsLogoff,
    OsShutdown,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::UserQuit => "user-quit",
            ExitReason::WindowClosed => "window-closed",
            // Windows Restart Manager: an installer wants a file this process
            // holds and is asking it to close before forcing the issue.
            ExitReason::OsCloseApp => "os-close-app",
            ExitReason::OsLogoff => "os-logoff",
            ExitReason::OsShutdown => "os-shutdown",
        }
    }
}

/// The single initializer.  The header has three possible authors — a panic
/// during config load, `session_begin`, and any write after the file has been
/// removed — and a record written into a headerless file has to be read back as
/// indeterminate, discarding information we actually had.
///
/// A file already sitting at our identity's path is reopened for append only if
/// it is the exact path this process itself settled on earlier; otherwise it is
/// left untouched and `create_new`'s collision retry claims the next ordinal
/// instead, so debris from an unrelated writer can never be corrupted by an
/// append and never gets mistaken for a readable header.
fn ensure_artifact(state: &mut State) -> Option<File> {
    let dir = state.dir.as_ref()?;

    if let Some(path) = &state.artifact
        && let Ok(file) = OpenOptions::new().append(true).open(path)
    {
        return Some(file);
    }
    // No confirmed artifact yet, or it was removed underneath us: either way,
    // fall through to the allocator below rather than losing the record.

    let mut id = logdir::process_id();
    // `create_new` is the allocator: a collision means debris under an
    // identity we believed unique, and truncating it would destroy a record.
    for _ in 0..32 {
        let path = dir.join(logdir::artifact_name(&id));
        match logdir::create_private_file(&path) {
            Ok(mut file) => {
                logdir::set_ordinal(id.ordinal);
                let header = line(&format!("{HEADER_MARKER}{} pid={}", state.version, id.pid));
                if file.write_all(header.as_bytes()).is_err() {
                    break;
                }
                let _ = file.flush();
                state.artifact = Some(path);
                return Some(file);
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => id.ordinal += 1,
            Err(_) => break,
        }
    }
    fail_once("cannot create a crash artifact");
    None
}

fn fail_once(what: &str) {
    if !BROKEN.swap(true, Ordering::Relaxed) {
        let _ = writeln!(std::io::stderr(), "alacritree: {what}; crash logging is off");
    }
}

/// One `write_all` of a fully built string, to a handle opened and closed per
/// event: if a panic ever does reach an abort, abort skips destructors, so
/// nothing may be left sitting in a buffer.
fn write_event(state: &mut State, event: &str) {
    let Some(mut file) = ensure_artifact(state) else { return };
    if file.write_all(event.as_bytes()).is_err() || file.flush().is_err() {
        fail_once("cannot write the crash artifact");
    }
}

fn record_panic(info: &PanicHookInfo<'_>) {
    if !writable() {
        return;
    }

    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed").to_string();
    let location = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "unknown location".to_string());
    let payload = payload_of(info);
    // Captures regardless of RUST_BACKTRACE, which we cannot set: `set_var` is
    // unsafe in edition 2024 and PTY threads are already running by now.
    let backtrace = Backtrace::force_capture();

    let mut event = line(&format!("{PANIC_MARKER}{thread}"));
    event.push_str(&format!("  at {location}\n"));
    event.push_str(&format!("  {payload}\n"));
    for bt_line in backtrace.to_string().lines() {
        event.push_str(&format!("  {bt_line}\n"));
    }

    // `try_lock`, never `lock`: a thread that panics while already holding this
    // mutex would wait on itself forever, and the mutex is not poisoned yet, so
    // recovering from poisoning cannot help.  A lost record beats a hang.
    match STATE.try_lock() {
        Ok(mut state) => record_bounded(&mut state, &location, &event),
        Err(TryLockError::Poisoned(p)) => record_bounded(&mut p.into_inner(), &location, &event),
        Err(TryLockError::WouldBlock) => {
            SKIPPED.fetch_add(1, Ordering::Relaxed);
            let _ = writeln!(std::io::stderr(), "alacritree: panic record skipped (recorder busy)");
        },
    }
}

/// Write a panic record unless this process has already said enough.
fn record_bounded(state: &mut State, location: &str, event: &str) {
    if state.last.as_deref() == Some(location) {
        state.repeats += 1;
        return;
    }
    state.last = Some(location.to_string());

    // Past the cap, a new location must not route through `flush_repeats`: that
    // would still perform one write per differing location forever, which is
    // the exact unbounded growth the cap exists to stop. Drop the pending run
    // instead of flushing it.
    if state.panics > MAX_PANIC_RECORDS {
        state.repeats = 0;
        return;
    }

    flush_repeats(state);

    if state.panics == MAX_PANIC_RECORDS {
        state.panics += 1;
        let notice = line(&format!("panic records suppressed after {MAX_PANIC_RECORDS}"));
        write_event(state, &notice);
        return;
    }

    state.panics += 1;
    write_event(state, event);
}

/// Close a collapsed run: written when a differing-location panic follows it or
/// the process exits.  A run still in progress when the process aborts loses its
/// count, not its record — the one full write already has the backtrace, which
/// is the diagnosis; the tally is a nice-to-have on top of it.  Once the cap has
/// fired, `record_bounded` drops a pending run itself rather than routing it
/// here, so no tally is ever written for a location seen past the cap.
fn flush_repeats(state: &mut State) {
    if state.repeats == 0 {
        return;
    }
    let repeats = state.repeats + 1;
    state.repeats = 0;
    let notice = line(&format!("  x{repeats} from the same location"));
    write_event(state, &notice);
}

fn payload_of(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "non-string panic payload".to_string()
}

/// How long an artifact outlives the process that wrote it.
const RETAIN_DAYS: u64 = 30;

pub fn prune() {
    let dir = STATE.lock().unwrap_or_else(PoisonError::into_inner).dir.clone();
    if let Some(dir) = dir {
        prune_in(&dir);
    }
}

/// Delete by filename and `stat` only — nothing is opened.
///
/// This is safe against a concurrent pruner without any claim protocol because
/// identities are never reused: a path is only deleted when its producer is
/// dead and the file is over `RETAIN_DAYS` old, and recreating that exact path
/// would need the same start nanosecond, pid, and ordinal.  If that invariant
/// is ever broken, deletion has to verify identity first.
fn prune_in(dir: &Path) {
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(RETAIN_DAYS * 86_400);
    let Ok(entries) = std::fs::read_dir(dir) else { return };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = logdir::parse_name("crash-", name) else { continue };
        if logdir::pid_is_live(id.pid) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if modified > cutoff {
            continue;
        }
        // A concurrent pruner reaching it first is the expected outcome, not a
        // problem worth reporting.
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Reading more of an artifact than this buys nothing: the markers that
/// classify it are lines, and one oversized malformed file must not be read in
/// full on every invocation.
pub(crate) const ARTIFACT_READ_CAP: usize = 256 * 1024;

/// What an artifact says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Running,
    Crashed,
    Indeterminate,
}

pub(crate) struct ArtifactSnapshot {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Read at most one byte beyond the reporting limit, so truncation is explicit
/// even if a file grows after it was opened.
pub(crate) fn read_artifact(path: &Path) -> io::Result<ArtifactSnapshot> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(ARTIFACT_READ_CAP + 1);
    file.take((ARTIFACT_READ_CAP + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > ARTIFACT_READ_CAP;
    bytes.truncate(ARTIFACT_READ_CAP);
    Ok(ArtifactSnapshot { bytes, truncated })
}

/// Read an artifact back and say what it recorded: a clean exit, a still-live
/// process, a crash, or too little to tell.  The one module that writes the
/// vocabulary is also the one that reads it, so the two cannot drift apart.
pub fn classify(path: &Path, pid: u32) -> Verdict {
    let Ok(snapshot) = read_artifact(path) else { return Verdict::Indeterminate };
    classify_snapshot(&snapshot, pid)
}

pub(crate) fn classify_snapshot(snapshot: &ArtifactSnapshot, pid: u32) -> Verdict {
    if snapshot.truncated {
        return Verdict::Indeterminate;
    }
    let text = String::from_utf8_lossy(&snapshot.bytes);

    let mut lines = text.lines();
    let has_header = lines
        .next()
        .and_then(|first| first.split_once(' '))
        .is_some_and(|(_, rest)| rest.starts_with(HEADER_MARKER));
    if !has_header {
        return Verdict::Indeterminate;
    }

    let mut exited = false;
    let mut panicked = false;
    for entry in lines {
        if entry.contains(PANIC_MARKER) || entry.contains(SKIPPED_MARKER) {
            panicked = true;
        }
        if entry.contains(EXIT_ERROR_MARKER) {
            return Verdict::Crashed;
        }
        if entry.contains(EXIT_OK_MARKER) {
            exited = true;
        }
    }

    if panicked {
        return Verdict::Crashed;
    }
    if exited {
        return Verdict::Clean;
    }
    if logdir::pid_is_live(pid) { Verdict::Running } else { Verdict::Crashed }
}

/// Panic while holding the recorder lock, so a test can prove the hook takes
/// the skip path instead of waiting on a mutex this thread already owns.
#[cfg(debug_assertions)]
pub fn provoke_lock_panic() {
    let dir = std::env::temp_dir().join("alacritree-provoke");
    install(&dir, "provoke");
    set_enabled(true);
    let _guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    panic!("provoked while holding the recorder lock");
}

#[cfg(test)]
pub fn reset_for_tests(dir: &Path) {
    // Wholesale, so a field added later cannot leak between test cases.
    {
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        *state = State::new();
        state.dir = Some(dir.to_path_buf());
    }
    // A test that deliberately poisons STATE (to exercise poison recovery)
    // would otherwise leave every later test's direct `.lock()` failing too:
    // poisoning is a property of the Mutex itself, not the value inside it,
    // and replacing the value above does not clear it.
    STATE.clear_poison();
    ARMED.store(true, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    BROKEN.store(false, Ordering::Relaxed);
    SKIPPED.store(0, Ordering::Relaxed);
    REASON_RECORDED.store(false, Ordering::Relaxed);
    logdir::reset_identity_for_tests();
}

#[cfg(test)]
pub fn artifact_path_for_tests() -> Option<PathBuf> {
    let dir = STATE.lock().unwrap_or_else(PoisonError::into_inner).dir.clone()?;
    let path = dir.join(logdir::artifact_name(&logdir::process_id()));
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logdir::ProcessId;

    /// Every hook-installing test runs through this so the harness's hook is
    /// restored: `take_hook` puts the default in place of what it removes
    /// rather than leaving a slot, so restoration has to be explicit — and it
    /// has to happen even when the body unwinds (a failing `assert!`), not just
    /// on the success path. Restoring from a `Drop` guard would not work here:
    /// `set_hook` itself panics when called from a thread that is already
    /// panicking, and a `Drop` runs while its unwind is still in flight, so a
    /// guard's `drop` would be a second panic during the first one's unwind —
    /// which Rust escalates straight to an abort. Catching the unwind first,
    /// restoring once it is no longer in flight, then resuming it is the only
    /// ordering that keeps `set_hook` outside of a panicking thread.
    fn with_recorder<T>(body: impl FnOnce(&Path) -> T) -> T {
        let dir = tempfile::tempdir().expect("a temp dir");
        with_recorder_in(dir.path(), body)
    }

    /// Takes the directory rather than making one, for the case that has to
    /// name a path before it exists.
    fn with_recorder_in<T>(dir: &Path, body: impl FnOnce(&Path) -> T) -> T {
        // Declared first so it outlives the restore below: the next test must
        // not install its hook until this one's is back in place.
        let _identity = logdir::lock_identity();
        let previous = std::panic::take_hook();
        reset_for_tests(dir);
        install(dir, "test");
        set_enabled(true);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(dir)));
        std::panic::set_hook(previous);
        match outcome {
            Ok(out) => out,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn artifact_text() -> String {
        let path = artifact_path_for_tests().expect("an artifact was created");
        String::from_utf8_lossy(&std::fs::read(path).expect("the artifact is readable")).into()
    }

    /// Counts header lines (`<timestamp> start ...`) rather than scanning for
    /// the substring " start " anywhere in the text: a backtrace frame symbol
    /// could otherwise render that substring and produce a spurious match.
    fn header_count(text: &str) -> usize {
        text.lines()
            .filter(|line| {
                line.split_once(' ').is_some_and(|(_, rest)| rest.starts_with(HEADER_MARKER))
            })
            .count()
    }

    /// The whole point: a panic that would otherwise vanish leaves a record
    /// naming what failed and where.
    #[test]
    fn a_panic_is_recorded_with_its_payload_location_and_thread() {
        with_recorder(|_| {
            let _ = std::panic::catch_unwind(|| panic!("boom-marker"));

            let text = artifact_text();
            assert!(text.contains("boom-marker"), "payload missing:\n{text}");
            assert!(text.contains("crash_log.rs"), "location missing:\n{text}");
            assert!(text.contains("thread="), "thread missing:\n{text}");
        });
    }

    /// A PTY thread panicking leaves the app running, so the record has to name
    /// the thread or it is unattributable.
    #[test]
    fn a_named_worker_thread_is_named_in_its_record() {
        with_recorder(|_| {
            std::thread::Builder::new()
                .name("pty-worker".into())
                .spawn(|| panic!("worker-boom"))
                .expect("spawn")
                .join()
                .expect_err("the thread panicked");

            let text = artifact_text();
            assert!(text.contains("pty-worker"), "thread name missing:\n{text}");
            assert!(text.contains("worker-boom"), "payload missing:\n{text}");
        });
    }

    /// A crash during config load happens before `session_begin`, and a file
    /// deleted underneath a live process has to come back — both go through the
    /// same initializer, so neither can produce a headerless artifact.
    #[test]
    fn every_writer_produces_exactly_one_header() {
        with_recorder(|_| {
            let _ = std::panic::catch_unwind(|| panic!("early"));
            session_begin();
            let text = artifact_text();

            assert_eq!(header_count(&text), 1, "not exactly one header:\n{text}");
        });
    }

    #[test]
    fn a_deleted_artifact_is_recreated_with_a_header() {
        with_recorder(|_| {
            session_begin();
            std::fs::remove_file(artifact_path_for_tests().unwrap()).expect("remove");

            let _ = std::panic::catch_unwind(|| panic!("after-delete"));

            let text = artifact_text();
            assert_eq!(header_count(&text), 1, "not exactly one header:\n{text}");
            assert!(text.contains("after-delete"), "payload missing:\n{text}");
        });
    }

    /// `create_new` is the allocator, never `create`: a file already occupying
    /// our identity's path is debris from an unrelated writer, not a header we
    /// can trust, so it must be left alone and the artifact has to land at the
    /// next ordinal — which also proves `set_ordinal` recorded what the retry
    /// actually settled on.
    #[test]
    fn a_colliding_path_is_left_untouched_and_the_next_ordinal_is_used() {
        with_recorder(|dir| {
            let id = logdir::process_id();
            let collision_path = dir.join(logdir::artifact_name(&id));
            std::fs::write(&collision_path, "not ours").expect("seed a collision");

            let _ = std::panic::catch_unwind(|| panic!("collision-marker"));

            let collision_content =
                std::fs::read_to_string(&collision_path).expect("still readable");
            assert_eq!(collision_content, "not ours", "the colliding file was overwritten");

            let text = artifact_text();
            assert!(text.contains("collision-marker"), "payload missing:\n{text}");
            assert_eq!(
                logdir::process_id().ordinal,
                id.ordinal + 1,
                "set_ordinal did not record the ordinal create_new settled on"
            );
        });
    }

    /// The gate is what `crash_log = false` buys, and it must silence writes
    /// without silencing the chained default hook.
    #[test]
    fn a_disabled_recorder_writes_nothing() {
        with_recorder(|dir| {
            set_enabled(false);

            let _ = std::panic::catch_unwind(|| panic!("silenced"));

            let entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
            assert!(entries.is_empty(), "wrote {} files while disabled", entries.len());
        });
    }

    /// The whole reason `set_dir` exists: the hook is armed against the default
    /// directory before the config that renames it has been read.
    #[test]
    fn a_configured_directory_takes_the_artifact() {
        let configured = tempfile::tempdir().expect("a temp dir");

        with_recorder(|default| {
            set_dir(configured.path());

            let _ = std::panic::catch_unwind(|| panic!("relocated"));

            assert!(artifact_text().contains("relocated"));
            assert!(
                artifact_path_for_tests().is_some_and(|p| p.starts_with(configured.path())),
                "the artifact did not follow the configured directory"
            );
            let left_behind: Vec<_> = std::fs::read_dir(default).unwrap().flatten().collect();
            assert!(left_behind.is_empty(), "wrote {} files to the default dir", left_behind.len());
        });
    }

    /// A process that has already written commits to where it wrote.  Proving
    /// that needs the artifact removed underneath the recorder, since that is
    /// the one path on which `ensure_artifact` consults the directory again
    /// rather than reopening the file it remembers.
    #[test]
    fn a_directory_that_arrives_after_the_artifact_is_refused() {
        let configured = tempfile::tempdir().expect("a temp dir");

        with_recorder(|default| {
            let _ = std::panic::catch_unwind(|| panic!("already-written"));
            std::fs::remove_file(artifact_path_for_tests().expect("an artifact")).unwrap();

            set_dir(configured.path());
            let _ = std::panic::catch_unwind(|| panic!("after-the-move"));

            assert!(artifact_text().contains("after-the-move"));
            assert!(
                artifact_path_for_tests().is_some_and(|p| p.starts_with(default)),
                "a late directory change split one process's records across two directories"
            );
        });
    }

    /// `install` runs before config is read, so a directory that does not exist
    /// yet must not cost the first crash its record.
    #[test]
    fn a_missing_log_directory_is_created() {
        let root = tempfile::tempdir().expect("a temp dir");
        let nested = root.path().join("does").join("not").join("exist");

        with_recorder_in(&nested, |dir| {
            let _ = std::panic::catch_unwind(|| panic!("first-launch"));

            assert!(dir.is_dir(), "the log directory was not created");
            assert!(artifact_text().contains("first-launch"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_crash_artifact_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        with_recorder(|_| {
            session_begin();
            let path = artifact_path_for_tests().expect("an artifact was created");

            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        });
    }

    #[test]
    fn a_clean_exit_is_recorded_and_nothing_is_deleted() {
        with_recorder(|_| {
            session_begin();

            record_exit(&Ok(()));

            let text = artifact_text();
            assert!(text.contains("exit ok"), "no exit marker:\n{text}");
        });
    }

    /// A detached worker can outlive the exit, and its record has to land in the
    /// same file rather than in a resurrected one.
    #[test]
    fn a_panic_after_the_exit_marker_lands_in_the_same_file() {
        with_recorder(|_| {
            session_begin();
            record_exit(&Ok(()));

            let _ = std::panic::catch_unwind(|| panic!("late-worker"));

            let text = artifact_text();
            assert!(text.contains("exit ok"), "exit marker lost:\n{text}");
            assert!(text.contains("late-worker"), "late panic lost:\n{text}");
        });
    }

    /// A panicking PTY thread leaves the app running and the IPC listener spawns a
    /// thread per connection, so one repeatable defect could otherwise append a
    /// backtrace per request forever.
    /// Alternating two call sites keeps every panic at a location distinct from the
    /// one before it, so the collapse never engages and each panic takes the write
    /// path — the only way to actually drive `panics` up to the cap.
    #[test]
    fn panic_records_stop_after_the_cap() {
        with_recorder(|_| {
            for i in 0..25 {
                if i % 2 == 0 {
                    let _ = std::panic::catch_unwind(|| panic!("cap-a"));
                } else {
                    let _ = std::panic::catch_unwind(|| panic!("cap-b"));
                }
            }

            let text = artifact_text();
            assert_eq!(text.matches("PANIC thread=").count(), 20, "cap not applied:\n{text}");
            assert!(text.contains("panic records suppressed after 20"), "no notice:\n{text}");

            // The cap's actual promise is that the file stops growing, not just
            // that it stops growing with PANIC records specifically — a leaked
            // "xN" tally line per differing location past the cap would still
            // fail this even though it contains no "PANIC thread=".
            let path = artifact_path_for_tests().expect("an artifact was created");
            let size_at_cap = std::fs::metadata(&path).expect("artifact metadata").len();

            for i in 0..10 {
                if i % 2 == 0 {
                    let _ = std::panic::catch_unwind(|| panic!("cap-a"));
                } else {
                    let _ = std::panic::catch_unwind(|| panic!("cap-b"));
                }
            }

            let size_after = std::fs::metadata(&path).expect("artifact metadata").len();
            assert_eq!(size_after, size_at_cap, "the artifact grew after the cap notice");

            let text = artifact_text();
            let after_notice = text.split("panic records suppressed after 20").nth(1).unwrap_or("");
            assert!(
                !after_notice.contains("from the same location"),
                "a repeat tally leaked past the cap:\n{text}"
            );
        });
    }

    #[test]
    fn identical_consecutive_panics_collapse_into_a_count() {
        with_recorder(|_| {
            for _ in 0..3 {
                let _ = std::panic::catch_unwind(|| panic!("same-place"));
            }
            // A collapsed run's count lives only in memory until something closes
            // it — a differing-location panic or, as here, process exit.
            record_exit(&Ok(()));

            let text = artifact_text();
            assert_eq!(text.matches("PANIC thread=").count(), 1, "not collapsed:\n{text}");
            assert!(text.contains("x3"), "no repeat count:\n{text}");
        });
    }

    /// The exit marker reports what the recorder could not write.  It is
    /// best-effort by construction: a skip that races `record_exit`'s read may be
    /// absent, and that is not a failure.
    #[test]
    fn skipped_records_are_counted_for_the_exit_marker() {
        with_recorder(|_| {
            let held = STATE.lock().expect("the recorder lock");
            let _ = std::panic::catch_unwind(|| panic!("while-held"));
            drop(held);

            record_exit(&Ok(()));

            let text = artifact_text();
            assert!(text.contains("panic records skipped: 1"), "no skip marker:\n{text}");
        });
    }

    /// A blocking `lock()` here waits on a mutex this very thread holds and never
    /// becomes poisoned.  This test hangs against that implementation.
    #[test]
    fn a_panic_while_holding_the_lock_does_not_hang() {
        with_recorder(|_| {
            let held = STATE.lock().expect("the recorder lock");

            let result = std::panic::catch_unwind(|| panic!("self-deadlock"));

            drop(held);
            assert!(result.is_err(), "the panic did not unwind");
        });
    }

    /// An earlier panic must not silence the next one.
    #[test]
    fn a_poisoned_lock_still_records() {
        with_recorder(|_| {
            let _ = std::panic::catch_unwind(|| {
                let _guard = STATE.lock().expect("the recorder lock");
                panic!("poisoning");
            });

            let _ = std::panic::catch_unwind(|| panic!("after-poison"));

            let text = artifact_text();
            assert!(text.contains("after-poison"), "record lost after poisoning:\n{text}");
        });
    }

    /// Retention is by age and liveness alone.  Reading a file to decide whether to
    /// keep it is what let two earlier designs delete the only record of a crash.
    #[test]
    fn pruning_ignores_contents_entirely() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let dead = ProcessId { start: 1, pid: 0, ordinal: 0 };
        let old_clean = dir.path().join(logdir::artifact_name(&dead));
        std::fs::write(&old_clean, "t1 start v pid=0\nt2 exit ok\n").unwrap();
        let old_crash = dir.path().join("crash-2-0.log");
        std::fs::write(&old_crash, "t1 start v pid=0\nt2 PANIC thread=main\n").unwrap();
        set_mtime_days_ago(&old_clean, 40);
        set_mtime_days_ago(&old_crash, 40);

        prune_in(dir.path());

        assert!(!old_clean.exists(), "an old dead-pid artifact survived");
        assert!(!old_crash.exists(), "contents changed the decision");
    }

    #[test]
    fn a_live_producers_artifact_is_never_pruned() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mine = ProcessId { start: 1, pid: std::process::id(), ordinal: 0 };
        let path = dir.path().join(logdir::artifact_name(&mine));
        std::fs::write(&path, "t1 start v\n").unwrap();
        set_mtime_days_ago(&path, 400);

        prune_in(dir.path());

        assert!(path.exists(), "a live process's artifact was deleted");
    }

    #[test]
    fn a_fresh_artifact_survives_even_from_a_dead_pid() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("crash-1-0.log");
        std::fs::write(&path, "t1 start v\n").unwrap();

        prune_in(dir.path());

        assert!(path.exists(), "a fresh artifact was deleted");
    }

    /// A concurrently starting instance can delete the same path first.
    #[test]
    fn a_vanished_path_is_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("crash-1-0.log");
        std::fs::write(&path, "t1 start v\n").unwrap();
        set_mtime_days_ago(&path, 40);

        prune_in(dir.path());
        prune_in(dir.path());

        assert!(!path.exists());
    }

    #[test]
    fn files_that_are_not_artifacts_are_left_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = dir.path().join("state.toml");
        std::fs::write(&state, "x").unwrap();
        set_mtime_days_ago(&state, 400);

        prune_in(dir.path());

        assert!(state.exists(), "an unrelated file was deleted");
    }

    /// No hand-written body can catch the writer and `classify` drifting apart —
    /// only driving both the real writer and the real reader end to end can.
    #[test]
    fn a_real_panic_classifies_as_crashed() {
        with_recorder(|_| {
            session_begin();
            let _ = std::panic::catch_unwind(|| panic!("classify-marker"));

            let path = artifact_path_for_tests().expect("an artifact was created");
            assert_eq!(classify(&path, std::process::id()), Verdict::Crashed);
        });
    }

    #[test]
    fn a_real_clean_exit_classifies_as_clean() {
        with_recorder(|_| {
            session_begin();
            record_exit(&Ok(()));

            let path = artifact_path_for_tests().expect("an artifact was created");
            assert_eq!(classify(&path, std::process::id()), Verdict::Clean);
        });
    }

    #[test]
    fn a_recorded_reason_lands_before_the_exit_line() {
        with_recorder(|_| {
            session_begin();
            record_reason(ExitReason::UserQuit);
            record_exit(&Ok(()));

            let text = artifact_text();
            let reason_pos = text.find("exit reason: user-quit").expect("reason line missing");
            let exit_pos = text.find(EXIT_OK_MARKER).expect("exit line missing");
            assert!(reason_pos < exit_pos, "reason did not precede exit:\n{text}");
        });
    }

    /// First writer wins: a `WM_CLOSE` following a session end must not
    /// overwrite `os-close-app` with `window-closed`.
    #[test]
    fn a_second_reason_is_dropped_by_the_latch() {
        with_recorder(|_| {
            session_begin();
            record_reason(ExitReason::OsCloseApp);
            record_reason(ExitReason::WindowClosed);

            let text = artifact_text();
            assert!(text.contains("exit reason: os-close-app"), "first reason lost:\n{text}");
            assert!(!text.contains("window-closed"), "second reason overwrote the first:\n{text}");
            assert_eq!(
                text.matches("exit reason:").count(),
                1,
                "more than one reason line:\n{text}"
            );
        });
    }

    #[test]
    fn a_reason_with_no_exit_line_classifies_as_crashed_for_a_dead_pid() {
        with_recorder(|_| {
            session_begin();
            record_reason(ExitReason::OsShutdown);

            let path = artifact_path_for_tests().expect("an artifact was created");
            let text = artifact_text();
            assert!(text.contains("exit reason: os-shutdown"), "reason missing:\n{text}");
            assert_eq!(classify(&path, 0), Verdict::Crashed);
        });
    }

    #[test]
    fn a_reason_line_does_not_disturb_a_live_pids_running_verdict() {
        with_recorder(|_| {
            session_begin();
            record_reason(ExitReason::OsLogoff);

            let path = artifact_path_for_tests().expect("an artifact was created");
            assert_eq!(classify(&path, std::process::id()), Verdict::Running);
        });
    }

    #[test]
    fn a_disabled_call_writes_nothing_and_does_not_burn_the_latch() {
        with_recorder(|dir| {
            set_enabled(false);

            record_reason(ExitReason::UserQuit);

            let entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
            assert!(entries.is_empty(), "wrote {} files while disabled", entries.len());

            set_enabled(true);
            session_begin();
            record_reason(ExitReason::UserQuit);

            let text = artifact_text();
            assert!(text.contains("exit reason: user-quit"), "reason missing:\n{text}");
        });
    }

    #[test]
    fn artifact_reads_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.log");
        std::fs::write(&path, vec![b'x'; ARTIFACT_READ_CAP + 100]).unwrap();

        let snapshot = read_artifact(&path).unwrap();

        assert_eq!(snapshot.bytes.len(), ARTIFACT_READ_CAP);
        assert!(snapshot.truncated);
    }

    #[test]
    fn an_exactly_capped_artifact_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("at-cap.log");
        std::fs::write(&path, vec![b'x'; ARTIFACT_READ_CAP]).unwrap();

        let snapshot = read_artifact(&path).unwrap();

        assert_eq!(snapshot.bytes.len(), ARTIFACT_READ_CAP);
        assert!(!snapshot.truncated);
    }

    #[test]
    fn a_truncated_artifact_is_indeterminate() {
        let mut bytes = b"t1 start v pid=0\nt2 PANIC thread=main\n".to_vec();
        bytes.resize(ARTIFACT_READ_CAP, b'x');
        let snapshot = ArtifactSnapshot { bytes, truncated: true };

        assert_eq!(classify_snapshot(&snapshot, 0), Verdict::Indeterminate);
    }

    fn set_mtime_days_ago(path: &Path, days: u64) {
        let when = SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let file = OpenOptions::new().write(true).open(path).expect("open for mtime");
        file.set_modified(when).expect("set mtime");
    }
}
