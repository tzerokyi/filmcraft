//! The font database: bundled fonts (always available, also on the web), the optional
//! [`CRAFT_FONTS`] (present when built with `CRAFT_FONTS_DIR`, also on the web) plus fonts
//! discovered by [`FontSource`]s (system font folders on native platforms).
//!
//! Faces are addressed by [`FaceId`] (an index that never changes for the life of the process).
//! [`resolve`] maps a family + style name (as stored in projects) to a face and the synthetic
//! bold/italic needed when the family lacks that style. Unknown families fall back to Inter.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use skrifa::instance::{LocationRef, Size};
use skrifa::{FontRef, MetadataProvider};

use crate::sfnt::{FaceNames, read_faces, read_faces_bytes};

/// Bundled fonts (SIL OFL 1.1; see `assets/fonts/*.attribution` and ATTRIBUTION.md).
pub static INTER_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
pub static INTER_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/Inter-Medium.ttf");
pub static INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");
pub static INTER_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-Bold.ttf");
pub static INTER_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/Inter-Italic.ttf");
pub static JETBRAINS_MONO_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
pub static NOTO_SERIF_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/NotoSerif-Regular.ttf");

/// A font from the optional craft-fonts build input (empty unless built with `CRAFT_FONTS_DIR`; see
/// `build.rs` and storytold/craft-fonts `docs/integration.md`).
pub struct CraftFont {
    pub family: &'static str,
    pub style: &'static str,
    /// ISO 15924 scripts the font is for, e.g. `"Jpan"`.
    pub scripts: &'static [&'static str],
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/craft_fonts.rs"));

/// The craft-fonts entries for Japanese (`Jpan`), in [`CRAFT_FONTS`] order. Empty when the app was
/// built without craft-fonts.
pub fn craft_japanese() -> impl Iterator<Item = &'static CraftFont> {
    CRAFT_FONTS.iter().filter(|f| f.scripts.contains(&"Jpan"))
}

/// Origin of faces registered from [`CRAFT_FONTS`].
pub const CRAFT_ORIGIN: &str = "craft-fonts";

/// The default family for new text.
pub const DEFAULT_FAMILY: &str = "Inter";

/// Index of a face in the database.
pub type FaceId = usize;

/// Where a face's bytes come from.
#[derive(Clone, Debug)]
pub enum FaceData {
    Static(&'static [u8]),
    Shared(Arc<Vec<u8>>),
    /// A file read on first use.
    File(PathBuf),
}

/// Description of a face offered by a [`FontSource`].
#[derive(Clone, Debug)]
pub struct FaceInfo {
    pub family: String,
    pub style: String,
    pub weight: u16,
    pub italic: bool,
    pub index: u32,
    pub data: FaceData,
    /// "bundled", "system", …
    pub origin: &'static str,
}

/// Something that can list fonts (system folders, a web font picker, a project's font folder…).
pub trait FontSource: Send + Sync {
    fn faces(&self) -> Vec<FaceInfo>;
}

/// Scans font folders on disk (reading only name tables). Native only; on the web there are no
/// folders and the scan finds nothing.
pub struct DirectorySource {
    pub dirs: Vec<PathBuf>,
}

impl DirectorySource {
    /// The platform's standard font folders.
    pub fn system() -> Self {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if cfg!(target_os = "macos") {
            dirs.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
            if let Some(h) = &home {
                dirs.push(h.join("Library/Fonts"));
            }
        } else if cfg!(windows) {
            let win = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
            dirs.push(win.join("Fonts"));
            if let Some(l) = std::env::var_os("LOCALAPPDATA") {
                dirs.push(PathBuf::from(l).join("Microsoft\\Windows\\Fonts"));
            }
        } else if cfg!(unix) && !cfg!(target_arch = "wasm32") {
            dirs.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
            if let Some(h) = &home {
                dirs.push(h.join(".local/share/fonts"));
                dirs.push(h.join(".fonts"));
            }
        }
        Self { dirs }
    }
}

fn walk(dir: &std::path::Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 4 {
                walk(&p, depth + 1, out);
            }
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc")) {
            out.push(p);
        }
    }
}

impl FontSource for DirectorySource {
    fn faces(&self) -> Vec<FaceInfo> {
        let mut files = Vec::new();
        for d in &self.dirs {
            walk(d, 0, &mut files);
        }
        files.sort();
        let mut out = Vec::new();
        for f in files {
            let Ok(mut file) = std::fs::File::open(&f) else { continue };
            for n in read_faces(&mut file) {
                out.push(info_from(n, FaceData::File(f.clone()), "system"));
            }
        }
        out
    }
}

fn info_from(n: FaceNames, data: FaceData, origin: &'static str) -> FaceInfo {
    FaceInfo { family: n.family, style: n.style, weight: n.weight, italic: n.italic, index: n.index, data, origin }
}

/// A loaded (or loadable) face.
pub struct Face {
    pub id: FaceId,
    pub info: FaceInfo,
    bytes: OnceLock<Option<FaceBytes>>,
    shaper: OnceLock<harfrust::ShaperData>,
}

#[derive(Clone)]
enum FaceBytes {
    Static(&'static [u8]),
    Shared(Arc<Vec<u8>>),
}

impl FaceBytes {
    fn get(&self) -> &[u8] {
        match self {
            FaceBytes::Static(b) => b,
            FaceBytes::Shared(b) => b,
        }
    }
}

/// Vertical metrics at a pixel size (all positive distances, y down from the baseline for the
/// underline position).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub cap_height: f32,
    pub x_height: f32,
    pub underline_pos: f32,
    pub underline_thickness: f32,
}

impl Face {
    fn bytes(&self) -> Option<&[u8]> {
        self.bytes
            .get_or_init(|| match &self.info.data {
                FaceData::Static(b) => Some(FaceBytes::Static(b)),
                FaceData::Shared(b) => Some(FaceBytes::Shared(b.clone())),
                FaceData::File(p) => std::fs::read(p).ok().map(|v| FaceBytes::Shared(Arc::new(v))),
            })
            .as_ref()
            .map(FaceBytes::get)
    }
    /// A copy of the font file's bytes (None if it is missing or unreadable).
    pub fn data(&self) -> Option<Vec<u8>> {
        self.bytes().map(<[u8]>::to_vec)
    }
    /// The parsed font (None if the file is missing or unreadable).
    pub fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(self.bytes()?, self.info.index).ok()
    }
    pub(crate) fn shaper_data(&self) -> Option<&harfrust::ShaperData> {
        let f = self.font()?;
        Some(self.shaper.get_or_init(|| harfrust::ShaperData::new(&f)))
    }
    pub fn units_per_em(&self) -> f32 {
        self.font().map(|f| f.metrics(Size::unscaled(), LocationRef::default()).units_per_em as f32).filter(|u| *u > 0.0).unwrap_or(1000.0)
    }
    /// Glyph for a character (None when the face does not cover it).
    pub fn glyph(&self, c: char) -> Option<u32> {
        self.font()?.charmap().map(c).map(|g| g.to_u32()).filter(|g| *g != 0)
    }
    pub fn has_char(&self, c: char) -> bool {
        self.glyph(c).is_some()
    }
    /// Whether the face covers every character of `text`. Unlike [`Face::has_char`], a face whose
    /// file is not loaded yet is read into a temporary buffer that is dropped afterwards, so
    /// probing many system faces does not keep every font file in memory.
    pub fn covers_text(&self, text: &str) -> bool {
        if let (FaceData::File(p), None) = (&self.info.data, self.bytes.get()) {
            let Ok(data) = std::fs::read(p) else { return false };
            let Ok(font) = FontRef::from_index(&data, self.info.index) else { return false };
            let charmap = font.charmap();
            return text.chars().all(|c| charmap.map(c).is_some_and(|g| g.to_u32() != 0));
        }
        text.chars().all(|c| self.has_char(c))
    }
    /// Height of the glyph's vertical origin above its baseline, in pixels.
    /// OpenType VORG takes precedence; TrueType uses the ink top plus vmtx's top bearing.
    pub(crate) fn vertical_origin(&self, gid: u32, px: f32) -> Option<f32> {
        use skrifa::raw::TableProvider;
        let font = self.font()?;
        let glyph = skrifa::GlyphId::new(gid);
        let units = match font.vorg() {
            Ok(table) => f32::from(table.vertical_origin_y(glyph)),
            Err(_) => {
                let bounds = font.glyph_metrics(Size::unscaled(), LocationRef::default()).bounds(glyph)?;
                bounds.y_max + f32::from(font.vmtx().ok()?.side_bearing(glyph)?)
            }
        };
        let origin = units * px / self.units_per_em();
        (origin.is_finite() && origin.abs() < px * 4.0).then_some(origin)
    }

    pub fn metrics(&self, px: f32) -> VMetrics {
        let Some(f) = self.font() else {
            return VMetrics {
                ascent: px * 0.8,
                descent: px * 0.2,
                cap_height: px * 0.7,
                x_height: px * 0.5,
                underline_pos: px * 0.1,
                underline_thickness: px * 0.06,
                ..Default::default()
            };
        };
        let m = f.metrics(Size::new(px), LocationRef::default());
        VMetrics {
            ascent: m.ascent,
            descent: -m.descent,
            line_gap: m.leading,
            cap_height: m.cap_height.unwrap_or(m.ascent * 0.7),
            x_height: m.x_height.unwrap_or(m.ascent * 0.5),
            underline_pos: m.underline.map_or(px * 0.1, |u| -u.offset),
            underline_thickness: m.underline.map_or(px * 0.06, |u| u.thickness.max(px * 0.03)),
        }
    }
    /// Whether the face's GSUB offers an OpenType feature (e.g. `smcp`).
    pub fn has_feature(&self, tag: &[u8; 4]) -> bool {
        use skrifa::raw::TableProvider;
        let Some(f) = self.font() else { return false };
        let Ok(gsub) = f.gsub() else { return false };
        let Ok(list) = gsub.feature_list() else { return false };
        list.feature_records().iter().any(|r| r.feature_tag().to_be_bytes() == *tag)
    }
}

struct Db {
    faces: Vec<Arc<Face>>,
    scanned: bool,
}

fn db() -> &'static RwLock<Db> {
    static DB: OnceLock<RwLock<Db>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = Db { faces: Vec::new(), scanned: false };
        for b in [INTER_REGULAR, INTER_MEDIUM, INTER_SEMIBOLD, INTER_BOLD, INTER_ITALIC, JETBRAINS_MONO_REGULAR, NOTO_SERIF_REGULAR] {
            for n in read_faces_bytes(b) {
                push(&mut db, info_from(n, FaceData::Static(b), "bundled"));
            }
        }
        // after the bundled faces, before anything a FontSource finds (a system copy of the same
        // family + style is then skipped as a duplicate)
        for f in CRAFT_FONTS {
            for n in read_faces_bytes(f.bytes) {
                push(&mut db, info_from(n, FaceData::Static(f.bytes), CRAFT_ORIGIN));
            }
        }
        RwLock::new(db)
    })
}

fn push(db: &mut Db, info: FaceInfo) -> FaceId {
    let id = db.faces.len();
    db.faces.push(Arc::new(Face { id, info, bytes: OnceLock::new(), shaper: OnceLock::new() }));
    id
}

/// Add faces from a source (skipping family+style pairs already present). Returns how many were
/// added.
pub fn add_source(src: &dyn FontSource) -> usize {
    let faces = src.faces();
    let mut d = db().write().unwrap_or_else(|e| e.into_inner());
    let mut n = 0;
    for f in faces {
        let dup = d.faces.iter().any(|g| g.info.family.eq_ignore_ascii_case(&f.family) && g.info.style.eq_ignore_ascii_case(&f.style));
        if !dup {
            push(&mut d, f);
            n += 1;
        }
    }
    n
}

/// Register a font from memory (e.g. a font file the user picked on the web). Returns its faces.
pub fn add_font_data(data: Vec<u8>) -> Vec<FaceId> {
    let data = Arc::new(data);
    let names = read_faces_bytes(&data);
    let mut d = db().write().unwrap_or_else(|e| e.into_inner());
    names.into_iter().map(|n| push(&mut d, info_from(n, FaceData::Shared(data.clone()), "user"))).collect()
}

/// Scan the system font folders once (native only; a no-op on wasm). Returns the number of faces
/// added by this call.
pub fn scan_system() -> usize {
    // Held for the whole scan: a concurrent caller waits for the complete list instead of
    // returning early and resolving against a half-filled database.
    static SCAN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _scan = SCAN.lock().unwrap_or_else(|e| e.into_inner());
    if system_scanned() {
        return 0;
    }
    let n = if cfg!(target_arch = "wasm32") { 0 } else { add_source(&DirectorySource::system()) };
    db().write().unwrap_or_else(|e| e.into_inner()).scanned = true;
    n
}

pub fn system_scanned() -> bool {
    db().read().unwrap_or_else(|e| e.into_inner()).scanned
}

/// A face by id (unknown ids give the default face).
// The bundled faces are compiled in (`include_bytes!`) and registered when the database is
// created, so `faces` is never empty.
#[allow(clippy::expect_used)]
pub fn face(id: FaceId) -> Arc<Face> {
    let d = db().read().unwrap_or_else(|e| e.into_inner());
    d.faces.get(id).or_else(|| d.faces.first()).cloned().expect("bundled fonts present")
}

/// All faces (bundled first).
pub fn all_faces() -> Vec<Arc<Face>> {
    db().read().unwrap_or_else(|e| e.into_inner()).faces.clone()
}

/// Families with their style names, sorted by family; styles in weight order.
pub fn families() -> Vec<(String, Vec<String>)> {
    let mut m: std::collections::BTreeMap<String, Vec<(bool, u16, String)>> = Default::default();
    for f in all_faces() {
        let v = m.entry(f.info.family.clone()).or_default();
        if !v.iter().any(|(_, _, s)| s == &f.info.style) {
            v.push((f.info.italic, f.info.weight, f.info.style.clone()));
        }
    }
    m.into_iter()
        .map(|(k, mut v)| {
            v.sort();
            (k, v.into_iter().map(|(_, _, s)| s).collect())
        })
        .collect()
}

/// The face chosen for a family/style request, plus synthetic emboldening / slant when the family
/// lacks a real bold / italic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Resolved {
    pub face: FaceId,
    pub synth_bold: bool,
    pub synth_italic: bool,
    /// The requested family was not found (Premiere shows such fonts as missing).
    pub missing: bool,
}

fn style_wants(style: &str) -> (u16, bool) {
    let s = style.to_ascii_lowercase().replace([' ', '-'], "");
    let italic = s.contains("italic") || s.contains("oblique");
    let w = if s.contains("thin") || s.contains("hairline") {
        100
    } else if s.contains("extralight") || s.contains("ultralight") {
        200
    } else if s.contains("light") {
        300
    } else if s.contains("medium") {
        500
    } else if s.contains("semibold") || s.contains("demibold") {
        600
    } else if s.contains("extrabold") || s.contains("ultrabold") {
        800
    } else if s.contains("black") || s.contains("heavy") {
        900
    } else if s.contains("bold") {
        700
    } else {
        400
    };
    (w, italic)
}

/// Resolve a family + style name. Family matching is case-insensitive; a style that the family
/// lacks picks the nearest weight and synthesises bold (≥ 600 requested, ≤ 500 found) or italic.
pub fn resolve(family: &str, style: &str) -> Resolved {
    if let Some(r) = resolve_in(family, style) {
        return r;
    }
    if !system_scanned() && !family.is_empty() && !family.eq_ignore_ascii_case(DEFAULT_FAMILY) {
        scan_system();
        if let Some(r) = resolve_in(family, style) {
            return r;
        }
    }
    let mut r = resolve_in(DEFAULT_FAMILY, style).unwrap_or(Resolved { face: 0, synth_bold: false, synth_italic: false, missing: true });
    r.missing = !family.is_empty() && !family.eq_ignore_ascii_case(DEFAULT_FAMILY);
    r
}

fn resolve_in(family: &str, style: &str) -> Option<Resolved> {
    let faces = all_faces();
    let fam: Vec<&Arc<Face>> = faces.iter().filter(|f| f.info.family.eq_ignore_ascii_case(family)).collect();
    if fam.is_empty() {
        return None;
    }
    if let Some(f) = fam.iter().find(|f| f.info.style.eq_ignore_ascii_case(style)) {
        return Some(Resolved { face: f.id, synth_bold: false, synth_italic: false, missing: false });
    }
    let (w, it) = style_wants(style);
    let best = fam.iter().min_by_key(|f| {
        let italic_pen = if f.info.italic == it { 0 } else { 1000 };
        italic_pen + (f.info.weight as i32 - w as i32).unsigned_abs()
    })?;
    Some(Resolved { face: best.id, synth_bold: w >= 600 && best.info.weight <= 500, synth_italic: it && !best.info.italic, missing: false })
}

/// Whether a family name reads as a serif face (Noto Serif, Shippori Mincho, …), to pick a Mincho
/// fallback for serif text and a Gothic one otherwise.
fn is_serif(family: &str) -> bool {
    let f = family.to_ascii_lowercase();
    (f.contains("serif") && !f.contains("sans")) || f.contains("mincho")
}

/// Native font family preferences by Unicode script. These are hints, not a fixed font list:
/// any installed font can serve a glyph if none of the preferred families has it.
/// Explicitly selected project fonts always take precedence when they cover a character.
fn preferred_system_families(c: char) -> &'static [&'static str] {
    match c as u32 {
        // Hangul Jamo, compatibility Jamo, syllables and extended Jamo.
        0x1100..=0x11ff | 0x3130..=0x318f | 0xa960..=0xa97f | 0xac00..=0xd7af | 0xd7b0..=0xd7ff | 0xffa0..=0xffdc => &[
            "Noto Sans KR", "Noto Sans CJK KR", "Noto Serif KR", "Noto Serif CJK KR",
            "NanumGothic", "Nanum Gothic", "NanumMyeongjo", "Malgun Gothic",
            "Apple SD Gothic Neo", "Source Han Sans K", "Source Han Serif K",
            "UnDotum", "Baekmuk Gulim",
        ],
        // Hiragana and Katakana.
        0x3040..=0x30ff | 0x31f0..=0x31ff => &[
            "Noto Sans JP", "Noto Sans CJK JP", "Noto Serif JP", "Noto Serif CJK JP",
            "Yu Gothic", "Hiragino Sans", "Meiryo", "BIZ UDPGothic",
        ],
        // Arabic (including supplementary Arabic blocks).
        0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff => &[
            "Noto Naskh Arabic", "Noto Sans Arabic", "Amiri", "Geeza Pro", "Arial",
        ],
        0x0590..=0x05ff | 0xfb1d..=0xfb4f => &["Noto Sans Hebrew", "Noto Serif Hebrew", "Arial Hebrew", "David", "Arial"],
        0x0900..=0x097f | 0xa8e0..=0xa8ff => &["Noto Sans Devanagari", "Noto Serif Devanagari", "Mangal", "Kohinoor Devanagari"],
        0x0e00..=0x0e7f => &["Noto Sans Thai", "Noto Serif Thai", "Leelawadee UI", "Thonburi"],
        // Han ideographs have shared code points across locales. A manually chosen
        // project font wins above; absent that, the existing craft Japanese priority
        // is retained for compatibility with Japanese title projects.
        0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x3134f => &[
            "Noto Sans CJK SC", "Noto Sans SC", "Source Han Sans SC",
            "Noto Sans CJK TC", "Noto Sans CJK JP", "Microsoft YaHei",
            "PingFang SC", "Hiragino Sans",
        ],
        _ => &[],
    }
}

fn is_hangul(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x11ff | 0x3130..=0x318f | 0xa960..=0xa97f | 0xac00..=0xd7af | 0xd7b0..=0xd7ff | 0xffa0..=0xffdc)
}

/// Find a native face without keeping the bytes of every probed system font in RAM.
/// We rank names/weights first, then probe cmap on demand; only the chosen face is
/// loaded into the text renderer. The database's system scan reads name tables only.
fn native_fallback(c: char, primary: &Face) -> Option<FaceId> {
    if cfg!(target_arch = "wasm32") {
        return None;
    }
    scan_system();
    let preferred = preferred_system_families(c);
    let serif = is_serif(&primary.info.family);
    let mut candidates: Vec<_> = all_faces().into_iter().filter(|f| f.info.origin == "system").collect();
    candidates.sort_by_key(|f| {
        let rank = preferred.iter().position(|name| f.info.family.eq_ignore_ascii_case(name)).unwrap_or(usize::MAX);
        (
            rank,
            is_serif(&f.info.family) != serif,
            f.info.italic != primary.info.italic,
            f.info.weight.abs_diff(primary.info.weight),
        )
    });
    let glyph = c.to_string();
    candidates.into_iter().find(|f| f.covers_text(&glyph)).map(|f| f.id)
}

/// A face that covers `c`. The requested font always wins. For characters absent
/// from it, prefer script-appropriate embedded/system faces (Korean for Hangul),
/// then any font that contains the glyph. Cache results per glyph and primary face
/// to avoid re-scanning font files for every frame of a caption.
/// A new registered font invalidates cached decisions through the face count.
pub fn fallback_for(c: char, prefer: FaceId) -> FaceId {
    let primary = face(prefer);
    if primary.has_char(c) || c.is_control() {
        return prefer;
    }
    // Native fonts are essential for Hangul on builds without a Korean craft face.
    // Scan before inspecting the cache so the first caption can use them immediately.
    let hangul = is_hangul(c);
    if hangul && !system_scanned() {
        scan_system();
    }
    type Cache = std::collections::HashMap<(char, FaceId), (usize, FaceId)>;
    static CACHE: OnceLock<std::sync::Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let faces = all_faces();
    let version = faces.len();
    if let Some(&(saved_version, id)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&(c, prefer))
        && saved_version == version
    {
        return id;
    }

    let serif = is_serif(&primary.info.family);
    let choose_craft = |korean_only: bool| {
        let mut craft: Vec<_> = faces.iter().filter(|f| {
            f.info.origin == CRAFT_ORIGIN
                && (!korean_only || CRAFT_FONTS.iter().any(|entry| {
                    entry.scripts.contains(&"Kore") && entry.family.eq_ignore_ascii_case(&f.info.family)
                }))
        }).collect();
        craft.sort_by_key(|f| (is_serif(&f.info.family) != serif, f.info.italic != primary.info.italic, f.info.weight.abs_diff(primary.info.weight)));
        craft.into_iter().find(|f| f.has_char(c)).map(|f| f.id)
    };

    let result = if hangul {
        // Do not let a pan-CJK Japanese/Chinese face shadow a Korean UI font.
        choose_craft(true)
            .or_else(|| native_fallback(c, &primary))
            .or_else(|| choose_craft(false))
    } else {
        faces.iter().find(|f| f.info.origin == "bundled" && f.has_char(c)).map(|f| f.id)
            .or_else(|| choose_craft(false))
            .or_else(|| faces.iter().find(|f| f.info.origin != "bundled" && f.info.origin != "system" && f.info.origin != CRAFT_ORIGIN && f.has_char(c)).map(|f| f.id))
            .or_else(|| native_fallback(c, &primary))
    }.unwrap_or(prefer);

    // The native scan might have registered faces since 'version' was captured.
    // Store the final count, and invalidate after any later add_source/add_font_data.
    let final_version = all_faces().len();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() > 8192 {
        guard.clear();
    }
    guard.insert((c, prefer), (final_version, result));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captions/titles must find installed Hangul glyphs even if Inter lacks them.
    /// This test runs without craft-fonts and skips only when no Korean glyphs
    /// exist on the runner (e.g. a minimal Linux CI image).
    #[test]
    fn korean_caption_uses_installed_font() {
        scan_system();
        let text = "한글 자막 테스트";
        let glyphs: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        let installed = all_faces().iter().any(|f| f.info.origin == "system" && glyphs.iter().all(|c| f.covers_text(&c.to_string())));
        let embedded = CRAFT_FONTS.iter().any(|f| glyphs.iter().all(|c| {
            read_faces_bytes(f.bytes).iter().any(|n| n.family == f.family && resolve(&n.family, &n.style).face != usize::MAX)
        }));
        if !installed && !embedded {
            eprintln!("SKIPPED: no installed Korean font or craft-fonts");
            return;
        }
        let default_face = resolve("Inter", "Regular").face;
        for c in glyphs {
            let id = fallback_for(c, default_face);
            assert!(face(id).has_char(c), "missing Korean glyph: {c}");
        }
        let layout = crate::layout(text, &crate::TextStyle { size: 42.0, ..Default::default() }, &Default::default());
        assert!(layout.glyphs.iter().all(|g| g.id != 0), "Korean caption rendered .notdef");
    }

    /// Explicit project font selection must not be overridden by fallback.
    #[test]
    fn explicit_font_has_priority_over_fallback() {
        let inter = resolve("Inter", "Regular").face;
        assert_eq!(fallback_for('A', inter), inter);
    }

    #[test]
    fn bundled_families_and_resolution() {
        let fams = families();
        let inter = fams.iter().find(|(f, _)| f == "Inter").expect("Inter");
        for s in ["Regular", "Medium", "SemiBold", "Bold", "Italic"] {
            assert!(inter.1.iter().any(|x| x == s), "{s} in {:?}", inter.1);
        }
        assert!(fams.iter().any(|(f, _)| f == "Noto Serif"));
        assert!(fams.iter().any(|(f, _)| f == "JetBrains Mono"));
        let b = resolve("inter", "bold");
        assert_eq!(face(b.face).info.style, "Bold");
        assert!(!b.synth_bold);
        // Noto Serif has only Regular: Bold Italic is synthesised
        let s = resolve("Noto Serif", "Bold Italic");
        assert_eq!(face(s.face).info.family, "Noto Serif");
        assert!(s.synth_bold && s.synth_italic);
        // Inter Bold Italic: nearest is Italic (400) → synth bold
        let bi = resolve("Inter", "Bold Italic");
        assert!(face(bi.face).info.italic && bi.synth_bold && !bi.synth_italic);
    }

    #[test]
    fn missing_family_falls_back_to_inter() {
        let r = resolve("No Such Font Family 123", "Regular");
        assert_eq!(face(r.face).info.family, "Inter");
        assert!(r.missing);
    }

    /// Native: the system scan reads name tables only and resolves families installed there.
    #[test]
    fn system_scan_finds_fonts() {
        let t = std::time::Instant::now();
        scan_system(); // (another test may have triggered the scan already)
        let n = all_faces().iter().filter(|f| f.info.origin == "system").count();
        eprintln!("system fonts: {n} faces in {:?}", t.elapsed());
        assert_eq!(scan_system(), 0, "scans once");
        if cfg!(target_os = "macos") {
            assert!(n > 20, "{n}");
            let r = resolve("Helvetica", "Bold");
            assert_eq!(face(r.face).info.family, "Helvetica");
            let l = crate::layout("Helvetica", &crate::TextStyle { family: "Helvetica".into(), size: 30.0, ..Default::default() }, &Default::default());
            assert!(!l.missing_font && l.glyphs.iter().all(|g| g.id != 0));
        }
    }

    /// craft-fonts faces are registered after the bundled ones and serve Japanese characters the
    /// requested face lacks: Gothic for sans text, Mincho for serif text.
    #[test]
    fn japanese_falls_back_to_craft_fonts() {
        let craft: Vec<_> = all_faces().into_iter().filter(|f| f.info.origin == CRAFT_ORIGIN).collect();
        if craft_japanese().next().is_none() {
            eprintln!("SKIPPED: built without craft-fonts (set CRAFT_FONTS_DIR to run)");
            assert!(craft.is_empty());
            return;
        }
        assert!(craft.len() >= craft_japanese().count(), "{} faces", craft.len());
        let bundled = all_faces().iter().take_while(|f| f.info.origin == "bundled").count();
        assert!(craft.iter().all(|f| f.id >= bundled), "craft-fonts faces come after the bundled faces");
        let sans = resolve("Inter", "Regular").face;
        let serif = resolve("Noto Serif", "Regular").face;
        for c in "日本語の文字".chars() {
            let g = face(fallback_for(c, sans));
            assert_eq!(g.info.origin, CRAFT_ORIGIN, "{c}");
            assert!(g.has_char(c) && !is_serif(&g.info.family), "{c}: {}", g.info.family);
            let m = face(fallback_for(c, serif));
            assert!(m.has_char(c) && is_serif(&m.info.family), "{c}: {}", m.info.family);
        }
        // the bold UI face for bold text
        let bold = face(fallback_for('日', resolve("Inter", "Bold").face));
        assert!(bold.info.weight >= 600, "{} {}", bold.info.family, bold.info.style);
        // and laid out with real glyphs (no tofu)
        let l = crate::layout("日本語の文字", &crate::TextStyle { size: 40.0, ..Default::default() }, &Default::default());
        assert_eq!(l.glyphs.len(), 6);
        assert!(l.glyphs.iter().all(|g| g.id != 0), "{:?}", l.glyphs);
    }

    #[test]
    fn metrics_and_coverage() {
        let f = face(resolve("Inter", "Regular").face);
        let m = f.metrics(100.0);
        assert!(m.ascent > 80.0 && m.ascent < 110.0, "{m:?}");
        assert!(m.descent > 10.0 && m.descent < 40.0);
        assert!(m.cap_height > 60.0 && m.cap_height < 80.0);
        assert!(f.has_char('A') && f.has_char('Ж'));
        assert!(!f.has_char('\u{5d0}'), "Inter has no Hebrew");
        assert!(f.has_feature(b"liga") || f.has_feature(b"calt"));
    }

    #[test]
    fn covers_text_does_not_keep_file_bytes() {
        let path = std::env::temp_dir().join(format!("filmcraft-covers-text-{}.ttf", std::process::id()));
        std::fs::write(&path, INTER_REGULAR).expect("write temp font");
        let info = FaceInfo {
            family: "Inter".into(),
            style: "Regular".into(),
            weight: 400,
            italic: false,
            index: 0,
            data: FaceData::File(path.clone()),
            origin: "system",
        };
        let f = Face { id: 0, info, bytes: OnceLock::new(), shaper: OnceLock::new() };
        assert!(f.covers_text("A\u{416}"));
        assert!(!f.covers_text("A\u{5d0}"));
        assert!(f.bytes.get().is_none(), "probing coverage must not cache the file");
        assert!(f.data().is_some());
        assert!(f.bytes.get().is_some());
        let _ = std::fs::remove_file(&path);
    }
}
