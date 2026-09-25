//! The `git diff` a diff viewer shows, and the range a review reads.

use alacritree_vcs::{DiffScope, DiffTarget};

/// git arguments after `git` for `target`.
pub(crate) fn diff_args(target: &DiffTarget) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    match &target.scope {
        DiffScope::Staged => args.push("--cached".to_string()),
        DiffScope::Working if target.untracked => args.push("--no-index".to_string()),
        DiffScope::Working => {},
        DiffScope::Base { base } => args.push(format!("{base}...")),
    }
    if let Some(file) = &target.file {
        args.push("--".to_string());
        if target.untracked {
            args.push("/dev/null".to_string());
        }
        args.push(file.clone());
    }
    args
}

/// The same triple-dot form the `Changes vs` section diffs with.
pub(crate) fn review_range(base: &str) -> String {
    format!("{base}...HEAD")
}

#[cfg(test)]
mod tests {
    use alacritree_vcs::{DiffScope, DiffTarget, VersionControl};

    use crate::{GitBackend, GitConfig};

    fn args(scope: DiffScope, file: Option<&str>, untracked: bool) -> Vec<String> {
        let target = DiffTarget { scope, file: file.map(str::to_string), untracked };
        GitBackend::new(&GitConfig::default()).diff_args(&target)
    }

    fn base(name: &str) -> DiffScope {
        DiffScope::Base { base: name.to_string() }
    }

    #[test]
    fn a_file_diffs_by_scope() {
        assert_eq!(args(DiffScope::Staged, Some("a.rs"), false), [
            "diff", "--cached", "--", "a.rs"
        ]);
        assert_eq!(args(DiffScope::Working, Some("a.rs"), false), ["diff", "--", "a.rs"]);
        assert_eq!(args(DiffScope::Working, Some("a.rs"), true), [
            "diff",
            "--no-index",
            "--",
            "/dev/null",
            "a.rs"
        ]);
        assert_eq!(args(base("main"), Some("a.rs"), false), ["diff", "main...", "--", "a.rs"]);
    }

    #[test]
    fn a_whole_scope_names_no_file() {
        assert_eq!(args(DiffScope::Staged, None, false), ["diff", "--cached"]);
        assert_eq!(args(DiffScope::Working, None, false), ["diff"]);
        assert_eq!(args(base("main"), None, false), ["diff", "main..."]);
    }

    #[test]
    fn a_base_review_range_is_the_triple_dot_range_to_head() {
        assert_eq!(
            GitBackend::new(&GitConfig::default()).review_range("origin/main"),
            "origin/main...HEAD"
        );
    }
}
