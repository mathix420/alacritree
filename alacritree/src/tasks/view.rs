//! The tasks tab: the backend's lists for one workspace, drawn as checklist
//! rows. Each change is a batch of edits on the pool, typed text shows at
//! once, and a reload replaces it with what the store holds. Agents write
//! the same store, so the tab re-lists while visible instead of trusting its
//! own copy.

use alacritree_vcs::Checkout;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{
    Color32, Key, Modifiers, Response, RichText, ScrollArea, Sense, Shape, Stroke, StrokeKind,
    TextEdit, Ui, Vec2, WidgetText, vec2,
};

use alacritree_common::jobs::{self, Job, Priority};
use alacritree_common::side::Side;
use alacritree_common::wsl;
use alacritree_tasks::scope::{GLOBAL, Place, node};
use alacritree_tasks::tree::{self, Row, Section};
use alacritree_tasks::{Edit, Filter, NodeMatch, Status, Task, TaskBackend, TaskError};

use crate::projects::Project;
use crate::tasks::backend::{self, Backend};
use crate::tasks::facts;

const RELOAD_EVERY: Duration = Duration::from_secs(1);
const INDENT: f32 = 16.0;
const CHEVRON: f32 = 12.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scope {
    pub side: Side,
    pub repo: Option<String>,
    pub workspace: Option<String>,
}

impl Scope {
    /// The names the sidebar already has, so the tab opens without waiting
    /// on git. `adopt` replaces them once git has answered.
    pub(crate) fn for_workspace(project: Option<&Project>, worktree: Option<&Checkout>) -> Self {
        let side = match project.map(|p| wsl::classify(&p.root)) {
            Some(wsl::Location::Wsl { distro, .. }) => Side::Wsl(distro),
            _ => Side::Native,
        };
        let workspace = project.zip(worktree).map(|(p, wt)| {
            let branch = wt.head.label().map_or_else(|| wt.name.clone(), str::to_string);
            node(&Place::Workspace { repo: p.name.clone(), branch }, None)
        });
        let repo = project.map(|p| node(&Place::Project { repo: p.name.clone() }, None));
        Self { side, repo, workspace }
    }

    /// Takes the names `task scope` gives the worktree, which follow git's
    /// own view of a detached or shared checkout where the sidebar's labels
    /// do not.
    pub(crate) fn adopt(&mut self, place: &Place) {
        if let Place::Workspace { repo, .. } = place {
            self.repo = Some(node(&Place::Project { repo: repo.clone() }, None));
            self.workspace = Some(node(place, None));
        }
    }

    /// The global list and the repository's whole subtree, since the tab
    /// shows the agent sessions below the workspace too.
    pub(crate) fn filter(&self) -> Filter {
        let repo = self.repo.iter().map(|repo| NodeMatch::Subtree(repo.clone()));
        Filter { nodes: repo.chain([NodeMatch::Exact(GLOBAL.to_string())]).collect() }
    }
}

/// Edits the tab asks for, with the row a failure is shown on. A frame
/// queues them and the next one spawns them, so what a frame decided can be
/// read before the pool runs it.
type QueuedWrite = (Option<String>, Vec<Edit>);

/// A write in flight, with the row its failure is shown on.
type PendingWrite = (Option<String>, Job<Result<(), TaskError>>);

/// A listing in flight, with the count of writes finished when it started.
type PendingReload = (u64, Job<Result<Vec<Task>, TaskError>>);

/// A row being typed that the store has not got yet.
struct NewRow {
    node: String,
    after: Option<String>,
    depth: usize,
    text: String,
    focus: bool,
}

pub(crate) struct TasksView {
    backend: Backend,
    scope: Scope,
    /// The worktree's names as `task scope` reads them from git.
    resolving: Option<Job<Place>>,
    tasks: Vec<Task>,
    load_error: Option<String>,
    /// A failed add has no row to show on; it stays until the next write.
    write_error: Option<String>,
    reload: Option<PendingReload>,
    last_reload: Option<Instant>,
    /// Set when a write finishes, so the export after it is not skipped
    /// while an older one is still running.
    stale: bool,
    writes_finished: u64,
    outbox: Vec<QueuedWrite>,
    writes: Vec<PendingWrite>,
    row_errors: HashMap<String, String>,
    /// Typed text a reload has not confirmed yet, by id.
    drafts: HashMap<String, String>,
    /// What the rows show until a listing started after every write has
    /// landed: toggled statuses, deleted rows, and added rows.
    statuses: HashMap<String, Status>,
    deleted: HashSet<String>,
    added: Vec<NewRow>,
    new_row: Option<NewRow>,
    collapsed: HashSet<String>,
    /// Tasks whose sub-tasks are hidden, by id.
    collapsed_tasks: HashSet<String>,
}

impl TasksView {
    /// Shows `scope` at once, and switches to the names git gives `worktree`
    /// once they are read.
    pub(crate) fn new(
        backend: Backend,
        scope: Scope,
        worktree: Option<PathBuf>,
        backends: Vec<crate::vcs::Vcs>,
    ) -> Self {
        let resolving = worktree.map(|dir| {
            jobs::pool()
                .spawn(Priority::Interactive, move |b| facts::place_for(&dir, &backends, b).1)
        });
        Self {
            backend,
            scope,
            resolving,
            tasks: Vec::new(),
            load_error: None,
            write_error: None,
            reload: None,
            last_reload: None,
            stale: false,
            writes_finished: 0,
            outbox: Vec::new(),
            writes: Vec::new(),
            row_errors: HashMap::new(),
            drafts: HashMap::new(),
            statuses: HashMap::new(),
            deleted: HashSet::new(),
            added: Vec::new(),
            new_row: None,
            collapsed: HashSet::new(),
            collapsed_tasks: HashSet::new(),
        }
    }

    /// The store's sections with the unconfirmed changes laid over them.
    /// An added row has no id yet.
    fn sections(&self) -> Vec<Section> {
        let mut sections = tree::sections(
            &self.tasks,
            self.scope.repo.as_deref(),
            self.scope.workspace.as_deref(),
        );
        for section in &mut sections {
            section.rows.retain(|r| !self.deleted.contains(&r.id));
            for row in &mut section.rows {
                if let Some(status) = self.statuses.get(&row.id) {
                    row.status = *status;
                }
            }
            for add in self.added.iter().filter(|a| a.node == section.node) {
                let rows = &section.rows;
                let anchor =
                    add.after.as_ref().and_then(|id| rows.iter().position(|r| &r.id == id));
                let at = anchor.map_or(rows.len(), |i| i + 1 + tree::descendants(rows, i));
                section.rows.insert(at, Row {
                    id: String::new(),
                    depth: add.depth,
                    text: add.text.clone(),
                    status: Status::Pending,
                    started: false,
                });
            }
        }
        sections
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
        self.tasks.iter().filter(|t| t.node() == node).cloned().collect()
    }

    fn write(&mut self, row: Option<String>, edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        match &row {
            Some(id) => self.row_errors.remove(id),
            None => self.write_error.take(),
        };
        self.outbox.push((row, edits));
    }

    fn write_one(&mut self, row: &str, edit: Edit) {
        self.write(Some(row.to_string()), vec![edit]);
    }

    fn finish_write(&mut self, row: Option<String>, result: Result<(), TaskError>) {
        self.writes_finished += 1;
        self.stale = true;
        let Err(e) = result else { return };
        match row {
            Some(id) => {
                self.drafts.remove(&id);
                self.row_errors.insert(id, e.to_string());
            },
            None => self.write_error = Some(e.to_string()),
        }
    }

    /// What a listing started now is tagged with.
    fn reload_epoch(&self) -> u64 {
        self.writes_finished
    }

    fn finish_reload(&mut self, epoch: u64, result: Result<Vec<Task>, TaskError>) {
        match result {
            Ok(tasks) => {
                self.load_error = None;
                // A draft stays until the store holds its text or the task
                // is gone.
                self.drafts
                    .retain(|id, text| tasks.iter().any(|t| &t.id == id && &t.description != text));
                let settled = epoch == self.writes_finished
                    && self.writes.is_empty()
                    && self.outbox.is_empty();
                if settled {
                    self.statuses.clear();
                    self.deleted.clear();
                    self.added.clear();
                }
                self.tasks = tasks;
            },
            Err(e) => self.load_error = Some(e.to_string()),
        }
    }

    /// Spawns queued writes, drains finished jobs, and reloads after any
    /// write or once a second.
    fn tick(&mut self) {
        if let Some(place) = self.resolving.as_ref().and_then(Job::poll) {
            self.resolving = None;
            self.scope.adopt(&place);
            self.stale = true;
        }
        for (row, edits) in std::mem::take(&mut self.outbox) {
            let (store, side) = (self.backend.clone(), self.scope.side.clone());
            let job = jobs::pool().spawn(Priority::Interactive, move |b| {
                backend::apply_all(&store, &side, &edits, b)
            });
            self.writes.push((row, job));
        }
        let mut finished = Vec::new();
        self.writes.retain(|(row, job)| match job.poll() {
            Some(result) => {
                finished.push((row.clone(), result));
                false
            },
            None => !job.failed(),
        });
        for (row, result) in finished {
            self.finish_write(row, result);
        }
        if let Some((epoch, result)) =
            self.reload.as_ref().and_then(|(epoch, job)| Some((*epoch, job.poll()?)))
        {
            self.reload = None;
            self.finish_reload(epoch, result);
        }
        let due = self.last_reload.is_none_or(|t| t.elapsed() >= RELOAD_EVERY);
        if self.reload.is_none() && (self.stale || due) {
            self.stale = false;
            let (store, scope) = (self.backend.clone(), self.scope.clone());
            let job = jobs::pool()
                .spawn(Priority::Background, move |b| store.list(&scope.side, &scope.filter(), b));
            self.reload = Some((self.reload_epoch(), job));
            self.last_reload = Some(Instant::now());
        }
    }
}

pub(crate) fn show(ui: &mut Ui, view: &mut TasksView, allow_focus: bool, style: Style) -> Response {
    view.tick();
    let response = draw(ui, view, allow_focus, style);
    if view.outbox.is_empty() {
        ui.ctx().request_repaint_after(RELOAD_EVERY);
    } else {
        ui.ctx().request_repaint();
    }
    response
}

fn draw(ui: &mut Ui, view: &mut TasksView, allow_focus: bool, style: Style) -> Response {
    // Registered before the rows: egui gives a click to the last widget
    // registered under the pointer, so every row widget wins over this.
    let background = ui.interact(ui.max_rect(), ui.id().with("tasks-tab"), Sense::click());
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for e in view.load_error.iter().chain(&view.write_error) {
            ui.label(RichText::new(e).color(style.error));
        }
        for section in view.sections() {
            show_section(ui, view, &section, allow_focus, style);
        }
    });
    background
}

/// `[ui.tasks]` with every color resolved against the palette.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Style {
    pub text: Color32,
    pub dim: Color32,
    pub error: Color32,
    pub chevron: Stroke,
    pub chevron_hover: Color32,
    pub section_chevron: Stroke,
    pub hidden_count: Color32,
    pub active_marker: Stroke,
    pub active_background: Color32,
    pub add_button: ButtonStyle,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ButtonStyle {
    pub text: Color32,
    pub hover_text: Color32,
    pub fill: Color32,
    pub hover_fill: Color32,
    pub pressed_fill: Color32,
}

fn show_section(ui: &mut Ui, view: &mut TasksView, section: &Section, allow_focus: bool, c: Style) {
    let open = !view.collapsed.contains(&section.node);
    let done = section.rows.iter().filter(|r| r.status == Status::Completed).count();
    let header = format!("{}  {done}/{}", section.node, section.rows.len());
    if heading(ui, open, &header, c).clicked() {
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
    for (row, below) in tree::fold(&section.rows, &view.collapsed_tasks) {
        show_row(ui, view, section, &tasks, row, below, allow_focus, c);
        let follows = |n: &NewRow| n.node == section.node && n.after.as_deref() == Some(&row.id);
        if view.new_row.as_ref().is_some_and(follows) {
            show_new_row(ui, view, &tasks, c);
        }
    }
    let adding_first = |n: &NewRow| n.node == section.node && n.after.is_none();
    if view.new_row.as_ref().is_some_and(adding_first) {
        show_new_row(ui, view, &tasks, c);
    } else if button(ui, "+ add a task", c.add_button).clicked() {
        // Drawn below the last root row from the next frame on.
        let last_root = section.rows.iter().rev().find(|r| r.depth == 0).map(|r| r.id.clone());
        view.new_row = Some(NewRow {
            node: section.node.clone(),
            after: last_root,
            depth: 0,
            text: String::new(),
            focus: true,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn show_row(
    ui: &mut Ui,
    view: &mut TasksView,
    section: &Section,
    tasks: &[Task],
    row: &Row,
    below: usize,
    allow_focus: bool,
    c: Style,
) {
    if row.id.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(row.depth as f32 * INDENT);
            toggle(ui, None, c.chevron, c.chevron_hover);
            ui.add_enabled(false, egui::Checkbox::without_text(&mut false));
            ui.label(RichText::new(&row.text).color(c.dim));
        });
        return;
    }
    let refs: Vec<&Task> = tasks.iter().collect();
    // Reserved before the row's widgets so the fill paints under them once
    // the row's full rect is known.
    let background = ui.painter().add(Shape::Noop);
    let line = ui.horizontal(|ui| {
        ui.add_space(row.depth as f32 * INDENT);
        let collapsed = view.collapsed_tasks.contains(&row.id);
        if toggle(ui, (below > 0).then_some(!collapsed), c.chevron, c.chevron_hover)
            && !view.collapsed_tasks.remove(&row.id)
        {
            view.collapsed_tasks.insert(row.id.clone());
        }
        let mut checked = row.status == Status::Completed;
        if ui.checkbox(&mut checked, "").changed() {
            let id = row.id.clone();
            let status = if checked { Status::Completed } else { Status::Pending };
            view.statuses.insert(id.clone(), status);
            view.write_one(&row.id, if checked { Edit::Done(id) } else { Edit::Undone(id) });
        }
        if row.started {
            let (rect, _) = ui.allocate_exact_size(glyph_size(ui), Sense::hover());
            paint_chevron(ui, rect.center(), false, c.active_marker);
        }
        if collapsed && below > 0 {
            ui.label(RichText::new(format!("+{below}")).color(c.hidden_count));
        }
        let mut buffer = view.drafts.get(&row.id).cloned().unwrap_or_else(|| row.text.clone());
        // Backspace on a row that is already empty deletes it. TextEdit reads
        // key events without consuming them, so the press that empties the
        // row is still in the queue after it.
        let was_empty = buffer.is_empty();
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
            view.drafts.insert(row.id.clone(), buffer.clone());
        }
        edit.context_menu(|ui| {
            for (label, start) in [("Start", true), ("Stop", false)] {
                if ui.button(label).clicked() {
                    let id = row.id.clone();
                    view.write_one(&row.id, if start { Edit::Start(id) } else { Edit::Stop(id) });
                    ui.close_menu();
                }
            }
        });
        if edit.has_focus() {
            let (tab, shift_tab, erase) = ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::Tab),
                    i.consume_key(Modifiers::SHIFT, Key::Tab),
                    was_empty && i.consume_key(Modifiers::NONE, Key::Backspace),
                )
            });
            if tab {
                let edits = tree::indent(&refs, &row.id);
                // A row indented under a collapsed task would vanish mid-edit.
                for edit in &edits {
                    if let Edit::Move { parent: Some(parent), .. } = edit {
                        view.collapsed_tasks.remove(parent);
                    }
                }
                view.write(Some(row.id.clone()), edits);
            }
            if shift_tab {
                view.write(Some(row.id.clone()), tree::dedent(&refs, &row.id));
            }
            if erase {
                view.drafts.remove(&row.id);
                view.deleted.insert(row.id.clone());
                view.write_one(&row.id, Edit::Delete(row.id.clone()));
            }
        }
        if edit.lost_focus() {
            let text = buffer.trim().to_string();
            if text != row.text && !text.is_empty() {
                view.write_one(&row.id, Edit::Describe { id: row.id.clone(), description: text });
            } else {
                view.drafts.remove(&row.id);
            }
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                view.new_row = Some(NewRow {
                    node: section.node.clone(),
                    after: Some(row.id.clone()),
                    depth: row.depth,
                    text: String::new(),
                    focus: true,
                });
            }
        }
    });
    if row.started {
        let radius = ui.visuals().widgets.inactive.corner_radius;
        let fill = Shape::rect_filled(line.response.rect, radius, c.active_background);
        ui.painter().set(background, fill);
    }
    // Below the row, since the text beside it takes the full width.
    if let Some(e) = view.row_errors.get(&row.id) {
        ui.horizontal(|ui| {
            ui.add_space((row.depth + 1) as f32 * INDENT);
            ui.add(egui::Label::new(RichText::new(e).color(c.error)).wrap());
        });
    }
}

/// A section heading as one click target: the chevron, the gap after it and
/// the label, all lit together under the pointer.
fn heading(ui: &mut Ui, open: bool, label: &str, c: Style) -> Response {
    let galley = WidgetText::from(RichText::new(label).color(c.text).strong()).into_galley(
        ui,
        None,
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    let square = ui.spacing().interact_size.y;
    let (gap, pad) = (ui.spacing().item_spacing.x, ui.spacing().button_padding.x);
    let size = vec2(square + gap + galley.size().x + pad, square.max(galley.size().y));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    if hovered {
        let fill = ui.visuals().widgets.hovered;
        ui.painter().rect_filled(rect, fill.corner_radius, fill.weak_bg_fill);
    }
    let chevron = egui::pos2(rect.left() + square / 2.0, rect.center().y);
    let color = if hovered { c.chevron_hover } else { c.section_chevron.color };
    paint_chevron(ui, chevron, open, Stroke { color, ..c.section_chevron });
    let text = egui::pos2(rect.left() + square + gap, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(text, galley, c.text);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A row-tall square that folds a section or a task's sub-tasks, drawing
/// only a chevron that points down while `open`. `None` leaves the square
/// blank, so checkboxes line up at every depth.
fn toggle(ui: &mut Ui, open: Option<bool>, stroke: Stroke, hover: Color32) -> bool {
    let sense = if open.is_some() { Sense::click() } else { Sense::hover() };
    let size = Vec2::splat(ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let Some(open) = open else { return false };
    let color = if response.hovered() { hover } else { stroke.color };
    paint_chevron(ui, rect.center(), open, Stroke { color, ..stroke });
    response.clicked()
}

fn glyph_size(ui: &Ui) -> Vec2 {
    Vec2::new(CHEVRON, ui.spacing().interact_size.y)
}

/// A chevron centred on `center`, pointing down when `open` and right
/// otherwise.
fn paint_chevron(ui: &Ui, center: egui::Pos2, open: bool, stroke: Stroke) {
    let r = CHEVRON / 4.0;
    let points = if open {
        vec![center + vec2(-r, -r / 2.0), center + vec2(0.0, r / 2.0), center + vec2(r, -r / 2.0)]
    } else {
        vec![center + vec2(-r / 2.0, -r), center + vec2(r / 2.0, 0.0), center + vec2(-r / 2.0, r)]
    };
    ui.painter().add(Shape::line(points, stroke));
}

/// A filled button whose fill and label change under the pointer and
/// again while pressed, outlined in the label color while either holds.
fn button(ui: &mut Ui, label: &str, b: ButtonStyle) -> Response {
    let galley =
        WidgetText::from(label).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Button);
    let padding = ui.spacing().button_padding;
    let (rect, response) = ui.allocate_exact_size(galley.size() + 2.0 * padding, Sense::click());
    let (fill, text) = if response.is_pointer_button_down_on() {
        (b.pressed_fill, b.hover_text)
    } else if response.hovered() {
        (b.hover_fill, b.hover_text)
    } else {
        (b.fill, b.text)
    };
    let radius = ui.visuals().widgets.inactive.corner_radius;
    ui.painter().rect_filled(rect, radius, fill);
    if response.hovered() {
        ui.painter().rect_stroke(rect, radius, Stroke::new(1.0_f32, text), StrokeKind::Inside);
    }
    ui.painter().galley(rect.min + padding, galley, text);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A store may reject an empty description, so a new row exists only here
/// until it has text; leaving it empty drops it.
fn show_new_row(ui: &mut Ui, view: &mut TasksView, tasks: &[Task], c: Style) {
    let Some(new) = view.new_row.as_mut() else { return };
    let mut committed = None;
    let mut dropped = false;
    ui.horizontal(|ui| {
        ui.add_space(new.depth as f32 * INDENT);
        toggle(ui, None, c.chevron, c.chevron_hover);
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
        view.write(None, edits);
        view.added.push(NewRow { text, ..new });
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
            vcs: None,
            trunk: None,
            checkouts: Vec::new(),
            expanded: false,
            shell_override: None,
            home: None,
        }
    }

    fn worktree(path: &str, name: &str, branch: Option<&str>) -> Checkout {
        Checkout {
            name: name.into(),
            path: PathBuf::from(path),
            head: alacritree_vcs::Head { name: branch.map(Into::into), ..Default::default() },
            is_main: false,
            gone: false,
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
    fn a_detached_checkout_names_the_node_task_scope_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = alacritree_git::test_support::init_repo(&dir.path().join("review"));
        alacritree_git::test_support::detach(&root);

        let project = jobs::on_this_thread(|b| {
            Project::discover(root.clone(), &crate::vcs::backends(&Default::default()), false, b)
        })
        .project;
        let worktree = &project.checkouts[0];
        let backends = crate::vcs::backends(&Default::default());
        let (_, place) =
            jobs::on_this_thread(|b| crate::tasks::facts::place_for(&worktree.path, &backends, b));
        let mut scope = Scope::for_workspace(Some(&project), Some(worktree));
        scope.adopt(&place);
        assert_eq!(scope.workspace.as_deref(), Some("review.review"));
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
    fn a_repo_lists_its_subtree_and_the_global_list() {
        let s = Scope { side: Side::Native, repo: Some("r".into()), workspace: None };
        assert_eq!(s.filter().nodes, [
            NodeMatch::Subtree("r".into()),
            NodeMatch::Exact(GLOBAL.into())
        ]);
    }

    #[test]
    fn home_filters_to_global() {
        let s = Scope::for_workspace(None, None);
        assert_eq!(s.filter().nodes, [NodeMatch::Exact(GLOBAL.into())]);
    }

    use egui::epaint::ClippedShape;
    use egui::{CentralPanel, Event, PointerButton, Pos2, RawInput, Rect, Shape, Vec2};

    fn test_style() -> Style {
        Style {
            text: Color32::WHITE,
            dim: Color32::GRAY,
            error: Color32::RED,
            chevron: Stroke::new(1.5_f32, Color32::GRAY),
            chevron_hover: Color32::WHITE,
            section_chevron: Stroke::new(2.5_f32, Color32::WHITE),
            hidden_count: Color32::GRAY,
            active_marker: Stroke::new(2.5_f32, Color32::WHITE),
            active_background: Color32::from_rgb(40, 40, 60),
            add_button: ButtonStyle {
                text: Color32::LIGHT_BLUE,
                hover_text: Color32::WHITE,
                fill: Color32::DARK_BLUE,
                hover_fill: Color32::BLUE,
                pressed_fill: Color32::BLUE,
            },
        }
    }

    fn pending(id: &str, text: &str) -> Task {
        Task {
            description: text.into(),
            order: Some(1024),
            ..alacritree_tasks::fake::task(id, GLOBAL)
        }
    }

    /// The tab drawn frame by frame with real egui input and no backend:
    /// writes collect in `ops` instead of reaching the pool.
    struct Harness {
        ctx: egui::Context,
        view: TasksView,
        ops: Vec<QueuedWrite>,
        /// Painted text and where it shows, clipped to what is on screen.
        texts: Vec<(String, Rect)>,
        /// Filled rectangles and their color.
        fills: Vec<(Color32, Rect)>,
        /// The bounds of each stroked path, such as a chevron.
        paths: Vec<Rect>,
        background_clicked: bool,
        style: Style,
    }

    fn collect_fills(shape: &Shape, fills: &mut Vec<(Color32, Rect)>, paths: &mut Vec<Rect>) {
        match shape {
            Shape::Rect(r) => fills.push((r.fill, r.rect)),
            Shape::Path(p) => paths.push(Rect::from_points(&p.points)),
            Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_fills(s, fills, paths)),
            _ => {},
        }
    }

    fn collect_texts(shape: &Shape, clip: Rect, out: &mut Vec<(String, Rect)>) {
        match shape {
            Shape::Text(t) => {
                let rect = t.galley.rect.translate(t.pos.to_vec2()).intersect(clip);
                if rect.is_positive() {
                    out.push((t.galley.text().to_string(), rect));
                }
            },
            Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_texts(s, clip, out)),
            _ => {},
        }
    }

    impl Harness {
        fn new(tasks: Vec<Task>) -> Self {
            let ctx = egui::Context::default();
            let scope = Scope::for_workspace(None, None);
            let mut view =
                TasksView::new(Backend::from_config(&Default::default()), scope, None, Vec::new());
            view.tasks = tasks;
            let (ops, texts, fills, paths) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut h = Self {
                ctx,
                view,
                ops,
                texts,
                fills,
                paths,
                background_clicked: false,
                style: test_style(),
            };
            h.frame(Vec::new());
            h
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let view = &mut self.view;
            let mut clicked = false;
            let style = self.style;
            let out = self.ctx.run(input, |ctx| {
                CentralPanel::default().show(ctx, |ui| {
                    clicked = draw(ui, view, true, style).clicked();
                });
            });
            self.background_clicked = clicked;
            self.ops.append(&mut self.view.outbox);
            self.texts.clear();
            self.fills.clear();
            self.paths.clear();
            let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
            for ClippedShape { clip_rect, shape } in &out.shapes {
                collect_texts(shape, clip_rect.intersect(screen), &mut self.texts);
                collect_fills(shape, &mut self.fills, &mut self.paths);
            }
        }

        fn visible(&self, text: &str) -> Option<Rect> {
            self.texts.iter().find(|(t, _)| t == text).map(|(_, r)| *r)
        }

        fn text(&self, text: &str) -> Rect {
            self.visible(text).unwrap_or_else(|| panic!("{text:?} not on screen: {:?}", self.texts))
        }

        /// The checkbox drawn just left of the row showing `text`.
        fn checkbox(&self, text: &str) -> Pos2 {
            let rect = self.text(text);
            Pos2::new(rect.left() - 18.0, rect.center().y)
        }

        /// The centre of the chevron painted on the row showing `text`.
        fn painted_chevron(&self, text: &str) -> Pos2 {
            let row = self.text(text);
            let on_row =
                |r: &&Rect| r.right() < row.left() && (r.center().y - row.center().y).abs() < 4.0;
            let leftmost =
                self.paths.iter().filter(on_row).min_by(|a, b| a.left().total_cmp(&b.left()));
            leftmost.unwrap_or_else(|| panic!("no chevron on {text:?}")).center()
        }

        /// The sub-task toggle left of the checkbox on the row showing `text`.
        fn chevron(&self, text: &str) -> Pos2 {
            self.checkbox(text) - vec2(22.0, 0.0)
        }

        fn click(&mut self, pos: Pos2) {
            let button = |pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(pos), button(true)]);
            self.frame(vec![button(false)]);
            self.frame(Vec::new());
        }

        fn key(&mut self, key: Key) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }]);
            self.frame(Vec::new());
        }

        /// Focuses the row showing `text`, with the cursor at its end.
        fn edit_end(&mut self, text: &str) {
            let rect = self.text(text);
            self.click(Pos2::new(rect.right() + 200.0, rect.center().y));
        }

        fn deleted(&self) -> bool {
            self.ops.iter().flat_map(|(_, edits)| edits).any(|e| matches!(e, Edit::Delete(_)))
        }
    }

    #[test]
    fn clicking_a_checkbox_marks_the_task_done_at_once() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        h.click(h.checkbox("one"));
        assert_eq!(h.ops, [(Some("a".to_string()), vec![Edit::Done("a".into())])]);
        assert_eq!(h.view.plain_lines(), ["## global", "- [x] one"]);
    }

    #[test]
    fn a_toggle_holds_until_a_reload_that_follows_the_write() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        h.click(h.checkbox("one"));
        let before_the_write = h.view.reload_epoch();
        h.view.finish_write(Some("a".into()), Ok(()));
        h.view.finish_reload(before_the_write, Ok(vec![pending("a", "one")]));
        assert_eq!(h.view.plain_lines()[1], "- [x] one", "a reload older than the write");
        let after_the_write = h.view.reload_epoch();
        h.view.finish_reload(after_the_write, Ok(vec![pending("a", "one")]));
        assert_eq!(h.view.plain_lines()[1], "- [ ] one", "the store has the last word");
    }

    #[test]
    fn clicking_empty_space_reaches_the_pane() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        let pos = Pos2::new(400.0, 550.0);
        h.frame(vec![Event::PointerMoved(pos), Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }]);
        h.frame(vec![Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }]);
        assert!(h.background_clicked);
    }

    #[test]
    fn backspace_on_the_last_character_keeps_the_task() {
        let mut h = Harness::new(vec![pending("a", "x")]);
        h.edit_end("x");
        h.key(Key::Backspace);
        assert_eq!(h.view.drafts.get("a").map(String::as_str), Some(""));
        assert!(!h.deleted());
    }

    #[test]
    fn backspace_on_an_empty_row_deletes_it() {
        let mut h = Harness::new(vec![pending("a", "x")]);
        h.edit_end("x");
        h.key(Key::Backspace);
        h.key(Key::Backspace);
        assert!(h.deleted());
        assert_eq!(h.view.plain_lines(), ["## global"], "gone before the store answers");
    }

    #[test]
    fn a_cleared_row_left_behind_shows_its_text_again() {
        let mut h = Harness::new(vec![pending("a", "x")]);
        h.edit_end("x");
        h.key(Key::Backspace);
        h.click(Pos2::new(400.0, 550.0));
        assert!(h.view.drafts.is_empty());
        assert!(h.ops.is_empty(), "{:?}", h.ops);
    }

    #[test]
    fn a_row_error_is_drawn_on_screen() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        h.view.row_errors.insert("a".into(), "task failed: gone".into());
        h.frame(Vec::new());
        let rect = h.text("task failed: gone");
        assert!(rect.left() < 800.0, "{rect:?}");
    }

    #[test]
    fn a_failed_add_outlives_the_reload_after_it() {
        let mut h = Harness::new(Vec::new());
        let refused = TaskError::Failed { program: "task".into(), stderr: "no".into() };
        h.view.finish_write(None, Err(refused));
        let epoch = h.view.reload_epoch();
        h.view.finish_reload(epoch, Ok(Vec::new()));
        h.frame(Vec::new());
        h.text("task failed: no");
    }

    #[test]
    fn a_new_task_shows_until_the_reload_lands() {
        let mut h = Harness::new(Vec::new());
        let rect = h.text("+ add a task");
        h.click(rect.center());
        h.frame(vec![Event::Text("milk".into())]);
        h.key(Key::Enter);
        assert!(matches!(h.ops.as_slice(), [(None, edits)] if edits.len() == 1), "{:?}", h.ops);

        assert_eq!(h.view.plain_lines(), ["## global", "- [ ] milk"]);
    }

    fn child(id: &str, parent: &str, text: &str) -> Task {
        Task { parent: Some(parent.into()), ..pending(id, text) }
    }

    #[test]
    fn collapsing_a_task_hides_its_subtasks_until_expanded() {
        let mut h = Harness::new(vec![
            pending("a", "parent"),
            child("b", "a", "child"),
            child("c", "b", "grandchild"),
        ]);
        let toggle = h.chevron("parent");
        h.click(toggle);
        assert_eq!(h.visible("child"), None);
        assert_eq!(h.visible("grandchild"), None);
        h.text("+2");
        assert_eq!(h.view.plain_lines().len(), 4, "agents still read every task");

        h.click(toggle);
        h.text("child");
        h.text("grandchild");
        assert_eq!(h.visible("+2"), None);
    }

    #[test]
    fn indenting_under_a_collapsed_task_expands_it() {
        let mut h = Harness::new(vec![pending("a", "parent"), child("a1", "a", "child"), Task {
            order: Some(2048),
            ..pending("b", "next")
        }]);
        h.click(h.chevron("parent"));
        h.edit_end("next");
        h.key(Key::Tab);
        h.text("child");
    }

    #[test]
    fn a_leaf_lines_up_with_a_parent_and_has_no_toggle() {
        let mut h = Harness::new(vec![pending("a", "parent"), child("a1", "a", "child"), Task {
            order: Some(2048),
            ..pending("b", "alone")
        }]);
        assert_eq!(h.text("parent").left(), h.text("alone").left());
        h.click(h.chevron("alone"));
        assert!(h.view.collapsed_tasks.is_empty());
    }

    #[test]
    fn a_started_task_sits_on_the_active_background() {
        let mut idle = pending("b", "idle");
        idle.order = Some(2048);
        let h = Harness::new(vec![Task { started: true, ..pending("a", "busy") }, idle]);
        let filled = |text: &str| {
            let at = h.text(text).center();
            h.fills.iter().any(|(c, r)| *c == h.style.active_background && r.contains(at))
        };
        assert!(filled("busy"));
        assert!(!filled("idle"));
    }

    #[test]
    fn the_toggle_answers_across_a_square_the_row_tall() {
        let mut h = Harness::new(vec![pending("a", "parent"), child("a1", "a", "child")]);
        // egui otherwise counts a click near a widget as on it, which would
        // hide how far the toggle's own rect reaches.
        h.ctx.style_mut(|s| s.interaction.interact_radius = 0.0);
        let reach = h.ctx.style().spacing.interact_size.y / 2.0 - 1.0;
        h.click(h.painted_chevron("parent") - vec2(reach, reach));
        assert_eq!(h.visible("child"), None);
    }

    #[test]
    fn a_section_folds_from_its_chevron_and_its_label() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        h.click(h.painted_chevron("global  0/1"));
        assert_eq!(h.visible("one"), None);
        h.click(h.text("global  0/1").center());
        h.text("one");
    }

    #[test]
    fn a_section_heading_highlights_and_folds_as_one_target() {
        let mut h = Harness::new(vec![pending("a", "one")]);
        let (chevron, label) = (h.painted_chevron("global  0/1"), h.text("global  0/1"));
        let gap = Pos2::new((chevron.x + label.left()) / 2.0, label.center().y);
        h.frame(vec![Event::PointerMoved(gap)]);
        let hover = h.ctx.style().visuals.widgets.hovered.weak_bg_fill;
        let lit = |at: Pos2| h.fills.iter().any(|(c, r)| *c == hover && r.contains(at));
        assert!(lit(chevron) && lit(gap) && lit(label.center()));

        h.click(gap);
        assert_eq!(h.visible("one"), None);
    }
}
