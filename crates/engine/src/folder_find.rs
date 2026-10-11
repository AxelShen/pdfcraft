//! Search the PDF files in a folder.
//!
//! Uses the same [`PageText::search`](pdfcraft_render::text::PageText::search) rules as Find
//! (case, whole word, VS Code regular expressions). Unreadable, encrypted, oversized and
//! symlinked files are skipped; one bad file does not stop the rest. The walk is synchronous:
//! callers that have a UI run it on a worker thread.

use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

use pdfcraft_render::text::FindOptions;
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_render::text::PageText;
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind};

/// How many PDFs one search will open. Further files set [`Report::truncated`].
pub const MAX_FILES: usize = 2_000;
/// How many matches one search keeps.
pub const MAX_MATCHES: usize = 500;
/// A larger PDF is skipped.
pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// Folder depth below the starting folder. Deeper folders set [`Report::truncated`].
pub const MAX_DEPTH: usize = 12;
/// Pages read from one PDF. A longer document sets [`Report::truncated`].
pub const MAX_PAGES: usize = 5_000;

const SNIPPET_GLYPHS: usize = 24;
const SNIPPET_CHARS: usize = 160;

/// What to look for. `limit` is clamped to `1..=`[`MAX_MATCHES`].
#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    /// Descend into subfolders. The starting folder is always searched.
    pub subfolders: bool,
    pub options: FindOptions,
    pub limit: usize,
}

/// One match. `page` is 1-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub page: u32,
    pub snippet: String,
}

/// The matches found, and why the walk may have stopped short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub hits: Vec<Hit>,
    /// PDFs that were opened and searched.
    pub files: usize,
    /// PDFs (and symlinks) that were skipped.
    pub skipped: usize,
    /// A cap was hit: files, depth, pages or matches.
    pub truncated: bool,
    /// `progress` returned `false` before every file was searched.
    pub cancelled: bool,
}

/// Why a search did not start.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("enter a word or phrase to search for")]
    Empty,
    /// The regular expression did not compile. The message is one line.
    #[error("{0}")]
    Pattern(String),
    #[error("{path} is not a folder")]
    NotAFolder { path: String },
    #[error("could not read {path}: {source}")]
    Unreadable { path: String, source: std::io::Error },
    #[error("folder search is not available")]
    Unavailable,
}

/// Search `folder` for `query`.
///
/// `progress(done, total)` runs before each file (`done` is how many files have been searched)
/// and once more at the end. Return `false` to stop. On wasm there is no folder walk.
#[cfg(target_arch = "wasm32")]
pub fn search(folder: &Path, query: &Query, mut progress: impl FnMut(usize, usize) -> bool) -> Result<Report, Error> {
    let _ = (folder, query, &mut progress);
    Err(Error::Unavailable)
}

/// Search `folder` for `query`.
///
/// `progress(done, total)` runs before each file (`done` is how many files have been searched)
/// and once more at the end. Return `false` to stop.
#[cfg(not(target_arch = "wasm32"))]
pub fn search(folder: &Path, query: &Query, mut progress: impl FnMut(usize, usize) -> bool) -> Result<Report, Error> {
    let text = if query.options.regex { query.text.clone() } else { query.text.trim().to_string() };
    if text.is_empty() {
        return Err(Error::Empty);
    }
    // Compile once, before opening any file. An invalid pattern is an error, not zero hits.
    PageText::default().search(&text, query.options).map_err(|e| Error::Pattern(e.to_string()))?;
    let meta = std::fs::metadata(folder).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotAFolder { path: folder.display().to_string() }
        } else {
            Error::Unreadable { path: folder.display().to_string(), source: e }
        }
    })?;
    if !meta.is_dir() {
        return Err(Error::NotAFolder { path: folder.display().to_string() });
    }
    let mut files = Vec::new();
    let mut skipped = 0;
    let mut truncated = false;
    walk(folder, 0, query.subfolders, &mut files, &mut skipped, &mut truncated)?;
    files.sort();
    let limit = query.limit.clamp(1, MAX_MATCHES);
    let total = files.len();
    let mut report = Report { hits: Vec::new(), files: 0, skipped, truncated, cancelled: false };
    for (i, path) in files.iter().enumerate() {
        if !progress(i, total) {
            report.cancelled = true;
            return Ok(report);
        }
        match search_file(path, &text, query.options, limit, &mut report) {
            Ok(true) => report.files += 1,
            Ok(false) => report.skipped += 1,
            Err(e) => return Err(e),
        }
        if report.hits.len() >= limit {
            report.truncated = true;
            break;
        }
    }
    let _ = progress(report.files, total);
    Ok(report)
}

/// `true` when the file was opened. `false` when it was skipped.
#[cfg(not(target_arch = "wasm32"))]
fn search_file(path: &Path, needle: &str, opts: FindOptions, limit: usize, report: &mut Report) -> Result<bool, Error> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Ok(false);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => Arc::new(b),
        Err(_) => return Ok(false),
    };
    let mut renderer = PageRenderer::new(bytes, RenderConfig::default());
    let count = renderer.page_count();
    if count == 0 {
        return Ok(false);
    }
    if count > MAX_PAGES {
        report.truncated = true;
    }
    for page in 0..count.min(MAX_PAGES) {
        if report.hits.len() >= limit {
            report.truncated = true;
            return Ok(true);
        }
        let rendered = renderer.render(RenderRequest { page, kind: RequestKind::Text, scale: 1.0, ..Default::default() });
        let Some(text) = rendered.text else { continue };
        let ranges = text.search(needle, opts).map_err(|e| Error::Pattern(e.to_string()))?;
        for range in ranges {
            if report.hits.len() >= limit {
                report.truncated = true;
                return Ok(true);
            }
            let page_no = u32::try_from(page.saturating_add(1)).unwrap_or(u32::MAX);
            report.hits.push(Hit { path: path.to_path_buf(), page: page_no, snippet: snippet(&text, range) });
        }
    }
    Ok(true)
}

#[cfg(not(target_arch = "wasm32"))]
fn walk(dir: &Path, depth: usize, subfolders: bool, files: &mut Vec<PathBuf>, skipped: &mut usize, truncated: &mut bool) -> Result<(), Error> {
    if *truncated || files.len() >= MAX_FILES {
        *truncated = true;
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut entries = Vec::new();
            for entry in rd {
                match entry {
                    Ok(entry) => entries.push(entry),
                    Err(_) => *skipped += 1,
                }
            }
            entries.sort_by_key(|entry| entry.file_name());
            entries
        }
        Err(e) if depth == 0 => return Err(Error::Unreadable { path: dir.display().to_string(), source: e }),
        Err(_) => {
            *skipped += 1;
            return Ok(());
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        if files.len() >= MAX_FILES {
            *truncated = true;
            return Ok(());
        }
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                *skipped += 1;
                continue;
            }
        };
        // `file_type` does not follow links, so a linked directory is not walked.
        if ft.is_symlink() {
            *skipped += 1;
            continue;
        }
        if ft.is_dir() {
            if subfolders {
                children.push(path);
            }
            continue;
        }
        if ft.is_file() && is_pdf(&path) {
            files.push(path);
        }
    }
    if !subfolders || children.is_empty() {
        return Ok(());
    }
    if depth + 1 >= MAX_DEPTH {
        *truncated = true;
        return Ok(());
    }
    for child in children {
        walk(&child, depth + 1, true, files, skipped, truncated)?;
        if *truncated {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn is_pdf(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

#[cfg(not(target_arch = "wasm32"))]
fn snippet(text: &PageText, range: std::ops::Range<usize>) -> String {
    let n = text.glyphs.len();
    let start = range.start.min(n);
    let end = range.end.min(n);
    let flat = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let before = flat(text.text_of(start.saturating_sub(SNIPPET_GLYPHS)..start));
    let hit = flat(text.text_of(start..end));
    let after = flat(text.text_of(end..(end + SNIPPET_GLYPHS).min(n)));
    let mut s = String::new();
    if !before.is_empty() {
        s.push_str(&before);
        s.push(' ');
    }
    s.push_str(&hit);
    if !after.is_empty() {
        s.push(' ');
        s.push_str(&after);
    }
    if s.chars().count() > SNIPPET_CHARS {
        let mut clipped: String = s.chars().take(SNIPPET_CHARS.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_page(text: &str) -> Vec<u8> {
        let body = format!("BT /F1 24 Tf 20 150 Td ({text}) Tj ET");
        let mut objs: Vec<Vec<u8>> = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()];
        objs.push(b"<< /Type /Pages /Kids [4 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_vec());
        objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
        objs.push(b"<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".to_vec());
        objs.push(format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()).into_bytes());
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, obj) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(obj);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.into(),
            subfolders: true,
            options: FindOptions { case_sensitive: false, whole_words: false, regex: false },
            limit: MAX_MATCHES,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pdfcraft-folder-find-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        dir
    }

    #[test]
    fn finds_pdfs_in_a_folder_and_its_subfolders() {
        let dir = temp_dir("walk");
        std::fs::write(dir.join("a.pdf"), one_page("Page 1")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"Page Nested marker").unwrap();
        std::fs::write(dir.join("bad.pdf"), b"not a pdf").unwrap();
        std::fs::write(dir.join("sub").join("nested.pdf"), one_page("Nested marker")).unwrap();

        let found = search(&dir, &query("Nested"), |_, _| true).unwrap();
        assert_eq!(found.hits.len(), 1, "{found:?}");
        assert!(found.hits[0].path.ends_with("nested.pdf"), "{}", found.hits[0].path.display());
        assert_eq!(found.hits[0].page, 1);
        assert!(found.hits[0].snippet.contains("Nested"), "{}", found.hits[0].snippet);
        assert!(found.skipped >= 1, "the unreadable PDF is skipped, not fatal: {found:?}");
        assert!(!found.hits.iter().any(|h| h.path.extension().is_some_and(|e| e != "pdf")));

        let mut top = query("Nested");
        top.subfolders = false;
        let top_only = search(&dir, &top, |_, _| true).unwrap();
        assert!(top_only.hits.is_empty(), "subfolders off stays in the starting folder: {top_only:?}");

        let mut whole = query("age");
        whole.options.whole_words = true;
        assert!(search(&dir, &whole, |_, _| true).unwrap().hits.is_empty(), "age is not the word Page");

        let mut case = query("page");
        case.options.case_sensitive = true;
        assert!(search(&dir, &case, |_, _| true).unwrap().hits.is_empty());

        let mut broken = query("(");
        broken.options.regex = true;
        let err = search(&dir, &broken, |_, _| true).unwrap_err();
        assert!(err.to_string().contains("unclosed"), "{err}");

        assert!(matches!(search(&dir, &query("   "), |_, _| true).unwrap_err(), Error::Empty));
        let file = dir.join("a.pdf");
        assert!(matches!(search(&file, &query("Page"), |_, _| true).unwrap_err(), Error::NotAFolder { .. }));

        let mut calls = 0;
        let cancelled = search(&dir, &query("Page"), |_, _| {
            calls += 1;
            false
        })
        .unwrap();
        assert!(cancelled.cancelled && cancelled.hits.is_empty() && calls == 1, "{cancelled:?} calls {calls}");
    }
}
