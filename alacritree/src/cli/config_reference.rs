//! `docs/config-reference.md`, rendered from the JSON Schema so the reference a
//! user reads and the hover text an editor shows come from the same `Raw*` doc
//! comments.

use std::collections::HashMap;
use std::fmt::Write;

use serde_json::{Map, Value};

const HEADER: &str = "# Configuration reference

Every key alacritree reads from `alacritty.toml` and `alacritree.toml`, with its type and default. \
                      Generated from the JSON Schema; regenerate with `ALACRITREE_UPDATE_SCHEMA=1 \
                      cargo test -p alacritree --test config_schema`.
";

/// The reference document, with a trailing newline.
pub fn document() -> String {
    let schema: Value =
        serde_json::from_str(&super::schema::document()).expect("the schema is JSON");
    let mut reference =
        Reference { defs: &schema["$defs"], out: HEADER.into(), shared: HashMap::new() };
    for (key, prop) in properties(&schema) {
        reference.table(key, prop, false);
    }
    reference.out
}

struct Reference<'a> {
    defs: &'a Value,
    out: String,
    /// Where a table form shared by several keys was first spelled out, so the
    /// others point there instead of repeating its fields.
    shared: HashMap<&'a str, String>,
}

impl<'a> Reference<'a> {
    fn table(&mut self, path: &str, prop: &'a Value, array: bool) {
        let schema = if array { self.resolve(&prop["items"]) } else { self.resolve(prop) };
        let (open, close) = if array { ("[[", "]]") } else { ("[", "]") };
        let level = if path.contains('.') { "###" } else { "##" };
        let _ = writeln!(self.out, "\n{level} `{open}{path}{close}`\n");
        if let Some(text) = self.description(prop) {
            let _ = writeln!(self.out, "{text}\n");
        }

        let mut nested = Vec::new();
        for (key, child) in properties(schema) {
            let path = format!("{path}.{key}");
            if self.is_table(child) {
                nested.push((path, child, false));
            } else if child.get("type") == Some(&"array".into()) && self.is_table(&child["items"]) {
                nested.push((path, child, true));
            } else if let Some(entry) = self.map_entry(child) {
                // A table of named tables: document the key, then one
                // `<name>` table for the shape every entry takes.
                self.key(key, &path, child);
                nested.push((format!("{path}.<name>"), entry, false));
            } else {
                self.key(key, &path, child);
            }
        }
        for (path, child, array) in nested {
            self.table(&path, child, array);
        }
    }

    fn key(&mut self, key: &str, path: &str, prop: &'a Value) {
        let line = self.entry(key, prop);
        let _ = writeln!(self.out, "- {line}");

        let Some((name, form)) = self.table_form(prop) else { return };
        if let Some(first) = self.shared.get(name) {
            let _ = writeln!(self.out, "  - The table form takes the same keys as `{first}`.");
            return;
        }
        self.shared.insert(name, path.to_string());
        for (key, field) in properties(form) {
            let line = self.entry(key, field);
            let _ = writeln!(self.out, "  - {line}");
        }
    }

    fn entry(&self, key: &str, prop: &'a Value) -> String {
        let mut line = format!("`{key}` ({}", self.type_name(prop));
        if let Some(default) = prop.get("default") {
            let _ = write!(line, ", default `{default}`");
        }
        line.push(')');
        if let Some(text) = self.description(prop) {
            let _ = write!(line, ": {text}");
        }
        line
    }

    /// The table branch of a key that also accepts a bare value, such as an
    /// icon written as a glyph or as a styled table.
    fn table_form(&self, prop: &'a Value) -> Option<(&'a str, &'a Value)> {
        let name = def_name(prop)?;
        let form =
            self.defs[name]["anyOf"].as_array()?.iter().find(|b| b.get("properties").is_some())?;
        Some((name, form))
    }

    /// The entry schema of a map whose values are tables, such as named
    /// command hooks.
    fn map_entry(&self, prop: &'a Value) -> Option<&'a Value> {
        let entry = self.resolve(prop).get("additionalProperties")?;
        self.is_table(entry).then_some(entry)
    }

    fn is_table(&self, prop: &'a Value) -> bool {
        self.resolve(prop).get("properties").is_some()
    }

    /// Follows a `$ref`, including the one-branch `anyOf` schemars wraps an
    /// optional reference in.
    fn resolve(&self, prop: &'a Value) -> &'a Value {
        match def_name(prop) {
            Some(name) => &self.defs[name],
            None => prop,
        }
    }

    fn description(&self, prop: &'a Value) -> Option<String> {
        let text = prop.get("description").or_else(|| self.resolve(prop).get("description"))?;
        Some(text.as_str()?.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn type_name(&self, prop: &'a Value) -> String {
        let schema = self.resolve(prop);
        if let Some(values) = schema["enum"].as_array() {
            return values.iter().map(Value::to_string).collect::<Vec<_>>().join(" | ");
        }
        if let Some(branches) = schema["anyOf"].as_array() {
            return branches.iter().map(|b| self.type_name(b)).collect::<Vec<_>>().join(" or ");
        }
        match schema["type"].as_str() {
            Some("array") => format!("array of {}", self.type_name(&schema["items"])),
            Some("object") => "table".into(),
            Some(name) => name.into(),
            None => "any".into(),
        }
    }
}

fn properties(schema: &Value) -> impl Iterator<Item = (&str, &Value)> {
    schema["properties"].as_object().into_iter().flat_map(Map::iter).map(|(k, v)| (k.as_str(), v))
}

fn def_name(prop: &Value) -> Option<&str> {
    let reference = match prop["anyOf"].as_array() {
        Some(branches) if branches.len() == 1 => &branches[0]["$ref"],
        _ => &prop["$ref"],
    };
    reference.as_str()?.strip_prefix("#/$defs/")
}

#[cfg(test)]
mod tests {
    /// Named tables are a map in the schema; their fields must still reach
    /// the reference, or command hooks are undocumented.
    #[test]
    fn a_map_of_tables_documents_its_entry_fields() {
        let doc = super::document();
        assert!(doc.contains("`[integrations.checkout_hooks.command.<name>]`"), "{doc}");
        assert!(doc.contains("- `on_created` (array of string"), "{doc}");
    }
}
