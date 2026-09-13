//! What the git panel's diff pane opens and the command that opens it.
//!
//! A viewer either lets git render the diff and pipe it through a pager, or
//! renders the diff itself from an argv template. The built-in viewers are
//! values of the same type a custom one resolves to, so every viewer takes
//! one path from a click to a spawn.

use std::path::Path;

use crate::tools::Tool;

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
pub fn diff_args(req: &DiffRequest) -> Vec<String> {
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

/// `pager` with its arguments, as one `core.pager` value.
pub fn pager_command(pager: &str, args: &[String]) -> String {
    let mut words = vec![pager.to_string()];
    words.extend(args.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

/// A registry-resolved executable path with arguments, as one `core.pager`
/// value. Git runs that value through a shell, so quote the path as one word.
pub fn executable_pager_command(path: &str, args: &[String]) -> String {
    let mut words = vec![shell_quote(path)];
    words.extend(args.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// git with the pager wired in as its `core.pager`.
pub fn native_pager_command(git: &str, pager: &str, git_args: &[String]) -> (String, Vec<String>) {
    let mut args = vec!["-c".to_string(), format!("core.pager={pager}")];
    args.extend(git_args.iter().cloned());
    (git.to_string(), args)
}

const LOGIN_SHELL: &str = r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=${SHELL:-/bin/sh}"#;

const PAGER_SCRIPT: &str =
    r#"export LESS="${LESS-R}"; g=$1; p=$2; shift 2; exec "$g" -c "core.pager=$p" "$@""#;

pub fn wsl_pager_command(
    distro: &str,
    workspace: &Path,
    git: &str,
    pager: &str,
    git_args: &[String],
) -> (String, Vec<String>) {
    let positional =
        [git.to_string(), pager.to_string()].into_iter().chain(git_args.iter().cloned());
    wsl_sh(distro, workspace, PAGER_SCRIPT.to_string(), positional)
}

pub fn wsl_pager_command_login(
    distro: &str,
    workspace: &Path,
    git: &str,
    pager: &str,
    git_args: &[String],
) -> (String, Vec<String>) {
    let script = format!(r#"{LOGIN_SHELL}; exec "$s" -lc '{PAGER_SCRIPT}' "$s" "$@""#);
    let positional =
        [git.to_string(), pager.to_string()].into_iter().chain(git_args.iter().cloned());
    wsl_sh(distro, workspace, script, positional)
}

pub fn wsl_direct_command(
    distro: &str,
    workspace: &Path,
    program: &str,
    args: &[String],
) -> (String, Vec<String>) {
    let mut argv = vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        workspace.to_string_lossy().into_owned(),
        "--exec".to_string(),
        program.to_string(),
    ];
    argv.extend(args.iter().cloned());
    ("wsl.exe".to_string(), argv)
}

pub fn wsl_direct_command_login(
    distro: &str,
    workspace: &Path,
    program: &str,
    args: &[String],
) -> (String, Vec<String>) {
    let script = format!(r#"{LOGIN_SHELL}; exec "$s" -lc 'exec "$@"' "$s" "$@""#);
    let positional = std::iter::once(program.to_string()).chain(args.iter().cloned());
    wsl_sh(distro, workspace, script, positional)
}

fn wsl_sh(
    distro: &str,
    workspace: &Path,
    script: String,
    positional: impl IntoIterator<Item = String>,
) -> (String, Vec<String>) {
    let mut args = vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        workspace.to_string_lossy().into_owned(),
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script,
        "sh".to_string(),
    ];
    args.extend(positional);
    ("wsl.exe".to_string(), args)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

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

    #[test]
    fn executable_pager_paths_are_quoted_but_custom_commands_keep_their_syntax() {
        let args = vec!["--side-by-side".to_string(), "arg with spaces".to_string()];
        assert_eq!(
            executable_pager_command(r"C:\Program Files\delta\delta.exe", &args),
            r#"'C:\Program Files\delta\delta.exe' '--side-by-side' 'arg with spaces'"#
        );
        assert_eq!(
            pager_command("delta --side-by-side", &["--paging=always".to_string()]),
            "delta --side-by-side '--paging=always'"
        );
    }

    #[test]
    fn a_native_pager_launch_runs_git_with_the_pager_wired_in() {
        let pager =
            executable_pager_command(r"C:\tools\delta.exe", &["--paging=always".to_string()]);
        let git_args = ["diff".to_string(), "--".to_string(), "a.rs".to_string()];
        let (program, args) = native_pager_command("git", &pager, &git_args);
        assert_eq!(program, "git");
        assert_eq!(args, [
            "-c",
            r"core.pager='C:\tools\delta.exe' '--paging=always'",
            "diff",
            "--",
            "a.rs"
        ]);
    }

    #[test]
    // Git starts a pager only when stdout is a terminal, which a test never
    // has, so this runs the `core.pager` value the way Git does: `sh -c`.
    #[allow(clippy::disallowed_methods)] // This test runs Git's real shell.
    fn a_real_git_pager_shell_handles_registry_paths_and_custom_arguments() {
        use std::io::Write;

        let Some(shell) = shell_for_git() else {
            eprintln!("skipping the Git pager boundary test: Git's shell was not found");
            return;
        };
        let temp = tempfile::tempdir().expect("a temp directory");
        let pager_dir = temp.path().join(if cfg!(windows) {
            "pager [metachar] space"
        } else {
            "pager [metachar] \\ 'quote' space"
        });
        fs::create_dir(&pager_dir).expect("create pager directory");
        let pager_path = pager_dir.join("pager");
        fs::write(
            &pager_path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ALACRITREE_DIFF_VIEWER_OUTPUT\"\ncat >> \
             \"$ALACRITREE_DIFF_VIEWER_OUTPUT\"\n",
        )
        .expect("write the pager script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&pager_path, fs::Permissions::from_mode(0o755))
                .expect("make pager executable");
        }
        let pager_path = pager_path.to_str().expect("pager path is UTF-8");

        let captured = temp.path().join("captured diff.txt");
        let run = |pager: String| {
            let mut child = crate::command_ext::hidden(&shell)
                .arg("-c")
                .arg(&pager)
                .env("ALACRITREE_DIFF_VIEWER_OUTPUT", &captured)
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("run the pager through Git's shell");
            child
                .stdin
                .take()
                .expect("the pager's stdin")
                .write_all(b"-before\n+after\n")
                .expect("feed the diff to the pager");
            let output = child.wait_with_output().expect("wait for the pager");
            let capture = fs::read_to_string(&captured).unwrap_or_else(|err| {
                panic!(
                    "the pager never ran ({err}): status={:?}, stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            capture.replace("\r\n", "\n")
        };

        let registry_pager = executable_pager_command(pager_path, &["--paging=always".to_string()]);
        assert_eq!(run(registry_pager), "--paging=always\n-before\n+after\n");

        let custom_pager = pager_command(&format!("{} --flag", shell_quote(pager_path)), &[
            "two words".to_string(),
        ]);
        assert_eq!(run(custom_pager), "--flag\ntwo words\n-before\n+after\n");
    }

    const WORKSPACE: &str = r"\\wsl.localhost\kali-linux\home\lev\proj";

    #[test]
    fn a_wsl_pager_launch_passes_git_pager_and_diff_as_positional_parameters() {
        let git_args = ["diff".to_string(), "--cached".to_string()];
        let (program, args) = wsl_pager_command(
            "kali-linux",
            Path::new(WORKSPACE),
            "git",
            "/bin/delta --paging=always",
            &git_args,
        );
        assert_eq!(program, "wsl.exe");
        assert_eq!(args[..7], ["-d", "kali-linux", "--cd", WORKSPACE, "--exec", "sh", "-c"]);
        assert_eq!(
            args[7],
            r#"export LESS="${LESS-R}"; g=$1; p=$2; shift 2; exec "$g" -c "core.pager=$p" "$@""#
        );
        assert_eq!(args[8..], ["sh", "git", "/bin/delta --paging=always", "diff", "--cached"]);
    }

    #[test]
    fn a_login_wsl_pager_launch_exports_less_after_the_profile() {
        let (_, args) = wsl_pager_command_login(
            "kali-linux",
            Path::new(WORKSPACE),
            "git",
            "delta --paging=always",
            &["diff".to_string()],
        );
        let script = &args[7];
        assert!(script.contains("getent passwd"), "resolves the login shell: {script}");
        assert!(
            script.contains(r#"-lc 'export LESS="${LESS-R}"; g=$1; p=$2; shift 2; exec "$g" -c "core.pager=$p" "$@"' "$s" "$@""#),
            "a LESS set by the profile still wins: {script}"
        );
        assert_eq!(args[8..], ["sh", "git", "delta --paging=always", "diff"]);
    }

    #[test]
    fn a_wsl_registry_pager_path_is_shell_quoted() {
        let pager = executable_pager_command(r"/opt/tools/delta path\with'quote;echo", &[
            "--side-by-side".to_string(),
        ]);
        let (_, args) =
            wsl_pager_command("kali-linux", Path::new(WORKSPACE), "/usr/bin/git", &pager, &[
                "diff".to_string(),
            ]);
        assert_eq!(args[10], r#"'/opt/tools/delta path\with'\''quote;echo' '--side-by-side'"#);
    }

    #[test]
    fn a_wsl_direct_launch_execs_the_resolved_program() {
        let (program, args) = wsl_direct_command(
            "kali-linux",
            Path::new(WORKSPACE),
            "/home/lev/.cargo/bin/tuicr",
            &["-w".to_string(), "-p".to_string(), "a b.rs".to_string()],
        );
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, [
            "-d",
            "kali-linux",
            "--cd",
            WORKSPACE,
            "--exec",
            "/home/lev/.cargo/bin/tuicr",
            "-w",
            "-p",
            "a b.rs"
        ]);
    }

    #[test]
    fn a_login_wsl_direct_launch_passes_the_program_as_a_parameter() {
        let (_, args) = wsl_direct_command_login("kali-linux", Path::new(WORKSPACE), "tuicr", &[
            "-w".to_string(),
        ]);
        assert_eq!(args[..7], ["-d", "kali-linux", "--cd", WORKSPACE, "--exec", "sh", "-c"]);
        assert!(args[7].contains("getent passwd"));
        assert!(args[7].ends_with(r#"exec "$s" -lc 'exec "$@"' "$s" "$@""#), "{}", args[7]);
        assert_eq!(args[8..], ["sh", "tuicr", "-w"]);
    }

    #[cfg(windows)]
    #[allow(clippy::disallowed_methods)] // This test probes Git's bundled shell.
    fn shell_for_git() -> Option<PathBuf> {
        let output = crate::command_ext::hidden("git")
            .arg("--exec-path")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?
            .wait_with_output()
            .ok()?;
        let exec_path = String::from_utf8(output.stdout).ok()?;
        let exec_path = PathBuf::from(exec_path.trim());
        let root = exec_path.parent()?.parent()?.parent()?;
        [root.join("usr/bin/sh.exe"), root.join("bin/sh.exe")]
            .into_iter()
            .find(|path| path.is_file())
    }

    #[cfg(not(windows))]
    fn shell_for_git() -> Option<PathBuf> {
        Some(PathBuf::from("/bin/sh"))
    }
}
