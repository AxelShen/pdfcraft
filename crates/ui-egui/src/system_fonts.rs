//! Last-resort interface fonts already installed on this machine.
//!
//! The embedded faces (Inter, egui's defaults, craft-fonts) come first in every family. These
//! only draw characters none of them has: a CJK face for Chinese and Japanese in a build
//! without craft-fonts, then one broad face (Arabic and the rest). They are read at runtime
//! and never embedded or shipped (AGENTS.md §1.4). `PDFCRAFT_SYSTEM_FONTS=0` turns them off
//! (published screenshots do).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use egui::FontData;

/// Larger files are not read: a font path is still untrusted input.
const MAX_BYTES: u64 = 32 << 20;
/// Faces tried in a collection (`.ttc`).
const MAX_FACES: u32 = 16;
/// Arabic letter alef: the broad face must have it to be worth loading.
const PROBE: char = '\u{0627}';
/// CJK unified ideograph "one": every Chinese and Japanese face has it.
const CJK_PROBE: char = '\u{4E00}';

/// Which CJK face to try first. The UI language decides; other faces stay as fallbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CjkOrder {
    /// Japanese, then Traditional Chinese, then Simplified Chinese.
    Japanese,
    /// Simplified Chinese first.
    Hans,
    /// Traditional Chinese first.
    Hant,
}

impl CjkOrder {
    /// `zh-hans` and `zh-hant` select those faces. Every other language code keeps Japanese first,
    /// which is the order the embedded craft-fonts use outside Simplified Chinese.
    pub fn from_code(code: &str) -> Self {
        match code {
            "zh-hans" => Self::Hans,
            "zh-hant" => Self::Hant,
            _ => Self::Japanese,
        }
    }
}

/// The installed broad fallback face, read once. `None` when it is turned off or no candidate fits.
pub fn fallback() -> Option<Arc<FontData>> {
    static CACHE: OnceLock<Option<Arc<FontData>>> = OnceLock::new();
    CACHE.get_or_init(load).clone()
}

/// The installed CJK face for `order`, read once per order.
pub fn cjk(order: CjkOrder) -> Option<Arc<FontData>> {
    match order {
        CjkOrder::Hans => cached(&HANS, order),
        CjkOrder::Hant => cached(&HANT, order),
        CjkOrder::Japanese => cached(&JPAN, order),
    }
}

static HANS: OnceLock<Option<Arc<FontData>>> = OnceLock::new();
static HANT: OnceLock<Option<Arc<FontData>>> = OnceLock::new();
static JPAN: OnceLock<Option<Arc<FontData>>> = OnceLock::new();

fn cached(slot: &OnceLock<Option<Arc<FontData>>>, order: CjkOrder) -> Option<Arc<FontData>> {
    slot.get_or_init(|| load_cjk(order)).clone()
}

fn fonts_disabled() -> bool {
    std::env::var_os("PDFCRAFT_SYSTEM_FONTS").is_some_and(|v| v == "0")
}

fn load() -> Option<Arc<FontData>> {
    if fonts_disabled() {
        return None;
    }
    candidates().iter().find_map(|path| read(path))
}

fn load_cjk(order: CjkOrder) -> Option<Arc<FontData>> {
    if fonts_disabled() {
        return None;
    }
    cjk_candidates(order).iter().find_map(|path| read_with(path, CJK_PROBE))
}

fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")).map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("Fonts")
}

/// Preferred face first. Later names cover a machine that lacks the preferred one.
fn cjk_candidates(order: CjkOrder) -> Vec<PathBuf> {
    let (hans, hant, jpan): (&[&str], &[&str], &[&str]) = if cfg!(windows) {
        (&["msyh.ttc", "simsun.ttc"], &["msjh.ttc", "mingliu.ttc"], &["YuGothR.ttc", "YuGothM.ttc"])
    } else if cfg!(target_os = "macos") {
        (
            &["/System/Library/Fonts/Hiragino Sans GB.ttc", "/System/Library/Fonts/STHeiti Medium.ttc"],
            &["/System/Library/Fonts/Supplemental/Songti.ttc", "/System/Library/Fonts/PingFang.ttc"],
            &["/System/Library/Fonts/Hiragino Sans W3.ttc"],
        )
    } else {
        (
            &["opentype/noto/NotoSansCJKsc-Regular.otf", "noto-cjk/NotoSansCJKsc-Regular.otf", "truetype/noto/NotoSansCJKsc-Regular.otf"],
            &["opentype/noto/NotoSansCJKtc-Regular.otf", "noto-cjk/NotoSansCJKtc-Regular.otf", "truetype/noto/NotoSansCJKtc-Regular.otf"],
            &["opentype/noto/NotoSansCJKjp-Regular.otf", "noto-cjk/NotoSansCJKjp-Regular.otf", "truetype/noto/NotoSansCJKjp-Regular.otf"],
        )
    };
    let groups: [&[&str]; 3] = match order {
        CjkOrder::Hans => [hans, hant, jpan],
        CjkOrder::Hant => [hant, hans, jpan],
        CjkOrder::Japanese => [jpan, hant, hans],
    };
    let mut paths: Vec<PathBuf> = groups.into_iter().flatten().copied().map(font_path).collect();
    // One last face that covers every CJK script, when a region-specific file is absent.
    // Windows already listed its in-box faces above.
    if cfg!(target_os = "macos") {
        paths.push(PathBuf::from("/System/Library/Fonts/PingFang.ttc"));
    } else if cfg!(not(windows)) {
        for rel in ["truetype/droid/DroidSansFallbackFull.ttf", "opentype/noto/NotoSansCJK-Regular.ttc", "noto-cjk/NotoSansCJK-Regular.ttc"] {
            paths.push(font_path(rel));
        }
    }
    paths
}

fn font_path(name: &str) -> PathBuf {
    if name.starts_with('/') {
        PathBuf::from(name)
    } else if cfg!(windows) {
        windows_fonts_dir().join(name)
    } else {
        Path::new("/usr/share/fonts").join(name)
    }
}

/// Well-known locations of faces with broad script coverage, best first.
fn candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        ["segoeui.ttf", "tahoma.ttf", "arial.ttf"].iter().map(|f| windows_fonts_dir().join(f)).collect()
    } else if cfg!(target_os = "macos") {
        ["/System/Library/Fonts/SFArabic.ttf", "/System/Library/Fonts/GeezaPro.ttc", "/System/Library/Fonts/Supplemental/Arial.ttf"]
            .iter()
            .map(PathBuf::from)
            .collect()
    } else {
        let files = [
            "truetype/noto/NotoSansArabic-Regular.ttf",
            "noto/NotoSansArabic-Regular.ttf",
            "google-noto/NotoSansArabic-Regular.ttf",
            "truetype/dejavu/DejaVuSans.ttf",
            "TTF/DejaVuSans.ttf",
            "dejavu/DejaVuSans.ttf",
            "dejavu-sans-fonts/DejaVuSans.ttf",
        ];
        ["/usr/share/fonts", "/usr/local/share/fonts"].iter().flat_map(|dir| files.iter().map(move |f| Path::new(dir).join(f))).collect()
    }
}

fn read(path: &Path) -> Option<Arc<FontData>> {
    read_with(path, PROBE)
}

fn read_with(path: &Path, probe: char) -> Option<Arc<FontData>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let index = face_with(&bytes, probe)?;
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    log::info!("interface font fallback: {}", path.display());
    Some(Arc::new(data))
}

/// The first face of the file that parses and maps `c`. egui parses fonts with the same skrifa,
/// so a face accepted here is one it can load.
fn face_with(bytes: &[u8], c: char) -> Option<u32> {
    use skrifa::MetadataProvider as _;
    (0..MAX_FACES).find(|&index| skrifa::FontRef::from_index(bytes, index).is_ok_and(|font| font.charmap().map(c).is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_and_missing_files_are_skipped() {
        assert_eq!(face_with(b"", PROBE), None);
        assert_eq!(face_with(b"not a font at all", PROBE), None);
        assert_eq!(face_with(&[0u8; 4096], PROBE), None);
        assert!(read(Path::new("definitely/not/here.ttf")).is_none());
        // A directory is not a font.
        assert!(read(&std::env::temp_dir()).is_none());
    }

    #[test]
    fn a_face_without_the_probe_is_rejected() {
        // Inter is Latin, Greek and Cyrillic only.
        let inter = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
        assert_eq!(face_with(inter, PROBE), None);
        assert_eq!(face_with(inter, 'A'), Some(0));
    }

    #[test]
    fn candidates_are_absolute_font_files() {
        let list = candidates();
        assert!(!list.is_empty());
        let font = |p: &Path| p.extension().is_some_and(|e| matches!(e.to_str(), Some("ttf" | "ttc" | "otf")));
        assert!(list.iter().all(|p| font(p)));
        for order in [CjkOrder::Hant, CjkOrder::Hans, CjkOrder::Japanese] {
            let cjk = cjk_candidates(order);
            assert!(!cjk.is_empty(), "{order:?}");
            assert!(cjk.iter().all(|p| p.is_absolute() && font(p)), "{cjk:?}");
        }
    }

    #[test]
    fn cjk_order_follows_the_ui_language() {
        assert_eq!(CjkOrder::from_code("zh-hant"), CjkOrder::Hant);
        assert_eq!(CjkOrder::from_code("zh-hans"), CjkOrder::Hans);
        assert_eq!(CjkOrder::from_code("ja"), CjkOrder::Japanese);
        assert_eq!(CjkOrder::from_code("en"), CjkOrder::Japanese);
    }
}
