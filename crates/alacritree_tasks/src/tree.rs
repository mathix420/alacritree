//! The tab's model: tasks split into scope sections, nested by `parent`,
//! ordered by `order`, folded under collapsed rows, and the edits that
//! inserting, indenting or moving turns into. Free of egui so it can be
//! tested without a frame.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::scope::GLOBAL;
use crate::{Edit, Status, Task};

pub const STRIDE: i64 = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub depth: usize,
    pub text: String,
    pub status: Status,
    pub started: bool,
    /// The task's direct subtasks.
    pub subtasks: Progress,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

/// Which side of the row it is dropped on a moved task lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    Global,
    Project,
    Workspace,
    Session,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub node: String,
    pub kind: SectionKind,
    pub rows: Vec<Row>,
}

/// Agents may add tasks with no `order`; those sort after ordered ones,
/// oldest first, so a new agent task lands at the bottom. Agents also pick
/// their own `order` and `entry` has whole seconds, so ties are common and
/// the id settles them.
fn sort_key(t: &Task) -> (bool, i64, Option<&str>, &str) {
    (t.order.is_none(), t.order.unwrap_or(0), t.entry.as_deref(), &t.id)
}

fn by_order(list: &mut [&Task]) {
    list.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
}

/// A parent outside `tasks` is treated as none, so a task whose parent was
/// deleted or lives in another scope still shows.
fn parent_of<'a>(t: &'a Task, present: &HashSet<&str>) -> Option<&'a str> {
    t.parent.as_deref().filter(|p| present.contains(p) && *p != t.id)
}

fn present<'a>(tasks: &[&'a Task]) -> HashSet<&'a str> {
    tasks.iter().map(|t| t.id.as_str()).collect()
}

pub fn sections(tasks: &[Task], repo: Option<&str>, workspace: Option<&str>) -> Vec<Section> {
    let in_node =
        |node: &str| -> Vec<&Task> { tasks.iter().filter(|t| t.node() == node).collect() };
    let section =
        |node: &str, kind| Section { node: node.to_string(), kind, rows: rows(&in_node(node)) };
    let mut out = vec![section(GLOBAL, SectionKind::Global)];
    out.extend(repo.map(|repo| section(repo, SectionKind::Project)));
    if let Some(workspace) = workspace {
        out.push(section(workspace, SectionKind::Workspace));
        let prefix = format!("{workspace}.");
        let mut sessions: Vec<&str> = tasks
            .iter()
            .map(Task::node)
            .filter(|p| p.starts_with(&prefix))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        // By when the session began rather than when it last changed, so a
        // section does not jump each time an agent ticks off a task.
        let began = |node: &str| {
            tasks.iter().filter(|t| t.node() == node).filter_map(|t| t.entry.as_deref()).min()
        };
        sessions.sort_by_key(|node| (Reverse(began(node)), *node));
        out.extend(sessions.into_iter().map(|node| section(node, SectionKind::Session)));
    }
    out
}

pub fn rows(tasks: &[&Task]) -> Vec<Row> {
    let present = present(tasks);
    let mut children: HashMap<Option<&str>, Vec<&Task>> = HashMap::new();
    for t in tasks {
        children.entry(parent_of(t, &present)).or_default().push(t);
    }
    for list in children.values_mut() {
        by_order(list);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(&children, None, 0, &mut seen, &mut out);
    // Tasks in a parent cycle have no root to be reached from.
    let mut stranded: Vec<&Task> =
        tasks.iter().copied().filter(|t| !seen.contains(t.id.as_str())).collect();
    by_order(&mut stranded);
    for t in stranded {
        if seen.insert(t.id.clone()) {
            out.push(row(t, 0, &children));
            walk(&children, Some(&t.id), 1, &mut seen, &mut out);
        }
    }
    out
}

type Children<'a> = HashMap<Option<&'a str>, Vec<&'a Task>>;

fn walk(
    children: &Children<'_>,
    parent: Option<&str>,
    depth: usize,
    seen: &mut HashSet<String>,
    out: &mut Vec<Row>,
) {
    for t in children.get(&parent).into_iter().flatten() {
        if seen.insert(t.id.clone()) {
            out.push(row(t, depth, children));
            walk(children, Some(&t.id), depth + 1, seen, out);
        }
    }
}

fn row(t: &Task, depth: usize, children: &Children<'_>) -> Row {
    let subtasks =
        children.get(&Some(t.id.as_str())).map_or_else(Progress::default, |list| Progress {
            done: list.iter().filter(|c| c.status == Status::Completed).count(),
            total: list.len(),
        });
    Row {
        id: t.id.clone(),
        depth,
        text: t.description.clone(),
        status: t.status,
        started: t.started,
        subtasks,
    }
}

/// How many rows after `rows[i]` sit below it.
pub fn descendants(rows: &[Row], i: usize) -> usize {
    rows[i + 1..].iter().take_while(|r| r.depth > rows[i].depth).count()
}

/// The rows left showing when each row in `collapsed` hides the ones below
/// it, each with its descendant count.
pub fn fold<'a>(rows: &'a [Row], collapsed: &HashSet<String>) -> Vec<(&'a Row, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let below = descendants(rows, i);
        out.push((&rows[i], below));
        i += 1 + if collapsed.contains(&rows[i].id) { below } else { 0 };
    }
    out
}

/// `rows` without the completed tasks whose whole subtree is completed, so
/// every open task still shows under its parents.
pub fn without_completed(rows: Vec<Row>) -> Vec<Row> {
    let open_below: Vec<bool> = (0..rows.len())
        .map(|i| {
            let below = &rows[i + 1..][..descendants(&rows, i)];
            rows[i].status == Status::Pending || below.iter().any(|r| r.status == Status::Pending)
        })
        .collect();
    rows.into_iter().zip(open_below).filter_map(|(row, keep)| keep.then_some(row)).collect()
}

fn siblings<'a>(tasks: &[&'a Task], parent: Option<&str>) -> Vec<&'a Task> {
    let present = present(tasks);
    let mut list: Vec<&Task> =
        tasks.iter().copied().filter(|t| parent_of(t, &present) == parent).collect();
    by_order(&mut list);
    list
}

/// The `order` for a new sibling placed after `list[index]`, or first when
/// `index` is `None`. When no integer fits between the neighbours, or a
/// sibling has no `order` to place against, the whole set is renumbered at
/// the stride first.
fn slot(list: &[&Task], index: Option<usize>) -> (Vec<Edit>, i64) {
    let ordered: Option<Vec<i64>> = list.iter().map(|t| t.order).collect();
    let gap = ordered.and_then(|orders| {
        let low = index.map_or(0, |i| orders[i]);
        match orders.get(index.map_or(0, |i| i + 1)) {
            None => Some(low + STRIDE),
            Some(&high) if high - low >= 2 => Some(low + (high - low) / 2),
            Some(_) => None,
        }
    });
    match gap {
        Some(order) => (Vec::new(), order),
        None => {
            let renumber = list
                .iter()
                .enumerate()
                .map(|(i, t)| Edit::Reorder { id: t.id.clone(), order: (i as i64 + 1) * STRIDE })
                .collect();
            let low = index.map_or(0, |i| (i as i64 + 1) * STRIDE);
            (renumber, low + STRIDE / 2)
        },
    }
}

pub fn insert_after(
    tasks: &[&Task],
    project: &str,
    after: Option<&str>,
    description: &str,
) -> Vec<Edit> {
    let present = present(tasks);
    let anchor = after.and_then(|u| tasks.iter().copied().find(|t| t.id == u));
    let parent = anchor.and_then(|t| parent_of(t, &present));
    let list = siblings(tasks, parent);
    let index = anchor.and_then(|a| list.iter().position(|t| t.id == a.id));
    let (mut edits, order) = slot(&list, index);
    edits.push(Edit::Add {
        project: project.to_string(),
        description: description.to_string(),
        parent: parent.map(str::to_string),
        order,
    });
    edits
}

pub fn indent(tasks: &[&Task], id: &str) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.id == id) else { return Vec::new() };
    let list = siblings(tasks, parent_of(me, &present));
    let Some(pos) = list.iter().position(|t| t.id == id) else { return Vec::new() };
    let Some(new_parent) = pos.checked_sub(1).map(|i| list[i]) else { return Vec::new() };
    let children = siblings(tasks, Some(&new_parent.id));
    let (mut edits, order) = slot(&children, children.len().checked_sub(1));
    edits.push(Edit::Move { id: id.to_string(), parent: Some(new_parent.id.clone()), order });
    edits
}

pub fn dedent(tasks: &[&Task], id: &str) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.id == id) else { return Vec::new() };
    let Some(parent) =
        parent_of(me, &present).and_then(|p| tasks.iter().copied().find(|t| t.id == p))
    else {
        return Vec::new();
    };
    let grandparent = parent_of(parent, &present);
    let list = siblings(tasks, grandparent);
    let index = list.iter().position(|t| t.id == parent.id);
    let (mut edits, order) = slot(&list, index);
    edits.push(Edit::Move { id: id.to_string(), parent: grandparent.map(str::to_string), order });
    edits
}

pub fn move_up(tasks: &[&Task], id: &str) -> Vec<Edit> {
    step(tasks, id, Landing::Before)
}

pub fn move_down(tasks: &[&Task], id: &str) -> Vec<Edit> {
    step(tasks, id, Landing::After)
}

/// Past the neighbouring sibling on the `toward` side.
fn step(tasks: &[&Task], id: &str, toward: Landing) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.id == id) else { return Vec::new() };
    let group = siblings(tasks, parent_of(me, &present));
    let Some(pos) = group.iter().position(|t| t.id == id) else { return Vec::new() };
    let neighbour = match toward {
        Landing::Before => pos.checked_sub(1),
        Landing::After => Some(pos + 1),
    };
    match neighbour.and_then(|i| group.get(i)) {
        Some(anchor) => move_to(tasks, id, &anchor.id, toward),
        None => Vec::new(),
    }
}

/// Moves `id` beside `anchor`, as a sibling of it. Nothing happens when
/// `anchor` is `id` or lies below it, since the task cannot nest in itself.
pub fn move_to(tasks: &[&Task], id: &str, anchor: &str, landing: Landing) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.id == id) else { return Vec::new() };
    let Some(target) = tasks.iter().copied().find(|t| t.id == anchor) else { return Vec::new() };
    if anchor == id || descendant_ids(tasks, id).iter().any(|d| d == anchor) {
        return Vec::new();
    }
    let parent = parent_of(target, &present);
    let list: Vec<&Task> = siblings(tasks, parent).into_iter().filter(|t| t.id != id).collect();
    let Some(at) = list.iter().position(|t| t.id == anchor) else { return Vec::new() };
    let index = match landing {
        Landing::Before => at.checked_sub(1),
        Landing::After => Some(at),
    };
    let (mut edits, order) = slot(&list, index);
    edits.push(if parent == parent_of(me, &present) {
        Edit::Reorder { id: id.to_string(), order }
    } else {
        Edit::Move { id: id.to_string(), parent: parent.map(str::to_string), order }
    });
    edits
}

/// Applies a `Move` or `Reorder` to `tasks` the way a store would, so a move
/// shows before the store confirms it. Any other edit changes nothing.
pub fn replay(tasks: &mut [Task], edit: &Edit) {
    match edit {
        Edit::Move { id, parent, order } => {
            for t in tasks.iter_mut().filter(|t| &t.id == id) {
                t.parent = parent.clone();
                t.order = Some(*order);
            }
        },
        Edit::Reorder { id, order } => {
            tasks.iter_mut().filter(|t| &t.id == id).for_each(|t| t.order = Some(*order));
        },
        _ => {},
    }
}

/// Every task nested below `id`, at any depth.
pub fn descendant_ids(tasks: &[&Task], id: &str) -> Vec<String> {
    let present = present(tasks);
    let mut out = Vec::new();
    let mut frontier = vec![id.to_string()];
    let mut seen: HashSet<String> = HashSet::from([id.to_string()]);
    while let Some(parent) = frontier.pop() {
        for t in tasks.iter().filter(|t| parent_of(t, &present) == Some(parent.as_str())) {
            if seen.insert(t.id.clone()) {
                out.push(t.id.clone());
                frontier.push(t.id.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, project: &str, parent: Option<&str>, order: Option<i64>) -> Task {
        Task {
            parent: parent.map(Into::into),
            order,
            entry: Some(format!("20260921T00000{}Z", id.len())),
            ..crate::fake::task(id, project)
        }
    }

    fn shape(rows: &[Row]) -> Vec<(&str, usize)> {
        rows.iter().map(|r| (r.id.as_str(), r.depth)).collect()
    }

    fn refs(tasks: &[Task]) -> Vec<&Task> {
        tasks.iter().collect()
    }

    fn applied(tasks: &[Task], edits: &[Edit]) -> Vec<Task> {
        let mut out = tasks.to_vec();
        edits.iter().for_each(|edit| crate::fake::apply(&mut out, edit));
        out
    }

    #[test]
    fn children_follow_their_parent_in_order() {
        let t = [
            task("b", "r", None, Some(2048)),
            task("a", "r", None, Some(1024)),
            task("a2", "r", Some("a"), Some(2048)),
            task("a1", "r", Some("a"), Some(1024)),
        ];
        assert_eq!(shape(&rows(&refs(&t))), [("a", 0), ("a1", 1), ("a2", 1), ("b", 0)]);
    }

    #[test]
    fn an_orphaned_parent_renders_as_root() {
        let t = [task("x", "r", Some("gone"), Some(1024))];
        assert_eq!(shape(&rows(&refs(&t))), [("x", 0)]);
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let t = [task("a", "r", Some("b"), Some(1)), task("b", "r", Some("a"), Some(2))];
        let got = rows(&refs(&t));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn a_collapsed_task_hides_its_whole_subtree_and_nothing_after() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("a1x", "r", Some("a1"), Some(1024)),
            task("a2", "r", Some("a"), Some(2048)),
            task("b", "r", None, Some(2048)),
        ];
        let all = rows(&refs(&t));
        let shown = |collapsed: &[&str]| -> Vec<(&str, usize)> {
            let collapsed = collapsed.iter().map(|id| id.to_string()).collect();
            fold(&all, &collapsed).into_iter().map(|(r, below)| (r.id.as_str(), below)).collect()
        };
        assert_eq!(shown(&["a"]), [("a", 3), ("b", 0)]);
        assert_eq!(shown(&["a1"]), [("a", 3), ("a1", 1), ("a2", 0), ("b", 0)]);
        assert_eq!(shown(&["b"]).len(), 5, "a leaf has nothing to hide");
    }

    #[test]
    fn unordered_tasks_sort_after_ordered_ones() {
        let t = [task("late", "r", None, None), task("first", "r", None, Some(1024))];
        assert_eq!(shape(&rows(&refs(&t))), [("first", 0), ("late", 0)]);
    }

    #[test]
    fn sections_split_by_node() {
        let t = [
            task("g", "global", None, None),
            task("p", "r", None, None),
            task("w", "r.main", None, None),
            task("s2", "r.main.codex-2", None, None),
            task("s1", "r.main.claude-1", None, None),
            task("other", "r.feat", None, None),
        ];
        let got = sections(&t, Some("r"), Some("r.main"));
        let kinds: Vec<(&str, SectionKind)> =
            got.iter().map(|s| (s.node.as_str(), s.kind)).collect();
        assert_eq!(kinds[..3], [
            ("global", SectionKind::Global),
            ("r", SectionKind::Project),
            ("r.main", SectionKind::Workspace)
        ]);
        assert_eq!(kinds.len(), 5, "r.feat belongs to another workspace");
        assert!(kinds[3..].iter().all(|(_, k)| *k == SectionKind::Session));
    }

    #[test]
    fn the_home_tab_shows_only_global() {
        let t = [task("g", "global", None, None), task("p", "r", None, None)];
        let got = sections(&t, None, None);
        assert_eq!(got.iter().map(|s| s.kind).collect::<Vec<_>>(), [SectionKind::Global]);
    }

    #[test]
    fn empty_scopes_still_get_a_section_to_type_into() {
        let got = sections(&[], Some("r"), Some("r.main"));
        let kinds: Vec<SectionKind> = got.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [SectionKind::Global, SectionKind::Project, SectionKind::Workspace]);
    }

    #[test]
    fn insert_takes_the_midpoint_of_the_gap() {
        let t = [task("a", "r", None, Some(1024)), task("b", "r", None, Some(2048))];
        assert_eq!(insert_after(&refs(&t), "r", Some("a"), "new"), [Edit::Add {
            project: "r".into(),
            description: "new".into(),
            parent: None,
            order: 1536
        }]);
    }

    #[test]
    fn insert_into_an_empty_section_starts_at_one_stride() {
        let Some(Edit::Add { order, .. }) = insert_after(&[], "r", None, "x").pop() else {
            panic!()
        };
        assert_eq!(order, STRIDE);
    }

    #[test]
    fn insert_at_the_end_adds_a_stride() {
        let t = [task("a", "r", None, Some(1024))];
        let Some(Edit::Add { order, .. }) = insert_after(&refs(&t), "r", Some("a"), "x").pop()
        else {
            panic!()
        };
        assert_eq!(order, 2048);
    }

    #[test]
    fn a_closed_gap_renumbers_the_siblings_first() {
        let t = [task("a", "r", None, Some(10)), task("b", "r", None, Some(11))];
        let edits = insert_after(&refs(&t), "r", Some("a"), "x");
        assert_eq!(edits[..2], [Edit::Reorder { id: "a".into(), order: 1024 }, Edit::Reorder {
            id: "b".into(),
            order: 2048
        },]);
        let Edit::Add { order, .. } = &edits[2] else { panic!() };
        assert_eq!(*order, 1536);
    }

    #[test]
    fn a_task_added_after_an_unordered_sibling_lands_below_it() {
        let t = [task("a", "r", None, Some(5000)), task("b", "r", None, None)];
        let edits = insert_after(&refs(&t), "r", Some("b"), "new");
        let got = applied(&t, &edits);
        assert_eq!(shape(&rows(&refs(&got))), [("a", 0), ("b", 0), ("new", 0)]);
    }

    #[test]
    fn indent_under_unordered_children_lands_last() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(5000)),
            task("a2", "r", Some("a"), None),
            task("b", "r", None, Some(2048)),
        ];
        let got = applied(&t, &indent(&refs(&t), "b"));
        assert_eq!(shape(&rows(&refs(&got))), [("a", 0), ("a1", 1), ("a2", 1), ("b", 1)]);
    }

    #[test]
    fn insert_below_a_child_stays_a_sibling_of_that_child() {
        let t = [task("a", "r", None, Some(1024)), task("a1", "r", Some("a"), Some(1024))];
        let Some(Edit::Add { parent, .. }) = insert_after(&refs(&t), "r", Some("a1"), "x").pop()
        else {
            panic!()
        };
        assert_eq!(parent.as_deref(), Some("a"));
    }

    #[test]
    fn indent_moves_under_the_previous_sibling_after_its_children() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        assert_eq!(indent(&refs(&t), "b"), [Edit::Move {
            id: "b".into(),
            parent: Some("a".into()),
            order: 2048
        }]);
    }

    #[test]
    fn indent_without_a_previous_sibling_does_nothing() {
        let t = [task("a", "r", None, Some(1024))];
        assert!(indent(&refs(&t), "a").is_empty());
    }

    #[test]
    fn dedent_lands_right_after_the_old_parent() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        assert_eq!(dedent(&refs(&t), "a1"), [Edit::Move {
            id: "a1".into(),
            parent: None,
            order: 1536
        }]);
    }

    #[test]
    fn dedent_at_the_root_does_nothing() {
        let t = [task("a", "r", None, Some(1024))];
        assert!(dedent(&refs(&t), "a").is_empty());
    }

    fn done(mut t: Task) -> Task {
        t.status = Status::Completed;
        t
    }

    #[test]
    fn equal_order_and_entry_fall_back_to_the_id() {
        let t = [task("b", "r", None, Some(1024)), task("a", "r", None, Some(1024))];
        assert_eq!(shape(&rows(&refs(&t))), [("a", 0), ("b", 0)]);
    }

    #[test]
    fn a_completed_task_keeps_its_place() {
        let t = [done(task("a", "r", None, Some(1024))), task("b", "r", None, Some(2048))];
        assert_eq!(shape(&rows(&refs(&t))), [("a", 0), ("b", 0)]);
    }

    #[test]
    fn sessions_keep_their_place_when_a_task_changes() {
        let at = |id, node: &str, entry: &str, modified: &str| Task {
            entry: Some(entry.into()),
            modified: Some(modified.into()),
            ..task(id, node, None, None)
        };
        let t = [
            at("old", "r.main.old", "20260901T000000Z", "20260925T000000Z"),
            at("new", "r.main.new", "20260920T000000Z", "20260920T000000Z"),
        ];
        let got = sections(&t, Some("r"), Some("r.main"));
        let nodes: Vec<&str> = got[3..].iter().map(|s| s.node.as_str()).collect();
        assert_eq!(nodes, ["r.main.new", "r.main.old"], "newest session first");
    }

    #[test]
    fn a_parent_counts_its_done_subtasks() {
        let t = [
            task("a", "r", None, Some(1024)),
            done(task("a1", "r", Some("a"), Some(1024))),
            task("a2", "r", Some("a"), Some(2048)),
        ];
        let got = rows(&refs(&t));
        assert_eq!(got[0].subtasks, Progress { done: 1, total: 2 });
        assert_eq!(got[1].subtasks, Progress::default());
    }

    #[test]
    fn hiding_completed_keeps_a_done_parent_with_open_subtasks() {
        let t = [
            done(task("a", "r", None, Some(1024))),
            task("a1", "r", Some("a"), Some(1024)),
            done(task("b", "r", None, Some(2048))),
            done(task("b1", "r", Some("b"), Some(1024))),
            done(task("c", "r", None, Some(3072))),
        ];
        assert_eq!(shape(&without_completed(rows(&refs(&t)))), [("a", 0), ("a1", 1)]);
    }

    #[test]
    fn move_up_swaps_with_the_previous_sibling() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        let got = applied(&t, &move_up(&refs(&t), "b"));
        assert_eq!(shape(&rows(&refs(&got))), [("b", 0), ("a", 0), ("a1", 1)]);
        assert!(move_up(&refs(&got), "b").is_empty(), "already first");
    }

    #[test]
    fn move_down_swaps_with_the_next_sibling() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("b", "r", None, Some(2048)),
            task("c", "r", None, Some(3072)),
        ];
        let got = applied(&t, &move_down(&refs(&t), "a"));
        assert_eq!(shape(&rows(&refs(&got))), [("b", 0), ("a", 0), ("c", 0)]);
    }

    #[test]
    fn a_completed_task_moves_past_a_pending_one() {
        let t = [task("a", "r", None, Some(1024)), done(task("b", "r", None, Some(2048)))];
        let got = applied(&t, &move_up(&refs(&t), "b"));
        assert_eq!(shape(&rows(&refs(&got))), [("b", 0), ("a", 0)]);
    }

    #[test]
    fn a_drop_on_another_parents_child_moves_under_that_parent() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        let got = applied(&t, &move_to(&refs(&t), "b", "a1", Landing::Before));
        assert_eq!(shape(&rows(&refs(&got))), [("a", 0), ("b", 1), ("a1", 1)]);
    }

    #[test]
    fn a_drop_inside_its_own_subtree_does_nothing() {
        let t = [task("a", "r", None, Some(1024)), task("a1", "r", Some("a"), Some(1024))];
        assert!(move_to(&refs(&t), "a", "a1", Landing::After).is_empty());
        assert!(move_to(&refs(&t), "a", "a", Landing::After).is_empty());
    }

    #[test]
    fn the_subtree_holds_every_descendant() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("a11", "r", Some("a1"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        let mut got = descendant_ids(&refs(&t), "a");
        got.sort();
        assert_eq!(got, ["a1", "a11"]);
    }
}
