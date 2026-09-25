//! What the git panel's diff pane opens and the command that opens it.
//!
//! A viewer either lets git render the diff and pipe it through a pager, or
//! renders the diff itself from an argv template. The built-in viewers are
//! values of the same type a custom one resolves to, so every viewer takes
//! one path from a click to a spawn.

mod launch;
mod settings;

use alacritree_common::tools::Tool;
use alacritree_vcs::{DiffScope, DiffTarget, VersionControl};

pub use settings::{
    DiffViewerConfig, DiffViewerPreset, RawCustomDiffViewer, RawDelta, RawDiffViewer, RawTuicr,
};

/// Which `git diff` flavor a git panel row opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    Staged,
    Worktree,
    Untracked,
    /// Triple-dot diff against this base ref (merge-base, matching the
    /// `Changes vs <branch>` sidebar section).
    Branch {
        base: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub file: String,
    pub source: DiffSource,
}

/// Stable identifier for "the diff this click would open". It matches the
/// active diff session's `SessionKind::Diff { key }` to highlight the row and
/// toggle the pane off when clicked again.
pub fn diff_key(req: &DiffRequest) -> String {
    let tag = match &req.source {
        DiffSource::Staged => "staged",
        DiffSource::Worktree => "worktree",
        DiffSource::Untracked => "untracked",
        DiffSource::Branch { .. } => "branch",
    };
    format!("{tag}:{}", req.file)
}

/// A whole git panel section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    Staged,
    Unstaged,
    Branch { base: String },
}

impl Section {
    /// What the pane's tab calls the section.
    pub fn label(&self) -> String {
        match self {
            Section::Staged => "staged changes".to_string(),
            Section::Unstaged => "unstaged changes".to_string(),
            Section::Branch { base } => format!("changes vs {base}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Row(DiffRequest),
    Section(Section),
}

impl Target {
    /// Stable identity of the pane this target opens.
    pub fn key(&self) -> String {
        match self {
            Target::Row(req) => diff_key(req),
            Target::Section(Section::Staged) => "section:staged".to_string(),
            Target::Section(Section::Unstaged) => "section:unstaged".to_string(),
            Target::Section(Section::Branch { .. }) => "section:branch".to_string(),
        }
    }

    fn file(&self) -> Option<&str> {
        match self {
            Target::Row(req) => Some(&req.file),
            Target::Section(_) => None,
        }
    }

    fn base(&self) -> Option<&str> {
        match self {
            Target::Row(DiffRequest { source: DiffSource::Branch { base }, .. })
            | Target::Section(Section::Branch { base }) => Some(base),
            _ => None,
        }
    }
}

/// A program the registry resolves, or one a custom viewer names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum Program {
    Tool(Tool),
    /// `path` runs natively as written. Inside WSL, `wsl_path` runs as
    /// written. Without one, `path` goes through the distro's login shell.
    Custom {
        path: String,
        wsl_path: Option<String>,
    },
}

/// One argv template per row kind and per section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Templates {
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub branch: Vec<String>,
    pub staged_scope: Vec<String>,
    pub unstaged_scope: Vec<String>,
    pub branch_scope: Vec<String>,
}

impl Templates {
    fn tuicr() -> Self {
        let words = |words: &[&str]| -> Vec<String> {
            words.iter().map(|word| (*word).to_string()).collect()
        };
        let uncommitted_row = words(&["-w", "-p", "{file}"]);
        Self {
            staged: uncommitted_row.clone(),
            unstaged: uncommitted_row.clone(),
            untracked: uncommitted_row,
            branch: words(&["-r", "{range}", "-p", "{file}"]),
            staged_scope: words(&["-w"]),
            unstaged_scope: words(&["-w"]),
            branch_scope: words(&["-r", "{range}"]),
        }
    }

    fn for_target(&self, target: &Target) -> &[String] {
        match target {
            Target::Row(req) => match req.source {
                DiffSource::Staged => &self.staged,
                DiffSource::Worktree => &self.unstaged,
                DiffSource::Untracked => &self.untracked,
                DiffSource::Branch { .. } => &self.branch,
            },
            Target::Section(Section::Staged) => &self.staged_scope,
            Target::Section(Section::Unstaged) => &self.unstaged_scope,
            Target::Section(Section::Branch { .. }) => &self.branch_scope,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum Viewer {
    /// git renders the diff and runs `pager` with `args` as `core.pager`.
    Pager { pager: Program, args: Vec<String> },
    /// `program` renders the diff itself from its template's argv.
    Direct { program: Program, templates: Templates },
}

impl Viewer {
    pub fn delta() -> Self {
        Viewer::Pager {
            pager: Program::Tool(Tool::Delta),
            args: vec!["--paging=always".to_string()],
        }
    }

    pub fn tuicr() -> Self {
        Viewer::Direct { program: Program::Tool(Tool::Tuicr), templates: Templates::tuicr() }
    }

    pub fn program(&self) -> &Program {
        match self {
            Viewer::Pager { pager, .. } => pager,
            Viewer::Direct { program, .. } => program,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Pager { pager: Program, pager_args: Vec<String>, git_args: Vec<String> },
    Direct { program: Program, args: Vec<String> },
}

/// Whether the viewer can open this target.
pub fn opens(viewer: &Viewer, target: &Target) -> bool {
    match viewer {
        Viewer::Pager { .. } => true,
        Viewer::Direct { templates, .. } => {
            let template = templates.for_target(target);
            !template.is_empty()
                && template.iter().all(|arg| {
                    (!arg.contains("{file}") || target.file().is_some())
                        && (!(arg.contains("{base}") || arg.contains("{range}"))
                            || target.base().is_some())
                })
        },
    }
}

/// How a viewer opens a target, or `None` when the target is unavailable.
/// `vcs` owns the checkout, and says what a diff and a review range are.
pub fn plan(viewer: &Viewer, target: &Target, vcs: &impl VersionControl) -> Option<Launch> {
    if !opens(viewer, target) {
        return None;
    }
    Some(match viewer {
        Viewer::Pager { pager, args } => Launch::Pager {
            pager: pager.clone(),
            pager_args: args.clone(),
            git_args: vcs.diff_args(&diff_target(target)),
        },
        Viewer::Direct { program, templates } => Launch::Direct {
            program: program.clone(),
            args: templates
                .for_target(target)
                .iter()
                .map(|arg| substitute(arg, target, vcs))
                .collect(),
        },
    })
}

/// What the backend is asked to diff for `target`.
fn diff_target(target: &Target) -> DiffTarget {
    let (scope, untracked) = match target {
        Target::Row(req) => match &req.source {
            DiffSource::Staged => (DiffScope::Staged, false),
            DiffSource::Worktree => (DiffScope::Working, false),
            DiffSource::Untracked => (DiffScope::Working, true),
            DiffSource::Branch { base } => (DiffScope::Base { base: base.clone() }, false),
        },
        Target::Section(Section::Staged) => (DiffScope::Staged, false),
        Target::Section(Section::Unstaged) => (DiffScope::Working, false),
        Target::Section(Section::Branch { base }) => (DiffScope::Base { base: base.clone() }, false),
    };
    DiffTarget { scope, file: target.file().map(str::to_string), untracked }
}

fn substitute(arg: &str, target: &Target, vcs: &impl VersionControl) -> String {
    let mut output = String::with_capacity(arg.len());
    let mut remaining = arg;
    while let Some(open) = remaining.find('{') {
        output.push_str(&remaining[..open]);
        let tail = &remaining[open..];
        if let Some(after) = tail.strip_prefix("{file}") {
            output.push_str(target.file().unwrap_or_default());
            remaining = after;
        } else if let Some(after) = tail.strip_prefix("{base}") {
            output.push_str(target.base().unwrap_or_default());
            remaining = after;
        } else if let Some(after) = tail.strip_prefix("{range}") {
            output.push_str(&target.base().map(|base| vcs.review_range(base)).unwrap_or_default());
            remaining = after;
        } else {
            output.push('{');
            remaining = &tail[1..];
        }
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use alacritree_vcs::fake::FakeVcs;
    use alacritree_vcs::{DiffScope, DiffTarget};

    use super::*;

    fn row(file: &str, source: DiffSource) -> Target {
        Target::Row(DiffRequest { file: file.to_string(), source })
    }

    fn branch() -> DiffSource {
        DiffSource::Branch { base: "refs/remotes/origin/main".to_string() }
    }

    fn branch_target(base: &str) -> Target {
        Target::Section(Section::Branch { base: base.to_string() })
    }

    fn custom_viewer(branch_scope: &[&str]) -> Viewer {
        Viewer::Direct {
            program: Program::Custom { path: "difft".to_string(), wsl_path: None },
            templates: Templates {
                branch_scope: branch_scope.iter().map(|arg| (*arg).to_string()).collect(),
                ..Templates::default()
            },
        }
    }

    fn direct_args_with(viewer: &Viewer, target: &Target, vcs: &FakeVcs) -> Option<Vec<String>> {
        match plan(viewer, target, vcs)? {
            Launch::Direct { args, .. } => Some(args),
            Launch::Pager { .. } => panic!("a direct viewer planned a pager launch"),
        }
    }

    fn direct_args(viewer: &Viewer, target: &Target) -> Option<Vec<String>> {
        direct_args_with(viewer, target, &FakeVcs::new("/r").with_range("RANGE"))
    }

    #[test]
    fn rows_and_sections_map_to_the_backend_target() {
        let target = |scope, file: Option<&str>, untracked| DiffTarget {
            scope,
            file: file.map(str::to_string),
            untracked,
        };
        let base = || DiffScope::Base { base: "main".to_string() };
        let main = DiffSource::Branch { base: "main".to_string() };
        assert_eq!(
            diff_target(&row("a.rs", DiffSource::Staged)),
            target(DiffScope::Staged, Some("a.rs"), false)
        );
        assert_eq!(
            diff_target(&row("a.rs", DiffSource::Worktree)),
            target(DiffScope::Working, Some("a.rs"), false)
        );
        assert_eq!(
            diff_target(&row("a.rs", DiffSource::Untracked)),
            target(DiffScope::Working, Some("a.rs"), true)
        );
        assert_eq!(diff_target(&row("a.rs", main)), target(base(), Some("a.rs"), false));
        assert_eq!(
            diff_target(&Target::Section(Section::Staged)),
            target(DiffScope::Staged, None, false)
        );
        assert_eq!(
            diff_target(&Target::Section(Section::Unstaged)),
            target(DiffScope::Working, None, false)
        );
        assert_eq!(diff_target(&branch_target("main")), target(base(), None, false));
    }

    #[test]
    fn row_and_section_keys_never_collide() {
        assert_eq!(row("a.rs", DiffSource::Staged).key(), "staged:a.rs");
        assert_eq!(row("a.rs", branch()).key(), "branch:a.rs");
        assert_eq!(Target::Section(Section::Staged).key(), "section:staged");
        assert_eq!(Target::Section(Section::Unstaged).key(), "section:unstaged");
        assert_eq!(
            Target::Section(Section::Branch { base: "main".into() }).key(),
            "section:branch"
        );
    }

    #[test]
    fn delta_pipes_the_backend_diff_through_the_pager() {
        let vcs = FakeVcs::new("/r");
        let launch = plan(&Viewer::delta(), &row("a.rs", DiffSource::Staged), &vcs).unwrap();
        assert_eq!(launch, Launch::Pager {
            pager: Program::Tool(Tool::Delta),
            pager_args: vec!["--paging=always".to_string()],
            git_args: vec!["diff".into()],
        });
    }

    #[test]
    fn tuicr_reviews_rows_by_path_and_sections_by_scope() {
        let tuicr = Viewer::tuicr();
        for source in [DiffSource::Staged, DiffSource::Worktree, DiffSource::Untracked] {
            assert_eq!(direct_args(&tuicr, &row("a.rs", source)).unwrap(), ["-w", "-p", "a.rs"]);
        }
        assert_eq!(direct_args(&tuicr, &row("a.rs", branch())).unwrap(), [
            "-r", "RANGE", "-p", "a.rs"
        ]);
        assert_eq!(direct_args(&tuicr, &Target::Section(Section::Staged)).unwrap(), ["-w"]);
        assert_eq!(direct_args(&tuicr, &Target::Section(Section::Unstaged)).unwrap(), ["-w"]);
        assert_eq!(direct_args(&tuicr, &branch_target("main")).unwrap(), ["-r", "RANGE"]);
        assert_eq!(tuicr.program(), &Program::Tool(Tool::Tuicr));
    }

    #[test]
    fn the_built_in_tuicr_templates_render_the_range_the_backend_gives() {
        let vcs = FakeVcs::new("/r").with_range("origin/main...HEAD");
        let args = direct_args_with(&Viewer::tuicr(), &branch_target("origin/main"), &vcs);
        assert_eq!(args.unwrap(), ["-r", "origin/main...HEAD"]);
    }

    #[test]
    fn a_custom_template_still_accepts_base() {
        let vcs = FakeVcs::new("/r").with_range("ignored");
        let viewer = custom_viewer(&["--base", "{base}"]);
        let args = direct_args_with(&viewer, &branch_target("origin/main"), &vcs);
        assert_eq!(args.unwrap(), ["--base", "origin/main"]);
    }

    #[test]
    fn a_file_name_stays_one_argument() {
        let file = "dir/my {base} {range} 'x'.rs";
        let args = direct_args(&Viewer::tuicr(), &row(file, DiffSource::Staged));
        assert_eq!(args.unwrap(), ["-w", "-p", file]);
    }

    #[test]
    fn an_empty_template_or_a_missing_placeholder_value_opens_nothing() {
        let viewer = Viewer::Direct {
            program: Program::Custom { path: "difft".to_string(), wsl_path: None },
            templates: Templates {
                staged: vec!["--base".to_string(), "{base}".to_string()],
                untracked: vec!["{range}".to_string()],
                ..Templates::default()
            },
        };
        let vcs = FakeVcs::new("/r");
        let staged_row = row("a.rs", DiffSource::Staged);
        assert!(!opens(&viewer, &staged_row));
        assert!(plan(&viewer, &staged_row, &vcs).is_none(), "a staged row has no base");
        let untracked_row = row("a.rs", DiffSource::Untracked);
        assert!(!opens(&viewer, &untracked_row), "an untracked row has no range");
        let unstaged_row = row("a.rs", DiffSource::Worktree);
        assert!(!opens(&viewer, &unstaged_row));
        assert!(plan(&viewer, &unstaged_row, &vcs).is_none(), "the unstaged template is empty");
        assert!(opens(&Viewer::delta(), &Target::Section(Section::Unstaged)));
    }
}
