//! Which checkout a directory belongs to, read by `git` on the side the
//! directory lives on. A Windows binary started from inside WSL sees its cwd
//! as a `\\wsl.localhost` path, and only the distro's own git reads that
//! checkout correctly.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use alacritree_common::command_ext::hidden;
use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;
use alacritree_common::tools::{self, Tool};
use alacritree_common::wsl;
use alacritree_vcs::{Head, Located};

pub(crate) fn locate(dir: &Path, b: &Blocking) -> Option<Located> {
    let (side, arg) = match wsl::classify(dir) {
        wsl::Location::Wsl { distro, linux_path } => (Side::Wsl(distro), linux_path),
        wsl::Location::Windows(path) => (Side::Native, path.display().to_string()),
    };
    let git = match &side {
        Side::Native => tools::program(Tool::Git),
        Side::Wsl(distro) => tools::wsl_in_job(Tool::Git, distro, b),
    };
    let run = |args: &[&str]| -> Option<String> {
        let mut argv = vec!["-C", arg.as_str()];
        argv.extend_from_slice(args);
        let (program, argv) = side.command(&git, &argv);
        let mut cmd = hidden(program);
        cmd.args(argv).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let output = b.run_cancellable(&mut cmd).ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let porcelain = run(&["worktree", "list", "--porcelain"])?;
    let toplevel = run(&["rev-parse", "--show-toplevel"]);
    let mut found = located_from(&porcelain, toplevel.as_deref().map(str::trim))?;
    if let Side::Wsl(distro) = &side {
        found.main = wsl::linux_to_windows(&found.main.to_string_lossy(), distro);
        found.checkout =
            found.checkout.map(|c| wsl::linux_to_windows(&c.to_string_lossy(), distro));
    }
    Some(found)
}

struct Entry {
    path: String,
    bare: bool,
    head: Option<String>,
    branch: Option<String>,
}

fn entries(porcelain: &str) -> Vec<Entry> {
    porcelain
        .replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let path = lines.next()?.strip_prefix("worktree ")?.to_string();
            let mut entry = Entry { path, bare: false, head: None, branch: None };
            for line in lines {
                if line == "bare" {
                    entry.bare = true;
                } else if let Some(head) = line.strip_prefix("HEAD ") {
                    entry.head = Some(head.to_string());
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                    entry.branch = Some(branch.to_string());
                }
            }
            Some(entry)
        })
        .collect()
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

/// `git worktree list` puts the main worktree first, and the main worktree is
/// what names the repository for all of them. A branch another worktree also
/// has out does not tell the two apart, so it names neither.
fn located_from(porcelain: &str, toplevel: Option<&str>) -> Option<Located> {
    let all = entries(porcelain);
    let main = PathBuf::from(&all.first()?.path);
    let current = toplevel
        .map(normalize)
        .and_then(|top| all.iter().find(|e| !e.bare && normalize(&e.path) == top));
    let Some(current) = current else {
        return Some(Located { main, checkout: None, head: Head::default() });
    };
    let shared =
        |branch: &String| all.iter().filter(|e| e.branch.as_ref() == Some(branch)).count() > 1;
    let head = Head {
        name: current.branch.clone().filter(|branch| !shared(branch)),
        revision: current.head.as_ref().map(|h| h.chars().take(7).collect()),
        distance: None,
    };
    Some(Located { main, checkout: Some(PathBuf::from(&current.path)), head })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alacritree_common::jobs;
    use alacritree_vcs::{Head, Located, VersionControl};

    use super::*;
    use crate::{GitBackend, GitConfig};

    const LINKED: &str = "worktree /src/alacritree\nHEAD aaaaaaaaaa\nbranch \
                          refs/heads/master\n\nworktree /src/alacritree-worktrees/feat/x\nHEAD \
                          bbbbbbbbbb\nbranch refs/heads/feat/x\n\n";

    fn located(main: &str, checkout: Option<&str>, name: Option<&str>, rev: &str) -> Located {
        Located {
            main: PathBuf::from(main),
            checkout: checkout.map(PathBuf::from),
            head: Head {
                name: name.map(str::to_string),
                revision: Some(rev.to_string()),
                distance: None,
            },
        }
    }

    #[test]
    fn the_main_worktree_leads_and_the_current_one_is_found_by_its_top_level() {
        assert_eq!(
            located_from(LINKED, Some("/src/alacritree-worktrees/feat/x")),
            Some(located(
                "/src/alacritree",
                Some("/src/alacritree-worktrees/feat/x"),
                Some("feat/x"),
                "bbbbbbb"
            ))
        );
        assert_eq!(
            located_from(LINKED, Some("/src/alacritree")),
            Some(located("/src/alacritree", Some("/src/alacritree"), Some("master"), "aaaaaaa"))
        );
    }

    #[test]
    fn a_detached_head_names_no_branch() {
        let porcelain = "worktree /src/r\nHEAD aaa\nbranch refs/heads/main\n\nworktree \
                         /src/wt/review\nHEAD bbb\ndetached\n\n";
        assert_eq!(
            located_from(porcelain, Some("/src/wt/review")),
            Some(located("/src/r", Some("/src/wt/review"), None, "bbb"))
        );
    }

    /// A branch forced into two worktrees names neither of them.
    #[test]
    fn a_branch_forced_into_two_worktrees_names_no_branch() {
        let porcelain = "worktree /src/r\nHEAD aaa\nbranch refs/heads/main\n\nworktree \
                         /src/wt/copy\nHEAD aaa\nbranch refs/heads/main\n\n";
        assert_eq!(
            located_from(porcelain, Some("/src/wt/copy")),
            Some(located("/src/r", Some("/src/wt/copy"), None, "aaa"))
        );
    }

    #[test]
    fn a_bare_repository_root_is_in_no_checkout() {
        let porcelain = "worktree /src/proj.git\nbare\n\nworktree /src/proj-main\nHEAD \
                         aaa\nbranch refs/heads/main\n\n";
        let root = located_from(porcelain, None).unwrap();
        assert_eq!(root.main, PathBuf::from("/src/proj.git"));
        assert_eq!(root.checkout, None);
        assert_eq!(
            located_from(porcelain, Some("/src/proj-main")),
            Some(located("/src/proj.git", Some("/src/proj-main"), Some("main"), "aaa"))
        );
    }

    #[test]
    fn a_submodule_is_its_own_repository() {
        let porcelain = "worktree /src/super/vendor/lib\nHEAD aaa\nbranch refs/heads/main\n\n";
        assert_eq!(
            located_from(porcelain, Some("/src/super/vendor/lib")),
            Some(located(
                "/src/super/vendor/lib",
                Some("/src/super/vendor/lib"),
                Some("main"),
                "aaa"
            ))
        );
    }

    #[test]
    fn windows_separators_and_trailing_slashes_still_match() {
        let porcelain = "worktree C:/src/r\nHEAD a\nbranch refs/heads/main\n\n";
        assert_eq!(
            located_from(porcelain, Some("C:\\src\\r\\")),
            Some(located("C:/src/r", Some("C:/src/r"), Some("main"), "a"))
        );
    }

    #[test]
    fn nothing_to_parse_locates_nothing() {
        assert_eq!(located_from("", Some("/x")), None);
    }

    #[test]
    fn a_directory_inside_a_linked_worktree_names_it_and_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::test_support::init_repo(&dir.path().join("repo"));
        let linked = crate::test_support::add_worktree(&repo, "topic");
        let sub = linked.join("sub");
        std::fs::create_dir(&sub).unwrap();

        let found =
            jobs::on_this_thread(|b| GitBackend::new(&GitConfig::default()).locate(&sub, b))
                .expect("the worktree is found");

        let same = |a: &std::path::Path, b: &std::path::Path| {
            a.canonicalize().unwrap() == b.canonicalize().unwrap()
        };
        assert!(same(&found.main, &repo), "{found:?}");
        assert!(same(found.checkout.as_deref().unwrap(), &linked), "{found:?}");
        assert_eq!(found.head.name.as_deref(), Some("topic"));
    }

    #[test]
    fn a_folder_outside_any_repository_is_not_located() {
        let dir = tempfile::tempdir().unwrap();
        let found =
            jobs::on_this_thread(|b| GitBackend::new(&GitConfig::default()).locate(dir.path(), b));
        assert_eq!(found, None);
    }
}
