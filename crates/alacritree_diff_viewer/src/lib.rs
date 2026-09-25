//! What the git panel's diff pane opens and the command that opens it.
//!
//! A viewer either lets git render the diff and pipe it through a pager, or
//! renders the diff itself from an argv template. The built-in viewers are
//! values of the same type a custom one resolves to, so every viewer takes
//! one path from a click to a spawn.

mod launch;
mod settings;

use alacritree_common::tools::Tool;

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

/// git arguments after `git` for the requested diff.
fn diff_args(req: &DiffRequest) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    match &req.source {
        DiffSource::Staged => args.push("--cached".to_string()),
        DiffSource::Worktree => {},
        DiffSource::Untracked => args.push("--no-index".to_string()),
        DiffSource::Branch { base } => args.push(format!("{base}...")),
    }
    args.push("--".to_string());
    if matches!(req.source, DiffSource::Untracked) {
        args.push("/dev/null".to_string());
    }
    args.push(req.file.clone());
    args
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
            branch: words(&["-r", "{base}...HEAD", "-p", "{file}"]),
            staged_scope: words(&["-w"]),
            unstaged_scope: words(&["-w"]),
            branch_scope: words(&["-r", "{base}...HEAD"]),
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
                        && (!arg.contains("{base}") || target.base().is_some())
                })
        },
    }
}

/// How a viewer opens a target, or `None` when the target is unavailable.
pub fn plan(viewer: &Viewer, target: &Target) -> Option<Launch> {
    if !opens(viewer, target) {
        return None;
    }
    Some(match viewer {
        Viewer::Pager { pager, args } => Launch::Pager {
            pager: pager.clone(),
            pager_args: args.clone(),
            git_args: target_git_args(target),
        },
        Viewer::Direct { program, templates } => Launch::Direct {
            program: program.clone(),
            args: templates.for_target(target).iter().map(|arg| substitute(arg, target)).collect(),
        },
    })
}

fn target_git_args(target: &Target) -> Vec<String> {
    match target {
        Target::Row(req) => diff_args(req),
        Target::Section(Section::Staged) => vec!["diff".to_string(), "--cached".to_string()],
        Target::Section(Section::Unstaged) => vec!["diff".to_string()],
        Target::Section(Section::Branch { base }) => vec!["diff".to_string(), format!("{base}...")],
    }
}

fn substitute(arg: &str, target: &Target) -> String {
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
    use super::*;

    fn row(file: &str, source: DiffSource) -> Target {
        Target::Row(DiffRequest { file: file.to_string(), source })
    }

    fn branch() -> DiffSource {
        DiffSource::Branch { base: "refs/remotes/origin/main".to_string() }
    }

    fn direct_args(viewer: &Viewer, target: &Target) -> Option<Vec<String>> {
        match plan(viewer, target)? {
            Launch::Direct { args, .. } => Some(args),
            Launch::Pager { .. } => panic!("a direct viewer planned a pager launch"),
        }
    }

    fn git_args(target: &Target) -> Vec<String> {
        match plan(&Viewer::delta(), target).expect("delta opens everything") {
            Launch::Pager { git_args, .. } => git_args,
            Launch::Direct { .. } => panic!("delta planned a direct launch"),
        }
    }

    #[test]
    fn diff_args_per_source() {
        let req = |source| DiffRequest { file: "a.rs".to_string(), source };
        assert_eq!(diff_args(&req(DiffSource::Staged)), ["diff", "--cached", "--", "a.rs"]);
        assert_eq!(diff_args(&req(DiffSource::Worktree)), ["diff", "--", "a.rs"]);
        assert_eq!(diff_args(&req(DiffSource::Untracked)), [
            "diff",
            "--no-index",
            "--",
            "/dev/null",
            "a.rs"
        ]);
        let base = DiffSource::Branch { base: "main".to_string() };
        assert_eq!(diff_args(&req(base)), ["diff", "main...", "--", "a.rs"]);
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
    fn delta_pipes_each_target_through_the_pager() {
        let launch = plan(&Viewer::delta(), &row("a.rs", DiffSource::Staged)).unwrap();
        assert_eq!(launch, Launch::Pager {
            pager: Program::Tool(Tool::Delta),
            pager_args: vec!["--paging=always".to_string()],
            git_args: vec!["diff".into(), "--cached".into(), "--".into(), "a.rs".into()],
        });
        assert_eq!(git_args(&Target::Section(Section::Staged)), ["diff", "--cached"]);
        assert_eq!(git_args(&Target::Section(Section::Unstaged)), ["diff"]);
        assert_eq!(git_args(&Target::Section(Section::Branch { base: "main".into() })), [
            "diff", "main..."
        ]);
    }

    #[test]
    fn tuicr_reviews_rows_by_path_and_sections_by_scope() {
        let tuicr = Viewer::tuicr();
        for source in [DiffSource::Staged, DiffSource::Worktree, DiffSource::Untracked] {
            assert_eq!(direct_args(&tuicr, &row("a.rs", source)).unwrap(), ["-w", "-p", "a.rs"]);
        }
        assert_eq!(direct_args(&tuicr, &row("a.rs", branch())).unwrap(), [
            "-r",
            "refs/remotes/origin/main...HEAD",
            "-p",
            "a.rs"
        ]);
        assert_eq!(direct_args(&tuicr, &Target::Section(Section::Staged)).unwrap(), ["-w"]);
        assert_eq!(direct_args(&tuicr, &Target::Section(Section::Unstaged)).unwrap(), ["-w"]);
        let changes = Target::Section(Section::Branch { base: "main".into() });
        assert_eq!(direct_args(&tuicr, &changes).unwrap(), ["-r", "main...HEAD"]);
        assert_eq!(tuicr.program(), &Program::Tool(Tool::Tuicr));
    }

    #[test]
    fn a_file_name_stays_one_argument() {
        let args = direct_args(&Viewer::tuicr(), &row("dir/my {base} 'x'.rs", DiffSource::Staged));
        assert_eq!(args.unwrap(), ["-w", "-p", "dir/my {base} 'x'.rs"]);
    }

    #[test]
    fn an_empty_template_or_a_missing_placeholder_value_opens_nothing() {
        let viewer = Viewer::Direct {
            program: Program::Custom { path: "difft".to_string(), wsl_path: None },
            templates: Templates {
                staged: vec!["--base".to_string(), "{base}".to_string()],
                ..Templates::default()
            },
        };
        let staged_row = row("a.rs", DiffSource::Staged);
        assert!(!opens(&viewer, &staged_row));
        assert!(plan(&viewer, &staged_row).is_none(), "a staged row has no base");
        let unstaged_row = row("a.rs", DiffSource::Worktree);
        assert!(!opens(&viewer, &unstaged_row));
        assert!(plan(&viewer, &unstaged_row).is_none(), "the unstaged template is empty");
        assert!(opens(&Viewer::delta(), &Target::Section(Section::Unstaged)));
    }
}
