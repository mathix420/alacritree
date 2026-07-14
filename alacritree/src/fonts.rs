//! Resolve font faces and register them as egui font families.
//!
//! Four faces are loaded so ANSI bold/italic cells use real Bold/Italic
//! glyphs.  On Unix we go through libfontconfig directly (same pattern flow
//! as `crossfont::ft::FreeTypeRasterizer::get_face`) — `fc-match` on the CLI
//! mishandles `family:weight=bold` patterns when the family is an `<alias>`,
//! so building the pattern programmatically is what makes weight/slant pick
//! the real variant for aliased families.
//!
//! Beyond the four explicit faces we ask fontconfig for a `FcFontSort`
//! Unicode-coverage-trimmed list and register every entry as a fallback.
//! egui resolves glyphs by walking each family's font list in order, so
//! this is what mirrors alacritty/crossfont's per-glyph fallback for
//! symbols and box-drawing characters that aren't in the primary face.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use egui::{Context, FontData, FontDefinitions, FontFamily, FontTweak};

use crate::config::FontConfig;

/// Hard cap on fallback faces.  fontconfig's trimmed sort tops out at a few
/// dozen on a typical system; this just bounds startup memory and parse cost
/// when someone has hundreds of fonts installed.
const MAX_FALLBACK_FACES: usize = 32;

pub const BOLD_FAMILY: &str = "alacritree_bold";
pub const ITALIC_FAMILY: &str = "alacritree_italic";
pub const BOLD_ITALIC_FAMILY: &str = "alacritree_bold_italic";

const NORMAL_FONT_ID: &str = "alacritree_terminal_normal";
const BOLD_FONT_ID: &str = "alacritree_terminal_bold";
const ITALIC_FONT_ID: &str = "alacritree_terminal_italic";
const BOLD_ITALIC_FONT_ID: &str = "alacritree_terminal_bold_italic";

#[derive(Clone, Copy)]
enum Variant {
    Normal,
    Bold,
    Italic,
    BoldItalic,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::Normal => "regular",
            Variant::Bold => "bold",
            Variant::Italic => "italic",
            Variant::BoldItalic => "bold italic",
        }
    }
}

/// Platform default that mirrors `crossfont::FontDescription::default`.  Used
/// when the user hasn't set `[font.normal] family`, so alacritree picks the
/// same face alacritty would pick from the same (empty) config.
const DEFAULT_FAMILY: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(windows) {
    "Consolas"
} else {
    "monospace"
};

/// Where `scanned_coverage` persists its results.  `Standard` resolves the
/// per-user location lazily at scan time; `Fixed` pins the cache to a given
/// file — or disables it with `None` — so tests never read or write the
/// user's real cache.
#[cfg(not(unix))]
#[derive(Default)]
enum CacheLocation {
    #[default]
    Standard,
    // Only tests pin the location; production always resolves `Standard`.
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Option<PathBuf>),
}

/// Lazily-loaded system font database shared by every resolution within one
/// `install_terminal_fonts` call.  Loading is deferred so Unix systems where
/// fontconfig answers everything never pay for a fontdb scan.
#[derive(Default)]
struct SystemFonts {
    db: OnceCell<fontdb::Database>,
    #[cfg(not(unix))]
    coverage: OnceCell<Vec<(coverage::Candidate, coverage::Coverage)>>,
    #[cfg(not(unix))]
    cache_location: CacheLocation,
}

impl SystemFonts {
    /// Pin the coverage cache to `cache_path`, or disable it with `None`.
    /// Compiled on Unix too (where there is no coverage cache and the
    /// location is ignored) so platform-neutral tests can call it.
    #[cfg(test)]
    fn with_cache_dir(cache_path: Option<PathBuf>) -> Self {
        #[cfg(unix)]
        let _ = cache_path;
        Self {
            #[cfg(not(unix))]
            cache_location: CacheLocation::Fixed(cache_path),
            ..Self::default()
        }
    }

    fn db(&self) -> &fontdb::Database {
        self.db.get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            db
        })
    }

    /// Scan every system face's cmap once per install; all four variant
    /// chains reorder and trim this shared list.
    #[cfg(not(unix))]
    fn scanned_coverage(&self) -> &[(coverage::Candidate, coverage::Coverage)] {
        self.coverage.get_or_init(|| {
            let cache_path = match &self.cache_location {
                CacheLocation::Standard => disk_cache::default_cache_path(),
                CacheLocation::Fixed(path) => path.clone(),
            };
            scan_coverage(self.db(), cache_path.as_deref())
        })
    }
}

/// Scan every system face's cmap, reusing ranges from `cache_path` for files
/// whose size and mtime still match a prior scan.  `cache_path` is a
/// parameter (rather than always `disk_cache::default_cache_path()`) so
/// tests can point it at a scratch directory instead of the real
/// `%LOCALAPPDATA%`.
#[cfg(not(unix))]
fn scan_coverage(
    db: &fontdb::Database,
    cache_path: Option<&Path>,
) -> Vec<(coverage::Candidate, coverage::Coverage)> {
    let started = std::time::Instant::now();
    let cache = cache_path.and_then(disk_cache::load).unwrap_or_default();
    let mut stat_memo: HashMap<PathBuf, Option<(u64, u64)>> = HashMap::new();
    let mut fresh_files: HashMap<String, disk_cache::CachedFile> = HashMap::new();
    let mut scanned = Vec::new();
    let mut hits = 0usize;
    let mut any_fresh = false;

    for face in db.faces() {
        let (path, face_index) = match &face.source {
            fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => (p.clone(), face.index),
            // Embedded faces aren't path-addressable by our loader.
            fontdb::Source::Binary(_) => continue,
        };
        let path_key = path.to_string_lossy().into_owned();
        let stat = *stat_memo.entry(path.clone()).or_insert_with(|| disk_cache::stat_file(&path));

        let cached_ranges = stat.and_then(|(size, mtime_millis)| {
            let cached_file = cache.get(&path_key)?;
            (cached_file.size == size && cached_file.mtime_millis == mtime_millis)
                .then(|| cached_file.faces.get(&face_index).cloned())
                .flatten()
        });

        let cov = match cached_ranges.and_then(coverage::Coverage::from_stored_ranges) {
            Some(cov) => {
                hits += 1;
                cov
            },
            None => {
                any_fresh = true;
                let Some(cov) = db
                    .with_face_data(face.id, |data, index| {
                        let parsed = ttf_parser::Face::parse(data, index).ok()?;
                        cmap_coverage(&parsed)
                    })
                    .flatten()
                else {
                    log::debug!("skipping unparseable font {}", path.display());
                    continue;
                };
                cov
            },
        };

        if let Some((size, mtime_millis)) = stat {
            fresh_files
                .entry(path_key)
                .or_insert_with(|| disk_cache::CachedFile {
                    size,
                    mtime_millis,
                    faces: HashMap::new(),
                })
                .faces
                .insert(face_index, cov.ranges().to_vec());
        }

        let family = face.families.first().map(|(name, _)| name.clone()).unwrap_or_default();
        scanned.push((
            coverage::Candidate {
                path,
                face_index,
                family,
                weight: face.weight.0,
                italic: face.style != fontdb::Style::Normal,
                monospaced: face.monospaced,
                bytes: stat.map_or(0, |(size, _)| size),
            },
            cov,
        ));
    }

    // A cache that was absent or invalid produced zero hits, so every face
    // above went through the fresh-parse branch and `any_fresh` is already
    // true; no separate "was the cache valid" bookkeeping is needed.
    if any_fresh {
        if let Some(cache_path) = cache_path {
            disk_cache::write(cache_path, &fresh_files);
        }
    }

    log::info!(
        "scanned {} font faces for fallback coverage in {} ms ({} from cache)",
        scanned.len(),
        started.elapsed().as_millis(),
        hits
    );
    scanned
}

/// Persists the coverage scan across launches, keyed by each font file's
/// size and mtime.  A custom binary format (rather than a serde crate) keeps
/// this cache std-only; corruption or a version mismatch just means the next
/// launch rescans, so the format has no need to be self-describing beyond a
/// magic/version check.
#[cfg(not(unix))]
mod disk_cache {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    const MAGIC: &[u8; 4] = b"ATCC";
    const VERSION: u32 = 1;

    pub struct CachedFile {
        pub size: u64,
        pub mtime_millis: u64,
        pub faces: HashMap<u32, Vec<(u32, u32)>>,
    }

    pub fn default_cache_path() -> Option<PathBuf> {
        let local_app_data = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(local_app_data).join("alacritree").join("coverage-cache.v1.bin"))
    }

    /// A file's identity for cache purposes: byte size plus modification
    /// time.  Either changing is treated as "this file might have new
    /// glyphs" and forces a rescan of every face in it.
    pub fn stat_file(path: &Path) -> Option<(u64, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?;
        let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
        Some((meta.len(), millis))
    }

    pub fn load(path: &Path) -> Option<HashMap<String, CachedFile>> {
        let bytes = std::fs::read(path).ok()?;
        parse(&bytes)
    }

    fn read_bytes<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
        let end = cursor.checked_add(len)?;
        let slice = bytes.get(*cursor..end)?;
        *cursor = end;
        Some(slice)
    }

    fn read_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
        Some(u32::from_le_bytes(read_bytes(bytes, cursor, 4)?.try_into().ok()?))
    }

    fn read_u64(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
        Some(u64::from_le_bytes(read_bytes(bytes, cursor, 8)?.try_into().ok()?))
    }

    fn parse(bytes: &[u8]) -> Option<HashMap<String, CachedFile>> {
        let cursor = &mut 0usize;
        if read_bytes(bytes, cursor, 4)? != MAGIC {
            return None;
        }
        if read_u32(bytes, cursor)? != VERSION {
            return None;
        }
        let file_count = read_u32(bytes, cursor)?;
        // Counts are untrusted until the reads they promise succeed, so no
        // pre-reservation: a corrupt count must fail at the bounds check, not
        // as a giant allocation that aborts the process.
        let mut files = HashMap::new();
        for _ in 0..file_count {
            let path_len = read_u32(bytes, cursor)? as usize;
            let path = String::from_utf8(read_bytes(bytes, cursor, path_len)?.to_vec()).ok()?;
            let size = read_u64(bytes, cursor)?;
            let mtime_millis = read_u64(bytes, cursor)?;
            let face_count = read_u32(bytes, cursor)?;
            let mut faces = HashMap::new();
            for _ in 0..face_count {
                let face_index = read_u32(bytes, cursor)?;
                let range_count = read_u32(bytes, cursor)?;
                let mut ranges = Vec::new();
                for _ in 0..range_count {
                    let start = read_u32(bytes, cursor)?;
                    let end = read_u32(bytes, cursor)?;
                    ranges.push((start, end));
                }
                faces.insert(face_index, ranges);
            }
            files.insert(path, CachedFile { size, mtime_millis, faces });
        }
        Some(files)
    }

    /// Font problems must never fail startup, so every I/O error here is
    /// swallowed after a debug log; the next launch simply rescans.
    pub fn write(path: &Path, files: &HashMap<String, CachedFile>) {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (file_path, cached) in files {
            let path_bytes = file_path.as_bytes();
            buf.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(path_bytes);
            buf.extend_from_slice(&cached.size.to_le_bytes());
            buf.extend_from_slice(&cached.mtime_millis.to_le_bytes());
            buf.extend_from_slice(&(cached.faces.len() as u32).to_le_bytes());
            for (face_index, ranges) in &cached.faces {
                buf.extend_from_slice(&face_index.to_le_bytes());
                buf.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
                for &(start, end) in ranges {
                    buf.extend_from_slice(&start.to_le_bytes());
                    buf.extend_from_slice(&end.to_le_bytes());
                }
            }
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::debug!("could not create font coverage cache dir {}: {e}", parent.display());
                return;
            }
        }
        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, &buf) {
            log::debug!("could not write font coverage cache {}: {e}", tmp_path.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            log::debug!("could not install font coverage cache {}: {e}", path.display());
        }
    }
}

/// One face in the order egui consults it: the primary, then the user's
/// `[font] fallback` entries, then the automatic system chain.  Colour-only
/// faces appear here even though they are withheld from egui, because the
/// colour glyph renderer resolves against this same order and must see the
/// face the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainFace {
    pub path: PathBuf,
    pub face_index: u32,
    /// egui cannot rasterize this face; only the colour glyph renderer can.
    pub color_only: bool,
}

/// Bookkeeping shared by all fallback registration within one install: which
/// files already back an egui font, which font id serves each file (so one
/// file can join several variants' family lists without duplicate data), and
/// which user entries have already produced a warning.
#[derive(Default)]
struct FallbackBook {
    loaded_paths: HashSet<PathBuf>,
    ids_by_path: HashMap<PathBuf, String>,
    warned_entries: HashSet<String>,
    /// Faces withheld from egui because they carry no outlines.  Kept so a
    /// later variant's chain doesn't re-read and re-probe the same file.
    color_only: HashSet<PathBuf>,
    /// Height ratio of the primary normal face, used to normalize fallback
    /// faces to the same visual size at a given point size.
    primary_height_ratio: Option<f32>,
    /// The normal-variant chain, in resolution order, for the colour renderer.
    chain: Vec<ChainFace>,
}

impl FallbackBook {
    /// Record a face in the normal-variant chain.  Other variants re-walk the
    /// same fallbacks and must not append to it a second time.
    fn extend_chain(&mut self, variant: Variant, path: &Path, face_index: u32, color_only: bool) {
        if !matches!(variant, Variant::Normal) {
            return;
        }
        let face = ChainFace { path: path.to_path_buf(), face_index, color_only };
        if !self.chain.contains(&face) {
            self.chain.push(face);
        }
    }
}

/// Whether `face` can hand egui an outline for `c`.  A face may claim a
/// character in its cmap and still have nothing to draw for it, which is what
/// makes a cell go blank.
#[cfg(test)]
#[cfg(test)]
pub fn face_outlines_char(face: &ChainFace, c: char) -> bool {
    let Ok(data) = std::fs::read(&face.path) else {
        return false;
    };
    let Ok(parsed) = ttf_parser::Face::parse(&data, face.face_index) else {
        return false;
    };
    let Some(glyph) = parsed.glyph_index(c) else {
        return false;
    };
    parsed.outline_glyph(glyph, &mut DiscardOutline).is_some()
}

struct DiscardOutline;
impl ttf_parser::OutlineBuilder for DiscardOutline {
    fn move_to(&mut self, _: f32, _: f32) {}
    fn line_to(&mut self, _: f32, _: f32) {}
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
    fn close(&mut self) {}
}

/// egui rasterizes `glyf`/`CFF` outlines and nothing else.  COLR and CBDT
/// emoji fonts keep their artwork in colour tables and leave the base glyphs
/// empty, so egui would claim every character such a face covers and then paint
/// a blank cell.  Those faces are withheld from egui and drawn by `color_glyph`
/// instead.
///
/// A table-level check is not enough — Twemoji has a `glyf` table full of empty
/// shapes — so this samples covered codepoints and asks for a real outline.
///
/// A face that does not parse is *not* colour-only: it is a font we know
/// nothing about, and withholding it here would quietly change which fonts
/// reach egui at all.
fn is_color_only(data: &[u8], index: u32) -> bool {
    /// Enough to clear any run of empty glyphs at the head of a cmap without
    /// walking a 20 000-glyph CJK face.
    const PROBE_LIMIT: usize = 64;

    let Ok(face) = ttf_parser::Face::parse(data, index) else {
        return false;
    };
    let Some(cmap) = face.tables().cmap else {
        return false;
    };

    let mut probed = 0usize;
    let mut outlined = false;
    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        subtable.codepoints(|cp| {
            if outlined || probed >= PROBE_LIMIT {
                return;
            }
            let Some(glyph) = char::from_u32(cp).and_then(|c| face.glyph_index(c)) else {
                return;
            };
            probed += 1;
            outlined |= face.outline_glyph(glyph, &mut DiscardOutline).is_some();
        });
        if outlined {
            break;
        }
    }
    probed > 0 && !outlined
}

/// Register the user-configured `[font] fallback` entries for one variant.
/// They slot between the primary face and the automatic system chain, in
/// list order.  Entries are family names or font file paths, resolved with
/// the variant's weight/slant so bold cells cascade through bold fallbacks.
fn register_user_fallbacks(
    defs: &mut FontDefinitions,
    entries: &[String],
    variant: Variant,
    targets: &[FontFamily],
    fonts: &SystemFonts,
    book: &mut FallbackBook,
) {
    for entry in entries {
        let Some(resolved) = resolve_face(entry, None, variant, fonts) else {
            if book.warned_entries.insert(entry.clone()) {
                log::warn!("font.fallback entry '{entry}' did not resolve to any font");
            }
            continue;
        };
        if book.color_only.contains(&resolved.path) {
            book.extend_chain(variant, &resolved.path, resolved.face_index, true);
            continue;
        }
        if let Some(id) = book.ids_by_path.get(&resolved.path) {
            for family in targets {
                defs.families.entry(family.clone()).or_default().push(id.clone());
            }
            book.extend_chain(variant, &resolved.path, resolved.face_index, false);
            continue;
        }
        if book.loaded_paths.contains(&resolved.path) {
            // Already registered as a primary face, which sits ahead of every
            // fallback in the family lists; appending it again is pointless.
            continue;
        }
        let bytes = match map_font_file(&resolved.path) {
            Ok(b) => b,
            Err(e) => {
                log::debug!("skipping fallback font {}: {e}", resolved.path.display());
                continue;
            },
        };
        if is_color_only(bytes, resolved.face_index) {
            log::debug!(
                "font.fallback entry '{entry}' has no outlines; drawing it as colour glyphs"
            );
            book.color_only.insert(resolved.path.clone());
            book.extend_chain(variant, &resolved.path, resolved.face_index, true);
            continue;
        }
        let id = format!("alacritree_fallback_{}", defs.font_data.len());
        let tweak = fallback_tweak(book.primary_height_ratio, bytes, resolved.face_index);
        let data = FontData { index: resolved.face_index, tweak, ..FontData::from_static(bytes) };
        defs.font_data.insert(id.clone(), Arc::new(data));
        for family in targets {
            defs.families.entry(family.clone()).or_default().push(id.clone());
        }
        book.extend_chain(variant, &resolved.path, resolved.face_index, false);
        book.loaded_paths.insert(resolved.path.clone());
        book.ids_by_path.insert(resolved.path, id);
    }
}

fn variant_query(variant: Variant) -> (fontdb::Weight, fontdb::Style) {
    match variant {
        Variant::Normal => (fontdb::Weight::NORMAL, fontdb::Style::Normal),
        Variant::Bold => (fontdb::Weight::BOLD, fontdb::Style::Normal),
        Variant::Italic => (fontdb::Weight::NORMAL, fontdb::Style::Italic),
        Variant::BoldItalic => (fontdb::Weight::BOLD, fontdb::Style::Italic),
    }
}

/// A font's visual height for a given point size is
/// `(ascender - descender) / units_per_em`, which varies between fonts.
fn face_height_ratio(data: &[u8], index: u32) -> Option<f32> {
    let face = ttf_parser::Face::parse(data, index).ok()?;
    let units = f32::from(face.units_per_em());
    let height = f32::from(face.ascender()) - f32::from(face.descender());
    (units > 0.0 && height > 0.0).then(|| height / units)
}

/// Scale a fallback face so one point of it is as tall as one point of the
/// primary face; without this, powerline caps, emoji, and CJK glyphs from
/// fallback fonts overshoot or undershoot the cell.  Clamped so a face with
/// broken metrics cannot render unreadably small or huge.
fn fallback_tweak(primary_ratio: Option<f32>, data: &[u8], index: u32) -> FontTweak {
    let scale = match (primary_ratio, face_height_ratio(data, index)) {
        (Some(primary), Some(own)) => (primary / own).clamp(0.5, 2.0),
        _ => 1.0,
    };
    FontTweak { scale, ..FontTweak::default() }
}

/// Register the terminal faces with egui and return the normal-variant
/// fallback chain, in the order egui consults it, for the colour glyph
/// renderer to resolve against.
pub fn install_terminal_fonts(ctx: &Context, font: &FontConfig) -> Vec<ChainFace> {
    let (normal, bold, italic, bold_italic) =
        (&font.normal, &font.bold, &font.italic, &font.bold_italic);
    let family = normal.family.as_deref().unwrap_or(DEFAULT_FAMILY);
    let fonts = SystemFonts::default();

    // The variant lookups compare their resolved path against this one to
    // detect when fontconfig substituted the regular face for a missing variant.
    let normal_match = match resolve_face(family, normal.style.as_deref(), Variant::Normal, &fonts)
    {
        Some(m) => m,
        None => {
            log::warn!("could not resolve font '{family}'; using bundled monospace");
            return Vec::new();
        },
    };
    let normal_bytes = match map_font_file(&normal_match.path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("could not read font file {}: {e}", normal_match.path.display());
            return Vec::new();
        },
    };

    // Bold/italic/bold-italic inherit the normal family unless overridden.
    let bold_family = bold.family.as_deref().unwrap_or(family);
    let italic_family = italic.family.as_deref().unwrap_or(family);
    let bold_italic_family = bold_italic.family.as_deref().unwrap_or(family);

    let bold_bytes =
        load_variant(bold_family, bold.style.as_deref(), Variant::Bold, &normal_match.path, &fonts);
    let italic_bytes = load_variant(
        italic_family,
        italic.style.as_deref(),
        Variant::Italic,
        &normal_match.path,
        &fonts,
    );
    let bold_italic_bytes = load_variant(
        bold_italic_family,
        bold_italic.style.as_deref(),
        Variant::BoldItalic,
        &normal_match.path,
        &fonts,
    );

    let mut defs = FontDefinitions::default();

    insert_face(&mut defs, NORMAL_FONT_ID, normal_bytes);
    register_default_family(&mut defs, FontFamily::Monospace, NORMAL_FONT_ID);
    register_default_family(&mut defs, FontFamily::Proportional, NORMAL_FONT_ID);

    register_variant(&mut defs, BOLD_FONT_ID, BOLD_FAMILY, bold_bytes, normal_bytes);
    register_variant(&mut defs, ITALIC_FONT_ID, ITALIC_FAMILY, italic_bytes, normal_bytes);
    register_variant(
        &mut defs,
        BOLD_ITALIC_FONT_ID,
        BOLD_ITALIC_FAMILY,
        bold_italic_bytes,
        normal_bytes,
    );

    let mut book = FallbackBook::default();
    book.loaded_paths.insert(normal_match.path.clone());
    book.primary_height_ratio = face_height_ratio(normal_bytes, normal_match.face_index);
    // The primary is registered unconditionally above, so it heads the chain
    // as an egui-drawable face even in the pathological case of a colour-only
    // font being configured as `[font.normal]`.
    book.chain.push(ChainFace {
        path: normal_match.path.clone(),
        face_index: normal_match.face_index,
        color_only: false,
    });

    // Each variant gets its own fallback chain seeded from that variant's
    // configured family — same as crossfont's per-FontDesc fallback search,
    // so bold cells cascade through bold's chain and so on.
    let normal_targets = [FontFamily::Monospace, FontFamily::Proportional];
    let variant_targets =
        [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY].map(|n| [FontFamily::Name(n.into())]);
    let seeds: [(&str, Option<&str>, Variant, &[FontFamily]); 4] = [
        (family, normal.style.as_deref(), Variant::Normal, &normal_targets),
        (bold_family, bold.style.as_deref(), Variant::Bold, &variant_targets[0]),
        (italic_family, italic.style.as_deref(), Variant::Italic, &variant_targets[1]),
        (
            bold_italic_family,
            bold_italic.style.as_deref(),
            Variant::BoldItalic,
            &variant_targets[2],
        ),
    ];
    for (family, style, variant, targets) in seeds {
        register_user_fallbacks(&mut defs, &font.fallback, variant, targets, &fonts, &mut book);
        register_fallback_faces(&mut defs, family, style, variant, targets, &fonts, &mut book);
    }

    ctx.set_fonts(defs);
    book.chain
}

/// Append every font from fontconfig's trimmed sort to `target_families` so
/// that glyphs missing from the primary face (symbols, box drawing, emoji)
/// fall through to a system font that has them.  Mirrors what crossfont does
/// per-glyph in upstream alacritty.
fn register_fallback_faces(
    defs: &mut FontDefinitions,
    family: &str,
    style: Option<&str>,
    variant: Variant,
    target_families: &[FontFamily],
    fonts: &SystemFonts,
    book: &mut FallbackBook,
) {
    // Only primaries lack an id to reuse; everything else the chain finds
    // can join this variant's family list without reloading.
    let primaries: HashSet<PathBuf> = book
        .loaded_paths
        .iter()
        .filter(|path| !book.ids_by_path.contains_key(*path))
        .cloned()
        .collect();
    let fallbacks =
        gather_fallback_faces(family, style, variant, &primaries, MAX_FALLBACK_FACES, fonts);
    if fallbacks.is_empty() {
        return;
    }

    for face in fallbacks {
        if book.color_only.contains(&face.path) {
            book.extend_chain(variant, &face.path, face.face_index, true);
            continue;
        }
        if let Some(id) = book.ids_by_path.get(&face.path) {
            for family in target_families {
                defs.families.entry(family.clone()).or_default().push(id.clone());
            }
            book.extend_chain(variant, &face.path, face.face_index, false);
            continue;
        }
        let bytes = match map_font_file(&face.path) {
            Ok(b) => b,
            Err(e) => {
                log::debug!("skipping fallback font {}: {e}", face.path.display());
                continue;
            },
        };
        if is_color_only(bytes, face.face_index) {
            book.color_only.insert(face.path.clone());
            book.extend_chain(variant, &face.path, face.face_index, true);
            continue;
        }
        let id = format!("alacritree_fallback_{}", defs.font_data.len());
        let tweak = fallback_tweak(book.primary_height_ratio, bytes, face.face_index);
        let data = FontData { index: face.face_index, tweak, ..FontData::from_static(bytes) };
        defs.font_data.insert(id.clone(), Arc::new(data));

        for family in target_families {
            defs.families.entry(family.clone()).or_default().push(id.clone());
        }
        book.extend_chain(variant, &face.path, face.face_index, false);
        book.loaded_paths.insert(face.path.clone());
        book.ids_by_path.insert(face.path, id);
    }
}

struct FallbackFace {
    path: PathBuf,
    face_index: u32,
}

#[cfg(unix)]
fn gather_fallback_faces(
    family: &str,
    style: Option<&str>,
    variant: Variant,
    skip_paths: &HashSet<PathBuf>,
    limit: usize,
    _fonts: &SystemFonts,
) -> Vec<FallbackFace> {
    fontconfig_resolve::sorted_fallbacks(family, style, variant, skip_paths, limit)
}

#[cfg(not(unix))]
fn cmap_coverage(face: &ttf_parser::Face) -> Option<coverage::Coverage> {
    let cmap = face.tables().cmap?;
    let mut codepoints = Vec::new();
    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        subtable.codepoints(|cp| codepoints.push(cp));
    }
    Some(coverage::Coverage::from_codepoints(codepoints))
}

/// Coverage of an already-resolved primary face.  Reads index 0, matching
/// how the primary bytes are handed to egui.
#[cfg(not(unix))]
fn face_coverage_from_path(path: &Path) -> Option<coverage::Coverage> {
    let data = std::fs::read(path).ok()?;
    let parsed = ttf_parser::Face::parse(&data, 0).ok()?;
    cmap_coverage(&parsed)
}

/// The fontdb equivalent of fontconfig's coverage-trimmed FcFontSort: order
/// every system face by affinity to the seed, then keep only faces that add
/// codepoints the seed and earlier picks don't cover.
#[cfg(not(unix))]
fn gather_fallback_faces(
    family: &str,
    style: Option<&str>,
    variant: Variant,
    skip_paths: &HashSet<PathBuf>,
    limit: usize,
    fonts: &SystemFonts,
) -> Vec<FallbackFace> {
    let seed_coverage = resolve_face(family, style, variant, fonts)
        .and_then(|face| face_coverage_from_path(&face.path))
        .unwrap_or_default();

    let mut candidates: Vec<_> = fonts
        .scanned_coverage()
        .iter()
        .filter(|(candidate, _)| !skip_paths.contains(&candidate.path))
        .cloned()
        .collect();
    let (weight, db_style) = variant_query(variant);
    coverage::order_candidates(
        &mut candidates,
        family,
        weight.0,
        db_style != fontdb::Style::Normal,
    );

    coverage::trim_by_coverage(candidates, &seed_coverage, limit)
        .into_iter()
        .map(|candidate| FallbackFace { path: candidate.path, face_index: candidate.face_index })
        .collect()
}

/// Face bytes reach egui as a mapping rather than a buffer.  `FontData` holds
/// a `Cow<'static, [u8]>` and epaint clones the whole buffer of every owned
/// entry when it builds the `ab_glyph` face, so a face handed over as bytes
/// costs its file size twice for the life of the process.  Handed over
/// borrowed it costs nothing: the pages stay file-backed, and a fallback face
/// no cell ever renders from resides as its table headers instead of its full
/// size.  This is what FreeType does for alacritty and wezterm, which is why
/// they carry a long fallback chain for a fraction of the memory.
///
/// A mapping outlives the egui context it is registered with, which lives as
/// long as the process — so the mappings do too.  Keying them by path is what
/// bounds that: a face maps once no matter how many variant chains list it,
/// and a second `install_terminal_fonts` reuses the mappings of the first.
static FONT_MAPS: OnceLock<Mutex<HashMap<PathBuf, &'static [u8]>>> = OnceLock::new();

fn map_font_file(path: &Path) -> std::io::Result<&'static [u8]> {
    let mut maps = FONT_MAPS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(bytes) = maps.get(path) {
        return Ok(bytes);
    }

    let file = std::fs::File::open(path)?;
    // SAFETY: the mapping is read-only and never written through.  Rewriting a
    // font file in place while it is mapped would fault the process — the same
    // bet FreeType makes when it maps a face, and fontdb already maps every
    // system font to scan its cmap.
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    let bytes: &'static [u8] = Box::leak(Box::new(mmap));
    maps.insert(path.to_path_buf(), bytes);
    Ok(bytes)
}

fn insert_face(defs: &mut FontDefinitions, id: &str, bytes: &'static [u8]) {
    defs.font_data.insert(id.to_string(), Arc::new(FontData::from_static(bytes)));
}

fn register_default_family(defs: &mut FontDefinitions, family: FontFamily, id: &str) {
    defs.families.entry(family).or_default().insert(0, id.to_string());
}

fn register_variant(
    defs: &mut FontDefinitions,
    font_id: &str,
    family_name: &str,
    bytes: Option<&'static [u8]>,
    fallback: &'static [u8],
) {
    let bytes = bytes.unwrap_or(fallback);
    insert_face(defs, font_id, bytes);
    defs.families.insert(FontFamily::Name(family_name.into()), vec![font_id.to_string()]);
}

/// Returns the bytes of the variant face if a *real* variant exists, or
/// `None` if the matcher fell back to the normal file.  The caller registers
/// the normal face as a fallback under the variant's family name.
fn load_variant(
    family: &str,
    style: Option<&str>,
    variant: Variant,
    normal_path: &Path,
    fonts: &SystemFonts,
) -> Option<&'static [u8]> {
    let resolved = resolve_face(family, style, variant, fonts)?;
    if resolved.path == normal_path {
        log::debug!(
            "no real {} face for '{family}'; cells with that style will use the regular face",
            variant.label()
        );
        return None;
    }
    match map_font_file(&resolved.path) {
        Ok(b) => Some(b),
        Err(e) => {
            log::warn!(
                "could not read {} font file {}: {e}",
                variant.label(),
                resolved.path.display()
            );
            None
        },
    }
}

struct ResolvedFace {
    path: PathBuf,
    /// Which face inside the file.  A `.ttc` holds several — `Noto Sans Mono
    /// CJK KR` and its `JP` sibling can share one file — so dropping this
    /// would silently load the wrong language's face.
    face_index: u32,
}

#[cfg(unix)]
fn resolve_face(
    family_or_path: &str,
    style: Option<&str>,
    variant: Variant,
    fonts: &SystemFonts,
) -> Option<ResolvedFace> {
    if let Some(face) = resolve_via_path(family_or_path) {
        return Some(face);
    }
    if let Some(face) = fontconfig_resolve::resolve(family_or_path, style, variant) {
        return Some(face);
    }
    // fontdb fallback for the case where libfontconfig isn't available; it
    // doesn't expand <alias> rules, so it's strictly second-best on Unix.
    resolve_via_fontdb(family_or_path, variant, fonts)
}

#[cfg(not(unix))]
fn resolve_face(
    family_or_path: &str,
    _style: Option<&str>,
    variant: Variant,
    fonts: &SystemFonts,
) -> Option<ResolvedFace> {
    if let Some(face) = resolve_via_path(family_or_path) {
        return Some(face);
    }
    resolve_via_fontdb(family_or_path, variant, fonts)
}

fn resolve_via_path(family_or_path: &str) -> Option<ResolvedFace> {
    let path = Path::new(family_or_path);
    if path.is_file() {
        return Some(ResolvedFace { path: path.to_path_buf(), face_index: 0 });
    }
    None
}

fn resolve_via_fontdb(family: &str, variant: Variant, fonts: &SystemFonts) -> Option<ResolvedFace> {
    let (weight, style) = variant_query(variant);
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight,
        stretch: fontdb::Stretch::Normal,
        style,
    };
    let db = fonts.db();
    let face_id = db.query(&query)?;
    let face_info = db.face(face_id)?;
    match &face_info.source {
        // A memory-mapped `SharedFile` still names a real file on disk.
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
            Some(ResolvedFace { path: path.clone(), face_index: face_info.index })
        },
        // Embedded faces aren't path-addressable; we'd have to re-architect
        // the loader to support them and they're rare.
        fontdb::Source::Binary(_) => None,
    }
}

#[cfg(unix)]
mod fontconfig_resolve {
    //! Mirrors `crossfont::ft::FreeTypeRasterizer::get_face`: build a pattern
    //! with family + weight + slant and let `font_match` run substitution.
    //! Doing this in code (vs `fc-match` CLI) is what makes `<alias>` rules
    //! plus weight/slant pick the right variant.

    use std::collections::HashSet;
    use std::ffi::CString;
    use std::path::PathBuf;

    use fontconfig::{
        FC_FAMILY, FC_SLANT, FC_SLANT_ITALIC, FC_SLANT_ROMAN, FC_STYLE, FC_WEIGHT, FC_WEIGHT_BOLD,
        FC_WEIGHT_REGULAR, Fontconfig, Pattern, sort_fonts,
    };

    use super::{FallbackFace, ResolvedFace, Variant};

    pub fn resolve(family: &str, style: Option<&str>, variant: Variant) -> Option<ResolvedFace> {
        let fc = Fontconfig::new()?;
        let mut pattern = Pattern::new(&fc);

        let family_c = CString::new(family).ok()?;
        pattern.add_string(FC_FAMILY, &family_c);

        if let Some(style) = style {
            if let Ok(style_c) = CString::new(style) {
                pattern.add_string(FC_STYLE, &style_c);
            }
        }

        let (weight, slant) = match variant {
            Variant::Normal => (FC_WEIGHT_REGULAR, FC_SLANT_ROMAN),
            Variant::Bold => (FC_WEIGHT_BOLD, FC_SLANT_ROMAN),
            Variant::Italic => (FC_WEIGHT_REGULAR, FC_SLANT_ITALIC),
            Variant::BoldItalic => (FC_WEIGHT_BOLD, FC_SLANT_ITALIC),
        };
        pattern.add_integer(FC_WEIGHT, weight);
        pattern.add_integer(FC_SLANT, slant);

        let matched = pattern.font_match();
        let path = matched.filename()?;
        let face_index = matched.face_index().unwrap_or(0).max(0) as u32;
        Some(ResolvedFace { path: PathBuf::from(path), face_index })
    }

    /// `FcFontSort` with `trim=true` returns fonts in match order, dropping
    /// any whose Unicode coverage is fully covered by an earlier entry.  This
    /// is the same chain `FcFontMatch` walks per glyph when crossfont misses,
    /// so registering it up front in egui gives equivalent coverage.
    pub fn sorted_fallbacks(
        family: &str,
        style: Option<&str>,
        variant: Variant,
        skip_paths: &HashSet<PathBuf>,
        limit: usize,
    ) -> Vec<FallbackFace> {
        let Some(fc) = Fontconfig::new() else {
            return Vec::new();
        };
        let mut pattern = Pattern::new(&fc);

        if let Ok(family_c) = CString::new(family) {
            pattern.add_string(FC_FAMILY, &family_c);
        }
        if let Some(style) = style {
            if let Ok(style_c) = CString::new(style) {
                pattern.add_string(FC_STYLE, &style_c);
            }
        }
        let (weight, slant) = match variant {
            Variant::Normal => (FC_WEIGHT_REGULAR, FC_SLANT_ROMAN),
            Variant::Bold => (FC_WEIGHT_BOLD, FC_SLANT_ROMAN),
            Variant::Italic => (FC_WEIGHT_REGULAR, FC_SLANT_ITALIC),
            Variant::BoldItalic => (FC_WEIGHT_BOLD, FC_SLANT_ITALIC),
        };
        pattern.add_integer(FC_WEIGHT, weight);
        pattern.add_integer(FC_SLANT, slant);

        // FcFontSort requires FcConfigSubstitute + FcDefaultSubstitute to have
        // been applied to the input pattern; otherwise <alias> rules never
        // expand and the result list misses the fonts the user actually has.
        // The 0.8 fontconfig wrapper keeps those private but applies them as
        // a side effect inside `font_match`, so we run it for the side effect
        // and discard the matched pattern.
        let _ = pattern.font_match();

        let sorted = sort_fonts(&pattern, true);
        let mut out = Vec::with_capacity(limit.min(16));
        for matched in sorted.iter() {
            if out.len() >= limit {
                break;
            }
            let Some(path_str) = matched.filename() else {
                continue;
            };
            let path = PathBuf::from(path_str);
            if skip_paths.contains(&path) {
                continue;
            }
            let face_index = matched.face_index().unwrap_or(0).max(0) as u32;
            out.push(FallbackFace { path, face_index });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_fallback_path_registers_for_every_variant() {
        // A file-path entry resolves to the same file for all four variants;
        // the bytes must be loaded once and the same egui font id appended to
        // each variant's family list (a plain HashSet dedup would starve
        // every variant after the first).
        let path = std::env::temp_dir().join("alacritree_test_user_fallback.ttf");
        std::fs::write(&path, b"egui parses this later; registration only reads bytes").unwrap();

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![path.to_string_lossy().into_owned()];

        let normal_targets = [FontFamily::Monospace];
        register_user_fallbacks(
            &mut defs,
            &entries,
            Variant::Normal,
            &normal_targets,
            &fonts,
            &mut book,
        );
        let bold_targets = [FontFamily::Name(BOLD_FAMILY.into())];
        register_user_fallbacks(
            &mut defs,
            &entries,
            Variant::Bold,
            &bold_targets,
            &fonts,
            &mut book,
        );

        assert_eq!(book.ids_by_path.len(), 1);
        let id = book.ids_by_path.values().next().unwrap();
        assert!(defs.families[&FontFamily::Monospace].contains(id));
        assert!(defs.families[&FontFamily::Name(BOLD_FAMILY.into())].contains(id));

        std::fs::remove_file(&path).ok();
    }

    // epaint clones the whole buffer of every `Cow::Owned` face it parses, so
    // an owned face costs its file size twice for the life of the process.
    #[test]
    fn registered_faces_hand_epaint_borrowed_bytes() {
        let path = std::env::temp_dir().join("alacritree_test_borrowed_bytes.ttf");
        std::fs::write(&path, b"registration only maps and stats bytes").unwrap();

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![path.to_string_lossy().into_owned()];

        let targets = [FontFamily::Monospace];
        register_user_fallbacks(&mut defs, &entries, Variant::Normal, &targets, &fonts, &mut book);

        let id = book.ids_by_path.get(&path).expect("fallback registered");
        let data = &defs.font_data[id];
        assert!(
            matches!(data.font, std::borrow::Cow::Borrowed(_)),
            "{id} owns its bytes; epaint will clone them"
        );

        std::fs::remove_file(&path).ok();
    }

    // Unix-excluded: fontconfig substitutes *some* font for any family name,
    // so an unresolvable entry only exists where fontdb answers the query.
    #[cfg(not(unix))]
    #[test]
    fn unresolved_user_fallback_warns_once_and_adds_nothing() {
        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![String::from("alacritree-no-such-family-6c1e")];
        let before = defs.families[&FontFamily::Monospace].len();

        let targets = [FontFamily::Monospace];
        register_user_fallbacks(&mut defs, &entries, Variant::Normal, &targets, &fonts, &mut book);
        register_user_fallbacks(&mut defs, &entries, Variant::Bold, &targets, &fonts, &mut book);

        assert_eq!(defs.families[&FontFamily::Monospace].len(), before);
        assert_eq!(book.warned_entries.len(), 1);
    }

    #[cfg(not(unix))]
    #[test]
    fn windows_chain_respects_limit_skip_set_and_uniqueness() {
        let fonts = SystemFonts::with_cache_dir(None);
        let skip = HashSet::new();
        let faces = gather_fallback_faces("Consolas", None, Variant::Normal, &skip, 8, &fonts);
        assert!(faces.len() <= 8);
        let mut seen = HashSet::new();
        for face in &faces {
            assert!(!skip.contains(&face.path));
            assert!(seen.insert((face.path.clone(), face.face_index)));
        }
        // On any machine with system fonts the chain must not be empty —
        // that emptiness is the Windows-tofu bug this feature fixes.
        if fonts.db().faces().next().is_some() {
            assert!(!faces.is_empty());
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn later_variants_reuse_faces_loaded_by_an_earlier_chain() {
        let fonts = SystemFonts::with_cache_dir(None);
        let mut defs = FontDefinitions::default();
        let mut book = FallbackBook::default();

        let normal_targets = [FontFamily::Monospace];
        register_fallback_faces(
            &mut defs,
            "Consolas",
            None,
            Variant::Normal,
            &normal_targets,
            &fonts,
            &mut book,
        );
        let normal_ids: HashSet<String> = book.ids_by_path.values().cloned().collect();

        let bold_family = FontFamily::Name(BOLD_FAMILY.into());
        let bold_targets = [bold_family.clone()];
        register_fallback_faces(
            &mut defs,
            "Consolas",
            Some("Bold"),
            Variant::Bold,
            &bold_targets,
            &fonts,
            &mut book,
        );

        if fonts.db().faces().next().is_some() && !normal_ids.is_empty() {
            // The bold chain must be able to reach faces the normal chain already
            // loaded; on any real system at least one top coverage-adder overlaps.
            assert!(defs.families[&bold_family].iter().any(|id| normal_ids.contains(id)));
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn automatic_chain_records_every_loaded_path_in_ids_by_path() {
        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();

        let targets = [FontFamily::Monospace];
        register_fallback_faces(
            &mut defs,
            "Consolas",
            None,
            Variant::Normal,
            &targets,
            &fonts,
            &mut book,
        );

        if fonts.db().faces().next().is_some() {
            let ids_keys: HashSet<_> = book.ids_by_path.keys().cloned().collect();
            assert_eq!(ids_keys, book.loaded_paths);
        }
    }

    #[test]
    fn fallback_tweak_defaults_to_unscaled_for_unparseable_data() {
        let tweak = fallback_tweak(Some(1.2), b"not a font", 0);
        assert_eq!(tweak.scale, 1.0);
    }

    #[test]
    fn user_fallbacks_precede_the_automatic_chain() {
        // User-configured fallbacks slot between the primary face and the
        // automatic system chain, so their font id must land ahead of every
        // id the automatic chain appends afterward in the family list.
        let path = std::env::temp_dir().join("alacritree_test_user_precedes.ttf");
        std::fs::write(&path, b"egui parses this later; registration only reads bytes").unwrap();

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![path.to_string_lossy().into_owned()];
        let targets = [FontFamily::Monospace];

        register_user_fallbacks(&mut defs, &entries, Variant::Normal, &targets, &fonts, &mut book);
        let user_id = book.ids_by_path.get(&path).cloned().unwrap();
        let user_index =
            defs.families[&FontFamily::Monospace].iter().position(|id| *id == user_id).unwrap();
        let before_len = defs.families[&FontFamily::Monospace].len();

        register_fallback_faces(
            &mut defs,
            DEFAULT_FAMILY,
            None,
            Variant::Normal,
            &targets,
            &fonts,
            &mut book,
        );

        let family_list = &defs.families[&FontFamily::Monospace];
        if family_list.len() > before_len {
            for id in &family_list[before_len..] {
                let index = family_list.iter().position(|x| x == id).unwrap();
                assert!(index > user_index);
            }
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn primary_faces_are_never_tweaked() {
        // Fallback faces get a tweak that rescales them to the primary
        // face's height; the primary itself is the scale reference and must
        // never carry a tweak, or scaling would be relative to a moving target.
        let mut defs = FontDefinitions::default();
        insert_face(&mut defs, "test_primary", b"egui parses this later; registration only maps");
        assert_eq!(defs.font_data["test_primary"].tweak, FontTweak::default());
    }

    #[cfg(not(unix))]
    #[test]
    fn fallback_tweak_normalizes_height_to_the_primary_face() {
        // Any real font gives a positive height ratio; scaling it against a
        // primary of half / double its ratio must move scale in that direction.
        let data = std::fs::read("C:/Windows/Fonts/arial.ttf").unwrap();
        let own = face_height_ratio(&data, 0).unwrap();
        assert!(own > 0.0);
        assert_eq!(fallback_tweak(Some(own), &data, 0).scale, 1.0);
        assert!(fallback_tweak(Some(own * 1.5), &data, 0).scale > 1.0);
        assert!(fallback_tweak(Some(own * 0.5), &data, 0).scale < 1.0);
    }

    #[cfg(not(unix))]
    fn scratch_cache_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("alacritree_test_coverage_cache_{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("coverage-cache.v1.bin")
    }

    #[cfg(not(unix))]
    #[test]
    fn coverage_cache_round_trips_across_scans() {
        let cache_path = scratch_cache_path("round_trip");
        std::fs::remove_file(&cache_path).ok();

        let cold_fonts = SystemFonts::with_cache_dir(None);
        let cold = scan_coverage(cold_fonts.db(), Some(&cache_path));
        assert!(cache_path.is_file());

        let warm_fonts = SystemFonts::with_cache_dir(None);
        let warm = scan_coverage(warm_fonts.db(), Some(&cache_path));

        assert_eq!(cold, warm);

        std::fs::remove_file(&cache_path).ok();
    }

    #[cfg(not(unix))]
    #[test]
    fn coverage_cache_corruption_falls_back_to_full_rescan() {
        let cache_path = scratch_cache_path("corruption");
        std::fs::remove_file(&cache_path).ok();

        let cold_fonts = SystemFonts::with_cache_dir(None);
        let cold = scan_coverage(cold_fonts.db(), Some(&cache_path));

        std::fs::write(&cache_path, b"not a valid coverage cache").unwrap();

        let rescanned_fonts = SystemFonts::with_cache_dir(None);
        let rescanned = scan_coverage(rescanned_fonts.db(), Some(&cache_path));

        assert_eq!(cold, rescanned);

        std::fs::remove_file(&cache_path).ok();
    }

    #[cfg(not(unix))]
    #[test]
    fn coverage_cache_rejects_huge_declared_counts_without_allocating() {
        // Counts come from an untrusted file; a corrupt buffer with intact
        // magic and version but a bogus count must fail at the bounds check,
        // not pre-allocate gigabytes and abort the process.
        let cache_path = scratch_cache_path("huge_counts");

        let mut huge_file_count = Vec::new();
        huge_file_count.extend_from_slice(b"ATCC");
        huge_file_count.extend_from_slice(&1u32.to_le_bytes()); // version
        huge_file_count.extend_from_slice(&u32::MAX.to_le_bytes()); // file_count
        std::fs::write(&cache_path, &huge_file_count).unwrap();
        assert!(disk_cache::load(&cache_path).is_none());

        let mut huge_range_count = Vec::new();
        huge_range_count.extend_from_slice(b"ATCC");
        huge_range_count.extend_from_slice(&1u32.to_le_bytes()); // version
        huge_range_count.extend_from_slice(&1u32.to_le_bytes()); // file_count
        huge_range_count.extend_from_slice(&1u32.to_le_bytes()); // path length
        huge_range_count.push(b'a');
        huge_range_count.extend_from_slice(&10u64.to_le_bytes()); // size
        huge_range_count.extend_from_slice(&20u64.to_le_bytes()); // mtime_millis
        huge_range_count.extend_from_slice(&1u32.to_le_bytes()); // face_count
        huge_range_count.extend_from_slice(&0u32.to_le_bytes()); // face_index
        huge_range_count.extend_from_slice(&u32::MAX.to_le_bytes()); // range_count
        std::fs::write(&cache_path, &huge_range_count).unwrap();
        assert!(disk_cache::load(&cache_path).is_none());

        std::fs::remove_file(&cache_path).ok();
    }
}

// Pure candidate-selection logic for the automatic fallback chain: Unicode
// coverage sets and FcFontSort-style greedy trimming.  Platform-neutral so
// the unit tests run on every platform, even though only the Windows chain
// consumes it at runtime.
#[cfg_attr(unix, allow(dead_code))]
mod coverage {
    use std::path::PathBuf;

    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Coverage {
        ranges: Vec<(u32, u32)>,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct Candidate {
        pub path: PathBuf,
        pub face_index: u32,
        pub family: String,
        pub weight: u16,
        pub italic: bool,
        pub monospaced: bool,
        /// Size of the file backing this face; 0 when it could not be stat'd.
        /// `trim_by_coverage` weighs the coverage a face adds against it.
        pub bytes: u64,
    }

    impl Coverage {
        /// Build from an arbitrary codepoint list: sorted, deduped, and
        /// collapsed into inclusive, disjoint ranges.
        pub fn from_codepoints(mut codepoints: Vec<u32>) -> Self {
            codepoints.sort_unstable();
            codepoints.dedup();
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            for cp in codepoints {
                match ranges.last_mut() {
                    Some((_, end)) if *end + 1 == cp => *end = cp,
                    _ => ranges.push((cp, cp)),
                }
            }
            Self { ranges }
        }

        /// Rebuild from ranges that were produced by `from_codepoints` and stored;
        /// validated so a corrupt cache cannot break the sortedness invariant.
        /// The Unicode bound matters too: a well-formed but bogus range like
        /// `(0, u32::MAX)` would mark everything as covered and silently empty
        /// the automatic chain until the font file changes.
        pub fn from_stored_ranges(ranges: Vec<(u32, u32)>) -> Option<Self> {
            if ranges.iter().any(|&(start, end)| start > end || end > 0x10FFFF) {
                return None;
            }
            if ranges.windows(2).any(|w| w[1].0 < w[0].1.saturating_add(2)) {
                return None;
            }
            Some(Self { ranges })
        }

        pub fn ranges(&self) -> &[(u32, u32)] {
            &self.ranges
        }

        pub fn merge(&mut self, other: &Coverage) {
            let mut merged: Vec<(u32, u32)> =
                Vec::with_capacity(self.ranges.len() + other.ranges.len());
            let push = |merged: &mut Vec<(u32, u32)>, range: (u32, u32)| match merged.last_mut() {
                Some((_, end)) if *end >= range.0.saturating_sub(1) => *end = (*end).max(range.1),
                _ => merged.push(range),
            };
            let (mut a, mut b) =
                (self.ranges.iter().copied().peekable(), other.ranges.iter().copied().peekable());
            while let (Some(&ra), Some(&rb)) = (a.peek(), b.peek()) {
                if ra.0 <= rb.0 {
                    push(&mut merged, ra);
                    a.next();
                } else {
                    push(&mut merged, rb);
                    b.next();
                }
            }
            for range in a {
                push(&mut merged, range);
            }
            for range in b {
                push(&mut merged, range);
            }
            self.ranges = merged;
        }

        /// How many codepoints `self` covers that `other` doesn't — the
        /// FcFontSort(trim) keep-test, counted rather than merely detected so
        /// the trim can weigh what a face adds against what it costs.
        pub fn novel_codepoints(&self, other: &Coverage) -> u64 {
            let mut novel = 0;
            let mut i = 0;
            for &(start, end) in &self.ranges {
                let mut cp = start;
                loop {
                    while i < other.ranges.len() && other.ranges[i].1 < cp {
                        i += 1;
                    }
                    match other.ranges.get(i) {
                        Some(&(other_start, other_end)) if other_start <= cp => {
                            // Covered through other_end; resume past it.
                            if other_end >= end {
                                break;
                            }
                            cp = other_end + 1;
                        },
                        // Novel up to the next covered range, or to the end.
                        Some(&(other_start, _)) if other_start <= end => {
                            novel += u64::from(other_start - cp);
                            cp = other_start;
                        },
                        _ => {
                            novel += u64::from(end - cp) + 1;
                            break;
                        },
                    }
                }
            }
            novel
        }
    }

    /// A fallback face is mapped and parsed at startup and stays registered
    /// with egui for the life of the process, so one that covers a handful of
    /// codepoints nothing will render is pure cost.  Weighing coverage against
    /// file size is what separates a 21 MiB CJK face carrying 58k codepoints
    /// from a 35 MiB one carrying three.  Faces below `CHEAP_FACE_BYTES` skip
    /// the test — at that size coverage alone is reason enough to keep them,
    /// and a small face with a few rare glyphs (powerline caps, a script's
    /// combining marks) is exactly what the chain exists to find.
    const CHEAP_FACE_BYTES: u64 = 4 * 1024 * 1024;
    const MIN_NOVEL_CODEPOINTS_PER_MIB: u64 = 64;

    fn earns_its_size(bytes: u64, novel: u64) -> bool {
        if bytes <= CHEAP_FACE_BYTES {
            return true;
        }
        let mib = bytes.div_ceil(1024 * 1024);
        novel / mib >= MIN_NOVEL_CODEPOINTS_PER_MIB
    }

    /// Order candidates by fontconfig-like affinity to the seed face:
    /// same-family siblings, then weight/slant matches, then monospace, then
    /// everything else; ties break on family name, path, and face index so
    /// the resulting chain is deterministic across runs.
    pub fn order_candidates(
        candidates: &mut [(Candidate, Coverage)],
        family: &str,
        weight: u16,
        italic: bool,
    ) {
        candidates.sort_by(|(a, _), (b, _)| {
            let affinity = |c: &Candidate| {
                (
                    !c.family.eq_ignore_ascii_case(family),
                    !(c.weight == weight && c.italic == italic),
                    !c.monospaced,
                )
            };
            affinity(a)
                .cmp(&affinity(b))
                .then_with(|| a.family.cmp(&b.family))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.face_index.cmp(&b.face_index))
        });
    }

    /// Greedy trim mirroring FcFontSort(trim=true): walk candidates in order,
    /// keeping only faces that cover codepoints the seed face and the
    /// already-kept faces don't — and, for the large ones, enough of them to
    /// justify carrying the face at all.
    pub fn trim_by_coverage(
        candidates: Vec<(Candidate, Coverage)>,
        seed_coverage: &Coverage,
        limit: usize,
    ) -> Vec<Candidate> {
        let mut covered = seed_coverage.clone();
        let mut kept = Vec::new();
        for (candidate, coverage) in candidates {
            if kept.len() >= limit {
                break;
            }
            let novel = coverage.novel_codepoints(&covered);
            if novel == 0 || !earns_its_size(candidate.bytes, novel) {
                continue;
            }
            covered.merge(&coverage);
            kept.push(candidate);
        }
        kept
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        pub(super) fn cand(family: &str) -> Candidate {
            Candidate {
                path: PathBuf::from(family),
                face_index: 0,
                family: family.into(),
                weight: 400,
                italic: false,
                monospaced: true,
                bytes: 0,
            }
        }

        fn sized_cand(family: &str, bytes: u64) -> Candidate {
            Candidate { bytes, ..cand(family) }
        }

        const MIB: u64 = 1024 * 1024;

        #[test]
        fn trim_drops_faces_that_do_not_earn_their_size() {
            // A 35 MiB CJK face that contributes three codepoints the chain
            // lacks is not worth mapping and parsing; a face of the same size
            // carrying tens of thousands of them is.  Small faces are cheap
            // enough that coverage alone decides.
            let seed = Coverage::from_codepoints((0..128).collect());
            let candidates = vec![
                (
                    sized_cand("HugeAndUseless", 35 * MIB),
                    Coverage::from_codepoints(vec![0x4E00, 0x4E01, 0x4E02]),
                ),
                (
                    sized_cand("HugeAndUseful", 21 * MIB),
                    Coverage::from_codepoints((0x20000..0x2A000).collect()),
                ),
                (sized_cand("SmallAndSparse", 64 * 1024), Coverage::from_codepoints(vec![0xE0B0])),
            ];

            let kept: Vec<String> =
                trim_by_coverage(candidates, &seed, 32).into_iter().map(|c| c.family).collect();

            assert_eq!(kept, ["HugeAndUseful", "SmallAndSparse"]);
        }

        #[test]
        fn trim_keeps_a_face_of_unknown_size() {
            // A face that could not be stat'd has no size to weigh coverage
            // against; dropping it would silently starve the chain.
            let seed = Coverage::from_codepoints((0..128).collect());
            let candidates =
                vec![(sized_cand("Unstatable", 0), Coverage::from_codepoints(vec![0xE0B0]))];

            let kept = trim_by_coverage(candidates, &seed, 32);

            assert_eq!(kept.len(), 1);
        }

        fn cand2(family: &str, weight: u16, italic: bool, monospaced: bool) -> Candidate {
            Candidate { weight, italic, monospaced, ..cand(family) }
        }

        #[test]
        fn orders_family_then_style_then_monospace_then_name() {
            let mut candidates = vec![
                (cand2("Zeta", 400, false, false), Coverage::default()),
                (cand2("Beta", 400, false, true), Coverage::default()),
                (cand2("Alpha", 700, true, false), Coverage::default()),
                (cand2("Seed Family", 400, false, false), Coverage::default()),
                (cand2("Beta", 700, false, true), Coverage::default()),
            ];
            order_candidates(&mut candidates, "seed family", 700, false);
            let order: Vec<_> =
                candidates.iter().map(|(c, _)| (c.family.as_str(), c.weight)).collect();
            assert_eq!(
                order,
                [
                    ("Seed Family", 400), // same family wins even without a style match
                    ("Beta", 700),        // style match + monospace
                    ("Beta", 400),        // monospace
                    ("Alpha", 700),       // italic mismatches the variant; name order
                    ("Zeta", 400),
                ]
            );
        }

        #[test]
        fn from_codepoints_sorts_dedups_and_merges_adjacent() {
            let c = Coverage::from_codepoints(vec![3, 1, 2, 2, 10]);
            assert_eq!(c, Coverage { ranges: vec![(1, 3), (10, 10)] });
        }

        #[test]
        fn merge_coalesces_overlapping_and_adjacent_ranges() {
            let mut a = Coverage::from_codepoints(vec![1, 2, 10]);
            a.merge(&Coverage::from_codepoints(vec![3, 4, 9]));
            assert_eq!(a, Coverage { ranges: vec![(1, 4), (9, 10)] });
        }

        #[test]
        fn novel_codepoint_counting() {
            let seed = Coverage::from_codepoints(vec![1, 2, 3, 4, 5]);
            assert_eq!(Coverage::from_codepoints(vec![2, 4]).novel_codepoints(&seed), 0);
            assert_eq!(Coverage::from_codepoints(vec![5, 6]).novel_codepoints(&seed), 1);
            assert_eq!(Coverage::from_codepoints(vec![100]).novel_codepoints(&seed), 1);
            assert_eq!(Coverage::default().novel_codepoints(&seed), 0);
            assert_eq!(seed.novel_codepoints(&Coverage::default()), 5);
            // A range straddling the seed on both sides counts only the gaps.
            assert_eq!(Coverage::from_codepoints((0..=9).collect()).novel_codepoints(&seed), 5);
        }

        #[test]
        fn trim_drops_subsumed_keeps_novel_respects_limit_in_order() {
            let seed = Coverage::from_codepoints((0x20u32..0x7f).collect());
            let candidates = vec![
                (cand("subsumed"), Coverage::from_codepoints(vec![0x41, 0x42])),
                (cand("nerd"), Coverage::from_codepoints(vec![0xE0A0, 0xE0B0])),
                (cand("nerd-dup"), Coverage::from_codepoints(vec![0xE0A0])),
                (cand("emoji"), Coverage::from_codepoints(vec![0x1F600])),
                (cand("cjk"), Coverage::from_codepoints(vec![0x4E00])),
            ];
            let kept = trim_by_coverage(candidates, &seed, 2);
            let names: Vec<_> = kept.iter().map(|c| c.family.as_str()).collect();
            assert_eq!(names, ["nerd", "emoji"]);
        }

        #[test]
        fn from_stored_ranges_accepts_disjoint_nonadjacent_ranges() {
            let ranges = vec![(1, 3), (10, 20), (25, 25)];
            assert_eq!(Coverage::from_stored_ranges(ranges.clone()), Some(Coverage { ranges }));
        }

        #[test]
        fn from_stored_ranges_rejects_malformed_ranges() {
            // start > end within a range.
            assert!(Coverage::from_stored_ranges(vec![(5, 3)]).is_none());
            // Overlapping ranges.
            assert!(Coverage::from_stored_ranges(vec![(1, 5), (5, 10)]).is_none());
            // Adjacent ranges that `from_codepoints` would have merged.
            assert!(Coverage::from_stored_ranges(vec![(1, 5), (6, 10)]).is_none());
            // Out of order.
            assert!(Coverage::from_stored_ranges(vec![(10, 20), (1, 5)]).is_none());
            // Beyond the last Unicode codepoint.
            assert!(Coverage::from_stored_ranges(vec![(0, u32::MAX)]).is_none());
            assert!(Coverage::from_stored_ranges(vec![(1, 3), (10, 0x110000)]).is_none());
        }
    }
}
