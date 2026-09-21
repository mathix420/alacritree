//! The tab's model: tasks split into scope sections, nested by `subof`,
//! ordered by `order`, and the edits that inserting or indenting turns into.
//! Free of egui so it can be tested without a frame.

use std::collections::{HashMap, HashSet};

use crate::tasks::scope::GLOBAL;
use crate::tasks::taskwarrior::{Status, Task};

pub(crate) const STRIDE: i64 = 1024;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub uuid: String,
    pub depth: usize,
    pub text: String,
    pub status: Status,
    pub started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionKind {
    Global,
    Project,
    Workspace,
    Session,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Section {
    pub node: String,
    pub kind: SectionKind,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    Add { project: String, description: String, subof: Option<String>, order: i64 },
    Modify { uuid: String, mods: Vec<String> },
}

fn project(t: &Task) -> &str {
    t.project.as_deref().unwrap_or(GLOBAL)
}

/// Agents may add tasks with no `order`; those sort after ordered ones,
/// oldest first, so a new agent task lands at the bottom.
fn sort_key(t: &Task) -> (bool, i64, String) {
    (t.order.is_none(), t.order.unwrap_or(0), t.entry.clone().unwrap_or_default())
}

/// A `subof` naming a task outside `tasks` is treated as none, so a task
/// whose parent was deleted or lives in another scope still shows.
fn parent_of<'a>(t: &'a Task, present: &HashSet<&str>) -> Option<&'a str> {
    t.subof.as_deref().filter(|p| present.contains(p) && *p != t.uuid)
}

fn present<'a>(tasks: &[&'a Task]) -> HashSet<&'a str> {
    tasks.iter().map(|t| t.uuid.as_str()).collect()
}

pub(crate) fn sections(
    tasks: &[Task],
    repo: Option<&str>,
    workspace: Option<&str>,
) -> Vec<Section> {
    let in_node =
        |node: &str| -> Vec<&Task> { tasks.iter().filter(|t| project(t) == node).collect() };
    let section =
        |node: &str, kind| Section { node: node.to_string(), kind, rows: rows(&in_node(node)) };
    let mut out = vec![section(GLOBAL, SectionKind::Global)];
    out.extend(repo.map(|repo| section(repo, SectionKind::Project)));
    if let Some(workspace) = workspace {
        out.push(section(workspace, SectionKind::Workspace));
        let prefix = format!("{workspace}.");
        let mut sessions: Vec<&str> = tasks
            .iter()
            .map(project)
            .filter(|p| p.starts_with(&prefix))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let latest = |node: &str| {
            in_node(node).iter().filter_map(|t| t.modified.clone()).max().unwrap_or_default()
        };
        sessions.sort_by_key(|node| (std::cmp::Reverse(latest(node)), node.to_string()));
        out.extend(sessions.into_iter().map(|node| section(node, SectionKind::Session)));
    }
    out
}

pub(crate) fn rows(tasks: &[&Task]) -> Vec<Row> {
    let present = present(tasks);
    let mut children: HashMap<Option<&str>, Vec<&Task>> = HashMap::new();
    for t in tasks {
        children.entry(parent_of(t, &present)).or_default().push(t);
    }
    for list in children.values_mut() {
        list.sort_by_key(|t| sort_key(t));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(&children, None, 0, &mut seen, &mut out);
    // Tasks in a `subof` cycle have no root to be reached from.
    let mut stranded: Vec<&Task> =
        tasks.iter().copied().filter(|t| !seen.contains(t.uuid.as_str())).collect();
    stranded.sort_by_key(|t| sort_key(t));
    for t in stranded {
        if seen.insert(t.uuid.clone()) {
            out.push(row(t, 0));
            walk(&children, Some(&t.uuid), 1, &mut seen, &mut out);
        }
    }
    out
}

fn walk(
    children: &HashMap<Option<&str>, Vec<&Task>>,
    parent: Option<&str>,
    depth: usize,
    seen: &mut HashSet<String>,
    out: &mut Vec<Row>,
) {
    for t in children.get(&parent).into_iter().flatten() {
        if seen.insert(t.uuid.clone()) {
            out.push(row(t, depth));
            walk(children, Some(&t.uuid), depth + 1, seen, out);
        }
    }
}

fn row(t: &Task, depth: usize) -> Row {
    Row {
        uuid: t.uuid.clone(),
        depth,
        text: t.description.clone(),
        status: t.status,
        started: t.start.is_some(),
    }
}

fn siblings<'a>(tasks: &[&'a Task], parent: Option<&str>) -> Vec<&'a Task> {
    let present = present(tasks);
    let mut list: Vec<&Task> =
        tasks.iter().copied().filter(|t| parent_of(t, &present) == parent).collect();
    list.sort_by_key(|t| sort_key(t));
    list
}

fn order_mod(order: i64) -> String {
    format!("order:{order}")
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
                .map(|(i, t)| Edit::Modify {
                    uuid: t.uuid.clone(),
                    mods: vec![order_mod((i as i64 + 1) * STRIDE)],
                })
                .collect();
            let low = index.map_or(0, |i| (i as i64 + 1) * STRIDE);
            (renumber, low + STRIDE / 2)
        },
    }
}

pub(crate) fn insert_after(
    tasks: &[&Task],
    project: &str,
    after: Option<&str>,
    description: &str,
) -> Vec<Edit> {
    let present = present(tasks);
    let anchor = after.and_then(|u| tasks.iter().copied().find(|t| t.uuid == u));
    let parent = anchor.and_then(|t| parent_of(t, &present));
    let list = siblings(tasks, parent);
    let index = anchor.and_then(|a| list.iter().position(|t| t.uuid == a.uuid));
    let (mut edits, order) = slot(&list, index);
    edits.push(Edit::Add {
        project: project.to_string(),
        description: description.to_string(),
        subof: parent.map(str::to_string),
        order,
    });
    edits
}

pub(crate) fn indent(tasks: &[&Task], uuid: &str) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.uuid == uuid) else { return Vec::new() };
    let list = siblings(tasks, parent_of(me, &present));
    let Some(pos) = list.iter().position(|t| t.uuid == uuid) else { return Vec::new() };
    let Some(new_parent) = pos.checked_sub(1).map(|i| list[i]) else { return Vec::new() };
    let children = siblings(tasks, Some(&new_parent.uuid));
    let (mut edits, order) = slot(&children, children.len().checked_sub(1));
    edits.push(Edit::Modify {
        uuid: uuid.to_string(),
        mods: vec![format!("subof:{}", new_parent.uuid), order_mod(order)],
    });
    edits
}

pub(crate) fn dedent(tasks: &[&Task], uuid: &str) -> Vec<Edit> {
    let present = present(tasks);
    let Some(me) = tasks.iter().copied().find(|t| t.uuid == uuid) else { return Vec::new() };
    let Some(parent) =
        parent_of(me, &present).and_then(|p| tasks.iter().copied().find(|t| t.uuid == p))
    else {
        return Vec::new();
    };
    let grandparent = parent_of(parent, &present);
    let list = siblings(tasks, grandparent);
    let index = list.iter().position(|t| t.uuid == parent.uuid);
    let (mut edits, order) = slot(&list, index);
    edits.push(Edit::Modify {
        uuid: uuid.to_string(),
        mods: vec![format!("subof:{}", grandparent.unwrap_or("")), order_mod(order)],
    });
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(uuid: &str, project: &str, subof: Option<&str>, order: Option<i64>) -> Task {
        Task {
            uuid: uuid.into(),
            description: uuid.into(),
            status: Status::Pending,
            start: None,
            subof: subof.map(Into::into),
            order,
            project: Some(project.into()),
            entry: Some(format!("20260921T00000{}Z", uuid.len())),
            modified: None,
        }
    }

    fn shape(rows: &[Row]) -> Vec<(&str, usize)> {
        rows.iter().map(|r| (r.uuid.as_str(), r.depth)).collect()
    }

    fn refs(tasks: &[Task]) -> Vec<&Task> {
        tasks.iter().collect()
    }

    /// `tasks` after taskwarrior applies `edits`, a new task taking its
    /// description as its uuid.
    fn applied(tasks: &[Task], edits: &[Edit]) -> Vec<Task> {
        let mut out = tasks.to_vec();
        for edit in edits {
            match edit {
                Edit::Add { project, description, subof, order } => {
                    out.push(task(description, project, subof.as_deref(), Some(*order)));
                },
                Edit::Modify { uuid, mods } => {
                    let t = out.iter_mut().find(|t| &t.uuid == uuid).unwrap();
                    for m in mods {
                        match m.split_once(':').unwrap() {
                            ("order", n) => t.order = Some(n.parse().unwrap()),
                            ("subof", "") => t.subof = None,
                            ("subof", p) => t.subof = Some(p.into()),
                            other => panic!("{other:?}"),
                        }
                    }
                },
            }
        }
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
    fn orphan_subof_renders_as_root() {
        let t = [task("x", "r", Some("gone"), Some(1024))];
        assert_eq!(shape(&rows(&refs(&t))), [("x", 0)]);
    }

    #[test]
    fn subof_cycle_terminates() {
        let t = [task("a", "r", Some("b"), Some(1)), task("b", "r", Some("a"), Some(2))];
        let got = rows(&refs(&t));
        assert_eq!(got.len(), 2);
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
            subof: None,
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
        assert_eq!(edits[..2], [
            Edit::Modify { uuid: "a".into(), mods: vec!["order:1024".into()] },
            Edit::Modify { uuid: "b".into(), mods: vec!["order:2048".into()] },
        ]);
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
        let Some(Edit::Add { subof, .. }) = insert_after(&refs(&t), "r", Some("a1"), "x").pop()
        else {
            panic!()
        };
        assert_eq!(subof.as_deref(), Some("a"));
    }

    #[test]
    fn indent_moves_under_the_previous_sibling_after_its_children() {
        let t = [
            task("a", "r", None, Some(1024)),
            task("a1", "r", Some("a"), Some(1024)),
            task("b", "r", None, Some(2048)),
        ];
        assert_eq!(indent(&refs(&t), "b"), [Edit::Modify {
            uuid: "b".into(),
            mods: vec!["subof:a".into(), "order:2048".into()]
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
        assert_eq!(dedent(&refs(&t), "a1"), [Edit::Modify {
            uuid: "a1".into(),
            mods: vec!["subof:".into(), "order:1536".into()]
        }]);
    }

    #[test]
    fn dedent_at_the_root_does_nothing() {
        let t = [task("a", "r", None, Some(1024))];
        assert!(dedent(&refs(&t), "a").is_empty());
    }
}
