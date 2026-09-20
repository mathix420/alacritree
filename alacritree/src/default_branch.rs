//! Which branch a repository treats as its default.
//!
//! git2, a shell script batched into WSL and a `git` subprocess all ask it.
//! The order is [`resolve`] and the names are [`WellKnown`], so a collector
//! only gathers [`Evidence`] the way its transport can.
//!
//! `resolve` ranks what it is given and never probes, which is what lets one
//! ordering serve three transports.

/// The branch names worth guessing when a repository does not say, in the
/// order they are guessed.
///
/// The shell collectors emit their own lists from these, so adding a name is
/// one edit here rather than six across two languages.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WellKnown {
    Main,
    Master,
    Trunk,
    Develop,
}

impl WellKnown {
    /// Every name, in guess order.
    pub const ALL: &'static [Self] = &[Self::Main, Self::Master, Self::Trunk, Self::Develop];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Master => "master",
            Self::Trunk => "trunk",
            Self::Develop => "develop",
        }
    }

    /// The names separated by spaces, for a shell `for c in ...` loop.
    pub fn shell_names() -> String {
        Self::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// `refs/heads/<name>` for each, for a shell `for-each-ref` argument list.
    pub fn shell_head_refs() -> String {
        Self::ALL.iter().map(|c| format!("refs/heads/{}", c.as_str())).collect::<Vec<_>>().join(" ")
    }
}

/// What a collector found out about one repository.
///
/// Each field is already verified: `origin_head` is what the symref resolved
/// to, `present` holds only branches this repository has, and `init_default`
/// is set only when `init.defaultBranch` names one of them.  An unverified
/// value here becomes an answer naming a branch that does not exist.
#[derive(Default)]
pub struct Evidence<'a> {
    /// A branch chosen for this repository, such as a recorded review base.
    /// A cached detection is not one: it belongs in `origin_head`.
    pub hint: Option<&'a str>,
    /// The branch `refs/remotes/origin/HEAD` points at.
    pub origin_head: Option<&'a str>,
    /// Which of [`WellKnown::ALL`] this repository has.
    pub present: Vec<&'a str>,
    /// `init.defaultBranch`, when it names a branch this repository has.
    pub init_default: Option<&'a str>,
}

/// The default branch the evidence supports, or `None` when it supports none.
///
/// Hint, then `origin/HEAD`, then the well-known names: a choice someone made
/// about this repository outranks a guess about repositories in general.
///
/// `init.defaultBranch` comes last because it says what `git init` names new
/// repositories, so a global `init.defaultBranch=master` would otherwise
/// hijack every checkout whose real default is `main`.
pub fn resolve(evidence: &Evidence<'_>) -> Option<String> {
    let non_empty = |s: &&str| !s.trim().is_empty();

    if let Some(hint) = evidence.hint.filter(non_empty) {
        return Some(hint.trim().to_string());
    }
    if let Some(head) = evidence.origin_head.filter(non_empty) {
        return Some(head.trim().to_string());
    }
    for candidate in WellKnown::ALL {
        if evidence.present.iter().any(|p| p.trim() == candidate.as_str()) {
            return Some(candidate.as_str().to_string());
        }
    }
    evidence.init_default.filter(non_empty).map(|c| c.trim().to_string())
}

/// The same ranking as [`resolve`], as `sh`, for a collector that has to pick
/// a branch inside the round trip because later commands diff against it.
///
/// Reads `$p` (the repository) and `$hint`, and leaves the answer in `$h`.
/// Kept here so the two rankings sit side by side and move together.
pub fn shell_ranking() -> String {
    format!(
        r#"h="$hint"
if [ -z "$h" ]; then
  h=$(git -C "$p" symbolic-ref refs/remotes/origin/HEAD 2>/dev/null)
  h="${{h#refs/remotes/origin/}}"
fi
if [ -z "$h" ]; then
  for c in {names}; do
    if git -C "$p" rev-parse --verify --quiet "refs/heads/$c" >/dev/null 2>&1; then h="$c"; break; fi
  done
fi
if [ -z "$h" ]; then
  c=$(git -C "$p" config init.defaultBranch 2>/dev/null)
  if [ -n "$c" ] && git -C "$p" rev-parse --verify --quiet "refs/heads/$c" >/dev/null 2>&1; then h="$c"; fi
fi"#,
        names = WellKnown::shell_names()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_outranks_every_other_answer() {
        let resolved = resolve(&Evidence {
            hint: Some("release"),
            origin_head: Some("main"),
            present: vec!["main", "master"],
            init_default: Some("master"),
        });
        assert_eq!(resolved.as_deref(), Some("release"));
    }

    #[test]
    fn origin_head_outranks_the_well_known_names() {
        let resolved = resolve(&Evidence {
            origin_head: Some("trunk"),
            present: vec!["main", "trunk"],
            ..Evidence::default()
        });
        assert_eq!(resolved.as_deref(), Some("trunk"));
    }

    #[test]
    fn the_well_known_names_keep_their_order_whatever_the_repository_lists_first() {
        let resolved = resolve(&Evidence {
            present: vec!["develop", "master", "main"],
            ..Evidence::default()
        });
        assert_eq!(resolved.as_deref(), Some("main"));
    }

    #[test]
    fn init_default_answers_only_when_nothing_else_does() {
        let resolved = resolve(&Evidence { init_default: Some("mainline"), ..Evidence::default() });
        assert_eq!(resolved.as_deref(), Some("mainline"));

        let outranked = resolve(&Evidence {
            present: vec!["master"],
            init_default: Some("mainline"),
            ..Evidence::default()
        });
        assert_eq!(outranked.as_deref(), Some("master"));
    }

    #[test]
    fn evidence_that_names_nothing_answers_nothing() {
        assert_eq!(resolve(&Evidence::default()), None);
        let blank = resolve(&Evidence {
            hint: Some("  "),
            origin_head: Some(""),
            present: vec![],
            init_default: Some("\n"),
        });
        assert_eq!(blank, None);
    }

    #[test]
    fn the_shell_lists_are_the_enum_in_order() {
        assert_eq!(WellKnown::shell_names(), "main master trunk develop");
        assert_eq!(
            WellKnown::shell_head_refs(),
            "refs/heads/main refs/heads/master refs/heads/trunk refs/heads/develop"
        );
    }

    #[test]
    fn the_shell_ranking_tries_the_same_sources_in_the_same_order() {
        let script = shell_ranking();
        let order: Vec<usize> =
            ["$hint", "origin/HEAD", &WellKnown::shell_names(), "init.defaultBranch"]
                .iter()
                .map(|needle| script.find(*needle).unwrap_or_else(|| panic!("missing {needle}")))
                .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "shell ranking drifted from resolve");
    }
}
