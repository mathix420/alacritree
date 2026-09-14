//! `alacritree crashes` — crashed and indeterminate sessions, newest first.
//!
//! `session_begin` creates an artifact on every GUI launch, not only a crash
//! one — that is what makes "header present, no exit marker, dead pid"
//! detectable at all. Most launches exit cleanly, so by default this shows
//! only what [`Verdict`] calls `Crashed` or `Indeterminate`; `--all` also
//! shows clean exits and still-running sessions.
//!
//! Strictly read-only.  The artifacts are per-process files that nothing
//! merges on disk; this derives the single view instead, so it can run at any
//! time without coordinating with a live instance.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use serde_json::{Value, json};

use crate::crash_log::{ARTIFACT_READ_CAP, Verdict, classify_snapshot, read_artifact};
use crate::logdir::{self, ProcessId};

struct Artifact {
    name: String,
    id: ProcessId,
    bytes: Vec<u8>,
    truncated: bool,
}

pub(super) fn run(as_json: bool, all: bool) -> i32 {
    let Some(dir) = logdir::log_dir() else {
        eprintln!("alacritree: no log directory on this platform");
        return 1;
    };
    let artifacts = collect(&dir, all);

    if as_json {
        println!("{:#}", to_json(&artifacts));
    } else {
        print_human(&artifacts);
    }
    0
}

fn collect(dir: &Path, all: bool) -> Vec<Artifact> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };

    let mut artifacts: Vec<Artifact> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let id = logdir::parse_name("crash-", &name)?;
            // A file pruned by a concurrently starting instance is expected;
            // anything else read stops being reported rather than aborting the
            // whole listing.
            let snapshot = read_artifact(&entry.path()).ok()?;
            let verdict = classify_snapshot(&snapshot, id.pid);
            if !should_show(verdict, all) {
                return None;
            }
            Some(Artifact { name, id, bytes: snapshot.bytes, truncated: snapshot.truncated })
        })
        .collect();

    artifacts.sort_by(|a, b| {
        logdir::sort_key(&a.id).cmp(&logdir::sort_key(&b.id)).then_with(|| a.name.cmp(&b.name))
    });
    artifacts
}

/// A `Running` artifact belongs to a live process that has not crashed, so it
/// is hidden right alongside `Clean` unless `--all` asks for everything.
fn should_show(verdict: Verdict, all: bool) -> bool {
    all || matches!(verdict, Verdict::Crashed | Verdict::Indeterminate)
}

fn print_human(artifacts: &[Artifact]) {
    let stdout = std::io::stdout();
    let terminal = stdout.is_terminal();
    let mut out = stdout.lock();
    let _ = print_human_to(artifacts, &mut out, terminal);
}

fn print_human_to(artifacts: &[Artifact], out: &mut dyn Write, terminal: bool) -> io::Result<()> {
    for artifact in artifacts {
        writeln!(out, "==> {} <==", artifact.name)?;
        if terminal {
            write_terminal_safe(out, &artifact.bytes)?;
        } else {
            // Redirected output remains byte-exact so callers can archive or
            // inspect damaged records without losing information.
            out.write_all(&artifact.bytes)?;
        }
        if artifact.truncated {
            writeln!(out, "\n[alacritree: artifact truncated after {ARTIFACT_READ_CAP} bytes]")?;
        } else {
            writeln!(out)?;
        }
    }
    Ok(())
}

/// Preserve readable text on an interactive terminal while rendering bytes
/// that could control the terminal as inert ASCII escapes.
fn write_terminal_safe(out: &mut dyn Write, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                write_safe_str(out, text)?;
                break;
            },
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // `valid_up_to` is guaranteed to be a UTF-8 boundary.
                    write_safe_str(out, std::str::from_utf8(&bytes[..valid]).unwrap())?;
                }
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                for byte in &bytes[valid..valid + invalid] {
                    write!(out, "\\x{byte:02x}")?;
                }
                bytes = &bytes[valid + invalid..];
            },
        }
    }
    Ok(())
}

fn write_safe_str(out: &mut dyn Write, text: &str) -> io::Result<()> {
    for character in text.chars() {
        if character == '\n' || !character.is_control() {
            write!(out, "{character}")?;
        } else {
            write!(out, "\\u{{{:x}}}", character as u32)?;
        }
    }
    Ok(())
}

fn to_json(artifacts: &[Artifact]) -> Value {
    Value::Array(
        artifacts
            .iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "start": a.id.start as u64,
                    "pid": a.id.pid,
                    "contents": String::from_utf8_lossy(&a.bytes),
                    "truncated": a.truncated,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("seed");
    }

    fn artifact(bytes: Vec<u8>, truncated: bool) -> Artifact {
        Artifact {
            name: "crash-1-1.log".to_string(),
            id: ProcessId { start: 1, pid: 1, ordinal: 0 },
            bytes,
            truncated,
        }
    }

    #[test]
    fn artifacts_are_ordered_newest_first_then_by_ordinal() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "crash-10-1.log", "old\n");
        seed(dir.path(), "crash-20-2.log", "new-a\n");
        seed(dir.path(), "crash-20-2-1.log", "new-b\n");

        let listed = collect(dir.path(), true);

        let names: Vec<_> = listed.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["crash-20-2.log", "crash-20-2-1.log", "crash-10-1.log"]);
    }

    /// A truncated artifact may not be valid UTF-8, and the one thing a crash
    /// reader must not do is refuse to show a damaged record.
    #[test]
    fn invalid_utf8_is_still_rendered() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("crash-1-1.log"), [0x74, 0x78, 0xff, 0x0a]).unwrap();

        let listed = collect(dir.path(), true);

        assert_eq!(listed.len(), 1);
        assert!(!listed[0].bytes.is_empty(), "a damaged artifact was dropped");
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "state.toml", "x");
        seed(dir.path(), "alacritree-1-1.log", "continuous");

        assert!(collect(dir.path(), true).is_empty());
    }

    /// Nothing has crashed is a success, not an error.
    #[test]
    fn a_missing_directory_lists_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let absent = dir.path().join("gone");

        assert!(collect(&absent, true).is_empty());
    }

    /// `session_begin` writes an artifact on every launch, so most artifacts on
    /// disk are clean exits — the default view must not bury the crash among
    /// them.
    #[test]
    fn a_clean_session_is_hidden_by_default_and_shown_with_all() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "crash-1-1.log", "t1 start v pid=1\nt2 exit ok\n");

        assert!(collect(dir.path(), false).is_empty(), "a clean session was shown");
        assert_eq!(collect(dir.path(), true).len(), 1, "--all did not show it");
    }

    /// A live process pins its own pid, so a header with no exit marker and a
    /// live pid is `Running`, not a crash — and `Running` is hidden by default
    /// too, since nothing has crashed yet.
    #[test]
    fn a_running_session_is_hidden_by_default_and_shown_with_all() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let body = format!("t1 start v pid={}\n", std::process::id());
        seed(dir.path(), &format!("crash-1-{}.log", std::process::id()), &body);

        assert!(collect(dir.path(), false).is_empty(), "a running session was shown");
        assert_eq!(collect(dir.path(), true).len(), 1, "--all did not show it");
    }

    #[test]
    fn a_crashed_session_is_shown_by_default() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "crash-1-1.log", "t1 start v pid=1\nt2 PANIC thread=main\n");

        assert_eq!(collect(dir.path(), false).len(), 1, "a crashed session was hidden");
    }

    /// A headerless artifact cannot be told apart from a clean one with any
    /// confidence, so it must err toward being shown rather than hidden.
    #[test]
    fn an_indeterminate_session_is_shown_by_default() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "crash-1-1.log", "no header here\n");

        assert_eq!(collect(dir.path(), false).len(), 1, "an indeterminate session was hidden");
    }

    #[test]
    fn the_json_shape_carries_the_parsed_identity() {
        let dir = tempfile::tempdir().expect("a temp dir");
        seed(dir.path(), "crash-42-7.log", "body\n");

        let value = to_json(&collect(dir.path(), true));

        let first = &value.as_array().expect("an array")[0];
        assert_eq!(first["name"], "crash-42-7.log");
        assert_eq!(first["start"], 42);
        assert_eq!(first["pid"], 7);
        assert_eq!(first["contents"], "body\n");
        assert_eq!(first["truncated"], false);
    }

    #[test]
    fn terminal_output_escapes_controls_but_keeps_unicode_and_newlines() {
        let input = "café\n\u{1b}]52;c;payload\u{7}\r\t\u{7f}\u{9b}";
        let mut output = Vec::new();

        write_terminal_safe(&mut output, input.as_bytes()).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(output, "café\n\\u{1b}]52;c;payload\\u{7}\\u{d}\\u{9}\\u{7f}\\u{9b}");
        assert!(output.chars().all(|character| character == '\n' || !character.is_control()));
    }

    #[test]
    fn terminal_output_hex_escapes_invalid_utf8() {
        let mut output = Vec::new();

        write_terminal_safe(&mut output, b"tx\xff\n").unwrap();

        assert_eq!(output, b"tx\\xff\n");
    }

    #[test]
    fn redirected_output_remains_byte_exact() {
        let payload = b"tx\x1b\xff\n";
        let mut output = Vec::new();

        print_human_to(&[artifact(payload.to_vec(), false)], &mut output, false).unwrap();

        let header_len = b"==> crash-1-1.log <==\n".len();
        assert_eq!(&output[header_len..header_len + payload.len()], payload);
    }

    #[test]
    fn truncation_is_reported_in_human_output_and_json() {
        let artifact = artifact(vec![b'x'; ARTIFACT_READ_CAP], true);
        let mut output = Vec::new();

        print_human_to(std::slice::from_ref(&artifact), &mut output, true).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("[alacritree: artifact truncated after 262144 bytes]"));
        let value = to_json(&[artifact]);
        assert_eq!(value[0]["truncated"], true);
        assert_eq!(value[0]["contents"].as_str().unwrap().len(), ARTIFACT_READ_CAP);
    }

    #[test]
    fn json_serialization_contains_no_literal_terminal_escape() {
        let value = to_json(&[artifact(b"before\x1bafter".to_vec(), false)]);
        let serialized = serde_json::to_vec(&value).unwrap();

        assert!(!serialized.contains(&0x1b));
        assert_eq!(value[0]["contents"], "before\u{1b}after");
        assert_eq!(value[0]["truncated"], false);
    }
}
