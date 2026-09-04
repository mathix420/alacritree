//! Duplicating the log stream to a file.
//!
//! `env_logger` writes to exactly one target, so mirroring to a file means
//! wrapping that target.  The sink is filled after `init()` because the
//! preference that enables it is not known until config has loaded, and
//! env_logger cannot be retargeted once built.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::logdir;

/// How long a continuous log outlives the process that wrote it.
const RETAIN_DAYS: u64 = 7;

pub struct Tee {
    sink: Arc<Mutex<Option<File>>>,
    /// stderr in production; a buffer in tests.
    primary: Box<dyn Write + Send>,
}

/// A tee plus the handle that fills its sink later.  `Target::Pipe` takes
/// `Box<dyn Write + Send>` and moves it, so the caller can only reach the sink
/// afterwards through a share it kept.
pub fn tee() -> (Tee, Arc<Mutex<Option<File>>>) {
    let sink = Arc::new(Mutex::new(None));
    (Tee { sink: sink.clone(), primary: Box::new(std::io::stderr()) }, sink)
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.primary.write(buf)?;

        // Only the prefix stderr accepted: env_logger retries the suffix, and
        // mirroring the whole buffer would write it to the file twice.
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = sink.as_mut()
            && file.write_all(&buf[..written]).is_err()
        {
            // Straight to stderr, never through `log::*`: env_logger holds its
            // own pipe mutex across this call, so logging here deadlocks.
            let _ = self.primary.write_all(b"alacritree: log file write failed; disabling it\n");
            *sink = None;
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = sink.as_mut() {
            let _ = file.flush();
        }
        self.primary.flush()
    }
}

/// This process's continuous log, sharing the artifact's identity so the two
/// files correlate.
pub fn open_session_log(dir: &Path) -> Option<File> {
    if logdir::prepare_log_dir(dir).is_err() {
        return None;
    }
    let id = logdir::process_id();
    let mut candidate = id;
    for _ in 0..32 {
        let path = dir.join(logdir::session_log_name(&candidate));
        match logdir::create_private_file(&path) {
            Ok(file) => {
                // Only write back when this succeeded at the ordinal
                // `process_id()` already reported. The crash recorder may
                // create its artifact first and settle the shared ordinal;
                // writing an advanced value here would make the *next* panic
                // record find no file at the new name and `create_new` a
                // second crash artifact, breaking the one-artifact-per-process
                // guarantee it depends on. Losing crash/session correlation in
                // this practically unreachable case is the lesser evil.
                if candidate.ordinal == id.ordinal {
                    logdir::set_ordinal(candidate.ordinal);
                }
                return Some(file);
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => candidate.ordinal += 1,
            Err(_) => return None,
        }
    }
    None
}

/// This process's log at a path the caller named.  Truncates rather than
/// refusing a file that exists, so re-running a measurement overwrites its own
/// output instead of failing on the second attempt.
///
/// Outside retention: [`prune_session_logs`] only considers names carrying the
/// generated `alacritree-<start>-<pid>` shape, and a file the user named is
/// theirs to delete.
pub fn open_log_at(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).ok()?;
    }
    File::create(path).ok()
}

/// Liveness first, age second.  An idle window can leave a week-old mtime while
/// still running, and Windows honors a delete against an open handle — the
/// process would keep writing into a file no path reaches.
pub fn prune_session_logs(dir: &Path) {
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(RETAIN_DAYS * 86_400);
    let Ok(entries) = std::fs::read_dir(dir) else { return };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = logdir::parse_name("alacritree-", name) else { continue };
        if logdir::pid_is_live(id.pid) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if modified > cutoff {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logdir::ProcessId;
    use std::fs::OpenOptions;

    #[test]
    fn a_named_log_replaces_what_an_earlier_run_left() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("measurement.log");
        std::fs::write(&path, b"stale output from the previous run").expect("seed the file");

        let mut file = open_log_at(&path).expect("the named log opens");
        file.write_all(b"fresh").expect("write");
        drop(file);

        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "fresh");
    }

    /// A path a shell tab-completed into a directory that does not exist yet
    /// should still produce a log rather than silently nothing.
    #[test]
    fn a_named_log_creates_the_directories_leading_to_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("runs").join("gate-off").join("session.log");

        assert!(open_log_at(&path).is_some(), "the named log opens");
        assert!(path.exists(), "the file exists at the path asked for");
    }

    /// Retention reads the generated `alacritree-<start>-<pid>` shape, so a
    /// file the caller named is never a deletion candidate.
    #[test]
    fn retention_leaves_a_named_log_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("keep-me.log");
        drop(open_log_at(&path).expect("the named log opens"));
        set_mtime_days_ago(&path, RETAIN_DAYS + 30);

        prune_session_logs(dir.path());

        assert!(path.exists(), "a named log outlives retention");
    }

    /// A sink filled after `Target::Pipe` has already moved the writer is the
    /// whole reason the handle is shared.
    #[test]
    fn a_sink_filled_after_construction_receives_writes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("late.log");
        let (mut tee, sink) = tee();

        tee.write_all(b"before\n").expect("write");
        *sink.lock().unwrap() = Some(File::create(&path).expect("create"));
        tee.write_all(b"after\n").expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("before"), "wrote to a sink that was not set yet");
        assert!(text.contains("after"), "the late sink got nothing");
    }

    #[cfg(unix)]
    #[test]
    fn a_session_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a temp dir");
        let file = open_session_log(dir.path()).expect("a session log");
        drop(file);
        let path = std::fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();

        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// If stderr accepts only a prefix, env_logger retries the suffix.  Writing
    /// the whole buffer while returning the short count duplicates it.
    #[test]
    fn a_short_write_mirrors_only_the_accepted_prefix() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("short.log");
        let sink = Arc::new(Mutex::new(Some(File::create(&path).expect("create"))));
        let mut tee = Tee { sink: sink.clone(), primary: Box::new(ShortWriter { limit: 3 }) };

        let written = tee.write(b"abcdefgh").expect("write");

        assert_eq!(written, 3);
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "abc", "the file got more than stderr accepted");
    }

    /// A full disk must degrade to today's behavior, not fail the log call.
    #[test]
    fn an_erroring_sink_is_dropped_without_failing_the_write() {
        let sink = Arc::new(Mutex::new(Some(broken_file())));
        let mut tee = Tee { sink: sink.clone(), primary: Box::new(Vec::new()) };

        let written = tee.write(b"hello").expect("the write must still succeed");

        assert_eq!(written, 5);
        assert!(sink.lock().unwrap().is_none(), "the broken sink was kept");
    }

    #[test]
    fn a_dead_producers_stale_log_is_pruned() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let stale = dir.path().join("alacritree-1-0.log");
        std::fs::write(&stale, "x").unwrap();
        set_mtime_days_ago(&stale, 10);

        prune_session_logs(dir.path());

        assert!(!stale.exists(), "a stale dead-pid log survived");
    }

    /// A window can idle for a week without logging, leaving a stale mtime while
    /// the process is alive.  Deleting it would leave that process writing into
    /// an unlinked file no path reaches.
    #[test]
    fn a_live_producers_stale_log_is_spared() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let id = ProcessId { start: 1, pid: std::process::id(), ordinal: 0 };
        let mine = dir.path().join(logdir::session_log_name(&id));
        std::fs::write(&mine, "x").unwrap();
        set_mtime_days_ago(&mine, 10);

        prune_session_logs(dir.path());

        assert!(mine.exists(), "a live process's log was deleted");
    }

    struct ShortWriter {
        limit: usize,
    }

    impl Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len().min(self.limit))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn broken_file() -> File {
        // A handle opened read-only fails on write on every supported
        // platform, unlike a handle to a deleted file, which Windows keeps
        // writable as long as it stays open.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("closed.log");
        File::create(&path).expect("create");
        OpenOptions::new().read(true).open(&path).expect("open read-only")
    }

    fn set_mtime_days_ago(path: &Path, days: u64) {
        let when = SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let file = OpenOptions::new().write(true).open(path).expect("open for mtime");
        file.set_modified(when).expect("set mtime");
    }
}
