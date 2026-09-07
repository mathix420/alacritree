//! Every config key whose absence resolves to a fixed value must publish that
//! value as a schema `default`.  The keys that legitimately have none are
//! listed in `schema-defaults-allowlist.txt`, so adding an `Option<T>` field
//! by reflex fails the build with the key named.
//!
//! `devkit run task test --env ALACRITREE_UPDATE_ALLOWLIST=1` rewrites the
//! list instead of failing.  Read the diff: a line appearing is a key that
//! lost its default, which is almost always a mistake.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn generated() -> Value {
    // The child is alacritree itself, a GUI-subsystem binary that allocates no
    // console, so there is no window to hide.  This crate has no lib target, so
    // an integration test cannot reach `command_ext::hidden`.
    #[allow(clippy::disallowed_methods)]
    let out = Command::new(env!("CARGO_BIN_EXE_alacritree")).arg("schema").output().unwrap();
    assert!(out.status.success(), "`alacritree schema` failed");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn allowlist_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/schema-defaults-allowlist.txt"))
}

/// Follow a `$ref` and unwrap the single-branch `anyOf` that `drop_nulls`
/// leaves behind on an optional table.
fn resolve<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if let [only] = branches.as_slice() {
            return resolve(only, root);
        }
    }
    let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
        return schema;
    };
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return schema;
    };
    root.get("$defs").and_then(|d| d.get(name)).map_or(schema, |target| resolve(target, root))
}

/// A property is a table when it resolves to a schema with its own
/// `properties` map — its keys are reached through that def's own entry, not
/// through this one.  An untagged enum with a scalar branch (`RawIconStyle`,
/// `RawShell`) has `anyOf` rather than `properties`, so it stays a leaf: the
/// key can be written as a bare value and therefore can carry a default.
fn is_table(schema: &Value, root: &Value) -> bool {
    resolve(schema, root).get("properties").is_some()
}

/// Every property map a def declares: its own, plus one per `anyOf` branch,
/// which is how an untagged enum's variant fields are reached.
fn property_maps(def: &Value) -> Vec<&serde_json::Map<String, Value>> {
    let mut maps = Vec::new();
    if let Some(props) = def.get("properties").and_then(Value::as_object) {
        maps.push(props);
    }
    for branch in def.get("anyOf").and_then(Value::as_array).into_iter().flatten() {
        if let Some(props) = branch.get("properties").and_then(Value::as_object) {
            maps.push(props);
        }
    }
    maps
}

/// Every leaf property in the document, keyed by the def that declares it, and
/// whether it carries a default.  Keyed by def rather than by config path
/// because `Color` is reached by dozens of paths and `RawIconStyle` by
/// twenty-four; a path-keyed list would be mostly copies of itself.
fn leaves(root: &Value) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut push = |def_name: &str, def: &Value| {
        for props in property_maps(def) {
            for (field, schema) in props {
                if is_table(schema, root) {
                    continue;
                }
                out.push((format!("{def_name}.{field}"), schema.get("default").is_some()));
            }
        }
    };
    push("RawConfig", root);
    for (name, def) in root.get("$defs").and_then(Value::as_object).into_iter().flatten() {
        push(name, def);
    }
    out
}

#[test]
fn every_leaf_without_a_default_is_allowlisted() {
    let schema = generated();
    let found: BTreeSet<String> =
        leaves(&schema).into_iter().filter(|(_, has)| !has).map(|(key, _)| key).collect();

    if std::env::var("ALACRITREE_UPDATE_ALLOWLIST").as_deref() == Ok("1") {
        let body: String = found.iter().map(|k| format!("{k}\n")).collect();
        std::fs::write(allowlist_path(), body).unwrap();
        return;
    }

    let committed = std::fs::read_to_string(allowlist_path()).unwrap_or_default();
    let allowed: BTreeSet<&str> =
        committed.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();

    let missing: Vec<&str> =
        found.iter().map(String::as_str).filter(|k| !allowed.contains(k)).collect();
    let stale: Vec<&str> = allowed.iter().copied().filter(|k| !found.contains(*k)).collect();

    assert!(
        missing.is_empty() && stale.is_empty(),
        "schema-defaults-allowlist.txt is out of date — regenerate with `devkit run task test \
         --env ALACRITREE_UPDATE_ALLOWLIST=1` and read the diff\n\nlost a default (or is newly \
         optional):\n  {}\n\ngained a default, drop from the list:\n  {}",
        missing.join("\n  "),
        stale.join("\n  "),
    );
}

/// The walk has to see a default that sits on an untagged enum's variant field
/// and one that sits on a plain property, since the inversion produces both.
#[test]
fn the_walk_sees_the_defaults_that_already_exist() {
    let schema = generated();
    let with_defaults: BTreeSet<String> =
        leaves(&schema).into_iter().filter(|(_, has)| *has).map(|(key, _)| key).collect();

    for key in [
        "RawColors.draw_bold_text_with_bright_colors",
        "RawIconStyle.bold",
        "RawIconStyle.italic",
        "RawProfile.args",
        "RawConfig.env",
    ] {
        assert!(with_defaults.contains(key), "the walk missed {key}: {with_defaults:#?}");
    }
}

/// A `default` that is not one of the spellings beside it is a typo the
/// parser hides: an unknown string falls back to the resolved default, so
/// the config still comes out right and only the schema is wrong.
#[test]
fn every_default_is_one_of_the_spellings_offered_beside_it() {
    fn walk(node: &Value, path: &str, bad: &mut Vec<String>) {
        match node {
            Value::Object(map) => {
                if let (Some(default), Some(Value::Array(spellings))) =
                    (map.get("default"), map.get("enum"))
                {
                    if !spellings.contains(default) {
                        bad.push(format!("{path}: default {default} is not one of {spellings:?}"));
                    }
                }
                for (key, child) in map {
                    walk(child, &format!("{path}.{key}"), bad);
                }
            },
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}[{i}]"), bad);
                }
            },
            _ => {},
        }
    }

    let mut bad = Vec::new();
    walk(&generated(), "", &mut bad);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
