//! The tasks tab: taskwarrior's lists for one workspace, drawn as checklist
//! rows. Each change is one `task` call on the pool, typed text shows at
//! once, and a reload replaces it with what taskwarrior holds. Agents write
//! the same store, so the tab re-exports while visible instead of trusting
//! its own copy.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Color32, Key, Modifiers, Response, RichText, ScrollArea, Sense, TextEdit, Ui};

use crate::jobs::{self, Blocking, Job, Priority};
use crate::multiplexer::Side;
use crate::projects::{Project, Worktree};
use crate::tasks::scope::{GLOBAL, Place, node};
use crate::tasks::taskwarrior::{Status, Task, TaskError, Taskwarrior};
use crate::tasks::tree::{self, Edit, Row, Section};
use crate::wsl;

const RELOAD_EVERY: Duration = Duration::from_secs(1);
const INDENT: f32 = 16.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scope {
    pub side: Side,
    pub repo: Option<String>,
    pub workspace: Option<String>,
}

impl Scope {
    /// Reads the sidebar's model, never git: a WSL worktree is a UNC path
    /// Windows-side git reads wrongly, and the sidebar already has both names.
    pub(crate) fn for_workspace(project: Option<&Project>, worktree: Option<&Worktree>) -> Self {
        let side = match project.map(|p| wsl::classify(&p.root)) {
            Some(wsl::Location::Wsl { distro, .. }) => Side::Wsl(distro),
            _ => Side::Native,
        };
        let workspace = project.zip(worktree).map(|(p, wt)| {
            let branch = wt.branch.clone().unwrap_or_else(|| wt.name.clone());
            node(&Place::Workspace { repo: p.name.clone(), branch }, None)
        });
        let repo = project.map(|p| node(&Place::Project { repo: p.name.clone() }, None));
        Self { side, repo, workspace }
    }

    /// `project:` is a left match, so `project:r` alone would also return a
    /// repository named `r-web`; the trailing dot keeps to `r`'s subtree.
    pub(crate) fn filter(&self) -> Vec<String> {
        let scopes = match &self.repo {
            Some(repo) => format!("(project.is:{repo} or project:{repo}. or project.is:{GLOBAL})"),
            None => format!("(project.is:{GLOBAL})"),
        };
        vec![scopes, "(status:pending or status:completed)".to_string()]
    }
}

/// A write in flight, with the row its failure is shown on.
type PendingWrite = (Option<String>, Job<Result<(), TaskError>>);

/// A row being typed that taskwarrior has not got yet.
struct NewRow {
    node: String,
    after: Option<String>,
    depth: usize,
    text: String,
    focus: bool,
}

pub(crate) struct TasksView {
    scope: Scope,
    tasks: Vec<Task>,
    load_error: Option<String>,
    reload: Option<Job<Result<Vec<Task>, TaskError>>>,
    last_reload: Option<Instant>,
    writes: Vec<PendingWrite>,
    row_errors: HashMap<String, String>,
    /// Typed text a reload has not confirmed yet, by uuid.
    drafts: HashMap<String, String>,
    new_row: Option<NewRow>,
    collapsed: HashSet<String>,
}

impl TasksView {
    pub(crate) fn new(scope: Scope) -> Self {
        Self {
            scope,
            tasks: Vec::new(),
            load_error: None,
            reload: None,
            last_reload: None,
            writes: Vec::new(),
            row_errors: HashMap::new(),
            drafts: HashMap::new(),
            new_row: None,
            collapsed: HashSet::new(),
        }
    }

    fn sections(&self) -> Vec<Section> {
        tree::sections(&self.tasks, self.scope.repo.as_deref(), self.scope.workspace.as_deref())
    }

    pub(crate) fn plain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for section in self.sections() {
            lines.push(format!("## {}", section.node));
            for row in section.rows {
                let mark = if row.status == Status::Completed { "x" } else { " " };
                lines.push(format!("{}- [{mark}] {}", "  ".repeat(row.depth), row.text));
            }
        }
        lines
    }

    fn section_tasks(&self, node: &str) -> Vec<Task> {
        self.tasks
            .iter()
            .filter(|t| t.project.as_deref().unwrap_or(GLOBAL) == node)
            .cloned()
            .collect()
    }

    fn write(
        &mut self,
        row: Option<String>,
        work: impl FnOnce(&Taskwarrior, &Blocking) -> Result<(), TaskError> + Send + 'static,
    ) {
        if let Some(uuid) = &row {
            self.row_errors.remove(uuid);
        }
        let side = self.scope.side.clone();
        let job = jobs::pool()
            .spawn(Priority::Interactive, move |b| work(&Taskwarrior::for_side(side, b), b));
        self.writes.push((row, job));
    }

    fn apply(&mut self, row: Option<String>, edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        self.write(row, move |tw, b| {
            for edit in edits {
                match edit {
                    Edit::Add { project, description, subof, order } => {
                        tw.add(&project, &description, subof.as_deref(), order, b)?;
                    },
                    Edit::Modify { uuid, mods } => tw.modify(&uuid, &mods, b)?,
                }
            }
            Ok(())
        });
    }

    /// Drains finished jobs, and reloads after any write or once a second.
    fn tick(&mut self) {
        let mut finished = Vec::new();
        self.writes.retain(|(row, job)| match job.poll() {
            Some(result) => {
                finished.push((row.clone(), result));
                false
            },
            None => !job.failed(),
        });
        let wrote = !finished.is_empty();
        for (row, result) in finished {
            let Err(e) = result else { continue };
            match row {
                Some(uuid) => {
                    self.drafts.remove(&uuid);
                    self.row_errors.insert(uuid, e.to_string());
                },
                None => self.load_error = Some(e.to_string()),
            }
        }
        if let Some(result) = self.reload.as_ref().and_then(Job::poll) {
            self.reload = None;
            match result {
                Ok(tasks) => {
                    self.load_error = None;
                    // A draft stays until the store holds its text or the
                    // task is gone.
                    self.drafts.retain(|uuid, text| {
                        tasks.iter().any(|t| &t.uuid == uuid && &t.description != text)
                    });
                    self.tasks = tasks;
                },
                Err(e) => self.load_error = Some(e.to_string()),
            }
        }
        let due = self.last_reload.is_none_or(|t| t.elapsed() >= RELOAD_EVERY);
        if self.reload.is_none() && (wrote || due) {
            let scope = self.scope.clone();
            self.reload = Some(jobs::pool().spawn(Priority::Background, move |b| {
                Taskwarrior::for_side(scope.side.clone(), b).export(&scope.filter(), b)
            }));
            self.last_reload = Some(Instant::now());
        }
    }
}

pub(crate) fn show(
    ui: &mut Ui,
    view: &mut TasksView,
    allow_focus: bool,
    text: Color32,
    dim: Color32,
    error: Color32,
) -> Response {
    view.tick();
    ui.ctx().request_repaint_after(RELOAD_EVERY);
    let colors = Colors { text, dim, error };
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if let Some(e) = &view.load_error {
            ui.label(RichText::new(e).color(error));
        }
        for section in view.sections() {
            show_section(ui, view, &section, allow_focus, colors);
        }
    });
    ui.interact(ui.min_rect(), ui.id().with("tasks-tab"), Sense::click())
}

#[derive(Clone, Copy)]
struct Colors {
    text: Color32,
    dim: Color32,
    error: Color32,
}

fn show_section(
    ui: &mut Ui,
    view: &mut TasksView,
    section: &Section,
    allow_focus: bool,
    c: Colors,
) {
    let open = !view.collapsed.contains(&section.node);
    let done = section.rows.iter().filter(|r| r.status == Status::Completed).count();
    let arrow = if open { "v" } else { ">" };
    let header = format!("{arrow} {}  {done}/{}", section.node, section.rows.len());
    if ui.selectable_label(false, RichText::new(header).color(c.text).strong()).clicked() {
        if open {
            view.collapsed.insert(section.node.clone());
        } else {
            view.collapsed.remove(&section.node);
        }
    }
    if !open {
        return;
    }
    let tasks = view.section_tasks(&section.node);
    for row in &section.rows {
        show_row(ui, view, section, &tasks, row, allow_focus, c);
        let follows = |n: &NewRow| n.node == section.node && n.after.as_deref() == Some(&row.uuid);
        if view.new_row.as_ref().is_some_and(follows) {
            show_new_row(ui, view, &tasks);
        }
    }
    let adding_first = |n: &NewRow| n.node == section.node && n.after.is_none();
    if view.new_row.as_ref().is_some_and(adding_first) {
        show_new_row(ui, view, &tasks);
    } else if ui.small_button(RichText::new("+ add a task").color(c.dim)).clicked() {
        // Drawn below the last root row from the next frame on.
        let last_root = section.rows.iter().rev().find(|r| r.depth == 0).map(|r| r.uuid.clone());
        view.new_row = Some(NewRow {
            node: section.node.clone(),
            after: last_root,
            depth: 0,
            text: String::new(),
            focus: true,
        });
    }
}

fn show_row(
    ui: &mut Ui,
    view: &mut TasksView,
    section: &Section,
    tasks: &[Task],
    row: &Row,
    allow_focus: bool,
    c: Colors,
) {
    let refs: Vec<&Task> = tasks.iter().collect();
    ui.horizontal(|ui| {
        ui.add_space(row.depth as f32 * INDENT);
        let mut checked = row.status == Status::Completed;
        if ui.checkbox(&mut checked, "").changed() {
            let uuid = row.uuid.clone();
            view.write(Some(row.uuid.clone()), move |tw, b| {
                if checked { tw.done(&uuid, b) } else { tw.undone(&uuid, b) }
            });
        }
        if row.started {
            ui.label(RichText::new(">").color(c.text));
        }
        let mut buffer = view.drafts.get(&row.uuid).cloned().unwrap_or_else(|| row.text.clone());
        // Locking focus keeps Tab for indenting instead of moving to the
        // next widget.
        let edit = ui.add_enabled(
            allow_focus,
            TextEdit::singleline(&mut buffer)
                .frame(false)
                .lock_focus(true)
                .text_color(if checked { c.dim } else { c.text })
                .desired_width(f32::INFINITY),
        );
        if edit.changed() {
            view.drafts.insert(row.uuid.clone(), buffer.clone());
        }
        edit.context_menu(|ui| {
            for (label, start) in [("Start", true), ("Stop", false)] {
                if ui.button(label).clicked() {
                    let uuid = row.uuid.clone();
                    view.write(Some(row.uuid.clone()), move |tw, b| {
                        if start { tw.start(&uuid, b) } else { tw.stop(&uuid, b) }
                    });
                    ui.close_menu();
                }
            }
        });
        if edit.has_focus() {
            let (tab, shift_tab, erase) = ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::Tab),
                    i.consume_key(Modifiers::SHIFT, Key::Tab),
                    buffer.is_empty() && i.consume_key(Modifiers::NONE, Key::Backspace),
                )
            });
            if tab {
                view.apply(Some(row.uuid.clone()), tree::indent(&refs, &row.uuid));
            }
            if shift_tab {
                view.apply(Some(row.uuid.clone()), tree::dedent(&refs, &row.uuid));
            }
            if erase {
                view.drafts.remove(&row.uuid);
                let uuid = row.uuid.clone();
                view.write(Some(row.uuid.clone()), move |tw, b| tw.delete(&uuid, b));
            }
        }
        if edit.lost_focus() {
            let text = buffer.trim().to_string();
            if text != row.text && !text.is_empty() {
                let uuid = row.uuid.clone();
                view.write(Some(row.uuid.clone()), move |tw, b| tw.describe(&uuid, &text, b));
            }
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                view.new_row = Some(NewRow {
                    node: section.node.clone(),
                    after: Some(row.uuid.clone()),
                    depth: row.depth,
                    text: String::new(),
                    focus: true,
                });
            }
        }
        if let Some(e) = view.row_errors.get(&row.uuid) {
            ui.label(RichText::new(e).color(c.error));
        }
    });
}

/// Taskwarrior rejects an empty description, so a new row exists only here
/// until it has text; leaving it empty drops it.
fn show_new_row(ui: &mut Ui, view: &mut TasksView, tasks: &[Task]) {
    let Some(new) = view.new_row.as_mut() else { return };
    let mut committed = None;
    let mut dropped = false;
    ui.horizontal(|ui| {
        ui.add_space(new.depth as f32 * INDENT);
        let edit = ui.add(
            TextEdit::singleline(&mut new.text).hint_text("new task").desired_width(f32::INFINITY),
        );
        if new.focus {
            edit.request_focus();
            new.focus = false;
        }
        if edit.lost_focus() {
            let text = new.text.trim().to_string();
            if text.is_empty() { dropped = true } else { committed = Some(text) }
        }
    });
    if dropped {
        view.new_row = None;
    }
    if let Some(text) = committed {
        let new = view.new_row.take().expect("present above");
        let refs: Vec<&Task> = tasks.iter().collect();
        let edits = tree::insert_after(&refs, &new.node, new.after.as_deref(), &text);
        view.apply(None, edits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project(root: &str, name: &str) -> Project {
        Project {
            root: PathBuf::from(root),
            name: name.into(),
            label: None,
            default_branch: None,
            worktrees: Vec::new(),
            expanded: false,
            shell_override: None,
            home: None,
        }
    }

    fn worktree(path: &str, name: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            name: name.into(),
            path: PathBuf::from(path),
            branch: branch.map(Into::into),
            is_main: false,
            prunable: false,
            upstream: None,
        }
    }

    #[test]
    fn home_has_only_global() {
        let s = Scope::for_workspace(None, None);
        assert_eq!((s.repo, s.workspace), (None, None));
    }

    #[test]
    fn a_worktree_names_repo_and_branch() {
        let p = project("C:/src/alacritree", "alacritree");
        let w = worktree("C:/src/wt/feat", "feat", Some("feat/x"));
        let s = Scope::for_workspace(Some(&p), Some(&w));
        assert_eq!(s.repo.as_deref(), Some("alacritree"));
        assert_eq!(s.workspace.as_deref(), Some("alacritree.feat-x"));
    }

    #[test]
    fn a_detached_worktree_uses_its_directory_name() {
        let p = project("C:/src/r", "r");
        let w = worktree("C:/src/wt/review", "review", None);
        assert_eq!(Scope::for_workspace(Some(&p), Some(&w)).workspace.as_deref(), Some("r.review"));
    }

    #[test]
    fn a_non_git_root_has_a_project_section_only() {
        let s = Scope::for_workspace(Some(&project("C:/notes", "notes")), None);
        assert_eq!((s.repo.as_deref(), s.workspace), (Some("notes"), None));
    }

    #[cfg(windows)]
    #[test]
    fn a_distro_project_runs_on_its_distro() {
        let p = project(r"\\wsl.localhost\Ubuntu\home\lev\r", "r");
        assert_eq!(Scope::for_workspace(Some(&p), None).side, Side::Wsl("Ubuntu".into()));
    }

    #[test]
    fn the_filter_keeps_other_repos_out() {
        let s = Scope { side: Side::Native, repo: Some("r".into()), workspace: None };
        assert_eq!(s.filter(), [
            "(project.is:r or project:r. or project.is:global)".to_string(),
            "(status:pending or status:completed)".to_string(),
        ]);
    }

    #[test]
    fn home_filters_to_global() {
        let s = Scope::for_workspace(None, None);
        assert_eq!(s.filter()[0], "(project.is:global)");
    }
}
