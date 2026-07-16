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

use crate::command_ext::CommandExt;
use crate::wsl;

/// Re-query at most this often.  PR base branches rarely change, and a stale
/// answer just falls back to the previous diff target — not worth hammering
/// `gh` on every status refresh.
const TTL: Duration = Duration::from_secs(300);

/// GitHub's PR lifecycle, folded to what the sidebar paints.  `gh` reports
/// draftness as a separate boolean, so OPEN splits into Open/Draft here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub base_branch: String,
    pub url: String,
    pub state: PrState,
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
    branch: String,
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
                entry.branch = Some(result.branch);
                entry.info = result.info;
                entry.queried_at = Some(Instant::now());
                entry.pending = None;
            }
        }

        // A `None` poll (the git-status compute hasn't produced a branch
        // yet, or never will) carries no information about the current
        // branch, so it must not evict or refresh a lookup keyed to a real
        // one from another caller — just read whatever is cached.
        let Some(branch) = branch else {
            return entry.info.clone();
        };

        let invalidate = should_invalidate(entry.branch.as_deref(), Some(branch));
        let fresh = entry.queried_at.map_or(false, |when| when.elapsed() < TTL);

        if (invalidate || !fresh) && entry.pending.is_none() {
            // Clear stale data immediately on branch switch so we don't show
            // a PR base that belongs to a different branch.
            if invalidate {
                entry.info = None;
            }
            entry.pending = Some(spawn_lookup(path.to_path_buf(), branch.to_string(), ctx.clone()));
        }

        entry.info.clone()
    }
}

/// A `None` incoming branch never invalidates — the caller has nothing to
/// compare against. A `Some` branch that disagrees with the cached one means
/// a real branch switch and must invalidate.
fn should_invalidate(cached_branch: Option<&str>, incoming_branch: Option<&str>) -> bool {
    match incoming_branch {
        None => false,
        Some(_) => cached_branch != incoming_branch,
    }
}

fn spawn_lookup(path: PathBuf, branch: String, ctx: egui::Context) -> Receiver<LookupResult> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let info = query_gh(&path, &branch);
        let _ = tx.send(LookupResult { branch, info });
        ctx.request_repaint();
    });
    rx
}

fn pr_state(state: &str, is_draft: bool) -> PrState {
    match state {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        "OPEN" if is_draft => PrState::Draft,
        // Unknown states paint as open rather than vanishing; gh's enum is
        // stable, so this is a forward-compatibility hedge, not a real case.
        _ => PrState::Open,
    }
}

/// Ask `gh` for the PR associated with `branch` in `path`.  Returns `None`
/// on any failure mode (no `gh`, not authenticated, no PR, non-GitHub
/// remote, ...).  The branch is passed as a positional selector so the
/// answer is tied to that specific branch rather than whatever ref happens
/// to be checked out in the worktree.
fn query_gh(path: &Path, branch: &str) -> Option<PrInfo> {
    let mut cmd = match wsl::classify(path) {
        wsl::Location::Windows(p) => {
            let mut c = Command::new("gh");
            c.hide_console().current_dir(p);
            c
        },
        // `gh` must be installed and authenticated *inside* the distro; any
        // failure falls back to the default branch, same as a missing
        // Windows gh.  `--cd` accepts the UNC path natively.
        wsl::Location::Wsl { distro, .. } => {
            let mut c = wsl::command(&distro, Some(path));
            c.arg("gh");
            c
        },
    };
    let output = cmd
        .args(["pr", "view", branch, "--json", "number,baseRefName,url,state,isDraft"])
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
    let state = value.get("state").and_then(|v| v.as_str()).unwrap_or("OPEN");
    let is_draft = value.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(PrInfo { number, base_branch: base, url, state: pr_state(state, is_draft) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_json() {
        let stdout =
            br#"{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}"#;
        let info = parse_gh_output(stdout).unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.base_branch, "main");
        assert_eq!(info.url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn rejects_empty_output() {
        assert!(parse_gh_output(b"").is_none());
    }

    #[test]
    fn parses_pr_states() {
        for (json_state, is_draft, expected) in [
            ("OPEN", false, PrState::Open),
            ("OPEN", true, PrState::Draft),
            ("MERGED", false, PrState::Merged),
            ("CLOSED", false, PrState::Closed),
            ("SOMETHING_NEW", false, PrState::Open),
        ] {
            let stdout = format!(
                r#"{{"baseRefName":"main","number":1,"url":"https://github.com/o/r/pull/1","state":"{json_state}","isDraft":{is_draft}}}"#
            );
            let info = parse_gh_output(stdout.as_bytes()).unwrap();
            assert_eq!(info.state, expected, "state={json_state} draft={is_draft}");
        }
    }

    #[test]
    fn missing_state_fields_default_to_open() {
        // Old gh versions may omit fields we didn't ask for; degrade, don't drop.
        let stdout =
            br#"{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}"#;
        assert_eq!(parse_gh_output(stdout).unwrap().state, PrState::Open);
    }

    fn sample_info() -> PrInfo {
        PrInfo {
            number: 7,
            base_branch: "main".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PrState::Open,
        }
    }

    #[test]
    fn none_branch_does_not_invalidate_a_cached_branch() {
        assert!(!should_invalidate(Some("b"), None));
    }

    #[test]
    fn mismatched_branch_invalidates() {
        assert!(should_invalidate(Some("b"), Some("a")));
    }

    #[test]
    fn matching_branch_does_not_invalidate() {
        assert!(!should_invalidate(Some("b"), Some("b")));
    }

    #[test]
    fn polling_with_none_retains_info_from_a_completed_some_branch_lookup() {
        let mut cache = PrCache::new();
        let path = PathBuf::from("/repo");
        cache.entries.insert(
            path.clone(),
            Entry {
                branch: Some("b".to_string()),
                info: Some(sample_info()),
                queried_at: Some(Instant::now()),
                pending: None,
            },
        );

        let ctx = egui::Context::default();
        let result = cache.poll(&path, None, &ctx);

        assert_eq!(result.map(|info| info.number), Some(7));
        let entry = cache.entries.get(&path).unwrap();
        assert_eq!(entry.branch.as_deref(), Some("b"));
        assert!(entry.info.is_some(), "None poll must not clear the cached info");
        assert!(entry.pending.is_none(), "None poll must not spawn a competing lookup");
    }
}
