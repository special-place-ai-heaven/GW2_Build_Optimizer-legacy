//! Overlay fonts via Nexus FontApi (host owns the ImGui atlas).
//!
//! Do not call `io.Fonts->AddFontFromFileTTF` here — Nexus rebuilds the atlas
//! for every addon. Missing glyphs (`?`) are the default GW2/Nexus Latin-only
//! atlas; we register faces with Nexus and `igPushFont` for our window.
//!
//! The catalog is the player's own font folder, plus five bundled OFL faces
//! for a bare system. A picked font draws every Latin language; Chinese,
//! Japanese and Korean always draw in their script face, because no Latin
//! font can draw them and offering one was a trap (a Japanese face picked
//! for an English UI drew the model's em dash as '?', 2026-09-07).

use nexus::font::{add_font_from_file, add_font_from_memory};
use nexus::imgui::sys::{
    self, ImFont, ImFontAtlas_GetGlyphRangesChineseSimplifiedCommon,
    ImFontAtlas_GetGlyphRangesJapanese, ImFontAtlas_GetGlyphRangesKorean, ImFontConfig,
    ImFontConfig_ImFontConfig, ImFontConfig_destroy, ImWchar,
};
use nexus::log::{log, LogLevel};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

const SIZE_PX: f32 = 16.0;
/// Native size for the now-playing ticker face — rasterized crisp at this
/// size instead of bitmap-upscaling the 16 px atlas (which reads as a badly
/// scaled image in-game).
const TICKER_SIZE_PX: f32 = 42.0;
/// The Latin face used when the player has not picked one.
const LATIN_FILES: &[&str] = &["segoeui.ttf", "arial.ttf", "tahoma.ttf", "calibri.ttf"];
const ZH_FILES: &[&str] = &["msyh.ttc", "msyh.ttf", "simsun.ttc"];
const JA_FILES: &[&str] = &["YuGothM.ttc", "YuGothR.ttc", "meiryo.ttc", "msgothic.ttc"];
const KO_FILES: &[&str] = &["malgun.ttf", "malgunsl.ttf"];
const ID_ZH: &str = "GW2BO_FONT_ZH";
const ID_JA: &str = "GW2BO_FONT_JA";
const ID_KO: &str = "GW2BO_FONT_KO";
const ID_TICKER: &str = "GW2BO_FONT_TICKER";
const FILE_PREFIX: &str = "file:";
const BUNDLED_PREFIX: &str = "bundled:";
const ID_FILE_PREFIX: &str = "GW2BO_FONT_FILE_";
const ID_BUNDLED_PREFIX: &str = "GW2BO_FONT_BUNDLED_";

/// Faces shipped inside the DLL (SIL Open Font License, see
/// `assets/fonts/OFL-*.txt`). Chosen for character, not coverage: Latin and,
/// where the file has it, Cyrillic. Every one is small.
const BUNDLED: &[(&str, &str, &[u8])] = &[
    (
        "cinzel",
        "Cinzel",
        include_bytes!("../../assets/fonts/Cinzel.ttf"),
    ),
    (
        "medievalsharp",
        "MedievalSharp",
        include_bytes!("../../assets/fonts/MedievalSharp.ttf"),
    ),
    (
        "caveat",
        "Caveat",
        include_bytes!("../../assets/fonts/Caveat.ttf"),
    ),
    (
        "comicneue",
        "Comic Neue",
        include_bytes!("../../assets/fonts/ComicNeue.ttf"),
    ),
    (
        "pressstart2p",
        "Press Start 2P",
        include_bytes!("../../assets/fonts/PressStart2P.ttf"),
    ),
];

/// Inclusive pairs, 0-terminated. Latin-1 + Ext-A (Polish) + Cyrillic +
/// General Punctuation + arrows + math + symbols.
///
/// Most of this text is written by a language model, not by us, and a glyph
/// the atlas lacks reaches the player as '?'. Our own strings can be held to
/// ASCII by a test; the model's cannot, and it writes em dashes, curly quotes,
/// bullets, `->` as an arrow and `>=` as a relation without being asked.
/// General Punctuation was cut at 0x2027, which covered dashes and the
/// ellipsis but stopped short of the primes and the wider quotes; arrows and
/// math were absent entirely. Rasterizing the rest costs a few kilobytes of a
/// 16 px atlas, which is cheaper than one more round of hunting a question
/// mark through twelve catalogs.
const LATIN_RANGES: &[ImWchar] = &[
    0x0020, 0x00FF, // Latin-1
    0x0100, 0x017F, // Latin Extended-A (Polish)
    0x0400, 0x04FF, // Cyrillic (Russian)
    0x2010, 0x205E, // General Punctuation: dashes, quotes, ellipsis, bullets
    0x20A0, 0x20CF, // Currency Symbols: the euro sign of the cost display
    0x2190, 0x21FF, // Arrows
    0x2200, 0x22FF, // Mathematical Operators
    0x2600, 0x27BF, // Misc symbols + dingbats
    0,
];

/// Font pointers by Nexus identifier, as handed to us by the font callback.
/// Null during an atlas rebuild.
static SLOTS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
/// Identifiers we already asked Nexus for, so a face whose file is broken is
/// not re-requested every frame.
static REQUESTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static TICKER: AtomicPtr<ImFont> = AtomicPtr::new(std::ptr::null_mut());

const RECEIVE: nexus::font::RawFontReceive = nexus::font_receive!(|id, font| {
    let ptr = font
        .map(|f| f as *mut ImFont)
        .unwrap_or(std::ptr::null_mut());
    store_ptr(id, ptr);
});

fn store_ptr(id: &str, ptr: *mut ImFont) {
    if id == ID_TICKER {
        TICKER.store(ptr, Ordering::Release);
        return;
    }
    if let Ok(mut slots) = SLOTS.lock() {
        slots
            .get_or_insert_with(HashMap::new)
            .insert(id.to_string(), ptr as usize);
    }
}

fn slot_ptr(id: &str) -> *mut ImFont {
    SLOTS
        .lock()
        .ok()
        .and_then(|slots| slots.as_ref().and_then(|s| s.get(id).copied()))
        .unwrap_or(0) as *mut ImFont
}

/// True the first time an identifier is seen, false after.
fn first_request(id: &str) -> bool {
    REQUESTED
        .lock()
        .map(|mut set| set.get_or_insert_with(HashSet::new).insert(id.to_string()))
        .unwrap_or(false)
}

/// Pops the font Nexus/`igPushFont` pushed. Must drop even if the window panics.
pub struct FontGuard;

impl Drop for FontGuard {
    fn drop(&mut self) {
        // Safety: paired with a successful `igPushFont` in `push`.
        unsafe { sys::igPopFont() };
    }
}

/// Register only the face this frame will push. Safe to call every frame.
///
/// Only `"game"` returns before touching the atlas; every language, English
/// included, loads the face `resolve_font_id` picks.
pub fn init(pref: &str, ui_language: &str) {
    let Some(id) = resolve_font_id(pref, ui_language) else {
        return;
    };
    init_id(&id);
}

/// The italic face beside the chosen family, when the font folder has one
/// (`segoeui.ttf` → `segoeuii.ttf`, `arial.ttf` → `ariali.ttf`, `georgia.ttf`
/// → `georgiai.ttf`). Bundled and script faces have none; the bubble falls
/// back to the muted colour for asides.
pub fn italic_font_id(pref: &str, ui_language: &str) -> Option<String> {
    let id = resolve_font_id(pref, ui_language)?;
    let file = id.strip_prefix(ID_FILE_PREFIX)?;
    let ext = Path::new(file).extension()?.to_string_lossy().to_string();
    let italic = format!("{}i.{ext}", stem_of(file));
    find_font_file(&italic).map(|_| format!("{ID_FILE_PREFIX}{italic}"))
}

/// Italic face id requested for the chat bubble, once `init_italic` found one.
static ITALIC: Mutex<Option<String>> = Mutex::new(None);

/// Request the italic face beside the chosen family, if the folder has one.
pub fn init_italic(pref: &str, ui_language: &str) {
    let Some(id) = italic_font_id(pref, ui_language) else {
        return;
    };
    init_id(&id);
    if let Ok(mut slot) = ITALIC.lock() {
        *slot = Some(id);
    }
}

/// True once the italic face is loaded and usable this frame.
pub fn has_italic() -> bool {
    ITALIC
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .is_some_and(|id| !slot_ptr(&id).is_null())
}

/// Push the italic face for one span. `None` when there is none, in which
/// case the caller draws in the muted colour instead.
pub fn push_italic() -> Option<FontGuard> {
    let id = ITALIC.lock().ok().and_then(|s| s.clone())?;
    let ptr = slot_ptr(&id);
    if ptr.is_null() {
        return None;
    }
    // Safety: `ptr` came from Nexus' font callback (null during atlas rebuild).
    unsafe { sys::igPushFont(ptr) };
    Some(FontGuard)
}

fn init_id(id: &str) {
    let id = id.to_string();
    if !slot_ptr(&id).is_null() || !first_request(&id) {
        return;
    }
    let atlas = unsafe {
        let io = sys::igGetIO();
        if io.is_null() {
            return;
        }
        (*io).Fonts
    };
    if atlas.is_null() {
        return;
    }

    let dir = windows_fonts_dir();
    // ImGui copies ImFontConfig during AddFont; only GlyphRanges must outlive Build.
    match id.as_str() {
        ID_ZH => {
            let zh_cfg = make_cfg(
                with_latin(unsafe { ImFontAtlas_GetGlyphRangesChineseSimplifiedCommon(atlas) }),
                1,
            );
            try_add(
                ID_ZH,
                first_existing(&dir, ZH_FILES),
                Some(&zh_cfg),
                SIZE_PX,
            );
        }
        ID_JA => {
            let ja_cfg = make_cfg(
                with_latin(unsafe { ImFontAtlas_GetGlyphRangesJapanese(atlas) }),
                1,
            );
            try_add(
                ID_JA,
                first_existing(&dir, JA_FILES),
                Some(&ja_cfg),
                SIZE_PX,
            );
        }
        ID_KO => {
            let ko_cfg = make_cfg(
                with_latin(unsafe { ImFontAtlas_GetGlyphRangesKorean(atlas) }),
                1,
            );
            try_add(
                ID_KO,
                first_existing(&dir, KO_FILES),
                Some(&ko_cfg),
                SIZE_PX,
            );
        }
        id if id.starts_with(ID_BUNDLED_PREFIX) => {
            let key = &id[ID_BUNDLED_PREFIX.len()..];
            let Some((_, label, bytes)) = BUNDLED.iter().find(|(k, _, _)| *k == key) else {
                return;
            };
            let cfg = make_cfg(LATIN_RANGES.as_ptr(), 2);
            add_font_from_memory(id, bytes, SIZE_PX, Some(&cfg), RECEIVE).revert_on_unload();
            log(
                LogLevel::Info,
                "GW2 Build Optimizer",
                format!("overlay font {id}: bundled {label}"),
            );
        }
        id if id.starts_with(ID_FILE_PREFIX) => {
            let file = &id[ID_FILE_PREFIX.len()..];
            let cfg = make_cfg(LATIN_RANGES.as_ptr(), 2);
            try_add(id, find_font_file(file), Some(&cfg), SIZE_PX);
        }
        _ => {}
    }
}

/// Register the big now-playing ticker face (Latin, 42 px native) once —
/// English UI included: the marquee wants a crisp large font, not a 3x
/// bitmap upscale of the 16 px atlas. Safe to call every frame.
pub fn init_ticker() {
    if !TICKER.load(Ordering::Acquire).is_null() || !first_request(ID_TICKER) {
        return;
    }
    let dir = windows_fonts_dir();
    let cfg = make_cfg(LATIN_RANGES.as_ptr(), 2);
    try_add(
        ID_TICKER,
        first_existing(&dir, LATIN_FILES),
        Some(&cfg),
        TICKER_SIZE_PX,
    );
}

/// Push the 42 px ticker face; `None` (atlas rebuild, no TTF found) lets the
/// caller fall back to scaling the current font.
pub fn push_ticker() -> Option<FontGuard> {
    let ptr = TICKER.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // Safety: from the Nexus font callback; FontGuard pops on drop.
    unsafe { sys::igPushFont(ptr) };
    Some(FontGuard)
}

/// Whether `u` is one of the codepoints the Latin face is built with.
///
/// Read straight off `LATIN_RANGES` rather than restated as a chain of range
/// checks, which is what the constant is for. The restatement drifted once
/// already: a gate narrower than the font meant one decoration symbol (stars,
/// notes) anywhere in a title kicked the whole string to the blurry 3x bitmap
/// path, where the ASCII-only base atlas also turned every accented char into
/// '?'. A copy that must be kept in step by hand eventually is not.
fn in_latin_ranges(u: u32) -> bool {
    // Pairs are inclusive; the terminating 0 is left over and ignored.
    LATIN_RANGES
        .as_chunks::<2>()
        .0
        .iter()
        .any(|[lo, hi]| (u32::from(*lo)..=u32::from(*hi)).contains(&u))
}

/// Whether every char of `text` is inside the ticker face's glyph ranges.
/// CJK titles fall back to the scaled UI font rather than 42 px tofu.
pub fn ticker_can_render(text: &str) -> bool {
    text.chars().all(|c| in_latin_ranges(c as u32))
}

fn try_add(id: &str, path: Option<PathBuf>, config: Option<&ImFontConfig>, size_px: f32) {
    let Some(path) = path else {
        log(
            LogLevel::Info,
            "GW2 Build Optimizer",
            format!("overlay font {id}: no font file found, skipping"),
        );
        return;
    };
    add_font_from_file(id, &path, size_px, config, RECEIVE).revert_on_unload();
    log(
        LogLevel::Info,
        "GW2 Build Optimizer",
        format!("overlay font {id}: {}", path.display()),
    );
}

/// ImGui's built-in CJK range lists cover their script plus Latin-1 and stop
/// there: no General Punctuation, no arrows, no math. A player who picks the
/// Japanese face for an English UI (`ui_font: "ja"`, seen 2026-09-07) then
/// reads the model's em dash as '?', exactly the bug the Latin face was
/// fixed for the day before. Append [`LATIN_RANGES`] to the built-in list so
/// no face choice can lose the punctuation a language model writes.
///
/// The merged list must outlive the atlas build, so it is leaked once per
/// face — three small allocations for the life of the process.
fn with_latin(builtin: *const ImWchar) -> *const ImWchar {
    let mut merged: Vec<ImWchar> = Vec::new();
    if !builtin.is_null() {
        let mut i = 0;
        // Safety: ImGui range lists are 0-terminated pairs.
        loop {
            let v = unsafe { *builtin.add(i) };
            if v == 0 {
                break;
            }
            merged.push(v);
            i += 1;
        }
    }
    merged.extend(
        LATIN_RANGES
            .iter()
            .copied()
            .take(LATIN_RANGES.len().saturating_sub(1)),
    );
    merged.push(0);
    Box::leak(merged.into_boxed_slice()).as_ptr()
}

fn make_cfg(ranges: *const ImWchar, oversample_h: i32) -> ImFontConfig {
    unsafe {
        let p = ImFontConfig_ImFontConfig();
        let mut c = *p;
        ImFontConfig_destroy(p);
        c.GlyphRanges = ranges;
        c.FontNo = 0;
        c.OversampleH = oversample_h;
        c.OversampleV = 1;
        c.PixelSnapH = true;
        c
    }
}

/// Push the configured overlay font. `None` keeps the Nexus/GW2 typeface.
pub fn push(pref: &str, ui_language: &str) -> Option<FontGuard> {
    let wanted = resolve_font_id(pref, ui_language)?;
    let ptr = live_ptr(&wanted);
    if ptr.is_null() {
        return None;
    }
    // Safety: `ptr` came from Nexus' font callback (null during atlas rebuild).
    // FontGuard pops on drop, including unwind inside `ui::render`'s catch_unwind.
    unsafe { sys::igPushFont(ptr) };
    Some(FontGuard)
}

/// The requested face, or the default Latin face while it is still loading
/// or if its file turned out unusable.
fn live_ptr(id: &str) -> *mut ImFont {
    let ptr = slot_ptr(id);
    if !ptr.is_null() {
        return ptr;
    }
    let fallback = default_latin_id();
    if id != fallback {
        return slot_ptr(&fallback);
    }
    std::ptr::null_mut()
}

fn default_latin_id() -> String {
    let dir = windows_fonts_dir();
    let file = LATIN_FILES
        .iter()
        .find(|f| dir.join(f).is_file())
        .copied()
        .unwrap_or(LATIN_FILES[0]);
    format!("{ID_FILE_PREFIX}{file}")
}

/// Which Nexus font identifier a preference resolves to for a UI language.
///
/// `"game"` never pushes. Chinese, Japanese and Korean always take their
/// script face — a Latin pick cannot draw them, so it is not offered there.
/// Everywhere else: `file:<name>` is a font in the player's font folders,
/// `bundled:<key>` one of ours, anything else (`auto`, the retired `segoe` /
/// `zh` / `ja` / `ko` values) the default Latin face.
pub fn resolve_font_id(pref: &str, ui_language: &str) -> Option<String> {
    if pref == "game" {
        return None;
    }
    match gw2_core::i18n::resolve(ui_language) {
        "zh" => return Some(ID_ZH.to_string()),
        "ja" => return Some(ID_JA.to_string()),
        "ko" => return Some(ID_KO.to_string()),
        _ => {}
    }
    if let Some(file) = pref.strip_prefix(FILE_PREFIX) {
        return Some(format!("{ID_FILE_PREFIX}{file}"));
    }
    if let Some(key) = pref.strip_prefix(BUNDLED_PREFIX) {
        if BUNDLED.iter().any(|(k, _, _)| *k == key) {
            return Some(format!("{ID_BUNDLED_PREFIX}{key}"));
        }
    }
    Some(default_latin_id())
}

/// The picker: preference value and the label to show for it.
///
/// Auto and Game first, then the bundled faces, then every `.ttf`/`.otf`/
/// `.ttc` in the system and per-user font folders — style variants (bold,
/// italic) folded away where the base file exists. For Chinese, Japanese and
/// Korean the list stops after Game: the script face is automatic.
pub fn combo_options(ui_language: &str) -> Vec<(String, String)> {
    let mut v = vec![
        ("auto".to_string(), gw2_core::i18n::t("settings.font_auto")),
        ("game".to_string(), gw2_core::i18n::t("settings.font_game")),
    ];
    if matches!(gw2_core::i18n::resolve(ui_language), "zh" | "ja" | "ko") {
        return v;
    }
    for (key, label, _) in BUNDLED {
        v.push((format!("{BUNDLED_PREFIX}{key}"), (*label).to_string()));
    }
    for (file, label) in system_fonts() {
        v.push((format!("{FILE_PREFIX}{file}"), label));
    }
    v
}

/// What the picker shows for a saved preference.
pub fn label_for(pref: &str) -> String {
    if pref == "game" {
        return gw2_core::i18n::t("settings.font_game");
    }
    if let Some(file) = pref.strip_prefix(FILE_PREFIX) {
        return pretty_font_name(file);
    }
    if let Some(key) = pref.strip_prefix(BUNDLED_PREFIX) {
        if let Some((_, label, _)) = BUNDLED.iter().find(|(k, _, _)| *k == key) {
            return (*label).to_string();
        }
    }
    gw2_core::i18n::t("settings.font_auto")
}

/// zh/ja/ko native names need a CJK face. Otherwise use the English catalog name.
pub fn language_label(
    lang: &gw2_core::i18n::Language,
    pref: &str,
    ui_language: &str,
) -> &'static str {
    if !matches!(lang.code, "zh" | "ja" | "ko") {
        return lang.native_name;
    }
    let resolved = resolve_font_id(pref, ui_language);
    let ok = matches!(
        (lang.code, resolved.as_deref()),
        ("zh", Some(ID_ZH)) | ("ja", Some(ID_JA)) | ("ko", Some(ID_KO))
    );
    if ok {
        lang.native_name
    } else {
        lang.choya_name
    }
}

/// Every usable font file the player has, as (file name, display label),
/// sorted by label. Symbol and icon fonts are left out; a bold/italic
/// variant is folded away when its base file is present.
pub fn system_fonts() -> Vec<(String, String)> {
    let mut files: Vec<String> = Vec::new();
    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            if !(lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc")) {
                continue;
            }
            if is_symbol_font(&lower) {
                continue;
            }
            files.push(name);
        }
    }
    let stems: HashSet<String> = files.iter().map(|f| stem_of(f)).collect();
    let mut out: Vec<(String, String)> = files
        .into_iter()
        .filter(|f| !is_style_variant(&stem_of(f), &stems))
        .map(|f| {
            let label = pretty_font_name(&f);
            (f, label)
        })
        .collect();
    out.sort_by_key(|(_, label)| label.to_lowercase());
    out.dedup_by(|a, b| a.1.eq_ignore_ascii_case(&b.1));
    out
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![windows_fonts_dir()];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    dirs
}

fn find_font_file(file: &str) -> Option<PathBuf> {
    font_dirs()
        .into_iter()
        .map(|d| d.join(file))
        .find(|p| p.is_file())
}

fn stem_of(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Icon and symbol faces: nothing a chat bubble can be read in.
fn is_symbol_font(lower: &str) -> bool {
    [
        "webdings",
        "wingding",
        "symbol",
        "marlett",
        "mdl2",
        "segoeicons",
        "seguiemj",
        "seguisym",
        "holomdl",
        "bssym",
        "outlook",
        "refspcl",
        "msuighur",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

/// `arialbd` is Arial Bold: fold it away when `arial` is on the list too.
fn is_style_variant(stem: &str, stems: &HashSet<String>) -> bool {
    const SUFFIXES: [&str; 14] = [
        "bd", "bi", "b", "i", "z", "l", "li", "lt", "it", "bl", "sb", "sbi", "blk", "k",
    ];
    SUFFIXES.iter().any(|suffix| {
        stem.strip_suffix(suffix)
            .is_some_and(|base| !base.is_empty() && stems.contains(base))
    })
}

/// `segoeui.ttf` → "Segoe UI" where a known name exists, else the stem with
/// its first letter up.
fn pretty_font_name(file: &str) -> String {
    let stem = stem_of(file);
    let known = [
        ("segoeui", "Segoe UI"),
        ("seguisb", "Segoe UI Semibold"),
        ("segoeuil", "Segoe UI Light"),
        ("arial", "Arial"),
        ("ariblk", "Arial Black"),
        ("verdana", "Verdana"),
        ("georgia", "Georgia"),
        ("calibri", "Calibri"),
        ("cambria", "Cambria"),
        ("tahoma", "Tahoma"),
        ("trebuc", "Trebuchet MS"),
        ("consola", "Consolas"),
        ("cour", "Courier New"),
        ("times", "Times New Roman"),
        ("comic", "Comic Sans MS"),
        ("impact", "Impact"),
        ("lucon", "Lucida Console"),
        ("pala", "Palatino Linotype"),
        ("bahnschrift", "Bahnschrift"),
        ("candara", "Candara"),
        ("constan", "Constantia"),
        ("corbel", "Corbel"),
        ("ebrima", "Ebrima"),
        ("gadugi", "Gadugi"),
        ("sylfaen", "Sylfaen"),
        ("mmrtext", "Myanmar Text"),
        ("ntailu", "Microsoft New Tai Lue"),
        ("micross", "Microsoft Sans Serif"),
        ("msyh", "Microsoft YaHei"),
        ("yugothm", "Yu Gothic"),
        ("malgun", "Malgun Gothic"),
        ("meiryo", "Meiryo"),
        ("simsun", "SimSun"),
    ];
    if let Some((_, name)) = known.iter().find(|(k, _)| *k == stem) {
        return (*name).to_string();
    }
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => stem,
    }
}

pub(crate) fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

pub(crate) fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn latin_ranges_are_zero_terminated_pairs() {
        assert_eq!(LATIN_RANGES.last().copied(), Some(0));
        assert_eq!(LATIN_RANGES.len() % 2, 1);
        assert!(LATIN_RANGES.len() >= 3);
    }

    #[test]
    fn resolve_game_never_pushes() {
        assert_eq!(resolve_font_id("game", "zh"), None);
        assert_eq!(resolve_font_id("game", "en"), None);
    }

    #[test]
    fn cjk_languages_always_take_their_script_face() {
        assert_eq!(resolve_font_id("auto", "zh").as_deref(), Some(ID_ZH));
        assert_eq!(resolve_font_id("auto", "ja").as_deref(), Some(ID_JA));
        assert_eq!(resolve_font_id("auto", "ko").as_deref(), Some(ID_KO));
        // A Latin pick cannot draw kanji, so it is not honoured there.
        assert_eq!(
            resolve_font_id("file:georgia.ttf", "ja").as_deref(),
            Some(ID_JA)
        );
    }

    /// English used to resolve to `None`, which drew the whole overlay in the
    /// Nexus/GW2 typeface - an atlas we do not build, so `LATIN_RANGES` bought
    /// it nothing and a missing glyph reached the player as '?'.
    #[test]
    fn auto_gives_every_latin_language_a_face_we_declare_ranges_on() {
        for lang in ["en", "fr", "de", "es", "it", "pt", "cs", "ru", "pl"] {
            let id = resolve_font_id("auto", lang).unwrap();
            assert!(
                id.starts_with(ID_FILE_PREFIX),
                "{lang} must draw in a face whose glyph ranges we control, got {id}"
            );
        }
    }

    /// The retired script picks. `ui_font: "ja"` on an English UI drew the
    /// model's em dash as '?' (2026-09-07); it now means Auto.
    #[test]
    fn retired_script_picks_mean_auto_on_a_latin_ui() {
        for legacy in ["ja", "zh", "ko", "segoe"] {
            assert_eq!(
                resolve_font_id(legacy, "en"),
                resolve_font_id("auto", "en"),
                "{legacy}"
            );
        }
    }

    #[test]
    fn a_picked_file_and_a_bundled_face_resolve_to_their_own_ids() {
        assert_eq!(
            resolve_font_id("file:georgia.ttf", "en").as_deref(),
            Some("GW2BO_FONT_FILE_georgia.ttf")
        );
        assert_eq!(
            resolve_font_id("bundled:cinzel", "de").as_deref(),
            Some("GW2BO_FONT_BUNDLED_cinzel")
        );
        assert_eq!(
            resolve_font_id("bundled:nope", "en"),
            resolve_font_id("auto", "en")
        );
    }

    /// The model writes this text, and no test can hold it to ASCII. In-game
    /// 2026-09-06 it wrote an em dash and the player read a question mark.
    #[test]
    fn the_typography_a_model_writes_is_inside_the_atlas() {
        for c in "—–…“”‘’•·→←≥≤×≈€".chars() {
            assert!(
                in_latin_ranges(c as u32),
                "U+{:04X} {c:?} is not in LATIN_RANGES; it would draw as '?'",
                c as u32
            );
        }
        // Our own cost strings, in both currencies, draw whole.
        use gw2_core::config::CostCurrency;
        let fx = gw2_optimizer::llm::pricing::fx();
        for currency in [CostCurrency::Usd, CostCurrency::Eur] {
            for usd in [0.0, 0.001, 0.02] {
                let text = crate::ui::cost_format::format_cost(Some(usd), currency, fx);
                for c in text.chars() {
                    assert!(
                        in_latin_ranges(c as u32),
                        "U+{:04X} in {text:?} would draw as '?'",
                        c as u32
                    );
                }
            }
        }
    }

    #[test]
    fn the_ticker_gate_tracks_the_font_it_guards() {
        assert!(ticker_can_render("Cafe - Bloc Party"));
        assert!(ticker_can_render("Пикник ★"));
        assert!(!ticker_can_render("東京"));
    }

    #[test]
    fn style_variants_fold_into_their_base_file() {
        let stems: HashSet<String> = ["arial", "arialbd", "georgia", "georgiaz", "cinzel"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_style_variant("arialbd", &stems));
        assert!(is_style_variant("georgiaz", &stems));
        assert!(!is_style_variant("arial", &stems));
        // No base file on the list: keep it, whatever its name ends in.
        assert!(!is_style_variant("cinzel", &stems));
    }

    #[test]
    fn names_read_like_the_font_menu_elsewhere() {
        assert_eq!(pretty_font_name("segoeui.ttf"), "Segoe UI");
        assert_eq!(pretty_font_name("trebuc.ttf"), "Trebuchet MS");
        assert_eq!(pretty_font_name("unknownface.otf"), "Unknownface");
        assert!(is_symbol_font("wingding.ttf"));
        assert!(!is_symbol_font("georgia.ttf"));
    }

    #[test]
    fn bundled_faces_are_real_font_files() {
        for (key, _, bytes) in BUNDLED {
            // TrueType starts with 0x00010000 or 'true'; OpenType CFF with 'OTTO'.
            let magic = &bytes[..4];
            assert!(
                magic == [0, 1, 0, 0] || magic == b"true" || magic == b"OTTO",
                "{key} does not start like a font"
            );
        }
    }

    #[test]
    fn first_existing_picks_first_real_file() {
        let dir = std::env::temp_dir().join(format!("gw2bo_font_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let hit = dir.join("arial.ttf");
        fs::write(&hit, b"x").unwrap();
        let found = first_existing(&dir, &["missing.ttf", "arial.ttf", "later.ttf"]);
        assert_eq!(found.as_deref(), Some(hit.as_path()));
        assert!(first_existing(&dir, &["nope.ttf"]).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}
