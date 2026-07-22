//! Persistent per-workspace scratchpad documents and their built-in editor.
//!
//! Closing the editor tab or deleting a worktree must never delete the notes.
//! File names include a deterministic digest of the workspace path so equal
//! leaf names in different projects cannot collide.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use egui::text::{CCursor, CCursorRange};
use egui::{
    Color32, FontId, Frame, Id, Margin, Response, RichText, ScrollArea, TextBuffer, TextEdit,
};
use serde_json::{Value, json};

use crate::app::WorkspaceKey;
use crate::state;

// A scratchpad is normally a few notes. Bounding an MCP response keeps an
// accidentally huge file from consuming the client's entire context window.
const MAX_MCP_BYTES: usize = 256 * 1024;

/// In-memory state for the built-in editor. Every mutation is immediately
/// written to `path`; `save_error` is painted in-place instead of replacing
/// the editor with a modal, so a transient filesystem failure never loses the
/// user's buffer.
pub struct Editor {
    path: PathBuf,
    text: String,
    save_error: Option<String>,
}

impl Editor {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let text = fs::read_to_string(&path)?;
        Ok(Self { path, text, save_error: None })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn insert_at_cursor(&mut self, ctx: &egui::Context, session_id: u64, text: &str) {
        if text.is_empty() {
            return;
        }
        let id = editor_id(session_id);
        let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
        let end = self.text.chars().count();
        let range =
            state.cursor.char_range().unwrap_or_else(|| CCursorRange::one(CCursor::new(end)));
        let [min, max] = range.sorted();
        self.text.delete_char_range(min.index..max.index);
        let inserted = self.text.insert_text(text, min.index);
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(min.index + inserted))));
        TextEdit::store_state(ctx, id, state);
        self.save();
    }

    pub fn selected_text(&self, ctx: &egui::Context, session_id: u64) -> Option<String> {
        let state = TextEdit::load_state(ctx, editor_id(session_id))?;
        let [min, max] = state.cursor.char_range()?.sorted();
        (min.index != max.index).then(|| self.text.char_range(min.index..max.index).to_string())
    }

    fn save(&mut self) {
        self.save_error = fs::write(&self.path, self.text.as_bytes())
            .err()
            .map(|error| format!("Autosave failed: {error}"));
    }
}

pub fn editor_id(session_id: u64) -> Id {
    Id::new(("scratchpad-editor", session_id))
}

/// Full-pane editor inspired by notes.vercel.app: no toolbar, border, status
/// chrome, or explicit save action—just a padded text surface that inherits
/// the terminal pane's background.
pub fn show_editor(
    ui: &mut egui::Ui,
    session_id: u64,
    editor: &mut Editor,
    allow_focus: bool,
    ui_scale: f32,
    text_color: Color32,
    hint_color: Color32,
    error_color: Color32,
) -> Response {
    let available = ui.available_size();
    let padding = 16.0 * ui_scale;
    let font_size = 20.0 * ui_scale;
    let editor_id = editor_id(session_id);
    if !allow_focus {
        ui.memory_mut(|memory| memory.surrender_focus(editor_id));
    }

    let response = Frame::default()
        .inner_margin(Margin::same(padding as i8))
        .show(ui, |ui| {
            let inner_size = egui::vec2(
                (available.x - padding * 2.0).max(1.0),
                (available.y - padding * 2.0).max(1.0),
            );
            ui.set_min_size(inner_size);
            let rows = (inner_size.y / (font_size * 1.35)).floor().max(4.0) as usize;
            let output = ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(inner_size.y)
                .show(ui, |ui| {
                    TextEdit::multiline(&mut editor.text)
                        .id(editor_id)
                        .font(FontId::monospace(font_size))
                        .text_color(text_color)
                        .hint_text(
                            RichText::new(
                                "Start typing.\n\nEverything is automatically saved to this workspace.",
                            )
                            .color(hint_color)
                            .monospace()
                            .size(font_size),
                        )
                        .frame(false)
                        .margin(Margin::ZERO)
                        .desired_width(inner_size.x)
                        .desired_rows(rows)
                        .show(ui)
                })
                .inner;
            if allow_focus && !output.response.has_focus() {
                output.response.request_focus();
            }
            if output.response.changed() {
                editor.save();
            }
            if let Some(error) = &editor.save_error {
                ui.painter().text(
                    ui.max_rect().right_bottom() - egui::vec2(4.0, 4.0),
                    egui::Align2::RIGHT_BOTTOM,
                    error,
                    FontId::monospace(11.0 * ui_scale),
                    error_color,
                );
            }
            output.response
        })
        .inner;
    response
}

pub fn ensure_file(workspace: &WorkspaceKey) -> io::Result<PathBuf> {
    let config_dir = state::config_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "could not locate alacritree's config directory")
    })?;
    ensure_file_in(&config_dir, workspace)
}

fn ensure_file_in(config_dir: &Path, workspace: &WorkspaceKey) -> io::Result<PathBuf> {
    let path = path_for_in(config_dir, workspace);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(path)
}

pub fn path_for(workspace: &WorkspaceKey) -> Option<PathBuf> {
    Some(path_for_in(&state::config_dir()?, workspace))
}

fn path_for_in(config_dir: &Path, workspace: &WorkspaceKey) -> PathBuf {
    let name = match workspace {
        None => "home.md".to_string(),
        Some(path) => {
            let label = path
                .file_name()
                .map(|name| slug(&name.to_string_lossy()))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "workspace".to_string());
            let identity = workspace_identity(path);
            format!("{label}-{:012x}.md", stable_digest(identity.as_bytes()) & 0xffffffffffff)
        },
    };
    config_dir.join("scratchpads").join(name)
}

fn workspace_identity(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { text.to_ascii_lowercase() } else { text }
}

fn slug(input: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in input.chars() {
        if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
            separator = false;
        } else if !separator && !out.is_empty() {
            out.push('-');
            separator = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// FNV-1a is small, deterministic across Rust versions, and sufficient here:
/// the digest disambiguates human-readable file names rather than protecting
/// an adversarial namespace.
fn stable_digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn read_json(workspace: &WorkspaceKey) -> Result<Value, String> {
    let path = path_for(workspace)
        .ok_or_else(|| "could not locate alacritree's config directory".to_string())?;
    read_json_at(&path, workspace)
}

fn read_json_at(path: &Path, workspace: &WorkspaceKey) -> Result<Value, String> {
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(json!({
                "workspace": workspace,
                "path": path,
                "exists": false,
                "content": "",
                "truncated": false,
            }));
        },
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let truncated = bytes.len() > MAX_MCP_BYTES;
    let content = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_MCP_BYTES)]).into_owned();
    let modified_ms = fs::metadata(&path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis());
    Ok(json!({
        "workspace": workspace,
        "path": path,
        "exists": true,
        "content": content,
        "modified_unix_ms": modified_ms,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use egui::{Event, Pos2, RawInput, Rect, Vec2};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn home_and_same_named_workspaces_get_distinct_readable_files() {
        let dir = TempDir::new().unwrap();
        let home = path_for_in(dir.path(), &None);
        let a = path_for_in(dir.path(), &Some(PathBuf::from("/one/topic")));
        let b = path_for_in(dir.path(), &Some(PathBuf::from("/two/topic")));

        assert_eq!(home.file_name().unwrap(), "home.md");
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("topic-"));
        assert_ne!(a, b);
        assert_eq!(a, path_for_in(dir.path(), &Some(PathBuf::from("/one/topic"))));
    }

    #[test]
    fn built_in_editor_inserts_at_the_cursor_and_saves_immediately() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "hello").unwrap();
        let mut editor = Editor::open(path.clone()).unwrap();
        let ctx = egui::Context::default();

        editor.insert_at_cursor(&ctx, 7, "!");
        assert_eq!(editor.text(), "hello!");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello!");

        let mut state = egui::text_edit::TextEditState::default();
        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(5))));
        TextEdit::store_state(&ctx, editor_id(7), state);
        editor.insert_at_cursor(&ctx, 7, "saved");
        assert_eq!(fs::read_to_string(path).unwrap(), "saved!");
    }

    #[test]
    fn typed_characters_are_saved_after_each_ui_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "").unwrap();
        let mut editor = Editor::open(path.clone()).unwrap();
        let ctx = egui::Context::default();

        let mut frame = |events| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_editor(
                        ui,
                        9,
                        &mut editor,
                        true,
                        1.0,
                        Color32::WHITE,
                        Color32::GRAY,
                        Color32::RED,
                    );
                });
            });
        };

        frame(Vec::new());
        frame(vec![Event::Text("a".into())]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a");
        frame(vec![Event::Text("b".into())]);
        assert_eq!(fs::read_to_string(path).unwrap(), "ab");
    }

    #[test]
    fn file_contents_survive_editor_session_lifetimes() {
        let dir = TempDir::new().unwrap();
        let workspace = Some(PathBuf::from("/repo/topic"));
        let path = ensure_file_in(dir.path(), &workspace).unwrap();
        fs::write(&path, "# Notes\n\nkeep me\n").unwrap();

        // Reading has no dependency on a live editor tab; closing/restarting
        // the app therefore cannot erase the document.
        let value = read_json_at(&path, &workspace).unwrap();
        assert_eq!(value["content"], "# Notes\n\nkeep me\n");
        assert_eq!(value["exists"], true);
        assert_eq!(ensure_file_in(dir.path(), &workspace).unwrap(), path);
        assert_eq!(fs::read_to_string(path).unwrap(), "# Notes\n\nkeep me\n");
    }
}
