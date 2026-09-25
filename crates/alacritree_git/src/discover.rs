//! Which checkouts a git repository has, read natively through libgit2 or
//! from one batched `sh` round trip into a WSL distro.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alacritree_common::{jobs, wsl};
use alacritree_vcs::{Checkout, Head, Liveness, Repository, VcsError};
use git2::Repository as GitRepo;

use crate::default_branch::{self, Evidence, WellKnown};
use crate::{liveness, upstream};

/// What an in-distro discovery round trip established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WslAnswer {
    Repo,
    NotARepo,
    /// The distro could not be reached, or answered malformed — the tree is
    /// unknown rather than empty.
    Unreachable,
}

/// Decide what a discovery round trip established, kept separate from running
/// it so all three outcomes are reachable without a WSL host.
fn classify_wsl_answer(reached: bool, is_repo: bool, worktrees_parsed: usize) -> WslAnswer {
    if !reached {
        WslAnswer::Unreachable
    } else if !is_repo {
        WslAnswer::NotARepo
    } else if worktrees_parsed == 0 {
        // A repository always has at least its main checkout.
        WslAnswer::Unreachable
    } else {
        WslAnswer::Repo
    }
}

pub(crate) fn discover(
    root: &Path,
    upstream: bool,
    blocking: &jobs::Blocking,
) -> Result<Repository, VcsError> {
    match wsl::classify(root) {
        wsl::Location::Wsl { distro, linux_path } => {
            let run = |script: &str, args: &[&str]| wsl::run_batch(&distro, script, args, blocking);
            discover_from_batch(root.to_path_buf(), &distro, &linux_path, upstream, run)
        },
        wsl::Location::Windows(_) => match GitRepo::open(root) {
            Ok(repo) => Ok(from_repo(root, &repo, upstream)),
            Err(_) => Err(VcsError::NotARepository(root.to_path_buf())),
        },
    }
}

/// Discovery once the round trip is somebody else's problem, so a test
/// can hand it recorded stdout instead of a live distro.
fn discover_from_batch(
    root: PathBuf,
    distro: &str,
    linux_path: &str,
    upstream: bool,
    run: impl Fn(&str, &[&str]) -> Result<Vec<u8>, wsl::BatchError>,
) -> Result<Repository, VcsError> {
    let upstream_arg = if upstream { "1" } else { "0" };
    let batch = discover_batch();
    let stdout = run(&batch.script(), &[linux_path, upstream_arg])
        .map_err(|e| VcsError::Unreachable(e.to_string()))?;
    let reply = batch.read(&stdout);

    let records = parse_worktree_list_z(reply.bytes("worktrees"));
    let upstreams = upstream::parse_for_each_ref(reply.bytes("upstreams"));
    let checkouts: Vec<Checkout> = records
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let path = wsl::linux_to_windows(&rec.path, distro);
            // Same shape as the git2 arm: the branch when there is one,
            // and the short OID either way.
            let head = Head {
                name: rec.branch.clone(),
                revision: rec.head.as_ref().map(|h| h.chars().take(7).collect()),
                distance: None,
            };
            let wt_name = if i == 0 { "main".to_string() } else { dir_name(&path) };
            let gone = i != 0 && liveness::presence(&path) == Liveness::Missing;
            let upstream = rec.branch.as_deref().and_then(|b| upstreams.get(b).cloned());
            Checkout { name: wt_name, path, head, is_main: i == 0, gone, upstream }
        })
        .collect();

    match classify_wsl_answer(true, reply.text("is_repo") == "yes", checkouts.len()) {
        WslAnswer::Unreachable => {
            Err(VcsError::Unreachable("the distro's reply held no worktree list".into()))
        },
        WslAnswer::NotARepo => Err(VcsError::NotARepository(root)),
        WslAnswer::Repo => Ok(Repository {
            trunk: default_branch_from_batch(
                &reply.text("origin_head"),
                &reply.text("well_known_heads"),
                &reply.text("init_default"),
            ),
            checkouts,
            home: Some(reply.text("home")).filter(|h| !h.is_empty()),
        }),
    }
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| wsl::display_path(path))
}

fn from_repo(root: &Path, repo: &GitRepo, upstream: bool) -> Repository {
    let main_path = repo.workdir().map(|p| p.to_path_buf()).unwrap_or_else(|| root.to_path_buf());

    let upstreams = if upstream { upstream::map_from_repo(repo) } else { HashMap::new() };
    // A detached head names no branch, so it never adopts the state of a
    // branch that happens to share its short OID.
    let lookup = |head: &Head| head.name.as_deref().and_then(|b| upstreams.get(b).cloned());

    let mut checkouts = Vec::new();
    let head = current_head(repo);
    let upstream = lookup(&head);
    checkouts.push(Checkout {
        name: "main".to_string(),
        path: main_path.clone(),
        head,
        is_main: true,
        gone: false,
        upstream,
    });

    if let Ok(names) = repo.worktrees() {
        for name in names.iter().flatten() {
            if let Ok(wt) = repo.find_worktree(name) {
                let path = wt.path().to_path_buf();
                let head = GitRepo::open(&path)
                    .ok()
                    .map(|wt_repo| current_head(&wt_repo))
                    .filter(|head| head.label().is_some())
                    .unwrap_or_else(|| Head {
                        name: branch_from_admin_head(repo, name),
                        ..Head::default()
                    });
                let upstream = lookup(&head);
                checkouts.push(Checkout {
                    name: name.to_string(),
                    // The checkout's own `.git`, not git2's `is_prunable`: a
                    // *locked* worktree with a missing checkout is not
                    // git-prunable but still cannot host a shell, and a
                    // half-finished remove leaves the directory behind
                    // without it.
                    gone: liveness::presence(&path) == Liveness::Missing,
                    path,
                    head,
                    is_main: false,
                    upstream,
                });
            }
        }
    }

    Repository { trunk: detect_default_branch(repo), checkouts, home: None }
}

/// The branch HEAD names, when it names one, and the short OID either way.
/// Keeping them apart is what tells a detached head from a branch that
/// happens to be named like an OID.
pub(crate) fn current_head(repo: &GitRepo) -> Head {
    let Ok(head) = repo.head() else {
        return Head::default();
    };
    let revision = head.target().map(|oid| oid.to_string().chars().take(7).collect());
    let name = if head.is_branch() { head.shorthand().map(str::to_string) } else { None };
    Head { name, revision, distance: None }
}

/// A prunable worktree's checkout is gone, so its HEAD can't be read via
/// `Repository::open`. Git still records it in the main repo's admin area
/// (`.git/worktrees/<name>/HEAD`) — parse the symref line from there.
fn branch_from_admin_head(repo: &GitRepo, worktree_name: &str) -> Option<String> {
    let head = repo.path().join("worktrees").join(worktree_name).join("HEAD");
    let contents = std::fs::read_to_string(head).ok()?;
    contents.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// What git2 can see about this repository's default branch, ranked by
/// [`default_branch::resolve`].
pub(crate) fn detect_default_branch(repo: &GitRepo) -> Option<String> {
    let has = |name: &str| repo.find_reference(&format!("refs/heads/{name}")).is_ok();

    let origin_head = repo
        .find_reference("refs/remotes/origin/HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().map(str::to_string))
        .and_then(|t| t.strip_prefix("refs/remotes/origin/").map(str::to_string));

    let present: Vec<&str> =
        WellKnown::ALL.iter().map(|c| c.as_str()).filter(|name| has(name)).collect();

    let init_default = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_string("init.defaultBranch").ok())
        .filter(|name| !name.is_empty() && has(name));

    default_branch::resolve(&Evidence {
        origin_head: origin_head.as_deref(),
        present,
        init_default: init_default.as_deref(),
        ..Evidence::default()
    })
}

/// Sections: 0 repo-or-not, 1 `worktree list --porcelain -z`,
/// 2 origin/HEAD symref, 3 which common default-branch names exist,
/// 4 `init.defaultBranch` only if it names an existing branch,
/// 5 the distro's `$HOME`, 6 upstream tracking state per local branch —
/// this last command only runs when `$2` is `"1"`, since `for-each-ref` with
/// `%(upstream:track)` computes divergence for every branch even when the
/// caller only wants the parse skipped.
/// The `for-each-ref` format the upstream section emits.  A macro rather than
/// a const so the script can `concat!` it and tests can hand the identical
/// string to git, instead of asserting against a second copy that can drift.
///
/// `lstrip=2` rather than `:short`: shortening consults other ref namespaces
/// and yields `heads/<name>` for a branch whose name a tag also uses, which
/// no longer joins against the plain branch name in a worktree record.
macro_rules! upstream_format {
    () => {
        "%(refname:lstrip=2)%09%(upstream:short)%09%(upstream:track,nobracket)"
    };
}

/// Everything discovery asks a WSL distro, in one round trip.  `$1` is the
/// repository path and `$2` is `"1"` when upstream tracking is wanted.
fn discover_batch() -> wsl::Batch {
    wsl::Batch::new(r#"p="$1""#)
        .section("is_repo", r#"git -C "$p" rev-parse --is-inside-work-tree >/dev/null 2>&1 && printf yes || printf no"#)
        .section("worktrees", r#"git -C "$p" worktree list --porcelain -z 2>/dev/null"#)
        .section("origin_head", r#"git -C "$p" symbolic-ref refs/remotes/origin/HEAD 2>/dev/null"#)
        .section(
            "well_known_heads",
            format!(
                r#"git -C "$p" for-each-ref --format='%(refname:short)' {} 2>/dev/null"#,
                WellKnown::shell_head_refs()
            ),
        )
        .section(
            "init_default",
            r#"cfg=$(git -C "$p" config init.defaultBranch 2>/dev/null)
if [ -n "$cfg" ] && git -C "$p" rev-parse --verify --quiet "refs/heads/$cfg" >/dev/null 2>&1; then printf '%s' "$cfg"; fi"#,
        )
        .section("home", r#"printf '%s' "$HOME""#)
        .section(
            "upstreams",
            concat!(
                r#"if [ "$2" = "1" ]; then
  LC_ALL=C git -C "$p" for-each-ref --format='"#,
                upstream_format!(),
                r#"' refs/heads/ 2>/dev/null
fi"#
            ),
        )
}

/// One record from `git worktree list --porcelain -z`.  The main worktree is
/// always the first record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    path: String,
    head: Option<String>,
    branch: Option<String>,
}

/// Parse `git worktree list --porcelain -z`: attributes are NUL-terminated
/// `label value` lines; an empty line (two consecutive NULs) ends a record.
/// `detached`/`bare`/`locked`/`prunable` labels need no handling — a
/// detached record simply carries no `branch`.
fn parse_worktree_list_z(bytes: &[u8]) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for token in bytes.split(|&b| b == 0) {
        let token = String::from_utf8_lossy(token);
        let token = token.trim_matches('\n');
        if token.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = token.strip_prefix("worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(WorktreeRecord { path: path.to_string(), head: None, branch: None });
        } else if let Some(record) = current.as_mut() {
            if let Some(sha) = token.strip_prefix("HEAD ") {
                record.head = Some(sha.to_string());
            } else if let Some(branch) = token.strip_prefix("branch ") {
                record.branch =
                    Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string());
            }
        }
    }
    if let Some(record) = current.take() {
        records.push(record);
    }
    records
}

/// The same ranking from batched output.  The script has already dropped an
/// `init.defaultBranch` naming no branch, so `config_default` arrives verified.
fn default_branch_from_batch(
    origin_head: &str,
    existing: &str,
    config_default: &str,
) -> Option<String> {
    default_branch::resolve(&Evidence {
        origin_head: origin_head.trim().strip_prefix("refs/remotes/origin/"),
        present: existing.lines().map(str::trim).collect(),
        init_default: Some(config_default),
        ..Evidence::default()
    })
}

#[cfg(test)]
// Fixtures drive real processes and wait on them; no frame is pending.
#[allow(clippy::disallowed_methods)]
mod tests {
    use alacritree_common::command_ext;
    use alacritree_vcs::VersionControl;

    use super::*;
    use crate::GitBackend;

    /// `%(refname:short)` shortens to the *unambiguous* name, so a branch that
    /// shares its name with a tag comes back as `heads/<name>`.  Worktree
    /// records carry the plain branch name, so any shortening that consults
    /// other ref namespaces breaks the join and the row loses its badge.
    #[test]
    fn the_upstream_format_keys_branches_by_plain_name_even_when_a_tag_shares_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let out = command_ext::hidden("git")
                .current_dir(dir.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        git(&["init", "-b", "main"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "x"]);
        git(&["branch", "release"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "tag", "-m", "collide", "release"]);

        let out =
            git(&["for-each-ref", &format!("--format={}", upstream_format!()), "refs/heads/"]);
        let map = upstream::parse_for_each_ref(&out);

        assert!(
            map.contains_key("release"),
            "expected a `release` key, got {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn only_a_reachable_distro_gives_an_authoritative_answer() {
        let (reached, unreachable) = (true, false);
        let (repo, not_repo) = (true, false);

        // The round trip failed: the tree is unknown, not empty.
        assert_eq!(classify_wsl_answer(unreachable, not_repo, 0), WslAnswer::Unreachable);
        // The distro answered "this is not a repository" — that is the truth.
        assert_eq!(classify_wsl_answer(reached, not_repo, 0), WslAnswer::NotARepo);
        // A repository always has at least its main checkout, so parsing none of
        // them means the round trip came back malformed.
        assert_eq!(classify_wsl_answer(reached, repo, 0), WslAnswer::Unreachable);
        assert_eq!(classify_wsl_answer(reached, repo, 2), WslAnswer::Repo);
    }

    #[test]
    fn parses_worktree_list_porcelain_z() {
        let bytes = b"worktree /home/lev/proj\0HEAD 1234567890abcdef\0branch refs/heads/main\0\0\
worktree /home/lev/wt/feat-x\0HEAD fedcba0987654321\0branch refs/heads/feat-x\0\0\
worktree /home/lev/wt/tmp\0HEAD 0011223344556677\0detached\0\0";
        let records = parse_worktree_list_z(bytes);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].path, "/home/lev/proj");
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert_eq!(records[1].branch.as_deref(), Some("feat-x"));
        assert_eq!(records[2].branch, None);
        assert_eq!(records[2].head.as_deref(), Some("0011223344556677"));
    }

    #[test]
    fn worktree_paths_with_spaces_survive() {
        let bytes = b"worktree /home/lev/my proj\0HEAD abc\0branch refs/heads/main\0\0";
        let records = parse_worktree_list_z(bytes);
        assert_eq!(records[0].path, "/home/lev/my proj");
    }

    /// What a distro sends back for a batch, section by section, so WSL
    /// discovery can be tested without one.
    fn recorded(sections: &[&[u8]]) -> impl Fn(&str, &[&str]) -> Result<Vec<u8>, wsl::BatchError> {
        let mut stdout = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            if i > 0 {
                stdout.extend_from_slice(wsl::SECTION_SEP);
            }
            stdout.extend_from_slice(section);
        }
        move |_script, _args| Ok(stdout.clone())
    }

    fn discover_recorded(sections: &[&[u8]]) -> Result<Repository, VcsError> {
        discover_from_batch(
            PathBuf::from(r"\wsl$\Ubuntu\home\lev\proj"),
            "Ubuntu",
            "/home/lev/proj",
            false,
            recorded(sections),
        )
    }

    #[test]
    fn wsl_discovery_reads_every_section_of_a_full_reply() {
        let repo = discover_recorded(&[
            b"yes",
            b"worktree /home/lev/proj\0HEAD abc1234\0branch refs/heads/main\0\0",
            b"refs/remotes/origin/trunk",
            b"main\nmaster",
            b"",
            b"/home/lev",
            b"",
        ])
        .unwrap();
        assert_eq!(repo.trunk.as_deref(), Some("trunk"), "origin/HEAD wins");
        assert_eq!(repo.home.as_deref(), Some("/home/lev"));
        assert_eq!(repo.checkouts.len(), 1);
        assert_eq!(repo.checkouts[0].head.name.as_deref(), Some("main"));
    }

    #[test]
    fn wsl_discovery_falls_back_through_the_well_known_names_then_the_config() {
        let names = discover_recorded(&[
            b"yes",
            b"worktree /home/lev/proj\0HEAD abc1234\0branch refs/heads/master\0\0",
            b"",
            b"master\ndevelop",
            b"mainline",
            b"/home/lev",
            b"",
        ])
        .unwrap();
        assert_eq!(
            names.trunk.as_deref(),
            Some("master"),
            "a present well-known name outranks init.defaultBranch"
        );

        let config = discover_recorded(&[
            b"yes",
            b"worktree /home/lev/proj\0HEAD abc1234\0branch refs/heads/mainline\0\0",
            b"",
            b"",
            b"mainline",
            b"/home/lev",
            b"",
        ])
        .unwrap();
        assert_eq!(config.trunk.as_deref(), Some("mainline"));
    }

    #[test]
    fn a_folder_the_distro_calls_no_repository_is_not_one() {
        let found = discover_recorded(&[b"no", b"", b"", b"", b"", b"/home/lev", b""]);
        assert!(matches!(found, Err(VcsError::NotARepository(_))), "{found:?}");
    }

    /// A truncated batch cannot be told from a repository with no worktrees,
    /// and both mean the answer must not overwrite a known worktree list.
    #[test]
    fn a_truncated_batch_is_unreachable() {
        let found = discover_recorded(&[b"yes"]);
        assert!(matches!(found, Err(VcsError::Unreachable(_))), "{found:?}");
    }

    #[test]
    fn an_unreachable_distro_is_unreachable_not_a_plain_folder() {
        let found = discover_from_batch(
            PathBuf::from(r"\wsl$\Ubuntu\home\lev\proj"),
            "Ubuntu",
            "/home/lev/proj",
            false,
            |_: &str, _: &[&str]| Err(wsl::BatchError::Refused { stderr: "no distro".into() }),
        );
        assert!(matches!(found, Err(VcsError::Unreachable(_))), "{found:?}");
    }

    #[test]
    fn a_bare_repository_root_is_discovered_with_its_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let seed = crate::test_support::init_repo(&dir.path().join("seed"));
        let bare = crate::test_support::bare_clone(&seed, &dir.path().join("repo.git"));
        let wt = crate::test_support::add_worktree(&bare, "feature");
        let backend = GitBackend;
        assert!(backend.claims(&bare));
        let repo = jobs::on_this_thread(|b| backend.discover(&bare, &[], false, b)).unwrap();
        assert!(repo.checkouts.iter().any(|c| c.path == wt), "{:?}", repo.checkouts);
    }

    #[test]
    fn a_folder_that_is_no_repository_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let backend = GitBackend;
        assert!(!backend.claims(dir.path()));
        let found = jobs::on_this_thread(|b| backend.discover(dir.path(), &[], false, b));
        assert!(matches!(found, Err(VcsError::NotARepository(_))));
    }

    /// The upstream section is the one command in the batch that must not run
    /// when the feature is off, since `%(upstream:track)` computes divergence
    /// for every branch even when the caller only wants the parse skipped.
    #[test]
    fn the_upstream_section_stays_gated_by_the_upstream_flag() {
        let script = discover_batch().script();
        let upstream = script
            .rsplit(
                "
sep
",
            )
            .next()
            .expect("a last section");
        assert!(upstream.contains(r#"if [ "$2" = "1" ]; then"#));
        assert!(upstream.contains("LC_ALL=C"), "the track vocabulary is localized");
    }

    /// The candidate names reach the script from the enum, so adding one is
    /// an edit in `default_branch` rather than in this script.
    #[test]
    fn the_well_known_section_asks_for_every_candidate_name() {
        let script = discover_batch().script();
        for candidate in WellKnown::ALL {
            assert!(
                script.contains(&format!("refs/heads/{}", candidate.as_str())),
                "the script never asks about {}",
                candidate.as_str()
            );
        }
    }

    /// A detached head's label is its short OID, and a real branch can be
    /// named the same thing.  Build a worktree where that collision actually
    /// occurs and check the lookup, not just that the record has no branch —
    /// the latter holds whether or not the lookup guards against the collision.
    #[test]
    fn a_detached_worktree_gets_no_badge_even_when_its_oid_looks_like_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        let oid = repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        // A branch named exactly like the short OID the detached row will show.
        let short: String = oid.to_string().chars().take(7).collect();
        repo.branch(&short, &repo.find_commit(oid).unwrap(), false).unwrap();
        repo.set_head_detached(oid).unwrap();

        let repo = jobs::on_this_thread(|b| discover(dir.path(), true, b)).unwrap();
        let main = &repo.checkouts[0];
        assert!(
            main.upstream.is_none(),
            "a detached row must not adopt the same-named branch's state"
        );
    }

    /// Proves the `upstream` flag gates the git2 branch walk itself, not just
    /// whether the result gets painted: build a branch with a genuinely
    /// resolvable upstream, discover the same repo with the flag on and off,
    /// and require the two runs to disagree. A test that only checked the
    /// off case would pass against code that never looked anything up at all.
    #[test]
    fn the_upstream_flag_gates_whether_the_walk_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = git2::Repository::open(crate::test_support::init_repo(&repo_dir)).unwrap();

        let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("upstream-branch", &head_commit, false).unwrap();
        let mut head_branch = repo.find_branch(&head_name, git2::BranchType::Local).unwrap();
        head_branch.set_upstream(Some("upstream-branch")).unwrap();

        let with_flag = jobs::on_this_thread(|b| discover(&repo_dir, true, b)).unwrap();
        let without_flag = jobs::on_this_thread(|b| discover(&repo_dir, false, b)).unwrap();

        assert_eq!(
            with_flag.checkouts[0].upstream,
            Some(alacritree_vcs::UpstreamState::Level { upstream: "upstream-branch".to_string() }),
            "a real upstream must be found when the flag is on"
        );
        assert_eq!(
            without_flag.checkouts[0].upstream, None,
            "the same upstream must not be found when the flag is off"
        );
    }

    #[test]
    fn default_branch_priority_matches_git2_arm() {
        // origin/HEAD wins.
        assert_eq!(
            default_branch_from_batch("refs/remotes/origin/dev\n", "main\nmaster", "master"),
            Some("dev".to_string())
        );
        // Then common names in priority order, regardless of listing order.
        assert_eq!(default_branch_from_batch("", "develop\nmain", ""), Some("main".to_string()));
        // init.defaultBranch is last (already existence-verified by the script).
        assert_eq!(default_branch_from_batch("", "", "trunk2"), Some("trunk2".to_string()));
        assert_eq!(default_branch_from_batch("", "", ""), None);
    }
}
