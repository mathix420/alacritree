//! A store held in memory that records every edit, for tests of code that
//! drives a backend. Clones share one store, so a test keeps a clone to read
//! after handing the backend away.

use std::sync::{Arc, Mutex, MutexGuard};

use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;

use crate::{Edit, Filter, Status, Task, TaskBackend, TaskError};

#[derive(Debug, Clone, Default)]
pub struct FakeBackend {
    store: Arc<Mutex<Store>>,
}

#[derive(Debug, Default)]
struct Store {
    tasks: Vec<Task>,
    edits: Vec<Edit>,
    refusing: bool,
}

impl FakeBackend {
    pub fn with_tasks(tasks: Vec<Task>) -> Self {
        let backend = Self::default();
        backend.store().tasks = tasks;
        backend
    }

    /// Fails every edit, as a store that is down would.
    pub fn refusing(self) -> Self {
        self.store().refusing = true;
        self
    }

    pub fn tasks(&self) -> Vec<Task> {
        self.store().tasks.clone()
    }

    /// Every edit asked for so far, refused ones included, in order.
    pub fn edits(&self) -> Vec<Edit> {
        self.store().edits.clone()
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TaskBackend for FakeBackend {
    fn list(&self, _: &Side, filter: &Filter, _: &Blocking) -> Result<Vec<Task>, TaskError> {
        Ok(self.store().tasks.iter().filter(|t| filter.matches(t)).cloned().collect())
    }

    fn apply(&self, _: &Side, edit: &Edit, _: &Blocking) -> Result<(), TaskError> {
        let mut store = self.store();
        store.edits.push(edit.clone());
        if store.refusing {
            return Err(TaskError::Failed { program: "fake".into(), stderr: "refused".into() });
        }
        apply(&mut store.tasks, edit);
        Ok(())
    }

    fn agent_guide(&self, project: &str) -> String {
        format!("Write your tasks to `{project}`.\n")
    }
}

/// A pending task on `project` whose description is its id.
pub fn task(id: &str, project: &str) -> Task {
    Task {
        id: id.into(),
        description: id.into(),
        status: Status::Pending,
        started: false,
        parent: None,
        order: None,
        project: Some(project.into()),
        entry: None,
        modified: None,
    }
}

/// `tasks` as a store holds them after `edit`. A new task takes its
/// description as its id, so a test can name it.
pub fn apply(tasks: &mut Vec<Task>, edit: &Edit) {
    fn find<'a>(tasks: &'a mut [Task], id: &str) -> Option<&'a mut Task> {
        tasks.iter_mut().find(|t| t.id == id)
    }
    match edit {
        Edit::Add { project, description, parent, order } => {
            let mut t = task(description, project);
            t.parent = parent.clone();
            t.order = Some(*order);
            tasks.push(t);
        },
        Edit::Move { id, parent, order } => {
            if let Some(t) = find(tasks, id) {
                t.parent = parent.clone();
                t.order = Some(*order);
            }
        },
        Edit::Reorder { id, order } => {
            find(tasks, id).into_iter().for_each(|t| t.order = Some(*order))
        },
        Edit::Describe { id, description } => {
            find(tasks, id).into_iter().for_each(|t| t.description = description.clone());
        },
        Edit::Done(id) => find(tasks, id).into_iter().for_each(|t| t.status = Status::Completed),
        Edit::Undone(id) => find(tasks, id).into_iter().for_each(|t| t.status = Status::Pending),
        Edit::Start(id) => find(tasks, id).into_iter().for_each(|t| t.started = true),
        Edit::Stop(id) => find(tasks, id).into_iter().for_each(|t| t.started = false),
        Edit::Delete(id) => tasks.retain(|t| &t.id != id),
    }
}
