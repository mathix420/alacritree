//! The CLI must not become a crash-logging process.
//!
//! Every check here is about which *process* does what, which no in-crate test
//! can observe: the crate is binary-only, so these drive the real executable.

// These tests exist to drive the real executable, so running and waiting on
// it is the point.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_alacritree")
}

/// Point every log-directory environment variable at a scratch path so the
/// developer's real artifacts are never touched.
fn run_isolated(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .env("LOCALAPPDATA", home)
        .env("APPDATA", home)
        .env("XDG_STATE_HOME", home)
        .env("HOME", home)
        .output()
        .expect("the binary runs")
}

fn log_dir(home: &Path) -> PathBuf {
    home.join("alacritree")
}

/// An earlier design installed the hook before `cli::run`, so every `--help`
/// created a log directory and `alacritree mcp` wrote records no config could
/// disable.  This is the regression guard for that.
#[test]
fn help_creates_no_log_directory() {
    let home = tempfile::tempdir().expect("a temp dir");

    let out = run_isolated(home.path(), &["--help"]);

    assert!(out.status.success(), "--help failed");
    assert!(!log_dir(home.path()).exists(), "--help created a log directory");
}

#[test]
fn doctor_creates_no_log_directory() {
    let home = tempfile::tempdir().expect("a temp dir");

    run_isolated(home.path(), &["doctor"]);

    assert!(!log_dir(home.path()).exists(), "doctor created a log directory");
}

#[test]
fn crashes_reports_nothing_when_nothing_has_crashed() {
    let home = tempfile::tempdir().expect("a temp dir");

    let out = run_isolated(home.path(), &["crashes"]);

    assert!(out.status.success(), "crashes failed on an empty directory");
    assert!(out.stdout.is_empty(), "unexpected output: {:?}", String::from_utf8_lossy(&out.stdout));
    assert!(!log_dir(home.path()).exists(), "crashes created a log directory");
}

#[test]
fn crashes_lists_seeded_artifacts_newest_first() {
    let home = tempfile::tempdir().expect("a temp dir");
    let dir = log_dir(home.path());
    std::fs::create_dir_all(&dir).expect("create");
    std::fs::write(dir.join("crash-10-1.log"), "older\n").unwrap();
    std::fs::write(dir.join("crash-20-2.log"), "newer\n").unwrap();

    let out = run_isolated(home.path(), &["crashes"]);

    let text = String::from_utf8_lossy(&out.stdout);
    let newer = text.find("newer").expect("the newer artifact is missing");
    let older = text.find("older").expect("the older artifact is missing");
    assert!(newer < older, "not newest first:\n{text}");
    assert!(text.contains("==> crash-20-2.log <=="), "no separator:\n{text}");
}

/// `--json` is global, so it must work in either position and must never emit
/// raw concatenation.
#[test]
fn crashes_emits_json_in_either_flag_position() {
    let home = tempfile::tempdir().expect("a temp dir");
    let dir = log_dir(home.path());
    std::fs::create_dir_all(&dir).expect("create");
    std::fs::write(dir.join("crash-42-7.log"), "body\n").unwrap();

    for args in [["crashes", "--json"], ["--json", "crashes"]] {
        let out = run_isolated(home.path(), &args);

        let text = String::from_utf8_lossy(&out.stdout);
        let value: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{args:?} is not JSON: {e}\n{text}"));
        assert_eq!(value[0]["pid"], 7, "{args:?} lost the pid");
    }
}

/// `crashes` copies bytes rather than decoding, because refusing to print a
/// damaged crash record is the one unacceptable outcome for this tool.  A
/// lossy re-encode of `0xff` into a 3-byte U+FFFD would pass a check that only
/// looks at decoded text, so this compares the raw stdout bytes instead.
#[test]
fn crashes_preserves_invalid_utf8_bytes_verbatim() {
    let home = tempfile::tempdir().expect("a temp dir");
    let dir = log_dir(home.path());
    std::fs::create_dir_all(&dir).expect("create");
    std::fs::write(dir.join("crash-1-1.log"), [0x74, 0x78, 0xff, 0x0a]).unwrap();

    let out = run_isolated(home.path(), &["crashes"]);

    assert!(
        out.stdout.windows(4).any(|w| w == [0x74, 0x78, 0xff, 0x0a]),
        "the invalid-UTF-8 byte sequence was not preserved verbatim in stdout"
    );
}

/// A blocking `lock()` in the hook waits on a mutex the panicking thread already
/// holds and never becomes poisoned.  A timeout here is the failure.
///
/// A process that merely exits promptly is not enough: a clap rejection of an
/// unknown subcommand exits just as fast as a correct run, so the deadline
/// alone cannot tell "the stimulus ran and hit the skip path" apart from "the
/// stimulus never existed".  The stderr checks below are what actually pin
/// that down.
#[test]
fn a_panic_holding_the_recorder_lock_does_not_hang() {
    // The stimulus is gated on `debug_assertions`, which a dev profile does not
    // guarantee: `-C debug-assertions=off` in RUSTFLAGS is a legitimate way to
    // build one, and the binary then carries no provoke path to drive. That
    // flag reaches this crate too, so the gate read here is the one the binary
    // was compiled under.
    if !cfg!(debug_assertions) {
        eprintln!("skipped: the binary under test was built without debug assertions");
        return;
    }

    let home = tempfile::tempdir().expect("a temp dir");
    let mut child = Command::new(binary())
        .arg("provoke-lock-panic")
        .env("LOCALAPPDATA", home.path())
        .env("APPDATA", home.path())
        .env("XDG_STATE_HOME", home.path())
        .env("HOME", home.path())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the process hung: the hook is waiting on a lock it already holds");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("piped stderr"), &mut stderr)
        .expect("read stderr");

    assert!(!status.success(), "the provoked panic did not unwind out of main:\n{stderr}");
    assert!(
        stderr.contains("provoked while holding the recorder lock"),
        "the provoke stimulus never ran:\n{stderr}"
    );
    // The panicking thread already holds the lock, so `try_lock` fails
    // deterministically here — this is single-thread self-contention, not a
    // race between threads, so the message is stable rather than timing-dependent.
    assert!(
        stderr.contains("panic record skipped (recorder busy)"),
        "the hook did not report taking the WouldBlock skip path:\n{stderr}"
    );
}
