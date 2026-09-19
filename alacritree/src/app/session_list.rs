//! The open sessions and each workspace's active one.  Sessions leave only
//! through `remove`, which repairs the active entries a removal invalidates,
//! so no caller has to remember to.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use super::{AppSession, SourceRepair, close_landing, plan_move};
use crate::config::SidebarFocus;
use crate::session::SessionId;
use crate::workspace::WorkspaceKey;

#[derive(Default)]
pub(super) struct SessionList {
    sessions: Vec<AppSession>,
    active: HashMap<WorkspaceKey, SessionId>,
}

/// Read and per-session access.  A slice has no way to add or drop an element,
/// so both stay behind `push` and `remove`.
impl Deref for SessionList {
    type Target = [AppSession];

    fn deref(&self) -> &[AppSession] {
        &self.sessions
    }
}

impl DerefMut for SessionList {
    fn deref_mut(&mut self) -> &mut [AppSession] {
        &mut self.sessions
    }
}

impl<'a> IntoIterator for &'a SessionList {
    type IntoIter = std::slice::Iter<'a, AppSession>;
    type Item = &'a AppSession;

    fn into_iter(self) -> Self::IntoIter {
        self.sessions.iter()
    }
}

impl SessionList {
    pub(super) fn push(&mut self, session: AppSession) {
        self.sessions.push(session);
    }

    pub(super) fn active(&self, workspace: &WorkspaceKey) -> Option<SessionId> {
        self.active.get(workspace).copied()
    }

    pub(super) fn has_active(&self, workspace: &WorkspaceKey) -> bool {
        self.active.contains_key(workspace)
    }

    pub(super) fn set_active(&mut self, workspace: WorkspaceKey, id: SessionId) {
        self.active.insert(workspace, id);
    }

    /// Take `ids` out of the list and hand them back, in the order given.  A
    /// workspace whose active session went hands its entry to the sibling
    /// `mode` lands on, or loses it when no sibling is left.  Ids not in the
    /// list are skipped.
    pub(super) fn remove(&mut self, ids: &[SessionId], mode: SidebarFocus) -> Vec<AppSession> {
        let mut removed = Vec::with_capacity(ids.len());
        for &id in ids {
            let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
                continue;
            };
            let session = self.sessions.remove(idx);
            let workspace = session.working_directory.clone();
            if self.active(&workspace) == Some(id) {
                let remaining: Vec<(WorkspaceKey, SessionId)> =
                    self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();
                match close_landing(&remaining, &workspace, idx, mode) {
                    Some(next) => {
                        self.active.insert(workspace, next);
                    },
                    None => {
                        self.active.remove(&workspace);
                    },
                }
            }
            removed.push(session);
        }
        removed
    }

    /// Re-home the session at `idx` to `target` and repair both workspaces'
    /// active entries.  True means the user was watching it, so the view
    /// should follow it there.
    pub(super) fn move_to(
        &mut self,
        idx: usize,
        target: &WorkspaceKey,
        current: &WorkspaceKey,
    ) -> bool {
        let id = self.sessions[idx].id;
        let source = self.sessions[idx].working_directory.clone();
        if source == *target {
            return false;
        }

        let was_source_active = self.active(&source) == Some(id);
        let on_screen = was_source_active && *current == source;
        self.sessions[idx].working_directory = target.clone();
        let next_in_source =
            self.sessions.iter().find(|s| s.working_directory == source).map(|s| s.id);

        let outcome =
            plan_move(was_source_active, on_screen, next_in_source, self.has_active(target));
        match outcome.source {
            SourceRepair::Keep => {},
            SourceRepair::Set(next) => {
                self.active.insert(source, next);
            },
            SourceRepair::Remove => {
                self.active.remove(&source);
            },
        }
        if outcome.claim_target {
            self.active.insert(target.clone(), id);
        }
        outcome.follow
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use egui::Context;

    use super::*;
    use crate::config::Config;
    use crate::session::{Session, TermSize};

    fn shell(workspace: WorkspaceKey) -> AppSession {
        Session::pending_shell(
            Context::default(),
            &Config::default(),
            workspace,
            TermSize { columns: 80, screen_lines: 24 },
            (8.0, 16.0),
            None,
            None,
        )
        .0
    }

    /// A list holding `count` sessions in `workspace`, the first one active.
    fn list_of(workspace: &WorkspaceKey, count: usize) -> (SessionList, Vec<SessionId>) {
        let mut list = SessionList::default();
        let ids: Vec<SessionId> = (0..count)
            .map(|_| {
                let session = shell(workspace.clone());
                let id = session.id;
                list.push(session);
                id
            })
            .collect();
        list.set_active(workspace.clone(), ids[0]);
        (list, ids)
    }

    #[test]
    fn removing_the_active_session_hands_the_entry_to_the_sibling_the_mode_lands_on() {
        let ws = Some(PathBuf::from("wt"));
        let (mut preserve, ids) = list_of(&ws, 3);
        preserve.set_active(ws.clone(), ids[1]);
        preserve.remove(&[ids[1]], SidebarFocus::Preserve);
        assert_eq!(preserve.active(&ws), Some(ids[0]));

        let (mut follow, ids) = list_of(&ws, 3);
        follow.set_active(ws.clone(), ids[1]);
        follow.remove(&[ids[1]], SidebarFocus::Follow);
        assert_eq!(follow.active(&ws), Some(ids[2]));
    }

    #[test]
    fn removing_every_session_in_a_workspace_drops_its_entry() {
        let ws = Some(PathBuf::from("wt"));
        let (mut list, ids) = list_of(&ws, 2);
        let removed = list.remove(&ids, SidebarFocus::Follow);
        assert_eq!(removed.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
        assert!(list.is_empty());
        assert!(!list.has_active(&ws));
    }

    #[test]
    fn removing_an_inactive_session_keeps_the_entry() {
        let ws = Some(PathBuf::from("wt"));
        let (mut list, ids) = list_of(&ws, 2);
        list.remove(&[ids[1]], SidebarFocus::Preserve);
        assert_eq!(list.active(&ws), Some(ids[0]));
    }
}
