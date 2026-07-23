//! Render templated sidebar row names.
//!
//! Templates come from `[ui] worktree_name` / `[ui] project_name` and use
//! subst's shell-style syntax: `$var`, `${var}`, and `${var:fallback}` (the
//! fallback may itself contain variables, so `${branch:$name}` reads "the
//! branch, or the worktree name when detached").  Variables that describe
//! something optional — `$branch` on a detached worktree, `$pr` with no
//! known PR — are absent rather than empty, so `${pr:}` conditionally shows
//! the PR number while a bare `$pr` treats its absence as an error.  Any
//! error — parse failure, unknown variable — falls back to the plain name
//! with one warning per config key, so a typo'd config degrades to today's
//! sidebar rather than blank rows.

use std::collections::{HashMap, HashSet};

use crate::pr_status::PrInfo;
use crate::projects::{Project, Worktree};

/// Substitute `vars` into `template`.  `None` on any subst error or when the
/// trimmed result is empty — the caller falls back to the plain name either
/// way, because a blank row label is as useless as a failed one.
pub fn render_label(template: &str, vars: &HashMap<String, String>) -> Option<String> {
    let rendered = subst::substitute(template, vars).ok()?;
    let trimmed = rendered.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The configured templates plus warn-once bookkeeping.  Config strings are
/// static per run, so one warning per config key + template covers every row
/// that hits the same mistake without flooding the log every frame — keying
/// on the template alone would let a second, independently broken config key
/// hide behind the first one's warning just because the text matched.
pub struct LabelTemplates {
    worktree: Option<String>,
    project: Option<String>,
    warned: HashSet<(String, String)>,
}

impl LabelTemplates {
    pub fn new(worktree: Option<String>, project: Option<String>) -> Self {
        Self { worktree, project, warned: HashSet::new() }
    }

    /// Display name for a worktree row.  Variables: `$name` (worktree name),
    /// `$branch` (absent when detached, so `${branch:...}` falls back),
    /// `$path` (full worktree path), `$pr` (the branch's PR number as
    /// `#123`, absent when none is known — `${pr:}` shows it only when one
    /// exists).
    pub fn worktree_label(&mut self, wt: &Worktree, pr: Option<&PrInfo>) -> String {
        let Some(template) = self.worktree.clone() else {
            return wt.name.clone();
        };
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), wt.name.clone());
        if let Some(branch) = &wt.branch {
            vars.insert("branch".to_string(), branch.clone());
        }
        vars.insert("path".to_string(), crate::wsl::display_path(&wt.path));
        if let Some(pr) = pr {
            vars.insert("pr".to_string(), format!("#{}", pr.number));
        }
        self.render_or_fallback("worktree_name", &template, &vars, &wt.name)
    }

    /// Display name for a project row.  A manual rename always wins — the
    /// template only shapes the *default* name.  Variables: `$name`
    /// (directory name), `$path` (full project root).
    pub fn project_label(&mut self, project: &Project) -> String {
        if let Some(label) = &project.label {
            return label.clone();
        }
        let Some(template) = self.project.clone() else {
            return project.name.clone();
        };
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), project.name.clone());
        vars.insert("path".to_string(), crate::wsl::display_path(&project.root));
        self.render_or_fallback("project_name", &template, &vars, &project.name)
    }

    fn render_or_fallback(
        &mut self,
        slot: &str,
        template: &str,
        vars: &HashMap<String, String>,
        fallback: &str,
    ) -> String {
        match render_label(template, vars) {
            Some(rendered) => rendered,
            None => {
                if self.warned.insert((slot.to_string(), template.to_string())) {
                    log::warn!(
                        "[ui] {slot} template {template:?} failed to render; using plain name"
                    );
                }
                fallback.to_string()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Worktree;
    use std::path::PathBuf;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn wt(name: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            name: name.to_string(),
            path: PathBuf::from("/tmp/wt").join(name),
            branch: branch.map(str::to_string),
            is_main: false,
            prunable: false,
        }
    }

    fn project(name: &str, label: Option<&str>) -> Project {
        Project {
            root: PathBuf::from("/tmp/projects").join(name),
            name: name.to_string(),
            label: label.map(str::to_string),
            default_branch: None,
            worktrees: Vec::new(),
            expanded: false,
            shell_override: None,
            home: None,
        }
    }

    #[test]
    fn plain_variable_substitutes() {
        assert_eq!(
            render_label("$name", &vars(&[("name", "feature-x")])).as_deref(),
            Some("feature-x")
        );
    }

    #[test]
    fn literal_text_passes_through() {
        assert_eq!(render_label("wt: $name", &vars(&[("name", "a")])).as_deref(), Some("wt: a"));
    }

    #[test]
    fn fallback_used_when_variable_missing() {
        let v = vars(&[("name", "main-wt")]);
        assert_eq!(render_label("${branch:$name}", &v).as_deref(), Some("main-wt"));
    }

    #[test]
    fn fallback_ignored_when_variable_present() {
        let v = vars(&[("name", "main-wt"), ("branch", "feat/x")]);
        assert_eq!(render_label("${branch:$name}", &v).as_deref(), Some("feat/x"));
    }

    #[test]
    fn unknown_variable_is_an_error() {
        assert_eq!(render_label("$nope", &vars(&[("name", "a")])), None);
    }

    #[test]
    fn empty_render_is_an_error() {
        assert_eq!(render_label("  ", &vars(&[])), None);
        assert_eq!(render_label("$name", &vars(&[("name", " ")])), None);
    }

    #[test]
    fn no_template_returns_plain_names() {
        let mut t = LabelTemplates::new(None, None);
        assert_eq!(t.worktree_label(&wt("alpha", Some("feat/a")), None), "alpha");
        assert_eq!(t.project_label(&project("proj", None)), "proj");
    }

    #[test]
    fn worktree_template_renders_branch_with_name_fallback() {
        let mut t = LabelTemplates::new(Some("${branch:$name}".into()), None);
        assert_eq!(t.worktree_label(&wt("alpha", Some("feat/a")), None), "feat/a");
        assert_eq!(t.worktree_label(&wt("detached", None), None), "detached");
    }

    #[test]
    fn project_template_renders_but_manual_label_wins() {
        let mut t = LabelTemplates::new(None, Some("[$name]".into()));
        assert_eq!(t.project_label(&project("proj", None)), "[proj]");
        assert_eq!(t.project_label(&project("proj", Some("Renamed"))), "Renamed");
    }

    #[test]
    fn bad_template_falls_back_to_plain_name() {
        let mut t = LabelTemplates::new(Some("$typo".into()), Some("$typo".into()));
        assert_eq!(t.worktree_label(&wt("alpha", None), None), "alpha");
        assert_eq!(t.project_label(&project("proj", None)), "proj");
    }

    #[test]
    fn warn_once_per_config_key_not_per_template() {
        let mut t = LabelTemplates::new(Some("$typo".into()), Some("$typo".into()));
        assert_eq!(t.worktree_label(&wt("alpha", None), None), "alpha");
        assert_eq!(t.worktree_label(&wt("alpha", None), None), "alpha");
        assert_eq!(t.project_label(&project("proj", None)), "proj");
        assert_eq!(t.project_label(&project("proj", None)), "proj");
        assert_eq!(t.warned.len(), 2);
    }

    #[test]
    fn path_variable_is_available() {
        let mut t = LabelTemplates::new(Some("$path".into()), Some("$path".into()));
        let w = wt("alpha", None);
        assert_eq!(t.worktree_label(&w, None), w.path.display().to_string());
        let p = project("proj", None);
        assert_eq!(t.project_label(&p), p.root.display().to_string());
    }

    fn pr(number: u64) -> PrInfo {
        PrInfo {
            number,
            base_branch: "master".into(),
            url: String::new(),
            state: crate::pr_status::PrState::Open,
        }
    }

    #[test]
    fn pr_variable_shows_conditionally() {
        let mut t = LabelTemplates::new(Some("${pr:} ${branch:$name}".into()), None);
        assert_eq!(t.worktree_label(&wt("alpha", Some("feat/a")), Some(&pr(42))), "#42 feat/a");
        // No PR: ${pr:} renders empty and the trim eats the stray space.
        assert_eq!(t.worktree_label(&wt("alpha", Some("feat/a")), None), "feat/a");
    }

    #[test]
    fn bare_pr_variable_falls_back_without_a_pr() {
        let mut t = LabelTemplates::new(Some("$pr $name".into()), None);
        assert_eq!(t.worktree_label(&wt("alpha", None), Some(&pr(7))), "#7 alpha");
        assert_eq!(t.worktree_label(&wt("alpha", None), None), "alpha");
    }

    /// `$path` is the template's window onto the filesystem, so a WSL worktree
    /// must substitute the path the user would type inside the distro.
    #[cfg(windows)]
    #[test]
    fn the_path_variable_uses_the_distros_spelling() {
        let wt = Worktree {
            name: "monorepo".to_string(),
            path: PathBuf::from(r"\\wsl.localhost\kali-linux\home\lev\Git\monorepo"),
            branch: Some("main".to_string()),
            is_main: true,
            prunable: false,
        };
        let mut templates = LabelTemplates::new(Some("$path".to_string()), None);
        assert_eq!(templates.worktree_label(&wt, None), "/home/lev/Git/monorepo");
    }
}
