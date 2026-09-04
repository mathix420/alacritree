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
#[cfg(not(unix))]
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use egui::{Context, FontData, FontDefinitions, FontFamily, FontTweak};

use crate::config::{FontConfig, UiFont};

/// Hard cap on fallback faces.  fontconfig's trimmed sort tops out at a few
/// dozen on a typical system; this just bounds startup memory and parse cost
/// when someone has hundreds of fonts installed.
const MAX_FALLBACK_FACES: usize = 32;

pub const BOLD_FAMILY: &str = "alacritree_bold";
pub const ITALIC_FAMILY: &str = "alacritree_italic";
pub const BOLD_ITALIC_FAMILY: &str = "alacritree_bold_italic";

/// Glyphs alacritree paints itself, so the chrome renders on systems whose
/// fonts lack them.  Appended last in each chrome family, so an installed
/// font that already has a glyph keeps rendering it.
const SYMBOLS_FONT: &[u8] = include_bytes!("../assets/alacritree-symbols.ttf");
const SYMBOLS_ID: &str = "alacritree_symbols";
const USER_FALLBACK_ID: &str = "alacritree_fallback_";

const NORMAL_FONT_ID: &str = "alacritree_terminal_normal";
const BOLD_FONT_ID: &str = "alacritree_terminal_bold";
const ITALIC_FONT_ID: &str = "alacritree_terminal_italic";
const BOLD_ITALIC_FONT_ID: &str = "alacritree_terminal_bold_italic";

const UI_FONT_ID: &str = "alacritree_ui_normal";
/// Temporary family the UI chain is assembled under before being spliced to
/// the head of `Proportional`; removed again so it never leaks to egui.
const UI_FAMILY: &str = "alacritree_ui";

/// Registered variant families for chrome text.  Distinct from
/// `BOLD_FAMILY`/`ITALIC_FAMILY`/`BOLD_ITALIC_FAMILY` (the terminal grid's
/// variant faces) so a `[ui.font]` override never changes what bold/italic
/// cells render in the terminal.
pub const UI_BOLD_FAMILY: &str = "alacritree_ui_bold";
pub const UI_ITALIC_FAMILY: &str = "alacritree_ui_italic";
pub const UI_BOLD_ITALIC_FAMILY: &str = "alacritree_ui_bold_italic";

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
    /// `RefCell` rather than `OnceCell` because the map is keyed and grows;
    /// `&self` access matches `db` and `coverage`.
    #[cfg(not(unix))]
    seed_coverage: RefCell<HashMap<(PathBuf, u32), Option<coverage::Coverage>>>,
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

    /// Coverage of a resolved seed face, computed at most once per install.
    /// The four variant seeds and the UI family commonly resolve to the same
    /// one or two files, and a miss is cached too so an unresolvable seed is
    /// not retried once per variant.
    #[cfg(not(unix))]
    fn seed_coverage(&self, face: &ResolvedFace) -> Option<coverage::Coverage> {
        let key = (face.path.clone(), face.face_index);
        if let Some(hit) = self.seed_coverage.borrow().get(&key) {
            return hit.clone();
        }
        // The borrow above is released here, so the fallback parse cannot
        // panic against the borrow_mut below.
        let computed = scanned_seed_coverage(self, face)
            .or_else(|| face_coverage(&face.path, face.face_index));
        self.seed_coverage.borrow_mut().insert(key, computed.clone());
        computed
    }
}

/// How many faces to scan at once.  Four is where the measured curve
/// flattens; the work is memory-bound, so more cores stop helping well
/// before they run out.  The floor keeps a restricted container from
/// asking for a pool with no threads.
#[cfg(not(unix))]
fn worker_count(reported: usize) -> usize {
    reported.clamp(1, 4)
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
    let workers = worker_count(std::thread::available_parallelism().map_or(1, |n| n.get()));
    scan_coverage_with_workers(db, cache_path, workers).0
}

/// The scan proper, with the worker count injected so tests can compare a
/// parallel run against a serial one.  Returns the candidate list and how
/// many faces came from the cache.
#[cfg(not(unix))]
fn scan_coverage_with_workers(
    db: &fontdb::Database,
    cache_path: Option<&Path>,
    workers: usize,
) -> (Vec<(coverage::Candidate, coverage::Coverage)>, usize) {
    use rayon::prelude::*;

    let started = std::time::Instant::now();
    let cache = cache_path.and_then(disk_cache::load).unwrap_or_default();

    // Faces addressable by path, in database order.  A `.ttc` contributes
    // several entries sharing one path.
    let faces: Vec<(PathBuf, u32, &fontdb::FaceInfo)> = db
        .faces()
        .filter_map(|face| match &face.source {
            // Embedded faces aren't path-addressable by our loader.
            fontdb::Source::Binary(_) => None,
            fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => {
                Some((p.clone(), face.index, face))
            },
        })
        .collect();

    // Stat once per distinct file, before the fan-out, so the parallel
    // phase reads a finished map instead of contending on one.
    let mut stat_memo: HashMap<PathBuf, Option<(u64, u64)>> = HashMap::new();
    for (path, _, _) in &faces {
        stat_memo.entry(path.clone()).or_insert_with(|| disk_cache::stat_file(path));
    }

    let scan_one = |(path, face_index, face): &(PathBuf, u32, &fontdb::FaceInfo)| {
        let path_key = path.to_string_lossy().into_owned();
        let stat = stat_memo[path];

        let cached_ranges = stat.and_then(|(size, mtime_millis)| {
            let cached_file = cache.get(&path_key)?;
            (cached_file.size == size && cached_file.mtime_millis == mtime_millis)
                .then(|| cached_file.faces.get(face_index).cloned())
                .flatten()
        });

        let (cov, from_cache) = match cached_ranges.and_then(coverage::Coverage::from_stored_ranges)
        {
            Some(cov) => (cov, true),
            None => {
                let parsed = db.with_face_data(face.id, |data, index| {
                    let parsed = ttf_parser::Face::parse(data, index).ok()?;
                    cmap_coverage(&parsed)
                });
                let Some(Some(cov)) = parsed else {
                    log::debug!("skipping unparseable font {}", path.display());
                    return None;
                };
                (cov, false)
            },
        };

        let family = face.families.first().map(|(name, _)| name.clone()).unwrap_or_default();
        let candidate = coverage::Candidate {
            path: path.clone(),
            face_index: *face_index,
            family,
            weight: face.weight.0,
            italic: face.style != fontdb::Style::Normal,
            monospaced: face.monospaced,
            bytes: stat.map_or(0, |(size, _)| size),
        };
        Some((path_key, *face_index, stat, candidate, cov, from_cache))
    };

    // `par_iter().collect()` preserves input order, so the scan stays
    // deterministic with nothing carried or sorted to make it so.  A local
    // pool rather than rayon's global one, which would keep its threads for
    // the life of the process for a scan that runs once.
    let scanned_faces: Vec<_> = match rayon::ThreadPoolBuilder::new().num_threads(workers).build() {
        Ok(pool) => pool.install(|| faces.par_iter().filter_map(scan_one).collect()),
        Err(err) => {
            log::debug!("scanning fonts serially, thread pool unavailable: {err}");
            faces.iter().filter_map(scan_one).collect()
        },
    };

    // Accumulation stays serial.  One `CachedFile` holds every face of a
    // collection, so folding per-worker fragments would have to merge those
    // per-file maps or silently drop faces.  This is one pass over a list
    // that is already built; parallelising it would buy nothing.
    let mut fresh_files: HashMap<String, disk_cache::CachedFile> = HashMap::new();
    let mut scanned = Vec::with_capacity(scanned_faces.len());
    let mut hits = 0usize;
    let mut any_fresh = false;

    for (path_key, face_index, stat, candidate, cov, from_cache) in scanned_faces {
        if from_cache {
            hits += 1;
        } else {
            any_fresh = true;
        }
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
        scanned.push((candidate, cov));
    }

    // A cache that was absent or invalid produced zero hits, so every face that parsed
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
    (scanned, hits)
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

/// Bookkeeping shared by all fallback registration within one install, keyed
/// by `(file, face index)` because a collection file holds many faces: which
/// faces already back an egui font, which font id serves each face (so one
/// face can join several variants' family lists without duplicate data), and
/// which user entries have already produced a warning.
#[derive(Default)]
struct FallbackBook {
    loaded_faces: HashSet<(PathBuf, u32)>,
    ids_by_face: HashMap<(PathBuf, u32), String>,
    warned_entries: HashSet<String>,
    /// Faces withheld from egui because they carry no outlines.  Kept so a
    /// later variant's chain doesn't re-read and re-probe the same face.
    color_only: HashSet<(PathBuf, u32)>,
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
pub(crate) fn face_outlines_char(face: &ChainFace, c: char) -> bool {
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

/// Whether epaint will accept this face.  epaint re-parses every registered
/// face with ab_glyph and aborts the process on failure, so a face must pass
/// this exact parse before it may reach the definitions.  System font indexes
/// commonly list faces ab_glyph rejects — macOS `.dfont` suitcases and
/// bitmap-only families — which the ttf_parser-based probes here don't catch.
fn epaint_can_parse(bytes: &[u8], face_index: u32) -> bool {
    ab_glyph::FontRef::try_from_slice_and_index(bytes, face_index).is_ok()
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
        let key = (resolved.path.clone(), resolved.face_index);
        if book.color_only.contains(&key) {
            book.extend_chain(variant, &resolved.path, resolved.face_index, true);
            continue;
        }
        if let Some(id) = book.ids_by_face.get(&key) {
            for family in targets {
                defs.families.entry(family.clone()).or_default().push(id.clone());
            }
            book.extend_chain(variant, &resolved.path, resolved.face_index, false);
            continue;
        }
        if book.loaded_faces.contains(&key) {
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
            book.color_only.insert(key);
            book.extend_chain(variant, &resolved.path, resolved.face_index, true);
            continue;
        }
        if !epaint_can_parse(bytes, resolved.face_index) {
            if book.warned_entries.insert(entry.clone()) {
                log::warn!("font.fallback entry '{entry}' is not a parseable TTF/OTF; skipping");
            }
            continue;
        }
        let id = format!("{USER_FALLBACK_ID}{}", defs.font_data.len());
        let tweak = fallback_tweak(book.primary_height_ratio, bytes, resolved.face_index);
        let data = FontData { index: resolved.face_index, tweak, ..FontData::from_static(bytes) };
        defs.font_data.insert(id.clone(), Arc::new(data));
        for family in targets {
            defs.families.entry(family.clone()).or_default().push(id.clone());
        }
        book.extend_chain(variant, &resolved.path, resolved.face_index, false);
        book.loaded_faces.insert(key.clone());
        book.ids_by_face.insert(key, id);
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

/// Em fractions used where a face reports nothing usable.  A zero in a metric
/// table means "not supplied" rather than "at the baseline", so every field is
/// checked against these rather than used as read.
const DEFAULT_ASCENDER: f32 = 0.8;
const DEFAULT_DESCENDER: f32 = -0.2;
const DEFAULT_UNDERLINE_POSITION: f32 = -0.1;
const DEFAULT_UNDERLINE_THICKNESS: f32 = 0.05;

/// Where a strikeout goes above the baseline when OS/2 does not say, as a
/// fraction of the ascender.  kitty spells the same rule as
/// `floor(baseline * 0.65)` measured down from the cell top.
const STRIKEOUT_ASCENDER_RATIO: f32 = 0.35;

/// What a face asks for its decorations, as fractions of the em measured from
/// the baseline with up positive.  That is the sign convention of the `post`
/// and OS/2 tables the numbers come from: an underline position is negative,
/// a strikeout position is positive, and so is the ascender while the
/// descender is negative.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaceMetrics {
    pub ascender: f32,
    pub descender: f32,
    pub underline_position: f32,
    pub underline_thickness: f32,
    pub strikeout_position: f32,
    pub strikeout_thickness: f32,
}

impl Default for FaceMetrics {
    fn default() -> Self {
        Self {
            ascender: DEFAULT_ASCENDER,
            descender: DEFAULT_DESCENDER,
            underline_position: DEFAULT_UNDERLINE_POSITION,
            underline_thickness: DEFAULT_UNDERLINE_THICKNESS,
            strikeout_position: STRIKEOUT_ASCENDER_RATIO * DEFAULT_ASCENDER,
            strikeout_thickness: DEFAULT_UNDERLINE_THICKNESS,
        }
    }
}

impl FaceMetrics {
    /// Read face `index` of `data`.  Anything the face leaves at zero, omits,
    /// or cannot express is filled in by `resolve_fallbacks`.
    pub fn from_face(data: &[u8], index: u32) -> Self {
        let Ok(face) = ttf_parser::Face::parse(data, index) else {
            log::warn!("could not parse the terminal face; using default decoration metrics");
            return Self::default();
        };
        let units = f32::from(face.units_per_em());
        if units <= 0.0 {
            log::warn!("the terminal face reports no em size; using default decoration metrics");
            return Self::default();
        }
        let em = |v: i16| f32::from(v) / units;
        let underline = face.underline_metrics();
        let strikeout = face.strikeout_metrics();

        resolve_fallbacks(Self {
            ascender: em(face.ascender()),
            descender: em(face.descender()),
            underline_position: underline.map_or(0.0, |m| em(m.position)),
            underline_thickness: underline.map_or(0.0, |m| em(m.thickness)),
            strikeout_position: strikeout.map_or(0.0, |m| em(m.position)),
            strikeout_thickness: strikeout.map_or(0.0, |m| em(m.thickness)),
        })
    }
}

/// Substitute for every field a face left at zero.  Split out from
/// `from_face` so each substitution is reachable from a test without a font
/// file engineered to be broken in exactly one way.
fn resolve_fallbacks(raw: FaceMetrics) -> FaceMetrics {
    let defaults = FaceMetrics::default();
    let ascender = correctly_signed(raw.ascender, true).unwrap_or(defaults.ascender);
    let underline_thickness =
        nonzero(raw.underline_thickness).unwrap_or(defaults.underline_thickness);
    FaceMetrics {
        ascender,
        descender: correctly_signed(raw.descender, false).unwrap_or(defaults.descender),
        underline_position: nonzero(raw.underline_position).unwrap_or(defaults.underline_position),
        underline_thickness,
        strikeout_position: nonzero(raw.strikeout_position)
            .unwrap_or(STRIKEOUT_ASCENDER_RATIO * ascender),
        strikeout_thickness: nonzero(raw.strikeout_thickness).unwrap_or(underline_thickness),
    }
}

fn nonzero(value: f32) -> Option<f32> {
    (value != 0.0 && value.is_finite()).then_some(value)
}

/// Like `nonzero`, but for a field whose downstream math assumes a sign: a
/// face reporting a non-negative descender or a non-positive ascender passes
/// the zero check yet still inverts the geometry that reads it, since zero is
/// not the only value that means "not supplied" for these two.
fn correctly_signed(value: f32, positive: bool) -> Option<f32> {
    let sign_ok = if positive { value > 0.0 } else { value < 0.0 };
    (sign_ok && value.is_finite()).then_some(value)
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

/// Put the `[ui.font]` family — and its own fallback chain — ahead of the
/// terminal font in egui's `Proportional` family, so all chrome text prefers
/// it.  `Monospace` (the grid) is untouched.  Returns `false` and leaves the
/// definitions unchanged when the family cannot be resolved or read, in which
/// case the chrome keeps using the terminal font.
fn install_ui_normal_chain(
    defs: &mut FontDefinitions,
    family_or_path: &str,
    fonts: &SystemFonts,
) -> bool {
    let Some(resolved) = resolve_ui_face(family_or_path, Variant::Normal, fonts) else {
        log::warn!("could not resolve ui font '{family_or_path}'; keeping the terminal font");
        return false;
    };
    let bytes = match map_font_file(&resolved.path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("could not read ui font file {}: {e}", resolved.path.display());
            return false;
        },
    };
    if !epaint_can_parse(bytes, resolved.face_index) {
        log::warn!(
            "ui font file {} is not a parseable TTF/OTF; keeping the terminal font",
            resolved.path.display()
        );
        return false;
    }
    insert_face(defs, UI_FONT_ID, bytes, resolved.face_index);
    let ui_family = FontFamily::Name(UI_FAMILY.into());
    defs.families.insert(ui_family.clone(), vec![UI_FONT_ID.to_string()]);

    // Its own book: the UI chain must not leak into the terminal's normal
    // chain (which feeds the colour-glyph renderer), and fallback height
    // normalization must anchor to the UI face, not the terminal face.
    let mut book = FallbackBook::default();
    book.loaded_faces.insert((resolved.path.clone(), resolved.face_index));
    book.primary_height_ratio = face_height_ratio(bytes, resolved.face_index);
    let targets = [ui_family.clone()];
    register_fallback_faces(
        defs,
        family_or_path,
        None,
        Variant::Normal,
        &targets,
        fonts,
        &mut book,
    );

    // Splice the assembled UI chain ahead of everything already in
    // `Proportional` (terminal font + its fallbacks).
    let ui_ids = defs.families.remove(&ui_family).unwrap_or_default();
    let prop = defs.families.entry(FontFamily::Proportional).or_default();
    for id in ui_ids.into_iter().rev() {
        prop.insert(0, id);
    }
    true
}

/// Splice `[ui.font] family` into `Proportional` when configured, then
/// register the bold/italic/bold-italic chrome families unconditionally —
/// they must exist even with no `[ui.font]` table so bold/italic chrome text
/// has somewhere to resolve to.  Returns whether the normal chain installed.
fn install_ui_font(defs: &mut FontDefinitions, ui: &UiFont, fonts: &SystemFonts) -> bool {
    let installed = match ui.family.as_deref() {
        Some(family_or_path) => install_ui_normal_chain(defs, family_or_path, fonts),
        None => false,
    };

    install_ui_variant(defs, ui, Variant::Bold, UI_BOLD_FAMILY, BOLD_FAMILY, fonts);
    install_ui_variant(defs, ui, Variant::Italic, UI_ITALIC_FAMILY, ITALIC_FAMILY, fonts);
    install_ui_variant(
        defs,
        ui,
        Variant::BoldItalic,
        UI_BOLD_ITALIC_FAMILY,
        BOLD_ITALIC_FAMILY,
        fonts,
    );

    installed
}

/// Order is explicit because egui families cannot nest: the configured UI
/// variant first, then the variant derived from the UI family, then the
/// terminal's variant ids, then normal.  Deduplicated by id.
fn install_ui_variant(
    defs: &mut FontDefinitions,
    ui: &UiFont,
    variant: Variant,
    ui_family_name: &str,
    terminal_family: &str,
    fonts: &SystemFonts,
) {
    let mut ids: Vec<String> = Vec::new();
    let configured = match variant {
        Variant::Bold => ui.bold_family.as_deref(),
        Variant::Italic => ui.italic_family.as_deref(),
        Variant::BoldItalic => ui.bold_italic_family.as_deref(),
        Variant::Normal => None,
    };
    for candidate in configured.into_iter().chain(ui.family.as_deref()) {
        let Some(resolved) = resolve_ui_face(candidate, variant, fonts) else {
            continue;
        };
        let Ok(bytes) = map_font_file(&resolved.path) else {
            continue;
        };
        // The normal UI face is validated before registration; a configured
        // variant path pointing at non-font bytes would otherwise register
        // and panic when egui parses it.
        if !epaint_can_parse(bytes, resolved.face_index) {
            log::warn!(
                "ui {} font {} is not parseable; skipping",
                variant.label(),
                resolved.path.display()
            );
            continue;
        }
        let id = format!("{ui_family_name}_{}", ids.len());
        insert_face(defs, &id, bytes, resolved.face_index);
        ids.push(id);
        // The configured family gets its own fallback chain registered
        // against a scratch family before the inherited terminal ids below
        // are appended, so the fallback machinery has somewhere to write to.
        let mut book = FallbackBook::default();
        book.loaded_faces.insert((resolved.path.clone(), resolved.face_index));
        book.primary_height_ratio = face_height_ratio(bytes, resolved.face_index);
        let target = FontFamily::Name(ui_family_name.into());
        defs.families.insert(target.clone(), ids.clone());
        register_fallback_faces(
            defs,
            candidate,
            None,
            variant,
            &[target.clone()],
            fonts,
            &mut book,
        );
        ids = defs.families.remove(&target).unwrap_or(ids);
    }
    let inherited =
        defs.families.get(&FontFamily::Name(terminal_family.into())).cloned().unwrap_or_default();
    ids.extend(inherited);
    ids.extend(defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());

    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    defs.families.insert(FontFamily::Name(ui_family_name.into()), ids);
}

/// Append the bundled symbol face to each chrome family as the last resort.
///
/// Runs after every family is assembled, never during.  `install_ui_font`
/// builds its chain under `UI_FAMILY` and then front-splices it into
/// `Proportional`, so a face added there would land *ahead* of the
/// terminal-derived fallbacks and override glyphs that already draw.
///
/// One font-data id per distinct height ratio among the targets: the ratio
/// anchors `fallback_tweak` to whichever face heads that family, and bold or
/// italic chrome can be a different face from normal.  Sharing bytes across
/// ids is free — `FontData::from_static` borrows.
fn install_symbol_fallback(defs: &mut FontDefinitions, ui: &UiFont) {
    if !ui.builtin_symbols {
        return;
    }
    if !epaint_can_parse(SYMBOLS_FONT, 0) {
        log::warn!("bundled symbol font is not parseable; chrome falls back to system fonts");
        return;
    }

    let targets = [
        FontFamily::Proportional,
        FontFamily::Name(UI_BOLD_FAMILY.into()),
        FontFamily::Name(UI_ITALIC_FAMILY.into()),
        FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()),
    ];

    let mut ids_by_ratio: Vec<(Option<u32>, String)> = Vec::new();
    for family in targets {
        let Some(existing) = defs.families.get(&family) else {
            continue;
        };
        if existing.iter().any(|id| id.starts_with(SYMBOLS_ID)) {
            continue;
        }
        // Quantized so ratios that differ only by float noise share one id.
        let ratio = existing
            .first()
            .and_then(|id| defs.font_data.get(id))
            .and_then(|d| face_height_ratio(&d.font, d.index))
            .map(|r| (r * 1000.0).round() as u32);

        let id = match ids_by_ratio.iter().find(|(r, _)| *r == ratio) {
            Some((_, id)) => id.clone(),
            None => {
                let id = format!("{SYMBOLS_ID}_{}", ids_by_ratio.len());
                let tweak = fallback_tweak(ratio.map(|r| r as f32 / 1000.0), SYMBOLS_FONT, 0);
                let data = FontData { index: 0, tweak, ..FontData::from_static(SYMBOLS_FONT) };
                defs.font_data.insert(id.clone(), Arc::new(data));
                ids_by_ratio.push((ratio, id.clone()));
                id
            },
        };
        defs.families.entry(family).or_default().push(id);
    }
}

/// Maps ANSI-style bold/italic flags onto the chrome font family carrying
/// that style; unstyled text keeps using `Proportional` directly.
pub fn ui_variant_family(bold: bool, italic: bool) -> FontFamily {
    match (bold, italic) {
        (false, false) => FontFamily::Proportional,
        (true, false) => FontFamily::Name(UI_BOLD_FAMILY.into()),
        (false, true) => FontFamily::Name(UI_ITALIC_FAMILY.into()),
        (true, true) => FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()),
    }
}

/// Register the terminal faces with egui and return the normal-variant
/// fallback chain, in the order egui consults it, for the colour glyph
/// renderer to resolve against, together with the decoration metrics of the
/// face at its head.
pub fn install_terminal_fonts(
    ctx: &Context,
    font: &FontConfig,
    ui: &UiFont,
) -> (Vec<ChainFace>, FaceMetrics) {
    let fonts = SystemFonts::default();
    match build_font_definitions(font, ui, &fonts) {
        Some((defs, chain)) => {
            ctx.set_fonts(defs);
            let metrics = primary_face_metrics(&chain);
            (chain, metrics)
        },
        None => {
            ctx.set_fonts(unresolvable_font_definitions(ui));
            (Vec::new(), FaceMetrics::default())
        },
    }
}

/// The chain's head is the `[font.normal]` face, pushed ahead of every
/// fallback, so its metrics are the ones the grid is laid out against.  An
/// empty chain means the family could not be resolved at all.
fn primary_face_metrics(chain: &[ChainFace]) -> FaceMetrics {
    let Some(primary) = chain.first() else {
        return FaceMetrics::default();
    };
    match map_font_file(&primary.path) {
        Ok(data) => FaceMetrics::from_face(data, primary.face_index),
        Err(err) => {
            log::warn!("could not read {} for decoration metrics: {err}", primary.path.display());
            FaceMetrics::default()
        },
    }
}

/// Bind every chrome and terminal variant family to a family egui's own
/// defaults always provide, for the case where `[font.normal]` cannot be
/// resolved at all and `build_font_definitions` returns `None`. Without
/// this, `ctx.set_fonts` is never called, so `alacritree_*`/`alacritree_ui_*`
/// are unbound names — egui panics the moment anything paints a bold or
/// italic cell or icon, rather than just falling back to the wrong face.
/// Separated from the egui handoff so tests can inspect what the aliasing
/// produced, the same way `build_font_definitions` is.
fn unresolvable_font_definitions(ui: &UiFont) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let monospace = defs.families[&FontFamily::Monospace].clone();
    let proportional = defs.families[&FontFamily::Proportional].clone();
    for name in [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY] {
        defs.families.insert(FontFamily::Name(name.into()), monospace.clone());
    }
    for name in [UI_BOLD_FAMILY, UI_ITALIC_FAMILY, UI_BOLD_ITALIC_FAMILY] {
        defs.families.insert(FontFamily::Name(name.into()), proportional.clone());
    }
    install_symbol_fallback(&mut defs, ui);
    defs
}

/// Resolve and register every face for `install_terminal_fonts`, separated
/// from the egui handoff so tests can inspect what registration produced.
fn build_font_definitions(
    font: &FontConfig,
    ui: &UiFont,
    fonts: &SystemFonts,
) -> Option<(FontDefinitions, Vec<ChainFace>)> {
    let (normal, bold, italic, bold_italic) =
        (&font.normal, &font.bold, &font.italic, &font.bold_italic);
    let family = normal.family.as_deref().unwrap_or(DEFAULT_FAMILY);

    // The variant lookups compare their resolved path against this one to
    // detect when fontconfig substituted the regular face for a missing variant.
    let normal_match = match resolve_face(family, normal.style.as_deref(), Variant::Normal, fonts) {
        Some(m) => m,
        None => {
            log::warn!("could not resolve font '{family}'; using bundled monospace");
            return None;
        },
    };
    let normal_bytes = match map_font_file(&normal_match.path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("could not read font file {}: {e}", normal_match.path.display());
            return None;
        },
    };
    if !epaint_can_parse(normal_bytes, normal_match.face_index) {
        log::warn!(
            "font '{family}' resolved to {} which is not a parseable TTF/OTF; using bundled \
             monospace",
            normal_match.path.display()
        );
        return None;
    }

    // Bold/italic/bold-italic inherit the normal family unless overridden.
    let bold_family = bold.family.as_deref().unwrap_or(family);
    let italic_family = italic.family.as_deref().unwrap_or(family);
    let bold_italic_family = bold_italic.family.as_deref().unwrap_or(family);

    let bold_face =
        load_variant(bold_family, bold.style.as_deref(), Variant::Bold, &normal_match, fonts);
    let italic_face =
        load_variant(italic_family, italic.style.as_deref(), Variant::Italic, &normal_match, fonts);
    let bold_italic_face = load_variant(
        bold_italic_family,
        bold_italic.style.as_deref(),
        Variant::BoldItalic,
        &normal_match,
        fonts,
    );

    let mut defs = FontDefinitions::default();
    let normal_face = (normal_bytes, normal_match.face_index);

    // egui seeds these families with faces of its own, and they are worth
    // keeping — `Ubuntu-Light` is what draws `√` when the terminal font cannot.
    // But `Ubuntu-Light` also fills the legacy Adobe PUA slots, where U+F001
    // and U+F002 hold the `fi`/`fl` ligatures, and Nerd Fonts put icons at
    // those codepoints.  Left where egui put them they answer before anything
    // the user configured, so they are lifted out here and appended last.
    let bundled = take_bundled_faces(&mut defs);

    insert_face(&mut defs, NORMAL_FONT_ID, normal_bytes, normal_match.face_index);
    register_default_family(&mut defs, FontFamily::Monospace, NORMAL_FONT_ID);
    register_default_family(&mut defs, FontFamily::Proportional, NORMAL_FONT_ID);

    register_variant(&mut defs, BOLD_FONT_ID, BOLD_FAMILY, bold_face, normal_face);
    register_variant(&mut defs, ITALIC_FONT_ID, ITALIC_FAMILY, italic_face, normal_face);
    register_variant(
        &mut defs,
        BOLD_ITALIC_FONT_ID,
        BOLD_ITALIC_FAMILY,
        bold_italic_face,
        normal_face,
    );

    let mut book = FallbackBook::default();
    book.loaded_faces.insert((normal_match.path.clone(), normal_match.face_index));
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
        register_user_fallbacks(&mut defs, &font.fallback, variant, targets, fonts, &mut book);
        register_fallback_faces(&mut defs, family, style, variant, targets, fonts, &mut book);
    }

    install_ui_font(&mut defs, ui, fonts);
    install_symbol_fallback(&mut defs, ui);
    restore_bundled_faces(&mut defs, bundled);

    Some((defs, book.chain))
}

/// The families egui fills in `FontDefinitions::default()`, emptied so that
/// registration builds each one from the configured faces alone.
fn take_bundled_faces(defs: &mut FontDefinitions) -> Vec<(FontFamily, Vec<String>)> {
    [FontFamily::Monospace, FontFamily::Proportional]
        .into_iter()
        .map(|family| {
            let faces = std::mem::take(defs.families.entry(family.clone()).or_default());
            (family, faces)
        })
        .collect()
}

fn restore_bundled_faces(defs: &mut FontDefinitions, bundled: Vec<(FontFamily, Vec<String>)>) {
    let proportional = bundled
        .iter()
        .find(|(family, _)| *family == FontFamily::Proportional)
        .map(|(_, faces)| faces.clone())
        .unwrap_or_default();
    for (family, faces) in bundled {
        defs.families.entry(family).or_default().extend(faces);
    }
    // UI variants used to inherit these through Proportional. They are built
    // while the bundled faces are lifted out, so append the same last-resort
    // list explicitly after the symbol fallback has been installed.
    for family in [UI_BOLD_FAMILY, UI_ITALIC_FAMILY, UI_BOLD_ITALIC_FAMILY] {
        defs.families
            .entry(FontFamily::Name(family.into()))
            .or_default()
            .extend(proportional.iter().cloned());
    }
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
    let primaries: HashSet<(PathBuf, u32)> = book
        .loaded_faces
        .iter()
        .filter(|face| !book.ids_by_face.contains_key(*face))
        .cloned()
        .collect();
    let fallbacks =
        gather_fallback_faces(family, style, variant, &primaries, MAX_FALLBACK_FACES, fonts);
    if fallbacks.is_empty() {
        return;
    }

    for face in fallbacks {
        let key = (face.path.clone(), face.face_index);
        if book.color_only.contains(&key) {
            book.extend_chain(variant, &face.path, face.face_index, true);
            continue;
        }
        if let Some(id) = book.ids_by_face.get(&key) {
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
            book.color_only.insert(key);
            book.extend_chain(variant, &face.path, face.face_index, true);
            continue;
        }
        if !epaint_can_parse(bytes, face.face_index) {
            log::debug!(
                "skipping fallback font {} (face {}): not a parseable TTF/OTF",
                face.path.display(),
                face.face_index
            );
            continue;
        }
        let id = format!("{USER_FALLBACK_ID}{}", defs.font_data.len());
        let tweak = fallback_tweak(book.primary_height_ratio, bytes, face.face_index);
        let data = FontData { index: face.face_index, tweak, ..FontData::from_static(bytes) };
        defs.font_data.insert(id.clone(), Arc::new(data));

        for family in target_families {
            defs.families.entry(family.clone()).or_default().push(id.clone());
        }
        book.extend_chain(variant, &face.path, face.face_index, false);
        book.loaded_faces.insert(key.clone());
        book.ids_by_face.insert(key, id);
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
    skip_faces: &HashSet<(PathBuf, u32)>,
    limit: usize,
    _fonts: &SystemFonts,
) -> Vec<FallbackFace> {
    fontconfig_resolve::sorted_fallbacks(family, style, variant, skip_faces, limit)
}

#[cfg(not(unix))]
fn cmap_coverage(face: &ttf_parser::Face) -> Option<coverage::Coverage> {
    let cmap = face.tables().cmap?;
    // A font's BMP and full subtables overlap heavily, so the per-subtable
    // sets are unioned rather than concatenated.  `merge` coalesces
    // overlapping and adjacent ranges, which concatenation would not.
    let mut covered = coverage::Coverage::default();
    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        let one = coverage::Coverage::from_ascending_walk(|emit| subtable.codepoints(emit));
        covered.merge(&one);
    }
    Some(covered)
}

#[cfg(all(not(unix), test))]
thread_local! {
    /// Per-thread because the Windows fallback tests call
    /// `gather_fallback_faces` concurrently at the default thread count, and a
    /// process-wide count would fold their parses into whichever test asserts.
    static FACE_COVERAGE_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(not(unix), test))]
fn reset_face_coverage_parses() {
    FACE_COVERAGE_PARSES.with(|n| n.set(0));
}

#[cfg(all(not(unix), test))]
fn face_coverage_parses() -> usize {
    FACE_COVERAGE_PARSES.with(|n| n.get())
}

/// The scan already carries every system face and is disk-cached across
/// launches, so a seed found here costs no parse at all.  `Candidate` carries
/// both path and face index, so the match is exact — face 0 of a collection
/// file can be an unrelated family.
#[cfg(not(unix))]
fn scanned_seed_coverage(fonts: &SystemFonts, face: &ResolvedFace) -> Option<coverage::Coverage> {
    fonts
        .scanned_coverage()
        .iter()
        .find(|(candidate, _)| {
            candidate.path == face.path && candidate.face_index == face.face_index
        })
        .map(|(_, coverage)| coverage.clone())
}

/// Coverage of an already-resolved primary face, read at its resolved face
/// index — face 0 of a collection file can be an unrelated family.
#[cfg(not(unix))]
fn face_coverage(path: &Path, face_index: u32) -> Option<coverage::Coverage> {
    #[cfg(test)]
    FACE_COVERAGE_PARSES.with(|n| n.set(n.get() + 1));
    let data = map_font_file(path).ok()?;
    let parsed = ttf_parser::Face::parse(data, face_index).ok()?;
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
    skip_faces: &HashSet<(PathBuf, u32)>,
    limit: usize,
    fonts: &SystemFonts,
) -> Vec<FallbackFace> {
    let seed_coverage = resolve_face(family, style, variant, fonts)
        .and_then(|face| fonts.seed_coverage(&face))
        .unwrap_or_default();

    let mut candidates: Vec<_> = fonts
        .scanned_coverage()
        .iter()
        .filter(|(candidate, _)| {
            !skip_faces.contains(&(candidate.path.clone(), candidate.face_index))
        })
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

pub(crate) fn map_font_file(path: &Path) -> std::io::Result<&'static [u8]> {
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

/// Whether `path` already has a mapping.  Test-only: nothing in the app needs
/// to ask, and a release build should not carry the lookup.  Gated on
/// `not(unix)` because its only caller is Windows-gated; otherwise this
/// test-only helper is dead code on Linux.
#[cfg(all(test, not(unix)))]
fn is_mapped(path: &Path) -> bool {
    FONT_MAPS.get().is_some_and(|maps| {
        maps.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(path)
    })
}

fn insert_face(defs: &mut FontDefinitions, id: &str, bytes: &'static [u8], face_index: u32) {
    let data = FontData { index: face_index, ..FontData::from_static(bytes) };
    defs.font_data.insert(id.to_string(), Arc::new(data));
}

fn register_default_family(defs: &mut FontDefinitions, family: FontFamily, id: &str) {
    defs.families.entry(family).or_default().insert(0, id.to_string());
}

fn register_variant(
    defs: &mut FontDefinitions,
    font_id: &str,
    family_name: &str,
    face: Option<(&'static [u8], u32)>,
    fallback: (&'static [u8], u32),
) {
    let (bytes, face_index) = face.unwrap_or(fallback);
    insert_face(defs, font_id, bytes, face_index);
    defs.families.insert(FontFamily::Name(family_name.into()), vec![font_id.to_string()]);
}

/// Returns the bytes and face index of the variant face if a *real* variant
/// exists, or `None` if the matcher fell back to the normal face.  A
/// collection file holds many faces, so "fell back" means the same file *and*
/// the same face index — the bold sibling of a `.ttc` family lives in the
/// same file.  The caller registers the normal face as a fallback under the
/// variant's family name.
fn load_variant(
    family: &str,
    style: Option<&str>,
    variant: Variant,
    normal: &ResolvedFace,
    fonts: &SystemFonts,
) -> Option<(&'static [u8], u32)> {
    let resolved = resolve_face(family, style, variant, fonts)?;
    if resolved.path == normal.path && resolved.face_index == normal.face_index {
        log::debug!(
            "no real {} face for '{family}'; cells with that style will use the regular face",
            variant.label()
        );
        return None;
    }
    match map_font_file(&resolved.path) {
        Ok(b) if epaint_can_parse(b, resolved.face_index) => Some((b, resolved.face_index)),
        Ok(_) => {
            log::warn!(
                "{} font file {} is not a parseable TTF/OTF; cells with that style will use the \
                 regular face",
                variant.label(),
                resolved.path.display()
            );
            None
        },
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

/// Strict resolution for the `[ui.font]` family.  `FcFontMatch` substitutes a
/// default face for unknown families instead of failing, which would turn a
/// typo'd family into a silent chrome-font change — so gate on fontdb, which
/// matches family names literally, and only then let fontconfig pick the face
/// (its alias and weight substitution beats fontdb's).  CSS generics skip the
/// gate: they are aliases, not listed families.  Custom `<alias>` names lose
/// out (fontdb can't see them), a fair trade for keeping typos on the
/// terminal font.
#[cfg(unix)]
fn resolve_ui_face(
    family_or_path: &str,
    variant: Variant,
    fonts: &SystemFonts,
) -> Option<ResolvedFace> {
    if let Some(face) = resolve_via_path(family_or_path) {
        return Some(face);
    }
    let generic = matches!(
        family_or_path.to_ascii_lowercase().as_str(),
        "sans-serif" | "serif" | "monospace" | "cursive" | "fantasy" | "system-ui"
    );
    let listed = resolve_via_fontdb(family_or_path, variant, fonts);
    if !generic && listed.is_none() {
        return None;
    }
    fontconfig_resolve::resolve(family_or_path, None, variant).or(listed)
}

/// fontdb already matches family names literally, so the shared path is
/// exactly as strict as the UI font needs.
#[cfg(not(unix))]
fn resolve_ui_face(
    family_or_path: &str,
    variant: Variant,
    fonts: &SystemFonts,
) -> Option<ResolvedFace> {
    resolve_face(family_or_path, None, variant, fonts)
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
        let face_index = plain_face_index(matched.face_index().unwrap_or(0));
        Some(ResolvedFace { path: PathBuf::from(path), face_index })
    }

    /// fontconfig hands back `(named_instance << 16) | face` for a variable
    /// font's named instances (FreeType's encoding).  ttf_parser and epaint
    /// take a plain collection index and cannot apply a named instance
    /// anyway, so the default instance stands in for the named one.
    pub fn plain_face_index(index: i32) -> u32 {
        (index.max(0) as u32) & 0xFFFF
    }

    /// `FcFontSort` with `trim=true` returns fonts in match order, dropping
    /// any whose Unicode coverage is fully covered by an earlier entry.  This
    /// is the same chain `FcFontMatch` walks per glyph when crossfont misses,
    /// so registering it up front in egui gives equivalent coverage.
    pub fn sorted_fallbacks(
        family: &str,
        style: Option<&str>,
        variant: Variant,
        skip_faces: &HashSet<(PathBuf, u32)>,
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
            let face_index = plain_face_index(matched.face_index().unwrap_or(0));
            if skip_faces.contains(&(path.clone(), face_index)) {
                continue;
            }
            out.push(FallbackFace { path, face_index });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real font on disk for tests that drive registration: faces failing
    /// epaint's parse are rejected, so junk bytes would never register.
    fn write_parseable_font(name: &str) -> PathBuf {
        let bytes = FontDefinitions::default()
            .font_data
            .values()
            .next()
            .expect("egui bundles default fonts")
            .font
            .to_vec();
        let path = crate::test_util::scratch_dir().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn user_fallback_path_registers_for_every_variant() {
        // A file-path entry resolves to the same file for all four variants;
        // the bytes must be loaded once and the same egui font id appended to
        // each variant's family list (a plain HashSet dedup would starve
        // every variant after the first).
        let path = write_parseable_font("user_fallback.ttf");

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

        assert_eq!(book.ids_by_face.len(), 1);
        let id = book.ids_by_face.values().next().unwrap();
        assert!(defs.families[&FontFamily::Monospace].contains(id));
        assert!(defs.families[&FontFamily::Name(BOLD_FAMILY.into())].contains(id));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ui_font_heads_the_proportional_family() {
        let path = write_parseable_font("ui_font.ttf");

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mono_before = defs.families[&FontFamily::Monospace].clone();

        let ui = UiFont { family: Some(path.to_string_lossy().into_owned()), ..UiFont::default() };
        assert!(install_ui_font(&mut defs, &ui, &fonts));

        let prop = &defs.families[&FontFamily::Proportional];
        assert_eq!(prop.first().map(String::as_str), Some(UI_FONT_ID));
        // The grid's family is untouched.
        assert_eq!(defs.families[&FontFamily::Monospace], mono_before);
        // The temporary splice family does not leak into the definitions.
        assert!(!defs.families.contains_key(&FontFamily::Name(UI_FAMILY.into())));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unresolvable_ui_font_changes_nothing() {
        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let before = defs.families[&FontFamily::Proportional].clone();

        let ui = UiFont {
            family: Some("alacritree-no-such-ui-family-9f3a".to_string()),
            ..UiFont::default()
        };
        assert!(!install_ui_font(&mut defs, &ui, &fonts));

        assert_eq!(defs.families[&FontFamily::Proportional], before);
    }

    /// An egui family is a flat vector of font ids — one family cannot reference
    /// another — so each UI variant chain must physically contain the terminal
    /// variant's ids, deduplicated, after its own faces.  `register_variant`
    /// with no real face falls back to normal bytes under a fresh id, so
    /// `BOLD_FONT_ID`/`ITALIC_FONT_ID`/`BOLD_ITALIC_FONT_ID` here are fallback
    /// ids, not real variant faces — this only proves the splice carries
    /// whatever ids the terminal chain published, real or not.
    #[test]
    fn ui_variant_families_inherit_the_terminal_chain() {
        let fonts = SystemFonts::with_cache_dir(None);
        let mut defs = FontDefinitions::default();
        let path = write_parseable_font("ui_variant_chain.ttf");
        let face = map_font_file(&path).unwrap();
        insert_face(&mut defs, NORMAL_FONT_ID, face, 0);
        register_variant(&mut defs, BOLD_FONT_ID, BOLD_FAMILY, None, (face, 0));
        register_variant(&mut defs, ITALIC_FONT_ID, ITALIC_FAMILY, None, (face, 0));
        register_variant(&mut defs, BOLD_ITALIC_FONT_ID, BOLD_ITALIC_FAMILY, None, (face, 0));

        install_ui_font(&mut defs, &UiFont::default(), &fonts);

        let proportional_head = defs.families[&FontFamily::Proportional][0].clone();
        for (ui_family, terminal_id) in [
            (UI_BOLD_FAMILY, BOLD_FONT_ID),
            (UI_ITALIC_FAMILY, ITALIC_FONT_ID),
            (UI_BOLD_ITALIC_FAMILY, BOLD_ITALIC_FONT_ID),
        ] {
            let ids = defs.families.get(&FontFamily::Name(ui_family.into())).expect(ui_family);
            assert!(
                ids.contains(&terminal_id.to_string()),
                "{ui_family} must inherit the terminal chain's {terminal_id}"
            );
            assert!(
                ids.contains(&proportional_head),
                "{ui_family} must inherit Proportional's head face"
            );
            let mut seen = std::collections::HashSet::new();
            assert!(ids.iter().all(|id| seen.insert(id)), "{ui_family} has duplicate ids");
        }

        std::fs::remove_file(&path).ok();
    }

    /// The variant parse gate at the heart of `install_ui_variant` exists so a
    /// configured `[ui.font]` variant family pointing at non-font bytes is
    /// skipped instead of registered — registering it would make egui panic
    /// when it parses the face at render time.
    #[test]
    fn unparseable_configured_variant_is_skipped_not_registered() {
        let fonts = SystemFonts::with_cache_dir(None);
        let mut defs = FontDefinitions::default();
        let junk_path = crate::test_util::scratch_dir().join("ui_variant_junk.ttf");
        std::fs::write(&junk_path, b"not a font").unwrap();
        let before = defs.font_data.len();

        let ui = UiFont {
            bold_family: Some(junk_path.to_string_lossy().into_owned()),
            ..UiFont::default()
        };
        install_ui_font(&mut defs, &ui, &fonts);

        assert_eq!(
            defs.font_data.len(),
            before,
            "unparseable bytes must not register a new font id"
        );
        assert!(
            !defs.font_data.contains_key(&format!("{UI_BOLD_FAMILY}_0")),
            "the skipped candidate must never mint an id"
        );

        std::fs::remove_file(&junk_path).ok();
    }

    #[test]
    fn ui_variant_family_maps_the_style_flags() {
        assert_eq!(ui_variant_family(false, false), FontFamily::Proportional);
        assert_eq!(ui_variant_family(true, false), FontFamily::Name(UI_BOLD_FAMILY.into()));
        assert_eq!(ui_variant_family(false, true), FontFamily::Name(UI_ITALIC_FAMILY.into()));
        assert_eq!(ui_variant_family(true, true), FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()));
    }

    // epaint clones the whole buffer of every `Cow::Owned` face it parses, so
    // an owned face costs its file size twice for the life of the process.
    #[test]
    fn registered_faces_hand_epaint_borrowed_bytes() {
        let path = write_parseable_font("borrowed_bytes.ttf");

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![path.to_string_lossy().into_owned()];

        let targets = [FontFamily::Monospace];
        register_user_fallbacks(&mut defs, &entries, Variant::Normal, &targets, &fonts, &mut book);

        let id = book.ids_by_face.get(&(path.clone(), 0)).expect("fallback registered");
        let data = &defs.font_data[id];
        assert!(
            matches!(data.font, std::borrow::Cow::Borrowed(_)),
            "{id} owns its bytes; epaint will clone them"
        );

        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn named_instance_bits_are_stripped_from_fontconfig_indices() {
        // Instance 7 of face 2 in a collection; the face index survives, the
        // instance selection does not.
        assert_eq!(fontconfig_resolve::plain_face_index((7 << 16) | 2), 2);
        assert_eq!(fontconfig_resolve::plain_face_index(3), 3);
        assert_eq!(fontconfig_resolve::plain_face_index(-1), 0);
    }

    // fontconfig returns `(named_instance << 16) | face` for variable fonts;
    // handing that through unmasked makes ttf_parser and epaint reject the
    // face outright, silently dropping every variable font from the chain.
    #[cfg(unix)]
    #[test]
    fn fontconfig_face_indices_never_carry_instance_bits() {
        let skip = HashSet::new();
        let faces = fontconfig_resolve::sorted_fallbacks(
            DEFAULT_FAMILY,
            None,
            Variant::Normal,
            &skip,
            MAX_FALLBACK_FACES,
        );
        for face in &faces {
            assert!(
                face.face_index < 0x1_0000,
                "{} face {} carries named-instance bits",
                face.path.display(),
                face.face_index
            );
        }
    }

    // epaint re-parses every registered face with ab_glyph at first layout
    // and panics on any it cannot parse, so the installed definitions must
    // never contain one.  macOS font indexes commonly carry such faces
    // (.dfont suitcases, bitmap-only families).
    #[test]
    fn every_registered_face_parses_like_epaint() {
        let ctx = Context::default();
        install_terminal_fonts(&ctx, &FontConfig::default(), &UiFont::default());
        // The first pass is what forces epaint to parse every face.
        ctx.begin_pass(Default::default());
        let _ = ctx.end_pass();
    }

    // Unix-excluded: fontconfig substitutes *some* font for any family name,
    // so `[font.normal]` never fails to resolve there and `None` is
    // unreachable through this path on that platform.
    #[cfg(not(unix))]
    #[test]
    fn an_unresolvable_normal_font_still_binds_every_chrome_family() {
        let ctx = Context::default();
        let config = FontConfig {
            normal: crate::config::FontFace {
                family: Some("alacritree-no-such-terminal-family-9f3a".to_string()),
                style: None,
            },
            ..Default::default()
        };
        let (chain, _) = install_terminal_fonts(&ctx, &config, &UiFont::default());
        assert!(chain.is_empty(), "an unresolvable family produces no fallback chain");

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(200.0, 200.0),
            )),
            ..Default::default()
        };
        // egui panics on an unbound `FontFamily::Name`; painting each one and
        // reaching the assertion below is the proof that all six are bound.
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for family in [
                    FontFamily::Name(BOLD_FAMILY.into()),
                    FontFamily::Name(ITALIC_FAMILY.into()),
                    FontFamily::Name(BOLD_ITALIC_FAMILY.into()),
                    FontFamily::Name(UI_BOLD_FAMILY.into()),
                    FontFamily::Name(UI_ITALIC_FAMILY.into()),
                    FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()),
                ] {
                    ui.label(egui::RichText::new("x").font(egui::FontId::new(12.0, family)));
                }
            });
        });
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
            assert!(!skip.contains(&(face.path.clone(), face.face_index)));
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
        let normal_ids: HashSet<String> = book.ids_by_face.values().cloned().collect();
        let loaded_before = defs.font_data.len();

        // A later chain that resolves to already-loaded faces (here: the same
        // family and variant, targeting another family list) must reuse them —
        // joining the new family list without registering duplicate font data.
        let bold_family = FontFamily::Name(BOLD_FAMILY.into());
        let bold_targets = [bold_family.clone()];
        register_fallback_faces(
            &mut defs,
            "Consolas",
            None,
            Variant::Normal,
            &bold_targets,
            &fonts,
            &mut book,
        );

        if fonts.db().faces().next().is_some() && !normal_ids.is_empty() {
            assert_eq!(defs.font_data.len(), loaded_before);
            assert!(defs.families[&bold_family].iter().any(|id| normal_ids.contains(id)));
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn automatic_chain_records_every_loaded_face_in_ids_by_face() {
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
            let ids_keys: HashSet<_> = book.ids_by_face.keys().cloned().collect();
            assert_eq!(ids_keys, book.loaded_faces);
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
        let path = write_parseable_font("user_precedes.ttf");

        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let mut book = FallbackBook::default();
        let entries = vec![path.to_string_lossy().into_owned()];
        let targets = [FontFamily::Monospace];

        register_user_fallbacks(&mut defs, &entries, Variant::Normal, &targets, &fonts, &mut book);
        let user_id = book.ids_by_face.get(&(path.clone(), 0)).cloned().unwrap();
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
    fn primary_face_keeps_its_collection_index() {
        // A family living at a nonzero face index of a collection file must
        // reach egui with that index — face 0 of the same file is a different
        // family entirely (every Sarasa family shares one .ttc, for example),
        // and rendering it gives every cell the wrong glyph and metrics.
        let fonts = SystemFonts::with_cache_dir(None);
        let families: Vec<String> = fonts
            .db()
            .faces()
            .filter(|face| face.index > 0)
            .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
            .collect();
        let Some((family, resolved)) = families.iter().find_map(|family| {
            let resolved = resolve_face(family, None, Variant::Normal, &fonts)?;
            (resolved.face_index > 0).then(|| (family.clone(), resolved))
        }) else {
            // No installed collection resolves past face 0; nothing to check.
            return;
        };

        let config = crate::config::FontConfig {
            normal: crate::config::FontFace { family: Some(family), style: None },
            ..Default::default()
        };
        let (defs, chain) =
            build_font_definitions(&config, &UiFont::default(), &fonts).expect("family resolves");

        assert_eq!(defs.font_data[NORMAL_FONT_ID].index, resolved.face_index);
        assert_eq!(chain[0].face_index, resolved.face_index);
    }

    #[test]
    fn egui_bundled_faces_answer_after_the_configured_fallbacks() {
        // `FontDefinitions::default()` seeds Monospace with epaint's own faces,
        // and `Ubuntu-Light` among them fills the legacy Adobe PUA slots, where
        // U+F001 and U+F002 hold the `fi`/`fl` ligatures.  Nerd Fonts put icons
        // at those codepoints, so a bundled face left ahead of the configured
        // fallbacks answers first and a magnifier draws as `fl`.  They stay in
        // the list — epaint ships `Ubuntu-Light` for `√` and friends — but only
        // once every configured face has had its turn.
        let defaults = FontDefinitions::default();
        let bundled_monospace = defaults.families[&FontFamily::Monospace].clone();
        let bundled_proportional = defaults.families[&FontFamily::Proportional].clone();
        assert!(!bundled_monospace.is_empty(), "egui bundles a monospace family");
        assert!(!bundled_proportional.is_empty(), "egui bundles a proportional family");

        let fonts = SystemFonts::with_cache_dir(None);
        let Some(family) = fonts.db().faces().find_map(|face| {
            let name = face.families.first().map(|(name, _)| name.clone())?;
            resolve_face(&name, None, Variant::Normal, &fonts).map(|_| name)
        }) else {
            // Nothing installed resolves here; there is no chain to order.
            return;
        };

        let path = write_parseable_font("bundled_last.ttf");
        let config = crate::config::FontConfig {
            normal: crate::config::FontFace { family: Some(family), style: None },
            fallback: vec![path.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let defs =
            build_font_definitions(&config, &UiFont::default(), &fonts).expect("family resolves").0;
        std::fs::remove_file(&path).ok();

        for (family, bundled) in [
            (FontFamily::Monospace, &bundled_monospace),
            (FontFamily::Proportional, &bundled_proportional),
            (FontFamily::Name(UI_BOLD_FAMILY.into()), &bundled_proportional),
            (FontFamily::Name(UI_ITALIC_FAMILY.into()), &bundled_proportional),
            (FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()), &bundled_proportional),
        ] {
            let ids = &defs.families[&family];
            let configured = ids
                .iter()
                .position(|id| id.starts_with(USER_FALLBACK_ID))
                .unwrap_or_else(|| panic!("the configured fallback registered in {family:?}"));
            for id in bundled {
                let at = ids.iter().position(|listed| listed == id).unwrap_or_else(|| {
                    panic!("egui's bundled '{id}' was dropped from {family:?}, not demoted")
                });
                assert!(
                    at > configured,
                    "egui's bundled '{id}' answers before a configured fallback in {family:?}"
                );
            }
        }
    }

    #[test]
    fn primary_faces_are_never_tweaked() {
        // Fallback faces get a tweak that rescales them to the primary
        // face's height; the primary itself is the scale reference and must
        // never carry a tweak, or scaling would be relative to a moving target.
        let mut defs = FontDefinitions::default();
        insert_face(
            &mut defs,
            "test_primary",
            b"egui parses this later; registration only maps",
            0,
        );
        assert_eq!(defs.font_data["test_primary"].tweak, FontTweak::default());
    }

    #[test]
    fn variant_faces_carry_their_collection_index() {
        // A real variant keeps its own face index; a missing variant falls
        // back to the normal face *and* its index.
        let mut defs = FontDefinitions::default();
        let normal: (&'static [u8], u32) = (b"normal bytes", 3);
        register_variant(&mut defs, BOLD_FONT_ID, BOLD_FAMILY, Some((b"bold bytes", 7)), normal);
        register_variant(&mut defs, ITALIC_FONT_ID, ITALIC_FAMILY, None, normal);
        assert_eq!(defs.font_data[BOLD_FONT_ID].index, 7);
        assert_eq!(defs.font_data[ITALIC_FONT_ID].index, 3);
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
        let dir = crate::test_util::scratch_dir().join(format!("coverage_cache_{name}"));
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
    fn worker_count_clamps_between_one_and_four() {
        assert_eq!(worker_count(1), 1);
        assert_eq!(worker_count(2), 2);
        assert_eq!(worker_count(4), 4);
        assert_eq!(worker_count(36), 4);
        // `available_parallelism` cannot report zero, but a caller mapping an
        // error to zero would build a pool with no threads.
        assert_eq!(worker_count(0), 1);
    }

    #[cfg(not(unix))]
    #[test]
    fn a_parallel_scan_matches_a_serial_one_element_for_element() {
        // Both runs pass `None`, so neither writes a cache the other could
        // read back: a second scan against a populated cache takes the
        // stored-ranges branch and parses no cmap at all.
        let fonts = SystemFonts::with_cache_dir(None);
        let (serial, _) = scan_coverage_with_workers(fonts.db(), None, 1);
        let (parallel, _) = scan_coverage_with_workers(fonts.db(), None, 4);

        assert_eq!(serial, parallel);
        assert!(!serial.is_empty(), "no faces were scanned, so this proved nothing");
    }

    #[cfg(not(unix))]
    #[test]
    fn every_face_of_a_collection_file_is_a_cache_hit_on_the_second_scan() {
        // A .ttc holds several faces behind one path, and they share one
        // CachedFile.  Accumulating that per worker rather than serially would
        // drop all but one worker's faces, and the only symptom would be that
        // they reparse on every launch.
        let cache_path = scratch_cache_path("collection_hits");
        std::fs::remove_file(&cache_path).ok();

        let cold_fonts = SystemFonts::with_cache_dir(None);
        let (cold, cold_hits) = scan_coverage_with_workers(cold_fonts.db(), Some(&cache_path), 4);
        assert_eq!(cold_hits, 0, "a cold scan cannot hit the cache");

        let warm_fonts = SystemFonts::with_cache_dir(None);
        let (warm, warm_hits) = scan_coverage_with_workers(warm_fonts.db(), Some(&cache_path), 4);

        assert_eq!(cold, warm);

        // Two faces of one path is the hazard this test exists for, so fail
        // loudly on a machine that has no collection file rather than pass
        // without exercising it.
        let mut faces_per_path: HashMap<&PathBuf, usize> = HashMap::new();
        for (candidate, _) in &warm {
            *faces_per_path.entry(&candidate.path).or_default() += 1;
        }
        assert!(
            faces_per_path.values().any(|&n| n > 1),
            "no multi-face font file was scanned, so this proved nothing"
        );

        // Not every face can be cached: one whose file cannot be stat'd never
        // reaches `fresh_files`, and one whose cmap emits a codepoint above
        // U+10FFFF is rejected on the way back out of the cache.  Both are
        // properties of the font set, not of the accumulation.
        let cacheable = cold
            .iter()
            .filter(|(candidate, cov)| {
                candidate.bytes > 0
                    && coverage::Coverage::from_stored_ranges(cov.ranges().to_vec()).is_some()
            })
            .count();
        assert_eq!(warm_hits, cacheable, "every cacheable face should come from the cache");

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

    /// Every glyph alacritree ships must be drawable from the baked face.  A
    /// cmap entry is not enough: a font can map a codepoint to a glyph with no
    /// outline, which paints as a blank box.
    #[test]
    fn the_baked_face_draws_every_glyph_alacritree_ships() {
        let face = ttf_parser::Face::parse(SYMBOLS_FONT, 0).expect("the baked face parses");
        let mut blank = Vec::new();
        for c in baked_glyphs() {
            match face.glyph_index(c) {
                None => blank.push((c, "absent from the cmap")),
                Some(id) => {
                    if face.outline_glyph(id, &mut DiscardOutline).is_none() {
                        blank.push((c, "mapped but has no outline"));
                    }
                },
            }
        }
        assert!(
            blank.is_empty(),
            "regenerate alacritree/assets/alacritree-symbols.ttf; see assets/README.md: {:?}",
            blank
                .iter()
                .map(|(c, why)| format!("U+{:04X} {c} {why}", *c as u32))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_baked_glyph_set_is_the_documented_size() {
        assert_eq!(baked_glyphs().len(), 24, "assets/README.md lists the codepoints");
    }

    /// Last position is the whole guarantee: an earlier face that already draws
    /// a glyph must keep drawing it.  Front-splicing the baked face would
    /// override working chrome instead of filling gaps.
    #[test]
    fn the_symbol_face_lands_last_in_every_chrome_family() {
        let path = write_parseable_font("symbol_order.ttf");
        let mut defs = FontDefinitions::default();
        let fonts = SystemFonts::with_cache_dir(None);
        let ui = UiFont { family: Some(path.to_string_lossy().into_owned()), ..UiFont::default() };
        assert!(install_ui_font(&mut defs, &ui, &fonts));
        install_symbol_fallback(&mut defs, &ui);

        for family in [
            FontFamily::Proportional,
            FontFamily::Name(UI_BOLD_FAMILY.into()),
            FontFamily::Name(UI_ITALIC_FAMILY.into()),
            FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()),
        ] {
            let ids = defs.families.get(&family).unwrap_or_else(|| panic!("{family:?} exists"));
            let at: Vec<usize> = ids
                .iter()
                .enumerate()
                .filter(|(_, id)| id.starts_with(SYMBOLS_ID))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(at.len(), 1, "{family:?} must carry exactly one symbol id, got {ids:?}");
            assert_eq!(at[0], ids.len() - 1, "{family:?} must carry it last, got {ids:?}");
        }
        std::fs::remove_file(&path).ok();
    }

    /// The grid keeps its own chain; a symbol face there would change how
    /// existing terminal output renders.
    #[test]
    fn the_symbol_face_stays_out_of_the_terminal_families() {
        let mut defs = FontDefinitions::default();
        // The named variant families only exist once registration has run, and
        // an absent family is skipped rather than appended to, so seed them or
        // three of the four assertions below would iterate over nothing.
        let mono = defs.families[&FontFamily::Monospace].clone();
        for name in [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY] {
            defs.families.insert(FontFamily::Name(name.into()), mono.clone());
        }
        install_symbol_fallback(&mut defs, &UiFont::default());
        for family in [
            FontFamily::Monospace,
            FontFamily::Name(BOLD_FAMILY.into()),
            FontFamily::Name(ITALIC_FAMILY.into()),
            FontFamily::Name(BOLD_ITALIC_FAMILY.into()),
        ] {
            let ids = defs.families.get(&family).cloned().unwrap_or_default();
            assert!(!ids.iter().any(|id| id.starts_with(SYMBOLS_ID)), "{family:?} got {ids:?}");
        }
    }

    /// Refusing the face must leave registration exactly as it was.
    #[test]
    fn refusing_builtin_symbols_registers_nothing() {
        let mut defs = FontDefinitions::default();
        let before = defs.families.clone();
        install_symbol_fallback(&mut defs, &UiFont { builtin_symbols: false, ..UiFont::default() });
        assert_eq!(defs.families, before);
        assert!(!defs.font_data.keys().any(|id| id.starts_with(SYMBOLS_ID)));
    }

    /// Running twice must not stack a second copy. Covers only the case where
    /// the face is already installed in every target family; a partially
    /// installed state is unreachable in production and untested here.
    #[test]
    fn installing_the_symbol_face_twice_is_idempotent() {
        let mut defs = FontDefinitions::default();
        install_symbol_fallback(&mut defs, &UiFont::default());
        let once = defs.families[&FontFamily::Proportional].clone();
        install_symbol_fallback(&mut defs, &UiFont::default());
        assert_eq!(defs.families[&FontFamily::Proportional], once);
    }

    /// Appending cannot move a family's metrics, which are read from its first
    /// font.  A future change that front-loads the face would silently alter
    /// line height for every chrome label.
    #[test]
    fn the_symbol_face_does_not_become_the_metrics_source() {
        let mut defs = FontDefinitions::default();
        let head_before = defs.families[&FontFamily::Proportional].first().cloned();
        install_symbol_fallback(&mut defs, &UiFont::default());
        assert_eq!(defs.families[&FontFamily::Proportional].first().cloned(), head_before);
    }

    /// When no configured font resolves, the chrome families are aliased to
    /// egui's bundled defaults.  That is precisely when a system is least likely
    /// to have the glyphs, so the face has to reach this path too.
    #[test]
    fn the_unresolvable_font_path_still_gets_the_symbol_face() {
        let defs = unresolvable_font_definitions(&UiFont::default());
        for family in [
            FontFamily::Proportional,
            FontFamily::Name(UI_BOLD_FAMILY.into()),
            FontFamily::Name(UI_ITALIC_FAMILY.into()),
            FontFamily::Name(UI_BOLD_ITALIC_FAMILY.into()),
        ] {
            let ids = defs.families.get(&family).unwrap_or_else(|| panic!("{family:?} exists"));
            assert_eq!(
                ids.iter().position(|id| id.starts_with(SYMBOLS_ID)),
                Some(ids.len() - 1),
                "{family:?} must carry the symbol id last, got {ids:?}"
            );
        }
    }

    /// Counts every glyph the crate paints from its own constants, deduplicated.
    fn baked_glyphs() -> Vec<char> {
        let mut seen: Vec<char> = crate::config::DEFAULT_ICON_GLYPHS
            .iter()
            .chain(crate::config::CHROME_GLYPHS.iter())
            .flat_map(|g| g.as_str().chars())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// A seed the coverage scan already carries must not be parsed again — the
    /// scan is disk-cached across launches and the parse is a whole-file read.
    #[cfg(not(unix))]
    #[test]
    fn a_seed_present_in_the_scan_is_not_parsed_again() {
        let fonts = SystemFonts::with_cache_dir(None);
        let Some(seed) = resolve_face("Consolas", None, Variant::Normal, &fonts) else {
            log::warn!("Consolas is not installed; nothing to assert");
            return;
        };
        // A seed missing from the scan would pass this by falling through, so
        // its presence is the precondition, not an assumption.
        assert!(
            fonts.scanned_coverage().iter().any(|(candidate, _)| candidate.path == seed.path
                && candidate.face_index == seed.face_index),
            "the seed is absent from the scan; this test would prove nothing"
        );

        reset_face_coverage_parses();
        let skip = HashSet::new();
        gather_fallback_faces("Consolas", None, Variant::Normal, &skip, 8, &fonts);

        assert_eq!(face_coverage_parses(), 0, "a seed already in the scan was parsed anyway");
        assert!(
            fonts.seed_coverage.borrow().contains_key(&(seed.path.clone(), seed.face_index)),
            "the seed was never resolved, so a parse count of zero proves nothing"
        );
    }

    /// A seed the scan cannot answer for is parsed at most once per install,
    /// however many variant chains ask for it.
    #[cfg(not(unix))]
    #[test]
    fn a_seed_outside_the_scan_is_parsed_once_per_install() {
        let fonts = SystemFonts::with_cache_dir(None);
        let path = write_parseable_font("seed_memo.ttf");
        let family = path.to_str().expect("the temp path is utf-8");
        let seed = resolve_face(family, None, Variant::Normal, &fonts).expect("a path resolves");
        // An explicit path is not automatically outside the scan: a system
        // font's own path resolves the same way, and the scan contains it.
        assert!(
            !fonts.scanned_coverage().iter().any(|(candidate, _)| candidate.path == seed.path
                && candidate.face_index == seed.face_index),
            "the fixture is in the scan; this test would prove nothing"
        );

        reset_face_coverage_parses();
        let skip = HashSet::new();
        gather_fallback_faces(family, None, Variant::Normal, &skip, 8, &fonts);
        gather_fallback_faces(family, None, Variant::Normal, &skip, 8, &fonts);

        assert_eq!(face_coverage_parses(), 1, "the seed was re-parsed for the second chain");
    }

    /// The fallback parse borrows the mapping like every other font read in
    /// this module, rather than pulling a whole collection onto the heap.
    #[cfg(not(unix))]
    #[test]
    fn face_coverage_maps_the_file_instead_of_reading_it() {
        let path = write_parseable_font("face_coverage_maps.ttf");
        assert!(!is_mapped(&path), "the fixture path must be untouched by other tests");

        let _ = face_coverage(&path, 0);

        assert!(is_mapped(&path), "face_coverage read the file instead of mapping it");
    }

    /// The collect-and-sort construction, kept as an oracle for the fold.
    /// If this and `cmap_coverage` ever disagree, the fold is wrong, not
    /// this.
    #[cfg(not(unix))]
    fn cmap_coverage_by_sorting(face: &ttf_parser::Face) -> Option<coverage::Coverage> {
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

    #[cfg(not(unix))]
    #[test]
    fn cmap_coverage_matches_the_sorting_implementation_on_every_system_face() {
        let fonts = SystemFonts::with_cache_dir(None);
        let db = fonts.db();
        let mut compared = 0usize;
        for face in db.faces() {
            let both = db.with_face_data(face.id, |data, index| {
                let parsed = ttf_parser::Face::parse(data, index).ok()?;
                Some((cmap_coverage(&parsed), cmap_coverage_by_sorting(&parsed)))
            });
            let Some(Some((folded, sorted))) = both else {
                continue;
            };
            assert_eq!(folded, sorted, "coverage differs for {:?}", face.source);
            compared += 1;
        }
        assert!(compared > 0, "no system faces were parsed, so this proved nothing");
    }

    /// Raw font units are in the hundreds; em fractions are not.  A face read
    /// without dividing by `units_per_em` passes every other test in this file
    /// and puts the underline several cells below the glyph.
    #[test]
    fn the_bundled_face_reports_em_fractions() {
        let m = FaceMetrics::from_face(SYMBOLS_FONT, 0);
        assert!((0.5..1.5).contains(&m.ascender), "ascender {}", m.ascender);
        assert!((-0.6..0.0).contains(&m.descender), "descender {}", m.descender);
        assert!(m.underline_position.abs() < 1.0, "underline {}", m.underline_position);
        assert!(m.strikeout_position.abs() < 1.0, "strikeout {}", m.strikeout_position);
        assert!(
            m.underline_thickness > 0.0 && m.underline_thickness <= 0.5,
            "underline thickness {}",
            m.underline_thickness
        );
        assert!(
            m.strikeout_thickness > 0.0 && m.strikeout_thickness <= 0.5,
            "strikeout thickness {}",
            m.strikeout_thickness
        );
    }

    /// Bytes that are not a font at all, which is what a truncated or swapped
    /// file looks like by the time it reaches here.
    #[test]
    fn an_unreadable_face_yields_defaults() {
        assert_eq!(FaceMetrics::from_face(b"not a font", 0), FaceMetrics::default());
    }

    /// ghostty guards the same way in `has_broken_strikethrough`: a zero in OS/2
    /// would otherwise draw a bar with no height at all.
    #[test]
    fn a_zero_strikeout_thickness_borrows_the_underline_weight() {
        let broken = FaceMetrics { strikeout_thickness: 0.0, ..FaceMetrics::default() };
        let fixed = resolve_fallbacks(broken);
        assert_eq!(fixed.strikeout_thickness, fixed.underline_thickness);
    }

    /// kitty puts the bar at `floor(baseline * 0.65)` from the cell top, which is
    /// 0.35 of the ascender above the baseline.
    #[test]
    fn a_zero_strikeout_position_follows_the_ascender() {
        let broken =
            FaceMetrics { strikeout_position: 0.0, ascender: 0.9, ..FaceMetrics::default() };
        let fixed = resolve_fallbacks(broken);
        assert!((fixed.strikeout_position - 0.315).abs() < 1e-6, "{}", fixed.strikeout_position);
    }

    #[test]
    fn a_zero_underline_pair_falls_back_to_the_defaults() {
        let broken = FaceMetrics {
            underline_position: 0.0,
            underline_thickness: 0.0,
            ..FaceMetrics::default()
        };
        let fixed = resolve_fallbacks(broken);
        assert_eq!(fixed.underline_position, FaceMetrics::default().underline_position);
        assert_eq!(fixed.underline_thickness, FaceMetrics::default().underline_thickness);
    }

    /// A non-negative descender passes the "is it zero" check but still
    /// inverts `descent` downstream, so it needs its own rejection.
    #[test]
    fn a_positive_descender_falls_back_to_the_default() {
        let broken = FaceMetrics { descender: 0.2, ..FaceMetrics::default() };
        let fixed = resolve_fallbacks(broken);
        assert_eq!(fixed.descender, FaceMetrics::default().descender);
    }

    /// A non-positive ascender passes the "is it zero" check but still flips
    /// `px_per_em` downstream.  The rejection also has to feed the *default*
    /// ascender into the strikeout-position fallback, not the rejected value.
    #[test]
    fn a_negative_ascender_falls_back_to_the_default() {
        let broken =
            FaceMetrics { ascender: -0.1, strikeout_position: 0.0, ..FaceMetrics::default() };
        let fixed = resolve_fallbacks(broken);
        assert_eq!(fixed.ascender, FaceMetrics::default().ascender);
        assert_eq!(fixed.strikeout_position, FaceMetrics::default().strikeout_position);
    }

    /// `[font.normal]` unresolvable means `build_font_definitions` returned
    /// `None` and there is no face to read, which is not the same case as a face
    /// that failed to parse but reaches the same place.
    #[test]
    fn an_empty_chain_yields_defaults() {
        assert_eq!(primary_face_metrics(&[]), FaceMetrics::default());
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

        /// Build from a walk that emits codepoints in ascending order, folding
        /// them into ranges as they arrive rather than sorting them afterwards.
        ///
        /// cmap subtables are required to enumerate ascending, so this is the
        /// normal path.  A walk that turns out not to be ascending is re-run
        /// through `from_codepoints`, which is why `walk` is `Fn`: `codepoints`
        /// has no early exit, so the first pass has to finish before the second
        /// can start.
        pub fn from_ascending_walk(walk: impl Fn(&mut dyn FnMut(u32))) -> Self {
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            let mut ascending = true;
            walk(&mut |cp| {
                match ranges.last_mut() {
                    Some((_, end)) if cp.checked_sub(1) == Some(*end) => *end = cp,
                    // Equality counts: a repeat would otherwise push a range that
                    // overlaps the one before it.  Rejecting `cp <= end` is also
                    // what keeps `end` the maximum seen so far, which is what
                    // makes this check total.
                    Some((_, end)) if cp <= *end => ascending = false,
                    _ => ranges.push((cp, cp)),
                }
            });
            if ascending {
                return Self { ranges };
            }
            let mut codepoints = Vec::new();
            walk(&mut |cp| codepoints.push(cp));
            Self::from_codepoints(codepoints)
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
        fn ascending_walk_folds_runs_into_ranges() {
            let cov = Coverage::from_ascending_walk(|emit| {
                for cp in [1u32, 2, 3, 10, 11, 50] {
                    emit(cp);
                }
            });
            assert_eq!(cov.ranges(), &[(1, 3), (10, 11), (50, 50)]);
        }

        #[test]
        fn ascending_walk_matches_from_codepoints() {
            let cps: Vec<u32> = (0..500).chain(1000..1200).chain([9000, 9001, 65535]).collect();
            let folded = Coverage::from_ascending_walk(|emit| {
                for &cp in &cps {
                    emit(cp);
                }
            });
            assert_eq!(folded, Coverage::from_codepoints(cps));
        }

        #[test]
        fn ascending_walk_falls_back_when_a_codepoint_repeats() {
            // A repeat is not a regression, but folding it blindly would push
            // (5, 5) after a range already ending at 5 and produce an overlap.
            let cov = Coverage::from_ascending_walk(|emit| {
                for cp in [1u32, 2, 5, 5, 6] {
                    emit(cp);
                }
            });
            assert_eq!(cov.ranges(), &[(1, 2), (5, 6)]);
        }

        #[test]
        fn ascending_walk_falls_back_when_the_walk_goes_backwards() {
            let cov = Coverage::from_ascending_walk(|emit| {
                for cp in [10u32, 11, 3, 4] {
                    emit(cp);
                }
            });
            assert_eq!(cov.ranges(), &[(3, 4), (10, 11)]);
        }

        #[test]
        fn ascending_walk_handles_a_codepoint_at_the_top_of_the_range() {
            // `end + 1` would overflow here; the fold compares the other way around.
            let cov = Coverage::from_ascending_walk(|emit| {
                emit(u32::MAX - 1);
                emit(u32::MAX);
            });
            assert_eq!(cov.ranges(), &[(u32::MAX - 1, u32::MAX)]);
        }

        #[test]
        fn ascending_walk_over_nothing_is_empty() {
            let cov = Coverage::from_ascending_walk(|_emit| {});
            assert_eq!(cov, Coverage::default());
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
