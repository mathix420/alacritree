//! Render templated sidebar row names.
//!
//! Templates come from `[ui] worktree_name` / `[ui] project_name` and use
//! subst's shell-style syntax: `$var`, `${var}`, and `${var:fallback}` (the
//! fallback may itself contain variables, so `${branch:$name}` reads "the
//! branch, or the worktree name when detached").  Any error — parse failure,
//! unknown variable — falls back to the plain name with one warning per
//! template string, so a typo'd config degrades to today's sidebar rather
//! than blank rows.

use std::collections::HashMap;

/// Substitute `vars` into `template`.  `None` on any subst error or when the
/// trimmed result is empty — the caller falls back to the plain name either
/// way, because a blank row label is as useless as a failed one.
pub fn render_label(template: &str, vars: &HashMap<String, String>) -> Option<String> {
    let rendered = subst::substitute(template, vars).ok()?;
    let trimmed = rendered.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_variable_substitutes() {
        assert_eq!(
            render_label("$name", &vars(&[("name", "feature-x")])).as_deref(),
            Some("feature-x")
        );
    }

    #[test]
    fn literal_text_passes_through() {
        assert_eq!(render_label("wt: $name", &vars(&[("name", "a")])).as_deref(), Some("wt: a"));
    }

    #[test]
    fn fallback_used_when_variable_missing() {
        let v = vars(&[("name", "main-wt")]);
        assert_eq!(render_label("${branch:$name}", &v).as_deref(), Some("main-wt"));
    }

    #[test]
    fn fallback_ignored_when_variable_present() {
        let v = vars(&[("name", "main-wt"), ("branch", "feat/x")]);
        assert_eq!(render_label("${branch:$name}", &v).as_deref(), Some("feat/x"));
    }

    #[test]
    fn unknown_variable_is_an_error() {
        assert_eq!(render_label("$nope", &vars(&[("name", "a")])), None);
    }

    #[test]
    fn empty_render_is_an_error() {
        assert_eq!(render_label("  ", &vars(&[])), None);
        assert_eq!(render_label("$name", &vars(&[("name", " ")])), None);
    }
}
