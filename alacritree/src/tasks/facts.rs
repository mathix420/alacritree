//! Repository facts for a working directory, read by `git` on the side the
//! directory lives on. A Windows binary started from inside WSL sees its cwd
//! as a `\\wsl.localhost` path, and only the distro's own git reads that
//! checkout correctly.

use std::path::Path;
use std::process::Stdio;

use crate::command_ext::hidden;
use crate::jobs::Blocking;
use crate::multiplexer::Side;
use crate::tasks::scope::Place;
use crate::tools::{self, Tool};
use crate::wsl;

pub(crate) fn side_of(cwd: &Path) -> (Side, String) {
    match wsl::classify(cwd) {
        wsl::Location::Wsl { distro, linux_path } => (Side::Wsl(distro), linux_path),
        wsl::Location::Windows(path) => (Side::Native, path.display().to_string()),
    }
}

pub(crate) fn place_for(cwd: &Path, b: &Blocking) -> (Side, Place) {
    let (side, dir) = side_of(cwd);
    let git = match &side {
        Side::Native => tools::program(Tool::Git),
        Side::Wsl(distro) => tools::wsl_in_job(Tool::Git, distro, b),
    };
    let run = |args: &[&str]| -> Option<String> {
        let mut argv = vec!["-C", dir.as_str()];
        argv.extend_from_slice(args);
        let (program, argv) = side.command(&git, &argv);
        let mut cmd = hidden(program);
        cmd.args(argv).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let output = b.run_cancellable(&mut cmd).ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let Some(porcelain) = run(&["worktree", "list", "--porcelain"]) else {
        return (side, Place::Global);
    };
    let toplevel = run(&["rev-parse", "--show-toplevel"]);
    let place = place_from(&porcelain, toplevel.as_deref().map(str::trim));
    (side, place)
}

struct Entry {
    path: String,
    bare: bool,
    branch: Option<String>,
}

fn entries(porcelain: &str) -> Vec<Entry> {
    porcelain
        .replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let path = lines.next()?.strip_prefix("worktree ")?.to_string();
            let mut entry = Entry { path, bare: false, branch: None };
            for line in lines {
                if line == "bare" {
                    entry.bare = true;
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

fn basename(path: &str) -> String {
    normalize(path).rsplit('/').next().unwrap_or_default().to_string()
}

/// `git worktree list` puts the main worktree first, and the main worktree is
/// what names the repository for all of them.
pub(crate) fn place_from(porcelain: &str, toplevel: Option<&str>) -> Place {
    let all = entries(porcelain);
    let Some(main) = all.first() else { return Place::Global };
    let name = basename(&main.path);
    let repo = match name.strip_suffix(".git") {
        Some(stripped) if main.bare => stripped.to_string(),
        _ => name,
    };
    let current = toplevel
        .map(normalize)
        .and_then(|top| all.iter().find(|e| !e.bare && normalize(&e.path) == top));
    let Some(current) = current else { return Place::Project { repo } };
    let shared =
        |branch: &String| all.iter().filter(|e| e.branch.as_ref() == Some(branch)).count() > 1;
    let branch = match &current.branch {
        Some(branch) if !shared(branch) => branch.clone(),
        _ => basename(&current.path),
    };
    Place::Workspace { repo, branch }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINKED: &str = "worktree /src/alacritree\nHEAD aaa\nbranch \
                          refs/heads/master\n\nworktree /src/alacritree-worktrees/feat/x\nHEAD \
                          bbb\nbranch refs/heads/feat/x\n\n";

    #[test]
    fn the_main_worktree_names_the_repo_and_the_current_one_the_branch() {
        assert_eq!(
            place_from(LINKED, Some("/src/alacritree-worktrees/feat/x")),
            Place::Workspace { repo: "alacritree".into(), branch: "feat/x".into() }
        );
        assert_eq!(place_from(LINKED, Some("/src/alacritree")), Place::Workspace {
            repo: "alacritree".into(),
            branch: "master".into()
        });
    }

    #[test]
    fn a_detached_head_falls_back_to_the_directory_name() {
        let porcelain = "worktree /src/r\nHEAD aaa\nbranch refs/heads/main\n\nworktree \
                         /src/wt/review\nHEAD bbb\ndetached\n\n";
        assert_eq!(place_from(porcelain, Some("/src/wt/review")), Place::Workspace {
            repo: "r".into(),
            branch: "review".into()
        });
    }

    #[test]
    fn a_branch_forced_into_two_worktrees_falls_back_to_the_directory_name() {
        let porcelain = "worktree /src/r\nHEAD aaa\nbranch refs/heads/main\n\nworktree \
                         /src/wt/copy\nHEAD aaa\nbranch refs/heads/main\n\n";
        assert_eq!(place_from(porcelain, Some("/src/wt/copy")), Place::Workspace {
            repo: "r".into(),
            branch: "copy".into()
        });
    }

    #[test]
    fn a_bare_repo_drops_its_git_suffix_and_has_no_workspace_at_its_root() {
        let porcelain = "worktree /src/proj.git\nbare\n\nworktree /src/proj-main\nHEAD \
                         aaa\nbranch refs/heads/main\n\n";
        assert_eq!(place_from(porcelain, None), Place::Project { repo: "proj".into() });
        assert_eq!(place_from(porcelain, Some("/src/proj-main")), Place::Workspace {
            repo: "proj".into(),
            branch: "main".into()
        });
    }

    #[test]
    fn a_submodule_is_named_by_its_own_top_level() {
        let porcelain = "worktree /src/super/vendor/lib\nHEAD aaa\nbranch refs/heads/main\n\n";
        assert_eq!(place_from(porcelain, Some("/src/super/vendor/lib")), Place::Workspace {
            repo: "lib".into(),
            branch: "main".into()
        });
    }

    #[test]
    fn windows_separators_and_trailing_slashes_still_match() {
        let porcelain = "worktree C:/src/r\nHEAD a\nbranch refs/heads/main\n\n";
        assert_eq!(place_from(porcelain, Some("C:\\src\\r\\")), Place::Workspace {
            repo: "r".into(),
            branch: "main".into()
        });
    }

    #[test]
    fn nothing_to_parse_is_global() {
        assert_eq!(place_from("", Some("/x")), Place::Global);
    }

    #[cfg(windows)]
    #[test]
    fn a_distro_path_runs_on_that_distro() {
        let (side, cwd) = side_of(Path::new(r"\\wsl.localhost\Ubuntu\home\lev\r"));
        assert_eq!(side, Side::Wsl("Ubuntu".into()));
        assert_eq!(cwd, "/home/lev/r");
    }

    #[test]
    fn a_real_repository_reports_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        std::fs::create_dir(&repo).unwrap();
        // A test has no UI thread for a blocking wait to stall.
        #[allow(clippy::disallowed_methods)]
        let init = hidden("git").args(["init", "-q", "-b", "trunk"]).current_dir(&repo).status();
        if !init.is_ok_and(|s| s.success()) {
            return;
        }
        let (_, place) = crate::jobs::on_this_thread(|b| place_for(&repo, b));
        assert_eq!(place, Place::Workspace { repo: "myrepo".into(), branch: "trunk".into() });
        let (_, outside) = crate::jobs::on_this_thread(|b| place_for(dir.path(), b));
        assert_eq!(outside, Place::Global);
    }
}
