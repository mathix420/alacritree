//! Abbreviated path rendering for sidebar rows and pane titles.
//!
//! Pure and egui-free so the table below can be unit-tested without a `Ui`,
//! and so the caller decides what counts as `home` — a WSL path's home lives
//! inside the distro and cannot be inferred from the path.

/// How a path is spelled to the user.  `Full` is the identity and the
/// default, so an unmodified config renders exactly what it renders today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathStyle {
    #[default]
    Full,
    /// Every parent segment collapses to its first character, fish-style.
    Fish,
    /// The filename leads, the parent trails it.
    Zed,
}

/// A path cut where it may be abbreviated.  `root` is never abbreviated and
/// never reordered; `parent` keeps its trailing separator, and is empty for a
/// bare name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parts {
    pub root: String,
    pub parent: String,
    pub name: String,
}

pub fn split(path: &str, style: PathStyle, home: Option<&str>) -> Parts {
    let (root, rest, sep) = split_root(path);
    let collapsed = match style {
        PathStyle::Full => None,
        _ => home.and_then(|home| strip_home(path, home, sep)),
    };
    let (root, segments) = match collapsed {
        // `~` replaces the root as well as the leading segments, so a
        // collapsed path carries no root of its own.
        Some(tail) => {
            let mut segments = vec!["~".to_string()];
            segments.extend(segments_of(tail, sep));
            (String::new(), segments)
        },
        None => (root, segments_of(rest, sep)),
    };

    let name = segments.last().cloned().unwrap_or_default();
    let mut parent = String::new();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        parent.push_str(&abbreviate(segment, style));
        parent.push(sep);
    }
    Parts { root, parent, name }
}

pub fn render(path: &str, style: PathStyle, home: Option<&str>) -> String {
    if style == PathStyle::Full {
        return path.to_string();
    }
    let parts = split(path, style, home);
    match style {
        PathStyle::Full => unreachable!("returned above"),
        PathStyle::Fish => format!("{}{}{}", parts.root, parts.parent, parts.name),
        PathStyle::Zed if parts.parent.is_empty() => format!("{}{}", parts.root, parts.name),
        PathStyle::Zed => format!("{} {}{}", parts.name, parts.root, parts.parent),
    }
}

/// The root token, the remainder, and the separator the root implies.
///
/// Matched by prefix and in this order, because `\` and `:` are legal inside
/// a Unix filename: scanning for separator characters would misread
/// `dir/name\part.txt` as a Windows path.
fn split_root(path: &str) -> (String, &str, char) {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return match segments_len(rest, 2) {
            Some(len) => (path[..r"\\?\UNC\".len() + len].to_string(), &rest[len..], '\\'),
            None => (path.to_string(), "", '\\'),
        };
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        if let Some(len) = drive_root_len(rest) {
            return (path[..r"\\?\".len() + len].to_string(), &rest[len..], '\\');
        }
    }
    if let Some(rest) = path.strip_prefix(r"\\") {
        return match segments_len(rest, 2) {
            Some(len) => (path[..2 + len].to_string(), &rest[len..], '\\'),
            None => (path.to_string(), "", '\\'),
        };
    }
    if let Some(len) = drive_root_len(path) {
        // Normalize `C:/` to `C:\`: a drive path is re-joined with backslashes.
        return (format!("{}:\\", &path[..1]), &path[len..], '\\');
    }
    if is_drive_relative(path) {
        // `C:foo` is relative to the drive's current directory, so no
        // separator may be inserted after the root.
        return (path[..2].to_string(), &path[2..], '\\');
    }
    if let Some(rest) = path.strip_prefix('/') {
        return ("/".to_string(), rest, '/');
    }
    (String::new(), path, '/')
}

/// Byte length of `<letter>:<sep>`, or `None` when `s` does not start with one.
fn drive_root_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let is_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    is_drive.then_some(3)
}

fn is_drive_relative(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Byte offset past `n` `\`-separated segments *and* the separator closing the
/// last one.  `None` when the string runs out first, which means the whole
/// input is root and there is nothing beneath it.
fn segments_len(s: &str, n: usize) -> Option<usize> {
    let mut idx = 0;
    for _ in 0..n {
        idx += s[idx..].find('\\')? + 1;
    }
    Some(idx)
}

/// Windows paths are split on either separator; POSIX paths only on `/`, so a
/// Unix filename containing `\` survives intact.
fn segments_of(rest: &str, sep: char) -> Vec<String> {
    let split_on = |c: char| c == sep || (sep == '\\' && c == '/');
    rest.split(split_on).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// The part of `path` beneath `home`, or `None` when it is not beneath it.
/// An exact match yields `""` — the path *is* home.
fn strip_home<'a>(path: &'a str, home: &str, sep: char) -> Option<&'a str> {
    // A home that is nothing but separators would collapse every absolute
    // path to `~`, which reads as a bug rather than as a shortening.  `/` is
    // no one's home directory; root's is `/root`.
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return None;
    }
    // A Windows filesystem is case-insensitive and accepts both separators;
    // a Unix one is neither, and `/Home` is a different directory.
    let same = |a: &str, b: &str| {
        if sep == '\\' { normalize_windows(a) == normalize_windows(b) } else { a == b }
    };
    if same(path, home) {
        return Some("");
    }
    let (head, tail) = path.split_at_checked(home.len())?;
    if !same(head, home) {
        return None;
    }
    let first = tail.chars().next()?;
    (first == sep || (sep == '\\' && first == '/')).then(|| &tail[first.len_utf8()..])
}

/// Full Unicode lowering, not `to_ascii_lowercase`: NTFS folds case beyond
/// ASCII, so a home under `C:\Üsers` must still match `c:\üsers`.
fn normalize_windows(s: &str) -> String {
    s.replace('/', "\\").to_lowercase()
}

/// Fish keeps enough of a segment to still recognize it: the first character,
/// plus one more when that character is a dot, so `.config` reads `.c`.
fn abbreviate(segment: &str, style: PathStyle) -> String {
    if style != PathStyle::Fish || segment == "~" {
        return segment.to_string();
    }
    let mut chars = segment.chars();
    let mut out = String::new();
    if let Some(first) = chars.next() {
        out.push(first);
        if first == '.' {
            out.extend(chars.next());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity guarantee: an unmodified config renders every path
    /// byte-for-byte, home directory included.
    #[test]
    fn full_is_the_identity() {
        let home = Some("/home/lev");
        for path in [
            "",
            "/",
            "src/app.rs",
            "/home/lev/Git/x/y.rs",
            r"C:\Program Files\Git",
            r"C:",
            r"C:foo\bar",
            r"\\server\share\x",
            r"\\wsl.localhost\kali-linux\home\lev",
            r"\\?\UNC\wsl.localhost\kali-linux\home\lev",
            r"\\?\C:\Users\Lev",
            r"dir/name\part.txt",
            "a/b/",
        ] {
            assert_eq!(render(path, PathStyle::Full, home), path, "Full changed {path:?}");
            assert_eq!(render(path, PathStyle::Full, None), path, "Full changed {path:?}");
        }
    }

    #[test]
    fn split_recognizes_roots_before_separators() {
        let cases: &[(&str, (&str, &str, &str))] = &[
            ("", ("", "", "")),
            ("/", ("/", "", "")),
            (r"C:\", (r"C:\", "", "")),
            ("C:", ("C:", "", "")),
            ("C:foo", ("C:", "", "foo")),
            (r"C:foo\bar", ("C:", r"f\", "bar")),
            ("f.txt", ("", "", "f.txt")),
            ("/f.txt", ("/", "", "f.txt")),
            ("a/b/", ("", "a/", "b")),
            (r"C:\Program Files\Git", (r"C:\", r"P\", "Git")),
            (r"\\server\share\a\b.txt", (r"\\server\share\", r"a\", "b.txt")),
            (
                r"\\wsl.localhost\kali-linux\home\lev\x.rs",
                (r"\\wsl.localhost\kali-linux\", r"h\l\", "x.rs"),
            ),
            (r"\\?\C:\Users\Lev\x", (r"\\?\C:\", r"U\L\", "x")),
            (r"\\?\UNC\server\share\a\b", (r"\\?\UNC\server\share\", r"a\", "b")),
        ];
        for (input, (root, parent, name)) in cases {
            let parts = split(input, PathStyle::Fish, None);
            assert_eq!(
                (parts.root.as_str(), parts.parent.as_str(), parts.name.as_str()),
                (*root, *parent, *name),
                "split({input:?})"
            );
        }
    }

    /// Backslash and `:` are legal in a Unix filename, so a path that does
    /// not match a Windows root prefix must split only on `/`.
    #[test]
    fn a_unix_filename_may_contain_a_backslash_or_colon() {
        let parts = split(r"dir/name\part.txt", PathStyle::Fish, None);
        assert_eq!(parts.root, "");
        assert_eq!(parts.parent, "d/");
        assert_eq!(parts.name, r"name\part.txt");

        let parts = split(r"dir/name:\part", PathStyle::Fish, None);
        assert_eq!(parts.name, r"name:\part");
    }

    /// Windows paths spelled with forward slashes split on either separator
    /// and re-join with a backslash.
    #[test]
    fn a_drive_path_accepts_forward_slashes() {
        assert_eq!(render("C:/Users/Lev/x.rs", PathStyle::Fish, None), r"C:\U\L\x.rs");
    }

    #[test]
    fn fish_abbreviates_parents_and_keeps_a_leading_dot() {
        assert_eq!(render("path/to/file.txt", PathStyle::Fish, None), "p/t/file.txt");
        assert_eq!(render("/a/.config/nvim/init.lua", PathStyle::Fish, None), "/a/.c/n/init.lua");
        assert_eq!(render("f.txt", PathStyle::Fish, None), "f.txt");
        assert_eq!(render("/", PathStyle::Fish, None), "/");
        assert_eq!(render("", PathStyle::Fish, None), "");
    }

    /// Zed gets the same input classes as Fish — drive-relative, UNC, dotted,
    /// trailing separator, empty, root-only — because the reorder is where a
    /// root can most easily be dropped or duplicated.
    #[test]
    fn zed_swaps_the_name_ahead_of_the_parent_and_keeps_the_root() {
        assert_eq!(render("path/to/file.txt", PathStyle::Zed, None), "file.txt path/to/");
        assert_eq!(render("/a/b/c.txt", PathStyle::Zed, None), "c.txt /a/b/");
        // No parent: the bare name, no trailing space — but the root stays.
        assert_eq!(render("f.txt", PathStyle::Zed, None), "f.txt");
        assert_eq!(render("/f.txt", PathStyle::Zed, None), "/f.txt");
        assert_eq!(render(r"C:\f.txt", PathStyle::Zed, None), r"C:\f.txt");
        assert_eq!(render("", PathStyle::Zed, None), "");
        assert_eq!(render("/", PathStyle::Zed, None), "/");
        assert_eq!(render("C:", PathStyle::Zed, None), "C:");
        // Drive-relative: no separator may appear after the root.
        assert_eq!(render(r"C:foo\bar", PathStyle::Zed, None), r"bar C:foo\");
        assert_eq!(render("a/b/", PathStyle::Zed, None), "b a/");
        assert_eq!(render("/a/.config/init.lua", PathStyle::Zed, None), "init.lua /a/.config/");
        assert_eq!(
            render(r"\\wsl.localhost\kali-linux\home\lev\x.rs", PathStyle::Zed, None),
            r"x.rs \\wsl.localhost\kali-linux\home\lev\"
        );
    }

    #[test]
    fn home_collapses_for_fish_and_zed_only() {
        let home = Some("/home/lev");
        assert_eq!(render("/home/lev/Git/x/y.rs", PathStyle::Fish, home), "~/G/x/y.rs");
        assert_eq!(render("/home/lev/Git/x/y.rs", PathStyle::Zed, home), "y.rs ~/Git/x/");
        assert_eq!(render("/home/lev", PathStyle::Fish, home), "~");
        assert_eq!(render("/home/lev/Git/x/y.rs", PathStyle::Full, home), "/home/lev/Git/x/y.rs");
        // A sibling directory whose name merely starts with the home prefix
        // is not inside it.
        assert_eq!(render("/home/levi/x.rs", PathStyle::Fish, home), "/h/l/x.rs");
        // No home, no guess.
        assert_eq!(render("/home/lev/Git/y.rs", PathStyle::Fish, None), "/h/l/G/y.rs");
    }

    #[test]
    fn home_matching_is_case_and_separator_insensitive_on_windows_paths() {
        let home = Some(r"C:\Users\Lev");
        assert_eq!(render(r"c:\users\lev\Git\y.rs", PathStyle::Fish, home), r"~\G\y.rs");
        assert_eq!(render("C:/Users/Lev/Git/y.rs", PathStyle::Fish, home), r"~\G\y.rs");
        // NTFS folds case past ASCII, so `to_ascii_lowercase` is not enough.
        assert_eq!(
            render(r"c:\üsers\lev\Git\y.rs", PathStyle::Fish, Some(r"C:\Üsers\Lev")),
            r"~\G\y.rs"
        );
        // POSIX paths compare exactly — case matters on a Unix filesystem.
        assert_eq!(render("/HOME/LEV/y.rs", PathStyle::Fish, Some("/home/lev")), "/H/L/y.rs");
        // A home of nothing but separators would collapse every absolute path.
        assert_eq!(render("/a/b.rs", PathStyle::Fish, Some("/")), "/a/b.rs");
    }

    /// The distro is the UNC *share*, so it is part of the root and never
    /// abbreviates away.
    #[test]
    fn a_wsl_unc_root_is_never_abbreviated() {
        assert_eq!(
            render(r"\\wsl.localhost\kali-linux\home\lev\x.rs", PathStyle::Fish, None),
            r"\\wsl.localhost\kali-linux\h\l\x.rs"
        );
    }

    /// `split_root` and `strip_home` index by byte, and a slice landing mid
    /// character panics rather than degrading — so a non-ASCII path is not an
    /// edge case, it is a crash waiting for a user with an accent in a
    /// directory name.
    #[test]
    fn multibyte_paths_do_not_panic() {
        for path in ["Ä/Ö/ü.rs", "/日本/語/x.rs", "Ä:foo", "日本語", "/Ä", "C:Ä\\ö"] {
            let _ = render(path, PathStyle::Fish, Some("/日本"));
            let _ = render(path, PathStyle::Zed, Some("Ä"));
            let _ = render(path, PathStyle::Fish, None);
        }
    }
}
