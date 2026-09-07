//! Branch upstream state for the sidebar badge.

use std::collections::HashMap;

use git2::{BranchType, ErrorCode, Repository};

/// What a branch's configured upstream is doing.  Nothing here contacts a
/// remote, so every state describes local refs only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamState {
    /// Tracks `upstream` and matches it.
    Level { upstream: String },
    /// Tracks `upstream` and differs from it.
    Diverged { upstream: String, ahead: usize, behind: usize },
    /// `branch.<name>.remote` and `.merge` name `upstream`, but no such
    /// reference exists locally.  A merged-and-deleted remote branch looks
    /// like this only once something has pruned.
    Gone { upstream: String },
    /// No upstream is configured.  Not the same as "never pushed": `git push
    /// origin <branch>` without `-u` pushes without configuring one.
    Untracked,
}

/// Parse the tab-delimited output of
/// `git for-each-ref
/// --format='%(refname:short)%09%(upstream:short)%09%(upstream:track,nobracket)'`. The track field
/// is empty for a level branch, absent-upstream branches carry an empty upstream, and `gone` marks
/// a configured upstream whose ref no longer resolves. Callers must run git under `LC_ALL=C`: the
/// track vocabulary is localized.
pub fn parse_for_each_ref(bytes: &[u8]) -> HashMap<String, UpstreamState> {
    let text = String::from_utf8_lossy(bytes);
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(branch), Some(upstream), Some(track)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if branch.is_empty() {
            continue;
        }
        if let Some(state) = classify(upstream, track) {
            map.insert(branch.to_string(), state);
        }
    }
    map
}

fn classify(upstream: &str, track: &str) -> Option<UpstreamState> {
    if upstream.is_empty() {
        return Some(UpstreamState::Untracked);
    }
    let upstream = upstream.to_string();
    match track.trim() {
        "" => Some(UpstreamState::Level { upstream }),
        "gone" => Some(UpstreamState::Gone { upstream }),
        track => match parse_track_counts(track) {
            Some((ahead, behind)) => Some(UpstreamState::Diverged { upstream, ahead, behind }),
            // A track string we cannot read means a git whose vocabulary
            // changed. No entry, so the row paints nothing — better than
            // "0 ahead, 0 behind" on a branch that is neither.
            None => None,
        },
    }
}

/// `ahead 2`, `behind 3`, or `ahead 2, behind 3`. `None` when no clause
/// parsed, which is the only signal that the vocabulary was not what
/// `LC_ALL=C` promised.
fn parse_track_counts(track: &str) -> Option<(usize, usize)> {
    let mut ahead = 0;
    let mut behind = 0;
    let mut parsed = false;
    for part in track.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => (ahead, parsed) = (n, true),
            (Some("behind"), Some(n)) => (behind, parsed) = (n, true),
            _ => return None,
        }
    }
    parsed.then_some((ahead, behind))
}

/// Every linked worktree of a project shares `refs/heads` and `refs/remotes`,
/// so one walk of the project's local branches answers for every row.
pub fn map_from_repo(repo: &Repository) -> HashMap<String, UpstreamState> {
    let mut map = HashMap::new();
    let Ok(branches) = repo.branches(Some(BranchType::Local)) else {
        return map;
    };
    for (branch, _) in branches.flatten() {
        let Some(name) = branch.name().ok().flatten() else {
            continue;
        };
        let name = name.to_string();
        let refname = format!("refs/heads/{name}");
        let state = match branch.upstream() {
            Ok(upstream) => tracked_state(repo, &branch, &upstream),
            // The tracking ref is absent.  Config still decides whether an
            // upstream was ever configured.
            Err(e) if e.code() == ErrorCode::NotFound => {
                let configured = repo.branch_upstream_name(&refname);
                configured_upstream(match &configured {
                    Ok(buf) => Ok(buf.as_str().unwrap_or("")),
                    Err(e) => Err(e.code()),
                })
            },
            // Any other libgit2 failure says nothing about the upstream.
            Err(_) => None,
        };
        // A branch we could not answer for gets no entry, so the row paints
        // no badge rather than a wrong one.
        if let Some(state) = state {
            map.insert(name, state);
        }
    }
    map
}

fn tracked_state(
    repo: &Repository,
    branch: &git2::Branch<'_>,
    upstream: &git2::Branch<'_>,
) -> Option<UpstreamState> {
    let name = upstream.name().ok().flatten().unwrap_or_default().to_string();
    let (Some(local), Some(remote)) = (branch.get().target(), upstream.get().target()) else {
        return None;
    };
    match repo.graph_ahead_behind(local, remote) {
        Ok((0, 0)) => Some(UpstreamState::Level { upstream: name }),
        Ok((ahead, behind)) => Some(UpstreamState::Diverged { upstream: name, ahead, behind }),
        // Failing to walk the graph is not evidence the branches are level.
        // No entry means no badge, which is what an unanswerable state looks
        // like everywhere else.
        Err(_) => None,
    }
}

/// Classify a branch whose tracking ref did not resolve.  A name in config
/// means an upstream was configured and its ref is gone; `NotFound` means
/// none was ever configured.  Every other failure leaves the question
/// unanswered, and an unanswered branch gets no entry — the same rule
/// `tracked_state` applies to a graph walk it could not complete.
fn configured_upstream(name: Result<&str, ErrorCode>) -> Option<UpstreamState> {
    match name {
        Ok(name) => Some(UpstreamState::Gone { upstream: shorten(name) }),
        Err(ErrorCode::NotFound) => Some(UpstreamState::Untracked),
        Err(_) => None,
    }
}

/// Tracking a local branch through remote `"."` yields a `refs/heads/` name,
/// so both namespaces have to come off for the badge to read the way the
/// `for-each-ref` path renders the same upstream.
fn shorten(refname: &str) -> String {
    for prefix in ["refs/remotes/", "refs/heads/"] {
        if let Some(rest) = refname.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    refname.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_upstream_state() {
        let out = b"level\torigin/level\t\n\
                    ahead\torigin/ahead\tahead 2\n\
                    behind\torigin/behind\tbehind 3\n\
                    both\torigin/both\tahead 2, behind 3\n\
                    dead\torigin/dead\tgone\n\
                    solo\t\t\n";
        let map = parse_for_each_ref(out);
        assert_eq!(map["level"], UpstreamState::Level { upstream: "origin/level".into() });
        assert_eq!(map["ahead"], UpstreamState::Diverged {
            upstream: "origin/ahead".into(),
            ahead: 2,
            behind: 0
        });
        assert_eq!(map["behind"], UpstreamState::Diverged {
            upstream: "origin/behind".into(),
            ahead: 0,
            behind: 3
        });
        assert_eq!(map["both"], UpstreamState::Diverged {
            upstream: "origin/both".into(),
            ahead: 2,
            behind: 3
        });
        assert_eq!(map["dead"], UpstreamState::Gone { upstream: "origin/dead".into() });
        assert_eq!(map["solo"], UpstreamState::Untracked);
    }

    /// A libgit2 failure other than `NotFound` says nothing about whether an
    /// upstream was configured, so it must not resolve to `Untracked` — that
    /// would paint a definite badge from an unreadable repository.
    #[test]
    fn only_a_not_found_config_lookup_means_untracked() {
        assert_eq!(
            configured_upstream(Ok("refs/remotes/origin/x")),
            Some(UpstreamState::Gone { upstream: "origin/x".into() })
        );
        assert_eq!(configured_upstream(Err(ErrorCode::NotFound)), Some(UpstreamState::Untracked));
        for code in [ErrorCode::GenericError, ErrorCode::Invalid, ErrorCode::Locked] {
            assert_eq!(configured_upstream(Err(code)), None, "{code:?} must not fabricate a state");
        }
    }

    /// A branch tracking another local branch through remote `"."` has a
    /// `refs/heads/…` upstream.  The badge shows this name to the user, so it
    /// must read the way the WSL and live paths render it.
    #[test]
    fn a_local_tracking_upstream_shortens_like_a_remote_one() {
        assert_eq!(shorten("refs/heads/main"), "main");
        assert_eq!(shorten("refs/remotes/origin/main"), "origin/main");
    }

    /// `|` is legal in a ref name, which is why the format is tab-delimited —
    /// git forbids ASCII control characters in refs.
    #[test]
    fn a_branch_name_containing_a_pipe_survives() {
        let map = parse_for_each_ref(b"feat|x\torigin/feat|x\t\n");
        assert_eq!(map["feat|x"], UpstreamState::Level { upstream: "origin/feat|x".into() });
    }

    #[test]
    fn malformed_and_empty_input_yield_no_entries() {
        assert!(parse_for_each_ref(b"").is_empty());
        assert!(parse_for_each_ref(b"\n\n").is_empty());
        assert!(parse_for_each_ref(b"no-tabs-at-all\n").is_empty());
    }

    /// A track string we cannot read must produce no entry at all.  Falling
    /// through to `Diverged { ahead: 0, behind: 0 }` would paint the divergence
    /// glyph and a "0 ahead, 0 behind" tooltip on a branch that is neither.
    #[test]
    fn an_unreadable_track_string_yields_no_entry() {
        assert!(parse_for_each_ref(b"b\torigin/b\tvorne 2\n").is_empty());
        assert!(parse_for_each_ref(b"b\torigin/b\tahead many\n").is_empty());
    }
}

#[cfg(test)]
mod git2_tests {
    use super::*;
    use git2::Repository;

    /// A branch with no `branch.<name>.remote` is untracked; one whose
    /// configured tracking ref was deleted is gone.  The two are separated by
    /// `branch_upstream_name`, which libgit2 builds from config and the fetch
    /// refspec *before* resolving any reference.
    #[test]
    fn separates_untracked_from_gone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_empty(&repo);

        repo.branch("solo", &repo.head().unwrap().peel_to_commit().unwrap(), false).unwrap();

        let mut branch =
            repo.branch("dead", &repo.head().unwrap().peel_to_commit().unwrap(), false).unwrap();
        branch.set_upstream(None).ok();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("branch.dead.remote", "origin").unwrap();
        cfg.set_str("branch.dead.merge", "refs/heads/dead").unwrap();
        cfg.set_str("remote.origin.url", "https://example.invalid/r.git").unwrap();
        cfg.set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*").unwrap();

        let map = map_from_repo(&repo);
        assert_eq!(map["solo"], UpstreamState::Untracked);
        assert_eq!(map["dead"], UpstreamState::Gone { upstream: "origin/dead".into() });
    }

    /// Exercises every value `graph_ahead_behind` can return: level (0, 0),
    /// ahead-only, behind-only, and diverged (both nonzero), with the exact
    /// counts asserted so the tuple order is pinned in both directions.
    /// `level`/`ahead` share a static upstream (`main`, never advanced) and
    /// so only ever probe the ahead side; `behind` and `diverged` each get
    /// their own upstream branch that advances independently of the local
    /// branch, which is what actually exercises the behind side. Also
    /// covers `remote = "."`, where `set_upstream` points at another local
    /// branch rather than a remote-tracking one.
    #[test]
    fn classifies_level_ahead_behind_and_diverged() {
        let dir = tempfile::tempdir().unwrap();
        // The upstreams below are named, so the branch the first commit lands
        // on has to be too: a plain `init` takes it from `init.defaultBranch`,
        // which is `master` wherever the developer has not set it.
        let mut opts = git2::RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = Repository::init_opts(dir.path(), &opts).unwrap();
        let base = commit_empty(&repo);

        // `main` never moves, so `level` stays level and `ahead` only grows
        // its own side of the pair.
        for (name, extra) in [("level", 0), ("ahead", 2)] {
            let mut b = repo.branch(name, &repo.find_commit(base).unwrap(), false).unwrap();
            b.set_upstream(Some("main")).unwrap();
            advance(&repo, base, &format!("refs/heads/{name}"), extra);
        }

        // `behind` never commits itself; only its dedicated upstream moves.
        repo.branch("upstream-behind", &repo.find_commit(base).unwrap(), false).unwrap();
        let mut behind = repo.branch("behind", &repo.find_commit(base).unwrap(), false).unwrap();
        behind.set_upstream(Some("upstream-behind")).unwrap();
        advance(&repo, base, "refs/heads/upstream-behind", 3);

        // `diverged` and its dedicated upstream each grow from `base` along
        // their own line, so neither is an ancestor of the other.
        repo.branch("upstream-diverged", &repo.find_commit(base).unwrap(), false).unwrap();
        let mut diverged =
            repo.branch("diverged", &repo.find_commit(base).unwrap(), false).unwrap();
        diverged.set_upstream(Some("upstream-diverged")).unwrap();
        advance(&repo, base, "refs/heads/diverged", 2);
        advance(&repo, base, "refs/heads/upstream-diverged", 4);

        let map = map_from_repo(&repo);
        assert_eq!(map["level"], UpstreamState::Level { upstream: "main".into() });
        assert_eq!(map["ahead"], UpstreamState::Diverged {
            upstream: "main".into(),
            ahead: 2,
            behind: 0
        });
        assert_eq!(map["behind"], UpstreamState::Diverged {
            upstream: "upstream-behind".into(),
            ahead: 0,
            behind: 3
        });
        assert_eq!(map["diverged"], UpstreamState::Diverged {
            upstream: "upstream-diverged".into(),
            ahead: 2,
            behind: 4
        });
    }

    fn commit_empty(repo: &Repository) -> git2::Oid {
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap()
    }

    fn advance(repo: &Repository, from: git2::Oid, refname: &str, extra: usize) -> git2::Oid {
        let mut parent = from;
        for i in 0..extra {
            parent = commit_onto(repo, parent, refname, i);
        }
        parent
    }

    fn commit_onto(repo: &Repository, parent: git2::Oid, refname: &str, n: usize) -> git2::Oid {
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        let parent = repo.find_commit(parent).unwrap();
        // The message folds in `refname` so two chains started from the same
        // `base` at the same wall-clock second never produce identical
        // (parent, tree, message) commits, which git would collapse into one
        // shared object and silently merge the two branches' histories.
        repo.commit(Some(refname), &sig, &sig, &format!("{refname} c{n}"), &tree, &[&parent])
            .unwrap()
    }
}
