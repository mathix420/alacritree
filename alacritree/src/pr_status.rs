//! Detect whether the current branch has an open PR on GitHub, and cache
//! its base branch so the sidebar diff can target the PR's base instead of
//! the repo's default branch.
//!
//! Why shell out to `gh` rather than hit the API directly: it inherits the
//! user's existing auth and host config (enterprise, multiple accounts), and
//! we already require `git` on PATH — adding `gh` is a familiar dependency
//! for anyone who lives in this workflow.  The lookup is best-effort: if
//! `gh` is missing, unauthenticated, or no PR exists, we silently fall back
//! to the repo's default branch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// Re-query at most this often.  PR base branches rarely change, and a stale
/// answer just falls back to the previous diff target — not worth hammering
/// `gh` on every status refresh.
const TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub base_branch: String,
    pub url: String,
}

#[derive(Default)]
pub struct PrCache {
    entries: HashMap<PathBuf, Entry>,
}

struct Entry {
    /// Branch the cached result was queried for.  Switching branches in the
    /// same worktree invalidates the entry.
    branch: Option<String>,
    info: Option<PrInfo>,
    queried_at: Option<Instant>,
    /// Set while a background thread is running; drained on the next poll.
    pending: Option<Receiver<LookupResult>>,
}

struct LookupResult {
    branch: Option<String>,
    info: Option<PrInfo>,
}

impl PrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the PR info known for `(path, branch)` right now, kicking off
    /// a background refresh if the cache is stale or branch-mismatched.
    /// Never blocks — the caller will see the previous value (or `None`)
    /// until the worker finishes and the next frame picks up the result.
    pub fn poll(
        &mut self,
        path: &Path,
        branch: Option<&str>,
        ctx: &egui::Context,
    ) -> Option<PrInfo> {
        let entry = self.entries.entry(path.to_path_buf()).or_insert_with(|| Entry {
            branch: None,
            info: None,
            queried_at: None,
            pending: None,
        });

        // Drain any completed background lookup before deciding whether to
        // refresh — a result that just arrived shouldn't be ignored.
        if let Some(rx) = entry.pending.as_ref() {
            if let Ok(result) = rx.try_recv() {
                entry.branch = result.branch;
                entry.info = result.info;
                entry.queried_at = Some(Instant::now());
                entry.pending = None;
            }
        }

        let branch_matches = entry.branch.as_deref() == branch;
        let fresh = entry.queried_at.map_or(false, |when| when.elapsed() < TTL);
        let needs_refresh = !branch_matches || !fresh;

        if needs_refresh && entry.pending.is_none() {
            // Clear stale data immediately on branch switch so we don't show
            // a PR base that belongs to a different branch.
            if !branch_matches {
                entry.info = None;
            }
            entry.pending =
                Some(spawn_lookup(path.to_path_buf(), branch.map(str::to_string), ctx.clone()));
        }

        entry.info.clone()
    }
}

fn spawn_lookup(
    path: PathBuf,
    branch: Option<String>,
    ctx: egui::Context,
) -> Receiver<LookupResult> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let info = branch.as_deref().and_then(|b| query_gh(&path, b));
        let _ = tx.send(LookupResult { branch, info });
        ctx.request_repaint();
    });
    rx
}

/// Ask `gh` for the PR associated with `branch` in `path`.  Returns `None`
/// on any failure mode (no `gh`, not authenticated, no PR, non-GitHub
/// remote, ...).  The branch is passed as a positional selector so the
/// answer is tied to that specific branch rather than whatever ref happens
/// to be checked out in the worktree.
fn query_gh(path: &Path, branch: &str) -> Option<PrInfo> {
    let output = Command::new("gh")
        .current_dir(path)
        .args(["pr", "view", branch, "--json", "number,baseRefName,url"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gh_output(&output.stdout)
}

fn parse_gh_output(stdout: &[u8]) -> Option<PrInfo> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let number = value.get("number")?.as_u64()?;
    let base = value.get("baseRefName")?.as_str()?.to_string();
    let url = value.get("url")?.as_str()?.to_string();
    Some(PrInfo { number, base_branch: base, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_json() {
        let stdout = br#"{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}"#;
        let info = parse_gh_output(stdout).unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.base_branch, "main");
        assert_eq!(info.url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn rejects_empty_output() {
        assert!(parse_gh_output(b"").is_none());
    }
}
