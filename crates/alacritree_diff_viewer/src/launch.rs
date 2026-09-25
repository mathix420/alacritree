//! The command line that opens a [`Launch`] on the side of a Windows and WSL
//! installation its workspace belongs to.

use std::path::Path;

use alacritree_common::tools::{self, Tool};
use alacritree_common::{side, wsl};

use crate::{Launch, Program};

impl Launch {
    /// Program and argv that open this launch in `workspace`. A tool whose
    /// path inside a distro is still being looked up goes through the
    /// distro's login shell this time, and `on_found` runs once the lookup
    /// lands so the caller can repaint.
    pub fn command_line(
        self,
        workspace: &Path,
        on_found: impl FnOnce() + Send + 'static,
    ) -> (String, Vec<String>) {
        match wsl::classify(workspace) {
            wsl::Location::Wsl { distro, .. } => wsl_command(self, &distro, workspace, on_found),
            wsl::Location::Windows(_) => native_command(self),
        }
    }
}

fn native_program(program: &Program) -> String {
    match program {
        Program::Tool(tool) => tools::program(*tool),
        Program::Custom { path, .. } => path.clone(),
    }
}

fn native_pager_value(program: &Program, args: &[String]) -> String {
    match program {
        Program::Tool(tool) => executable_pager_command(&tools::program(*tool), args),
        Program::Custom { path, .. } => pager_command(path, args),
    }
}

fn native_command(launch: Launch) -> (String, Vec<String>) {
    match launch {
        Launch::Pager { pager, pager_args, git_args } => native_pager_command(
            &tools::program(Tool::Git),
            &native_pager_value(&pager, &pager_args),
            &git_args,
        ),
        Launch::Direct { program, args } => (native_program(&program), args),
    }
}

fn wsl_program(
    distro: &str,
    program: &Program,
    on_found: impl FnOnce() + Send + 'static,
) -> Option<String> {
    match program {
        Program::Tool(tool) => tools::wsl_resolved(*tool, distro, on_found),
        Program::Custom { wsl_path, .. } => wsl_path.clone(),
    }
}

fn program_name(program: &Program) -> &str {
    match program {
        Program::Tool(tool) => tool.name(),
        Program::Custom { path, .. } => path,
    }
}

fn wsl_pager_value(program: &Program, path: &str, args: &[String]) -> String {
    match program {
        Program::Tool(_) => executable_pager_command(path, args),
        Program::Custom { .. } => pager_command(path, args),
    }
}

fn wsl_command(
    launch: Launch,
    distro: &str,
    workspace: &Path,
    on_found: impl FnOnce() + Send + 'static,
) -> (String, Vec<String>) {
    let git = tools::wsl_program(Tool::Git);
    match launch {
        Launch::Pager { pager, pager_args, git_args } => {
            match wsl_program(distro, &pager, on_found) {
                Some(path) => {
                    let pager = wsl_pager_value(&pager, &path, &pager_args);
                    wsl_pager_command(distro, workspace, &git, &pager, &git_args)
                },
                None => {
                    let pager = pager_command(program_name(&pager), &pager_args);
                    wsl_pager_command_login(distro, workspace, &git, &pager, &git_args)
                },
            }
        },
        Launch::Direct { program, args } => {
            let program = side::Program {
                native: native_program(&program),
                wsl: wsl_program(distro, &program, on_found),
                name: program_name(&program).to_string(),
            };
            side::wsl_command_line(distro, workspace, &program, &args)
        },
    }
}

/// `pager` with its arguments, as one `core.pager` value.
fn pager_command(pager: &str, args: &[String]) -> String {
    let mut words = vec![pager.to_string()];
    words.extend(args.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

/// A registry-resolved executable path with arguments, as one `core.pager`
/// value. Git runs that value through a shell, so quote the path as one word.
fn executable_pager_command(path: &str, args: &[String]) -> String {
    let mut words = vec![shell_quote(path)];
    words.extend(args.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// git with the pager wired in as its `core.pager`.
fn native_pager_command(git: &str, pager: &str, git_args: &[String]) -> (String, Vec<String>) {
    let mut args = vec!["-c".to_string(), format!("core.pager={pager}")];
    args.extend(git_args.iter().cloned());
    (git.to_string(), args)
}

const PAGER_SCRIPT: &str =
    r#"export LESS="${LESS-R}"; g=$1; p=$2; shift 2; exec "$g" -c "core.pager=$p" "$@""#;

fn wsl_pager_command(
    distro: &str,
    workspace: &Path,
    git: &str,
    pager: &str,
    git_args: &[String],
) -> (String, Vec<String>) {
    wsl_pager_script(distro, workspace, PAGER_SCRIPT.to_string(), git, pager, git_args)
}

/// Runs the pager script inside the login shell, so a `LESS` the profile
/// sets still wins.
fn wsl_pager_command_login(
    distro: &str,
    workspace: &Path,
    git: &str,
    pager: &str,
    git_args: &[String],
) -> (String, Vec<String>) {
    let script = side::login_shell_script(PAGER_SCRIPT);
    wsl_pager_script(distro, workspace, script, git, pager, git_args)
}

fn wsl_pager_script(
    distro: &str,
    workspace: &Path,
    script: String,
    git: &str,
    pager: &str,
    git_args: &[String],
) -> (String, Vec<String>) {
    let argv = ["sh".to_string(), "-c".to_string(), script, "sh".to_string()]
        .into_iter()
        .chain([git.to_string(), pager.to_string()])
        .chain(git_args.iter().cloned());
    wsl::exec_invocation_in(distro, workspace, argv)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

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
            let mut child = alacritree_common::command_ext::hidden(&shell)
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
    fn a_wsl_diff_uses_the_configured_wsl_git_not_the_native_git() {
        use strum::EnumCount;

        let _lock = tools::test_configuration_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        struct RestoreToolConfiguration([tools::ToolPaths; Tool::COUNT]);
        impl Drop for RestoreToolConfiguration {
            fn drop(&mut self) {
                tools::configure(self.0.clone());
            }
        }
        let _restore = RestoreToolConfiguration(tools::test_configuration());
        let mut configured = Tool::table(tools::ToolPaths::named);
        configured[Tool::Git as usize] = tools::ToolPaths {
            native: "C:/native/git.exe".to_string(),
            wsl: Some("/opt/wsl/bin/git".to_string()),
        };
        tools::configure(configured);
        let launch = Launch::Pager {
            pager: Program::Custom {
                path: "delta --side-by-side".to_string(),
                wsl_path: Some("/opt/delta".to_string()),
            },
            pager_args: Vec::new(),
            git_args: vec!["diff".to_string()],
        };
        let (_, args) = wsl_command(launch, "kali-linux", Path::new(WORKSPACE), || {});
        assert_eq!(args[9], "/opt/wsl/bin/git");
        assert!(!args[9..].iter().any(|arg| arg == "C:/native/git.exe"));
    }

    #[cfg(windows)]
    #[allow(clippy::disallowed_methods)] // This test probes Git's bundled shell.
    fn shell_for_git() -> Option<PathBuf> {
        let output = alacritree_common::command_ext::hidden("git")
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
