//! GitHub as a forge, reached through the `gh` CLI.
//!
//! Why shell out to `gh` rather than hit the API directly: it inherits the
//! user's existing auth and host config (enterprise, multiple accounts), and
//! we already require `git` on PATH, so adding `gh` is a familiar dependency
//! for anyone who lives in this workflow. The lookup is best-effort: if `gh`
//! is missing, unauthenticated, or no PR exists, the caller falls back to the
//! repo's default branch.

mod graphql;
mod settings;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use alacritree_common::jobs::Blocking;
use alacritree_common::tools::{self, Tool};
use alacritree_common::{command_ext, wsl};
use alacritree_forge::{ForgeError, Head, PrInfo, PrState, PullRequests, RemoteForge};

pub use settings::{GhConfig, MovedGhKeys, RawGh};

const GH: &str = "gh";

#[derive(Debug, Clone, Copy, Default)]
pub struct GhForge;

impl RemoteForge for GhForge {
    /// Group a whole burst and ask for each group in turn. The requests
    /// block.
    fn pull_requests(&self, heads: Vec<Head>, blocking: &Blocking) -> PullRequests {
        let mut out = HashMap::new();
        for group in groups(heads, blocking) {
            // A cancel landing between groups has no child to kill, since
            // neither the request nor the sweep registers one, so each group
            // asks before starting rather than forking `gh` for a caller that
            // is gone.
            if blocking.cancelled() {
                break;
            }
            out.extend(query_group(&group, graphql::run, |m, head_owner| {
                query_gh(&m.path, &m.branch, head_owner, blocking)
            }));
        }
        out
    }
}

/// What one request covers: the branches asked about, and one worktree inside
/// the repository to run `gh` from. An absent `slug` means this group has no
/// batched form and runs the per-branch path instead.
struct Group {
    /// Any worktree of this repository; `gh` resolves the repo from its cwd.
    cwd: PathBuf,
    slug: Option<(String, String)>,
    members: Vec<Head>,
    /// The owner each branch pushes to, where one could be read.
    head_owners: HashMap<String, String>,
}

/// One request per repository, chunked, plus one per path that cannot be
/// grouped. Resolving costs a `gh` process per repository, which is why this
/// runs on a worker rather than on the frame.
fn groups(due: Vec<Head>, blocking: &Blocking) -> Vec<Group> {
    groups_with(due, |cwd| resolve_repo(cwd, blocking))
}

/// `resolve` names the repository a group asks about, given any worktree of
/// it. Separate from [`groups`] so a test can pin which repository a group
/// ends up asking without a `gh` process deciding it.
fn groups_with(due: Vec<Head>, resolve: impl Fn(&Path) -> Option<(String, String)>) -> Vec<Group> {
    let mut by_repo: HashMap<(String, String), Group> = HashMap::new();
    let mut ungrouped = Vec::new();
    for m in due {
        // `origin` groups the checkouts that share a repository, and the
        // push remote's owner is whose pull request the branch can have. A
        // WSL checkout arrives without remotes, and its `gh` runs as a
        // script rather than a `Command`.
        let (slug, head_owner) = match &m.remotes {
            Some(remotes) => (
                remotes.origin_url.as_deref().and_then(github_slug_from_url),
                remotes.push_url.as_deref().and_then(github_slug_from_url).map(|(owner, _)| owner),
            ),
            None => (None, None),
        };
        let group = match slug {
            Some((owner, name)) => {
                by_repo.entry((owner.clone(), name.clone())).or_insert_with(|| Group {
                    cwd: m.path.clone(),
                    slug: Some((owner, name)),
                    members: Vec::new(),
                    head_owners: HashMap::new(),
                })
            },
            None => {
                ungrouped.push(Group {
                    cwd: m.path.clone(),
                    slug: None,
                    members: Vec::new(),
                    head_owners: HashMap::new(),
                });
                ungrouped.last_mut().expect("just pushed")
            },
        };
        if let Some(owner) = head_owner {
            group.head_owners.insert(m.branch.clone(), owner);
        }
        group.members.push(m);
    }
    by_repo
        .into_values()
        .flat_map(|mut g| {
            // `origin` says only which worktrees share a repository. Which
            // repository to ask is `gh`'s answer, and the two differ on a fork
            // checkout: `origin` names the fork, while a pull request is listed
            // under the repository it targets. One resolve per repository, so
            // a project's worktrees still cost one process between them.
            g.slug = resolve(&g.cwd);
            g.members
                .chunks(graphql::CHUNK)
                .map(|c| Group {
                    cwd: g.cwd.clone(),
                    slug: g.slug.clone(),
                    members: c.to_vec(),
                    head_owners: g.head_owners.clone(),
                })
                .collect::<Vec<_>>()
        })
        .chain(ungrouped)
        .collect()
}

/// Ask GitHub about a whole group in one request, falling back to the
/// per-branch path when there is no batched form or the request produced no
/// usable answer. GraphQL can need scopes `gh pr list` does not, so an
/// install that works today can fail here, and a project's badges must not
/// vanish when it does.
///
/// An answer naming no PR at all is still an answer and returns as one: a
/// repository whose branches have no open PRs is the common case, and
/// sweeping it per branch would find the same nothing at one process each.
///
/// `request` and `per_branch` are injected so a test can pin which of the two
/// paths a given response takes without spawning `gh`.
fn query_group(
    group: &Group,
    request: impl Fn(&Path, &str) -> Option<Vec<u8>>,
    per_branch: impl Fn(&Head, Option<&str>) -> Result<Option<PrInfo>, ForgeError>,
) -> PullRequests {
    let branches: Vec<String> = group.members.iter().map(|m| m.branch.clone()).collect();
    let head_owner = |branch: &str| group.head_owners.get(branch).map(String::as_str);
    if let Some((owner, name)) = &group.slug {
        let query = graphql::build(owner, name, &branches);
        if let Some(stdout) = request(&group.cwd, &query) {
            if let Some(parsed) = graphql::parse(&stdout, &branches, head_owner) {
                return group
                    .members
                    .iter()
                    .map(|m| (m.path.clone(), Ok(parsed.get(&m.branch).cloned())))
                    .collect();
            }
        }
    }
    group.members.iter().map(|m| (m.path.clone(), per_branch(m, head_owner(&m.branch)))).collect()
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

/// Ask `gh` for the PR associated with `branch` in `path`. The branch is
/// named explicitly so the answer is tied to that specific branch rather than
/// whatever ref happens to be checked out in the worktree.
///
/// `--head` rather than `gh pr view <branch>`: `pr view` matches a PR's head
/// *label*, which is the bare branch only while the head lives in the base
/// repo and becomes `owner:branch` once it lives on a fork. A checkout whose
/// `origin` is a personal fork therefore finds nothing. `--head` filters on
/// the head ref name alone, which both layouts share, and `--state all` keeps
/// the merged and closed badges that `pr list` would otherwise drop.
#[allow(clippy::disallowed_methods)] // Running `gh` is this function's job.
fn query_gh(
    path: &Path,
    branch: &str,
    head_owner: Option<&str>,
    blocking: &Blocking,
) -> Result<Option<PrInfo>, ForgeError> {
    const PR_JSON_FIELDS: &str = "number,baseRefName,url,state,isDraft,headRepositoryOwner";
    // `--head` matches the ref name in every head repository and `--state all`
    // keeps the closed and merged ones, so a generic branch name in a busy base
    // repo overflows `gh`'s default page of 30 and the owner preference below
    // never sees this checkout's own PR.
    const PR_LIMIT: &str = "100";
    match wsl::classify(path) {
        wsl::Location::Windows(p) => {
            let output = command_ext::hidden(tools::program(Tool::Gh))
                .current_dir(p)
                .args([
                    "pr",
                    "list",
                    "--head",
                    branch,
                    "--state",
                    "all",
                    "--limit",
                    PR_LIMIT,
                    "--json",
                    PR_JSON_FIELDS,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .output()
                .map_err(|source| ForgeError::Spawn { program: GH, source })?;
            if !output.status.success() {
                return Err(ForgeError::Failed { program: GH, status: output.status });
            }
            parse_gh_output(&output.stdout, head_owner)
        },
        // WSL gh needs distro-local auth; the registry keeps helper-resolved
        // per-user installs that the default `--exec` PATH cannot find.
        wsl::Location::Wsl { distro, linux_path } => {
            let gh = tools::wsl_in_job(Tool::Gh, &distro, blocking);
            // The push remote's URL rides along on the first line: nothing on
            // the Windows side reads a repository that lives inside the
            // distro, and a second round trip would double the cost of a
            // badge that already forks `gh`. The remote is chosen in git's own
            // push order. The substitution collapses a missing remote to a
            // blank line, so the JSON always starts after exactly one newline.
            let script = r#"cd "$1" || exit 1
r=$(git config --get "branch.$3.pushRemote" || git config --get remote.pushDefault || git config --get "branch.$3.remote")
case "$r" in ''|.) r=origin ;; esac
printf '%s\n' "$(git config --get "remote.$r.url" 2>/dev/null)"
exec "$2" pr list --head "$3" --state all --limit "$4" --json "$5""#;
            let stdout = wsl::run_batch(
                &distro,
                script,
                &[&linux_path, &gh, branch, PR_LIMIT, PR_JSON_FIELDS],
                blocking,
            )
            .map_err(|source| ForgeError::Wsl { program: GH, source })?;
            let (push_url, json) = split_remote_url_line(&stdout);
            let owner = push_url.and_then(github_slug_from_url).map(|(owner, _)| owner);
            parse_gh_output(json, owner.as_deref())
        },
    }
}

/// Split the WSL batch's leading push remote URL off the JSON that follows
/// it. An empty first line means the branch has no readable push remote.
fn split_remote_url_line(stdout: &[u8]) -> (Option<&str>, &[u8]) {
    let Some(end) = stdout.iter().position(|b| *b == b'\n') else {
        // Nothing ran far enough to emit the line; hand the payload to the
        // JSON parser, which rejects it the way it rejects any non-JSON.
        return (None, stdout);
    };
    let url = std::str::from_utf8(&stdout[..end]).ok().map(str::trim).filter(|u| !u.is_empty());
    (url, &stdout[end + 1..])
}

/// The repository `gh` itself would act on from this worktree, which is the
/// one holding the pull requests: `origin` on a fork checkout names the fork,
/// while a pull request opened from it is listed under the repository it
/// targets. Asking `gh` rather than reimplementing its resolution also
/// honours `gh repo set-default` and the `upstream` remote convention.
///
/// `None` for anything that does not answer with a GitHub `owner/name`, which
/// leaves the group on the per-branch path.
#[allow(clippy::disallowed_methods)] // Running `gh` is this function's job.
fn resolve_repo(cwd: &Path, _blocking: &Blocking) -> Option<(String, String)> {
    let output = command_ext::hidden(tools::program(Tool::Gh))
        .current_dir(cwd)
        .args(["repo", "view", "--json", "nameWithOwner"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_name_with_owner(&output.stdout)
}

/// Split `gh repo view --json nameWithOwner` into its two halves.
fn parse_name_with_owner(stdout: &[u8]) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let (owner, name) = value.get("nameWithOwner")?.as_str()?.split_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

/// Owner and repository of a GitHub remote URL, for the shapes git accepts:
/// `https://github.com/owner/repo.git`, `git@github.com:owner/repo.git`, and
/// the scp-style host alias `gh:owner/repo.git`. The `.git` suffix comes off
/// so that two spellings of one remote group together. `None` for anything
/// else, since the owner only breaks ties and an ungroupable worktree just
/// takes the per-branch path.
fn github_slug_from_url(url: &str) -> Option<(String, String)> {
    let (host, path) = split_remote_url(url.trim())?;
    if !is_github_host(host) {
        return None;
    }
    let (owner, repo) = path.trim_start_matches('/').split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

/// Host and path of a remote URL, covering both the scheme form and the
/// scp-style `[user@]host:path` one that git reads whenever the colon comes
/// before any slash.
fn split_remote_url(url: &str) -> Option<(&str, &str)> {
    if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        return Some((remote_host(authority), path));
    }
    let (authority, path) = url.split_once(':')?;
    // A leading slash means an absolute local path (`C:/repos/x`), which git
    // does not read as scp-style however much it looks like one.
    if authority.contains('/') || path.starts_with('/') {
        return None;
    }
    Some((remote_host(authority), path))
}

fn remote_host(authority: &str) -> &str {
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    host.split(':').next().unwrap_or(host)
}

/// `github.com`, or any host with no dot in it. A dotless host is an
/// `~/.ssh/config` alias whose real target we cannot see, and aliases are how
/// fork checkouts pick an SSH identity, so refusing them would blind the owner
/// preference to the layout it exists for. Guessing wrong on one just yields
/// an owner no PR matches.
fn is_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com") || !host.contains('.')
}

/// Pick the PR a head branch's badge should show. Two rules, in order:
///
/// `--head` matches the ref name across *every* head repository, so a generic
/// branch name ("dev", "patch-1") also collects PRs strangers opened from their
/// own forks, and upstream-sync PRs whose head is the upstream's `main`. With
/// `head_owner`, the account the branch pushes to, only that account's PRs
/// and ones reporting no owner stay in the running, and a branch with none of
/// its own gets no badge. Without a readable owner, every PR stays in.
///
/// Among what survives, `gh pr list` answers newest first and a branch
/// accumulates PRs over its life; an open one is the live PR, so it outranks a
/// newer abandoned attempt. Drafts report `OPEN` too, so this covers them.
/// Mirrors how `gh pr view` orders its own candidates.
fn select_pr<'a>(
    prs: &'a [serde_json::Value],
    head_owner: Option<&str>,
) -> Option<&'a serde_json::Value> {
    open_or_newest(prs.iter().filter(|pr| {
        head_owner.is_none_or(|owner| {
            pr_head_owner(pr).is_none_or(|login| login.eq_ignore_ascii_case(owner))
        })
    }))
}

fn open_or_newest<'a>(
    prs: impl Iterator<Item = &'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let mut newest = None;
    for pr in prs {
        if pr.get("state").and_then(|s| s.as_str()) == Some("OPEN") {
            return Some(pr);
        }
        newest = newest.or(Some(pr));
    }
    newest
}

/// `None` from a `gh` too old to report it, or a head repository since
/// deleted. Neither is evidence the PR belongs to someone else.
fn pr_head_owner(pr: &serde_json::Value) -> Option<&str> {
    pr.get("headRepositoryOwner")?.get("login")?.as_str()
}

/// `gh` answers errors as a bare object, so valid JSON that is not a list is
/// as malformed as output that is not JSON at all.
fn parse_gh_output(stdout: &[u8], head_owner: Option<&str>) -> Result<Option<PrInfo>, ForgeError> {
    let list: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|_| ForgeError::Malformed { program: GH })?;
    let prs = list.as_array().ok_or(ForgeError::Malformed { program: GH })?;
    Ok(select_and_build(prs, head_owner))
}

/// Select the winning PR from a candidate list and build the `PrInfo` for it.
/// Shared by the single-branch `gh pr list` path and the batched GraphQL one,
/// so a change to selection or field reads applies to both by construction.
fn select_and_build(prs: &[serde_json::Value], head_owner: Option<&str>) -> Option<PrInfo> {
    let value = select_pr(prs, head_owner)?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use alacritree_common::jobs;
    use alacritree_vcs::Remotes;

    /// Remotes as the git backend reads them for a native checkout.
    fn remotes(origin: &str, push: &str) -> Option<Remotes> {
        Some(Remotes { origin_url: Some(origin.into()), push_url: Some(push.into()) })
    }

    fn parsed(stdout: &[u8], head_owner: Option<&str>) -> PrInfo {
        parse_gh_output(stdout, head_owner).expect("well-formed").expect("a PR")
    }

    #[test]
    fn parses_gh_json() {
        let stdout =
            br#"[{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}]"#;
        let info = parsed(stdout, None);
        assert_eq!(info.number, 42);
        assert_eq!(info.base_branch, "main");
        assert_eq!(info.url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn rejects_empty_output() {
        assert!(matches!(parse_gh_output(b"", None), Err(ForgeError::Malformed { .. })));
    }

    #[test]
    fn an_empty_pr_list_means_no_pr() {
        assert!(parse_gh_output(b"[]", None).unwrap().is_none());
    }

    /// `gh` answers errors as a bare object, so valid JSON that is not a list
    /// must degrade the same way malformed output does.
    #[test]
    fn rejects_json_that_is_not_a_list() {
        assert!(matches!(parse_gh_output(b"{}", None), Err(ForgeError::Malformed { .. })));
    }

    /// A head branch accumulates PRs over its life. `gh pr list` answers newest
    /// first, but the open one is the live PR, and a newer abandoned attempt
    /// must not shadow it.
    #[test]
    fn an_open_pr_wins_over_a_newer_closed_one() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"CLOSED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"OPEN","isDraft":false}
        ]"#;
        let info = parsed(stdout, None);
        assert_eq!(info.number, 4);
        assert_eq!(info.state, PrState::Open);
    }

    #[test]
    fn a_draft_counts_as_open_when_selecting() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"MERGED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"OPEN","isDraft":true}
        ]"#;
        let info = parsed(stdout, None);
        assert_eq!(info.number, 4);
        assert_eq!(info.state, PrState::Draft);
    }

    /// With nothing open, the newest attempt is the one worth painting.
    #[test]
    fn the_newest_pr_wins_when_none_are_open() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"MERGED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"CLOSED","isDraft":false}
        ]"#;
        let info = parsed(stdout, None);
        assert_eq!(info.number, 9);
        assert_eq!(info.state, PrState::Merged);
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
                r#"[{{"baseRefName":"main","number":1,"url":"https://github.com/o/r/pull/1","state":"{json_state}","isDraft":{is_draft}}}]"#
            );
            let info = parsed(stdout.as_bytes(), None);
            assert_eq!(info.state, expected, "state={json_state} draft={is_draft}");
        }
    }

    #[test]
    fn missing_state_fields_default_to_open() {
        // Old gh versions may omit fields we didn't ask for; degrade, don't drop.
        let stdout =
            br#"[{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}]"#;
        assert_eq!(parsed(stdout, None).state, PrState::Open);
    }

    fn pr(number: u64, state: &str, head_owner: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "baseRefName": "main",
            "url": format!("u{number}"),
            "state": state,
            "isDraft": false,
            "headRepositoryOwner": { "login": head_owner },
        })
    }

    fn number_of(pr: Option<&serde_json::Value>) -> Option<u64> {
        pr?.get("number")?.as_u64()
    }

    #[test]
    fn select_pr_prefers_an_open_pr_over_a_newer_non_open_one() {
        let prs = [pr(9, "MERGED", "someone"), pr(4, "OPEN", "someone")];
        assert_eq!(number_of(select_pr(&prs, None)), Some(4));
    }

    #[test]
    fn select_pr_takes_the_newest_when_none_are_open() {
        let prs = [pr(9, "MERGED", "someone"), pr(4, "CLOSED", "someone")];
        assert_eq!(number_of(select_pr(&prs, None)), Some(9));
    }

    /// `--head` matches the ref name in *every* head repository, so a generic
    /// branch name ("dev", "patch-1") collects strangers' PRs. Theirs must not
    /// decide this worktree's badge or diff base, however live they are.
    #[test]
    fn select_pr_prefers_the_head_owners_pr_over_a_strangers_open_one() {
        let prs = [pr(9, "OPEN", "stranger"), pr(4, "MERGED", "me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_prefers_an_open_pr_among_the_head_owners_own() {
        let prs = [pr(9, "MERGED", "me"), pr(7, "OPEN", "stranger"), pr(4, "OPEN", "me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    /// GitHub logins are case-insensitive, so a remote URL that disagrees with
    /// the API's casing still names the same account.
    #[test]
    fn select_pr_matches_the_owner_case_insensitively() {
        let prs = [pr(9, "OPEN", "stranger"), pr(4, "MERGED", "Me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_takes_nothing_when_every_pr_is_another_owners() {
        let prs = [pr(9, "MERGED", "stranger"), pr(4, "OPEN", "other")];
        assert!(select_pr(&prs, Some("me")).is_none());
    }

    #[test]
    fn select_pr_keeps_every_pr_without_a_readable_owner() {
        let prs = [pr(9, "MERGED", "stranger"), pr(4, "OPEN", "other")];
        assert_eq!(number_of(select_pr(&prs, None)), Some(4));
    }

    /// A `gh` too old to report the head owner must not filter every candidate
    /// away, since an unknown owner is no evidence the PR belongs to someone
    /// else.
    #[test]
    fn select_pr_tolerates_a_missing_head_owner() {
        let prs = [serde_json::json!({"number": 4, "state": "OPEN"})];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_reports_nothing_for_an_empty_list() {
        assert!(select_pr(&[], Some("me")).is_none());
    }

    /// The regression this preference exists for: a stale PR opened by a
    /// stranger on the same branch name, still carrying the base branch the
    /// repository has since renamed away from.
    #[test]
    fn the_head_owners_pr_decides_the_diff_base() {
        let stdout = br#"[
            {"baseRefName":"master","number":9,"url":"u9","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"stranger"}},
            {"baseRefName":"main","number":4,"url":"u4","state":"MERGED","isDraft":false,"headRepositoryOwner":{"login":"me"}}
        ]"#;
        let info = parsed(stdout, Some("me"));
        assert_eq!(info.number, 4);
        assert_eq!(info.base_branch, "main");
    }

    #[test]
    fn derives_the_slug_from_an_https_remote() {
        let slug = github_slug_from_url("https://github.com/owner/repo.git");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn derives_the_slug_from_an_scp_style_ssh_remote() {
        let slug = github_slug_from_url("git@github.com:owner/repo.git");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    /// The slug is the grouping key, so two spellings of one remote have to
    /// produce the same one or a repository splits into two requests.
    #[test]
    fn two_spellings_of_one_remote_share_a_grouping_key() {
        let with_suffix = github_slug_from_url("https://github.com/owner/repo.git");
        let without = github_slug_from_url("git@github.com:owner/repo");
        assert_eq!(with_suffix, Some(("owner".to_string(), "repo".to_string())));
        assert_eq!(with_suffix, without);
    }

    /// An `~/.ssh/config` alias is how a fork checkout picks an identity, and it
    /// hides the host it resolves to. Reading it as foreign would blind the
    /// preference to exactly the layout it exists for.
    #[test]
    fn derives_the_owner_from_an_ssh_host_alias() {
        let slug = github_slug_from_url("gh:owner/repo.git");
        assert_eq!(slug.map(|(owner, _)| owner).as_deref(), Some("owner"));
    }

    #[test]
    fn rejects_a_non_github_remote() {
        assert!(github_slug_from_url("https://gitlab.com/owner/repo.git").is_none());
        assert!(github_slug_from_url("git@gitlab.com:owner/repo.git").is_none());
    }

    #[test]
    fn rejects_a_malformed_remote() {
        assert!(github_slug_from_url("").is_none());
        assert!(github_slug_from_url("not a url").is_none());
        assert!(github_slug_from_url("https://github.com/owner").is_none());
        assert!(github_slug_from_url("C:/repos/checkout").is_none());
    }

    #[test]
    fn the_wsl_batch_line_carries_the_origin_url() {
        let (url, json) = split_remote_url_line(b"gh:me/repo.git\n[]");
        assert_eq!(url, Some("gh:me/repo.git"));
        assert_eq!(json, b"[]".as_slice());
    }

    #[test]
    fn a_worktree_without_a_remote_leaves_the_wsl_line_blank() {
        let (url, json) = split_remote_url_line(b"\n[]");
        assert_eq!(url, None);
        assert_eq!(json, b"[]".as_slice());
    }

    fn sample_info() -> PrInfo {
        PrInfo {
            number: 7,
            base_branch: "main".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PrState::Open,
        }
    }

    fn group_of(branches: &[&str]) -> Group {
        Group {
            cwd: PathBuf::from("/repo"),
            slug: Some(("owner".to_string(), "repo".to_string())),
            members: branches
                .iter()
                .map(|b| Head {
                    path: PathBuf::from(format!("/repo/{b}")),
                    branch: (*b).into(),
                    remotes: None,
                })
                .collect(),
            head_owners: HashMap::new(),
        }
    }

    /// The PR numbers an answer names, in path order.
    fn numbers(answer: &PullRequests) -> Vec<(PathBuf, Option<u64>)> {
        let mut out: Vec<_> = answer
            .iter()
            .map(|(path, found)| {
                let found = found.as_ref().expect("no lookup failed");
                (path.clone(), found.as_ref().map(|pr| pr.number))
            })
            .collect();
        out.sort();
        out
    }

    /// A repository where nothing has a PR is the common case. Reading its
    /// answer as a failure would spend one `gh pr list` per branch finding the
    /// same nothing, every TTL, which is the cost this batching exists to
    /// remove.
    #[test]
    fn a_good_response_with_no_prs_does_not_fall_back() {
        let group = group_of(&["topic-a", "topic-b"]);
        let sweeps = AtomicUsize::new(0);

        let found = query_group(
            &group,
            |_, _| {
                Some(br#"{"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[]}}}}"#.to_vec())
            },
            |_, _| {
                sweeps.fetch_add(1, Ordering::Relaxed);
                Ok(Some(sample_info()))
            },
        );

        assert_eq!(numbers(&found), [
            (PathBuf::from("/repo/topic-a"), None),
            (PathBuf::from("/repo/topic-b"), None),
        ]);
        assert_eq!(sweeps.load(Ordering::Relaxed), 0, "an answer of `none` is still an answer");
    }

    /// GraphQL can need scopes `gh pr list` does not, and GitHub reports a
    /// query it could not run as an HTTP 200 with a null `repository`. Every
    /// badge in the project depends on that reading as a failure.
    #[test]
    fn a_failed_request_sweeps_the_group_per_branch() {
        let group = group_of(&["topic-a", "topic-b"]);
        let sweeps = AtomicUsize::new(0);

        let found = query_group(
            &group,
            |_, _| Some(br#"{"data":{"repository":null},"errors":[{"message":"nope"}]}"#.to_vec()),
            |_, _| {
                sweeps.fetch_add(1, Ordering::Relaxed);
                Ok(Some(sample_info()))
            },
        );

        assert_eq!(sweeps.load(Ordering::Relaxed), 2, "one lookup per branch");
        assert_eq!(numbers(&found), [
            (PathBuf::from("/repo/topic-a"), Some(7)),
            (PathBuf::from("/repo/topic-b"), Some(7)),
        ]);
    }

    /// A per-branch lookup that fails reports its own failure, and leaves the
    /// other branches' answers alone.
    #[test]
    fn a_failed_per_branch_lookup_reports_for_its_own_checkout() {
        let mut group = group_of(&["topic-a", "topic-b"]);
        group.slug = None;

        let found = query_group(
            &group,
            |_, _| panic!("a group with no repository has nothing to ask about"),
            |m, _| {
                if m.branch == "topic-a" {
                    Err(ForgeError::Malformed { program: GH })
                } else {
                    Ok(Some(sample_info()))
                }
            },
        );

        assert!(matches!(found[Path::new("/repo/topic-a")], Err(ForgeError::Malformed { .. })));
        assert_eq!(found[Path::new("/repo/topic-b")].as_ref().unwrap(), &Some(sample_info()));
    }

    /// Two clones of one repository on the same branch share a group, and
    /// each still gets the answer its own lookup found.
    #[test]
    fn checkouts_sharing_a_branch_name_keep_their_own_answers() {
        let mut group = group_of(&[]);
        group.slug = None;
        group.members = ["/a", "/b"]
            .into_iter()
            .map(|path| Head { path: PathBuf::from(path), branch: "main".into(), remotes: None })
            .collect();

        let found = query_group(
            &group,
            |_, _| None,
            |m, _| Ok((m.path == Path::new("/a")).then(sample_info)),
        );

        assert_eq!(numbers(&found), [(PathBuf::from("/a"), Some(7)), (PathBuf::from("/b"), None)]);
    }

    /// A group with no repository to name, a WSL worktree or one whose remote
    /// nothing could read, never reaches the batched form at all.
    #[test]
    fn a_group_without_a_repository_never_asks_for_a_batch() {
        let mut group = group_of(&["topic"]);
        group.slug = None;

        let found = query_group(
            &group,
            |_, _| panic!("a group with no repository has nothing to ask about"),
            |_, _| Ok(Some(sample_info())),
        );

        assert_eq!(found.len(), 1);
    }

    /// A cancel landing between groups has no child to kill, since neither the
    /// batched request nor the per-branch sweep registers one, so the loop has
    /// to ask. Otherwise a burst keeps forking `gh` to build an answer the
    /// caller has already backed off and nobody will read.
    #[test]
    fn a_lookup_stops_between_groups_once_cancelled() {
        let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("temp dir")).collect();
        let due: Vec<Head> = dirs
            .iter()
            .map(|d| Head { path: d.path().to_path_buf(), branch: "topic".into(), remotes: None })
            .collect();
        let (tx, rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();

        let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            // The started signal keeps the task off the pool's pre-start skip,
            // and the gate keeps it from racing past the first check before
            // the cancel flag lands.
            let _ = started_tx.send(());
            let _ = gate_rx.recv();
            let _ = tx.send(GhForge.pull_requests(due, blocking));
        });
        started_rx.recv_timeout(Duration::from_secs(5)).expect("the job never started");
        drop(job);
        let _ = gate_tx.send(());

        let out = rx.recv_timeout(Duration::from_secs(30)).expect("the lookup never returned");
        assert!(out.is_empty(), "a cancelled burst kept asking: {out:?}");
    }

    /// A WSL worktree arrives with no remotes and has no `Command` to pipe a
    /// query into. Grouping must leave it on the per-branch path rather than
    /// dropping it, or its badge disappears.
    #[test]
    fn an_ungroupable_path_still_gets_its_own_group() {
        let dir = tempfile::tempdir().expect("temp dir");
        let due = vec![Head { path: dir.path().to_path_buf(), branch: "topic".into(), remotes: None }];
        let resolves = AtomicUsize::new(0);

        let out = groups_with(due, |_| {
            resolves.fetch_add(1, Ordering::Relaxed);
            Some(("resolved".to_string(), "repo".to_string()))
        });

        assert_eq!(out.len(), 1);
        assert!(out[0].slug.is_none(), "a repo with no readable origin cannot be grouped");
        assert_eq!(out[0].members.len(), 1);
        assert_eq!(resolves.load(Ordering::Relaxed), 0, "and costs no `gh` process to find out");
    }

    /// `origin` on a fork checkout names the fork, and a pull request opened
    /// from it belongs to the repository it targets. Asking the fork finds
    /// nothing, so the group must ask whatever `gh` resolves instead, once for
    /// the repository rather than once per worktree.
    #[test]
    fn a_group_asks_the_resolved_repository_not_its_origin() {
        let fork = || remotes("https://github.com/me/fork.git", "https://github.com/me/fork.git");
        let due = vec![
            Head { path: PathBuf::from("/r/main"), branch: "topic-a".into(), remotes: fork() },
            Head { path: PathBuf::from("/r/wt-topic-b"), branch: "topic-b".into(), remotes: fork() },
        ];
        let resolves = AtomicUsize::new(0);

        let out = groups_with(due, |_| {
            resolves.fetch_add(1, Ordering::Relaxed);
            Some(("upstream".to_string(), "repo".to_string()))
        });

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].slug, Some(("upstream".to_string(), "repo".to_string())));
        assert_eq!(resolves.load(Ordering::Relaxed), 1, "one resolve for the whole repository");
    }

    /// The remotes arrive with the head, so grouping reads no repository: the
    /// checkout here does not even exist.
    #[test]
    fn a_head_is_grouped_by_the_remotes_it_carries() {
        let remotes = Some(Remotes {
            origin_url: Some("https://github.com/up/repo.git".into()),
            push_url: Some("git@github.com:me/repo.git".into()),
        });
        let due = vec![Head { path: PathBuf::from("/no/such/checkout"), branch: "topic".into(), remotes }];

        let out = groups_with(due, |_| Some(("up".to_string(), "repo".to_string())));

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].slug, Some(("up".to_string(), "repo".to_string())));
        assert_eq!(out[0].head_owners.get("topic").map(String::as_str), Some("me"));
    }

    /// Nothing groups a worktree whose repository cannot be resolved, so it
    /// keeps the per-branch path rather than losing its badge.
    #[test]
    fn a_repository_that_does_not_resolve_falls_back_to_per_branch() {
        let origin = "https://github.com/owner/repo.git";
        let due = vec![Head {
            path: PathBuf::from("/r"),
            branch: "topic".into(),
            remotes: remotes(origin, origin),
        }];

        let out = groups_with(due, |_| None);

        assert_eq!(out.len(), 1);
        assert!(out[0].slug.is_none());
        assert_eq!(out[0].members.len(), 1);
    }

    /// A GraphQL answer carrying `nodes` as the only branch's PRs.
    fn graphql_answer(nodes: &[serde_json::Value]) -> Vec<u8> {
        let nodes: Vec<_> = nodes
            .iter()
            .map(|pr| {
                let mut pr = pr.clone();
                pr["url"] = "u".into();
                pr["baseRefName"] = "main".into();
                pr
            })
            .collect();
        serde_json::json!({ "data": { "repository": { "b0": { "nodes": nodes } } } })
            .to_string()
            .into_bytes()
    }

    /// Group `branch` of a repository whose `origin` and push remote have
    /// these URLs, and ask with `answer` standing in for GitHub. Returns the
    /// PR number the branch's badge shows, if any.
    fn ask_as_github(origin: &str, push: &str, branch: &str, answer: Vec<u8>) -> Option<u64> {
        let due =
            vec![Head { path: PathBuf::from("/r"), branch: branch.into(), remotes: remotes(origin, push) }];
        let groups = groups_with(due, |_| Some(("upstream".to_string(), "repo".to_string())));
        assert_eq!(groups.len(), 1);
        let found = query_group(
            &groups[0],
            |_, _| Some(answer.clone()),
            |_, _| panic!("the batch answered"),
        );
        numbers(&found).into_iter().next().and_then(|(_, number)| number)
    }

    /// An upstream-sync PR opened from the upstream's `main` matches a fork's
    /// `main` by head ref name. It is not the fork's PR, and a branch with no
    /// PR of its own gets no badge.
    #[test]
    fn a_branch_with_only_another_owners_pr_gets_no_badge() {
        let found = ask_as_github(
            "gh:me/repo.git",
            "gh:me/repo.git",
            "main",
            graphql_answer(&[pr(12, "CLOSED", "upstream")]),
        );

        assert_eq!(found, None, "matched another owner's PR");
    }

    /// A branch pushed to a fork while `origin` names the upstream has its PR
    /// under the fork's owner, however live the upstream's own PR is.
    #[test]
    fn a_branch_pushed_to_a_fork_keeps_the_forks_pr() {
        let found = ask_as_github(
            "https://github.com/upstream/repo.git",
            "https://github.com/me/repo.git",
            "topic",
            graphql_answer(&[pr(9, "OPEN", "upstream"), pr(4, "OPEN", "me")]),
        );

        assert_eq!(found, Some(4));
    }

    #[test]
    fn reads_the_repository_gh_resolved() {
        let slug = parse_name_with_owner(br#"{"nameWithOwner":"mathix420/alacritree"}"#);
        assert_eq!(slug, Some(("mathix420".to_string(), "alacritree".to_string())));
    }

    #[test]
    fn rejects_output_that_names_no_repository() {
        assert!(parse_name_with_owner(b"").is_none());
        assert!(parse_name_with_owner(b"{}").is_none());
        assert!(parse_name_with_owner(br#"{"nameWithOwner":"alacritree"}"#).is_none());
        assert!(parse_name_with_owner(br#"{"nameWithOwner":"/alacritree"}"#).is_none());
    }

    /// Branches of one repository share a request; a chunk boundary splits them
    /// into two rather than growing one request without limit.
    #[test]
    fn one_repository_chunks_at_the_limit() {
        let origin = "https://github.com/owner/repo.git";
        let due: Vec<Head> = (0..graphql::CHUNK + 1)
            .map(|i| Head {
                path: PathBuf::from("/r"),
                branch: format!("b{i}"),
                remotes: remotes(origin, origin),
            })
            .collect();

        let out = groups_with(due, |_| Some(("owner".to_string(), "repo".to_string())));

        assert_eq!(out.len(), 2, "one chunk over the limit is two requests");
        assert!(out.iter().all(|g| g.slug == Some(("owner".into(), "repo".into()))));
        assert_eq!(out.iter().map(|g| g.members.len()).sum::<usize>(), graphql::CHUNK + 1);
    }
}
