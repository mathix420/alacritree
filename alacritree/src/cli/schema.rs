//! `alacritree schema` — the JSON Schema for the config files, plus the
//! command that points a config at it.
//!
//! The document itself comes from [`crate::config::json_schema`], reflected off
//! the `Raw*` structs serde reads.  What lives here is everything about
//! *publishing* it: the id it is served under, the header an editor looks for,
//! and the wording that only makes sense to someone reading the file.
//!
//! Editors that speak the TOML language server (taplo, and the Even Better TOML
//! extension built on it) use the schema for completion, hover docs and
//! validation; see `docs/alacritree.md`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The published id.  The release workflow attaches this file to every GitHub
/// Release, so `releases/latest/download` always resolves to the newest
/// released schema — and, unlike a URL naming one tag, never needs editing when
/// a release ships.  Someone wanting to validate against the version they run
/// substitutes their tag: `releases/download/v0.9.0/alacritree-config.json`.
/// Both beat pointing at `master`, which would validate every config against
/// unreleased keys.
pub const ID: &str =
    "https://github.com/mathix420/alacritree/releases/latest/download/alacritree-config.json";

/// The schema document, pretty-printed with a trailing newline.
pub fn document() -> String {
    let mut schema =
        serde_json::to_value(crate::config::json_schema()).expect("a schema serializes");
    let obj = schema.as_object_mut().expect("a struct schema is a JSON object");
    obj.insert("$id".into(), ID.into());
    obj.insert("title".into(), "alacritree configuration".into());
    obj.insert(
        "description".into(),
        "Everything alacritree reads out of alacritty.toml and alacritree.toml. \
         Unknown keys are allowed: the two files are layers, and alacritty.toml \
         legitimately carries keys only the real alacritty acts on."
            .into(),
    );

    drop_nulls(&mut schema);

    format!("{}\n", serde_json::to_string_pretty(&schema).expect("a JSON value pretty-prints"))
}

/// Strip the `null`s schemars emits for every `Option`.
///
/// TOML has no null.  A key is present with a value or it is absent, so an
/// `anyOf` branch of `{"type": "null"}`, a `"null"` in a type union, and a
/// `"default": null` all describe something no config file can hold — and the
/// last of the three is worse than noise, since an editor shows it as the key's
/// default where the real one is whatever `Config::default` returns.
///
/// Keywords holding instance values rather than subschemas are left alone: a
/// `null` inside one of those is data, not an artefact of `Option`.
fn drop_nulls(value: &mut serde_json::Value) {
    const INSTANCE_KEYWORDS: [&str; 4] = ["enum", "examples", "const", "default"];

    match value {
        serde_json::Value::Object(map) => {
            if map.get("default").is_some_and(serde_json::Value::is_null) {
                map.remove("default");
            }
            if let Some(types) = map.get_mut("type").and_then(serde_json::Value::as_array_mut) {
                types.retain(|t| t != "null");
                if let [only] = types.as_slice() {
                    let only = only.clone();
                    map.insert("type".into(), only);
                }
            }
            if let Some(branches) = map.get_mut("anyOf").and_then(serde_json::Value::as_array_mut) {
                branches.retain(|branch| *branch != serde_json::json!({ "type": "null" }));
            }
            for (key, child) in map.iter_mut() {
                if !INSTANCE_KEYWORDS.contains(&key.as_str()) {
                    drop_nulls(child);
                }
            }
        },
        serde_json::Value::Array(items) => items.iter_mut().for_each(drop_nulls),
        _ => {},
    }
}

pub fn print() {
    // Written to the raw handle rather than `println!`, which panics on a
    // closed pipe — `alacritree schema | head` is an ordinary thing to run.
    let _ = std::io::stdout().write_all(document().as_bytes());
}

/// taplo reads the schema association from a `#:schema` directive, which it
/// honors only as a header — first line, preceded at most by other directives
/// and comments.
fn directive() -> String {
    format!("#:schema {ID}")
}

/// A starter with every setting commented out, so nothing is active until its
/// owner has read it.  alacritree runs on its defaults with no config at all,
/// and a starter that changed the sidebar colours on contact would be a worse
/// introduction than an empty file.
const STARTER: &str = r##"
# alacritree-only options.  Shared options — palette, cursor, scrolling,
# shell, key bindings — go in alacritty.toml next to this file, where the
# real alacritty reads them too.

# [ui]
# sidebar_accent = "#6a9fb5"
# pr_status = true          # poll `gh` for each branch's open pull request
# upstream_status = true    # badge each worktree with its branch's upstream state

# Where `new worktree` puts a checkout.  $project is the repository's
# directory name.
# [workspace]
# worktree_dir = "~/Git/$project-worktrees"
"##;

/// Point `path` at the published schema, creating it from [`STARTER`] when it
/// does not exist.  Idempotent: a file that already carries a directive is left
/// exactly as it is, so this is safe to run against a config under review.
pub fn init(path: &Path) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };

    let body = match existing {
        Some(body) if body.contains("#:schema") => {
            println!("{} already points at a schema", path.display());
            return Ok(());
        },
        Some(body) => format!("{}\n{body}", directive()),
        None => format!("{}\n{STARTER}", directive()),
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, &body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!("{} now points at {ID}", path.display());
    Ok(())
}

/// Where `schema init` writes when given no path: the `alacritree.toml`
/// already in use, or the one the search path would pick up next.
pub fn default_config_path() -> PathBuf {
    crate::config::diagnose()
        .files
        .into_iter()
        .find(|f| f.stem == "alacritree")
        .and_then(|f| f.path)
        .unwrap_or_else(crate::config::preferred_alacritree_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::BindingAction;

    fn parsed() -> serde_json::Value {
        serde_json::from_str(&document()).unwrap()
    }

    #[test]
    fn the_document_names_the_published_id() {
        assert_eq!(parsed()["$id"], ID);
    }

    #[test]
    fn a_partial_config_is_not_an_error() {
        // Each file is a layer, and either may carry a subset — a config that
        // only sets `[ui] sidebar_accent` is the common case.  Editors validate
        // the file in front of them, never the merged result, so a required key
        // anywhere at the top level would mark correct files as wrong.
        assert!(parsed().get("required").is_none());
        assert!(parsed().get("additionalProperties").is_none());
    }

    #[test]
    fn every_table_alacritree_reads_is_described() {
        let schema = parsed();
        let props = schema["properties"].as_object().unwrap();
        for table in [
            "colors",
            "cursor",
            "debug",
            "env",
            "font",
            "general",
            "keyboard",
            "scrolling",
            "selection",
            "terminal",
            "ui",
            "window",
            "workspace",
            "wsl",
        ] {
            assert!(props.contains_key(table), "missing table: {table}");
        }
    }

    #[test]
    fn doc_comments_reach_the_schema_as_hover_text() {
        let schema = parsed();
        let ui = &schema["$defs"]["RawUi"]["properties"];
        assert!(
            ui["pr_status"]["description"].as_str().unwrap().contains("gh"),
            "descriptions are the only documentation an editor shows"
        );
    }

    #[test]
    fn a_closed_value_set_is_offered_as_an_enum() {
        let schema = parsed();
        let values = schema["$defs"]["RawUi"]["properties"]["confirm_session_close"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(values, &["never", "busy", "always"]);
    }

    #[test]
    fn an_optional_key_is_not_described_as_nullable() {
        // TOML cannot write null, so a `["string", "null"]` union offers a
        // value no config file can hold.
        let schema = parsed();
        let pr_status = &schema["$defs"]["RawUi"]["properties"]["pr_status"];
        assert_eq!(pr_status["type"], "boolean");
        assert!(pr_status.get("default").is_none(), "null is not this key's default");
    }

    #[test]
    fn an_optional_table_refers_straight_to_its_own_schema() {
        let schema = parsed();
        let branches =
            schema["$defs"]["RawColors"]["properties"]["dim"]["anyOf"].as_array().unwrap();
        assert_eq!(branches, &[serde_json::json!({ "$ref": "#/$defs/RawSet" })]);
    }

    #[test]
    fn a_null_inside_an_instance_value_survives() {
        // `enum`, `const`, `examples` and `default` hold values rather than
        // subschemas, so a null in one of them is what the config may write —
        // not an artefact of an `Option`.
        let mut value = serde_json::json!({
            "enum": [null, "on"],
            "examples": [{ "type": "null" }],
            "properties": { "a": { "type": ["string", "null"], "default": null } },
        });

        drop_nulls(&mut value);

        assert_eq!(value["enum"], serde_json::json!([null, "on"]));
        assert_eq!(value["examples"], serde_json::json!([{ "type": "null" }]));
        assert_eq!(value["properties"]["a"], serde_json::json!({ "type": "string" }));
    }

    #[test]
    fn a_color_is_constrained_to_the_spellings_that_parse() {
        let schema = parsed();
        assert_eq!(schema["$defs"]["Color"]["pattern"], "^(0[xX]|#)?[0-9a-fA-F]{6}$");
    }

    /// The suggestions and the parser are two hand-maintained lists.  A name
    /// completed from the schema that the parser then calls unknown is worse
    /// than no suggestion at all.
    #[test]
    fn every_suggested_action_is_one_the_parser_accepts() {
        let schema = parsed();
        let names = schema["$defs"]["RawBinding"]["properties"]["action"]["anyOf"][0]["enum"]
            .as_array()
            .unwrap();
        assert!(!names.is_empty());
        for name in names {
            let name = name.as_str().unwrap();
            assert!(
                matches!(crate::bindings::parse_action(name), BindingAction::Named(_)),
                "the schema suggests `{name}`, which does not parse"
            );
        }
    }

    /// An alacritty-only action in the shared `alacritty.toml` is something
    /// alacritree ignores, not something the editor should paint red.
    #[test]
    fn an_action_alacritree_does_not_implement_still_validates() {
        let schema = parsed();
        let branches =
            schema["$defs"]["RawBinding"]["properties"]["action"]["anyOf"].as_array().unwrap();
        let names = branches[0]["enum"].as_array().unwrap();

        assert!(!names.contains(&serde_json::json!("ToggleViMode")));
        assert_eq!(branches[1], serde_json::json!({ "type": "string" }));
    }

    #[test]
    fn init_leaves_a_config_that_already_names_a_schema_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alacritree.toml");
        let body = "#:schema ./elsewhere.json\n[ui]\npr_status = true\n";
        std::fs::write(&path, body).unwrap();

        init(&path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn init_creates_a_starter_that_changes_nothing_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alacritree.toml");

        init(&path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(&directive()));
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(parsed.as_table().map(toml::Table::len), Some(0));
    }

    #[test]
    fn init_keeps_the_settings_a_config_already_had() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alacritree.toml");
        std::fs::write(&path, "[ui]\npr_status = true\n").unwrap();

        init(&path).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(&directive()));
        assert!(body.contains("pr_status = true"));
    }
}
