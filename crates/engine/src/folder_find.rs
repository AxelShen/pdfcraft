//! Search the PDF files in a folder.
//!
//! Uses the same [`PageText::search`](pdfcraft_render::text::PageText::search) rules as Find
//! (case, whole word, VS Code regular expressions). Unreadable, encrypted, oversized and
//! symlinked files are skipped; one bad file does not stop the rest. The walk is synchronous:
//! callers that have a UI run it on a worker thread. A page that runs longer than a few seconds
//! is abandoned and the rest of that file is skipped, so one damaged page cannot hold the search.
//! The interpreter only notices a cancel between operators, so one operator that never returns
//! can still run long.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::Ordering;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

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
/// One page may take this long. After that the rest of that file is skipped, so a damaged
/// page cannot hold the search.
#[cfg(not(target_arch = "wasm32"))]
const PAGE_BUDGET: Duration = Duration::from_secs(5);
/// How often a running page re-checks [`Query::cancel`].
#[cfg(not(target_arch = "wasm32"))]
const POLL: Duration = Duration::from_millis(40);

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
    /// Set this to stop the search, including the page being read.
    pub cancel: Option<Arc<AtomicBool>>,
}

/// Where the walk is, for a progress callback.
#[derive(Clone, Debug)]
pub struct Progress {
    /// Files already searched. The file in [`Self::path`] is not included.
    pub files_done: usize,
    pub files_total: usize,
    /// The file being read, when one is.
    pub path: Option<PathBuf>,
    /// 1-based page being read. `0` while counting files or between pages.
    pub page: u32,
    /// Pages in the file being read. `0` until that file is open.
    pub pages: u32,
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
    /// `progress` returned `false`, or [`Query::cancel`] was set, before the walk finished.
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
/// `progress` runs while files are counted and before each page. `hits` is everything found so
/// far. Return `false` to stop. On wasm there is no folder walk.
#[cfg(target_arch = "wasm32")]
pub fn search(folder: &Path, query: &Query, mut progress: impl FnMut(&Progress, &[Hit]) -> bool) -> Result<Report, Error> {
    let _ = (folder, query, &mut progress);
    Err(Error::Unavailable)
}

/// Search `folder` for `query`.
///
/// `progress` runs while files are counted and before each page. `hits` is everything found so
/// far. Return `false` to stop.
#[cfg(not(target_arch = "wasm32"))]
pub fn search(folder: &Path, query: &Query, mut progress: impl FnMut(&Progress, &[Hit]) -> bool) -> Result<Report, Error> {
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
    let mut cancelled = false;
    walk(
        folder,
        0,
        query.subfolders,
        &mut Basket { files: &mut files, skipped: &mut skipped, truncated: &mut truncated, cancelled: &mut cancelled },
        query.cancel.as_ref(),
        &mut progress,
    )?;
    let mut report = Report { hits: Vec::new(), files: 0, skipped, truncated, cancelled };
    if cancelled {
        return Ok(report);
    }
    files.sort();
    let scan =
        Scan { needle: &text, opts: query.options, limit: query.limit.clamp(1, MAX_MATCHES), total: files.len(), cancel: query.cancel.as_ref() };
    for (i, path) in files.iter().enumerate() {
        let state = Progress { files_done: i, files_total: scan.total, path: Some(path.clone()), page: 0, pages: 0 };
        if !tick(scan.cancel, &mut progress, &state, &report.hits) {
            report.cancelled = true;
            return Ok(report);
        }
        match search_file(path, &scan, i, &mut report, &mut progress)? {
            Read::Opened => report.files += 1,
            Read::Skipped => report.skipped += 1,
            Read::Stop { opened } => {
                if opened {
                    report.files += 1;
                }
                report.cancelled = true;
                return Ok(report);
            }
        }
        if report.hits.len() >= scan.limit {
            report.truncated = true;
            break;
        }
    }
    let _ = progress(&Progress { files_done: scan.total, files_total: scan.total, path: None, page: 0, pages: 0 }, &report.hits);
    Ok(report)
}

#[cfg(not(target_arch = "wasm32"))]
struct Basket<'a> {
    files: &'a mut Vec<PathBuf>,
    skipped: &'a mut usize,
    truncated: &'a mut bool,
    cancelled: &'a mut bool,
}

#[cfg(not(target_arch = "wasm32"))]
struct Scan<'a> {
    needle: &'a str,
    opts: FindOptions,
    limit: usize,
    total: usize,
    cancel: Option<&'a Arc<AtomicBool>>,
}

#[cfg(not(target_arch = "wasm32"))]
enum Read {
    Opened,
    Skipped,
    /// Stop the walk. `opened` counts the file as searched.
    Stop {
        opened: bool,
    },
}

#[cfg(not(target_arch = "wasm32"))]
enum PageEnd {
    Finished,
    TimedOut,
    Cancelled,
}

#[cfg(not(target_arch = "wasm32"))]
fn counting() -> Progress {
    Progress { files_done: 0, files_total: 0, path: None, page: 0, pages: 0 }
}

#[cfg(not(target_arch = "wasm32"))]
fn stopped(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
}

/// `false` stops the walk. A set cancel flag stops even when `progress` would continue.
#[cfg(not(target_arch = "wasm32"))]
fn tick(cancel: Option<&Arc<AtomicBool>>, progress: &mut impl FnMut(&Progress, &[Hit]) -> bool, state: &Progress, hits: &[Hit]) -> bool {
    !stopped(cancel) && progress(state, hits)
}

#[cfg(not(target_arch = "wasm32"))]
struct PageTimer {
    tx: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<PageEnd>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for PageTimer {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read one page, giving up after [`PAGE_BUDGET`]. `stop` is what the interpreter checks between
/// operators.
#[cfg(not(target_arch = "wasm32"))]
fn read_bounded(
    renderer: &mut PageRenderer,
    page: usize,
    stop: &Arc<AtomicBool>,
    user: Option<&Arc<AtomicBool>>,
) -> (pdfcraft_render::RenderedPage, PageEnd) {
    stop.store(false, Ordering::Release);
    let (tx, rx) = mpsc::channel();
    let stop_flag = Arc::clone(stop);
    let user_flag = user.cloned();
    let spawned = std::thread::Builder::new().name("pdfcraft-folder-find-page".into()).spawn(move || {
        let started = std::time::Instant::now();
        loop {
            if user_flag.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)) {
                stop_flag.store(true, Ordering::Release);
                return PageEnd::Cancelled;
            }
            let left = PAGE_BUDGET.saturating_sub(started.elapsed());
            if left.is_zero() {
                stop_flag.store(true, Ordering::Release);
                return PageEnd::TimedOut;
            }
            match rx.recv_timeout(left.min(POLL)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return PageEnd::Finished,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });
    let mut timer = match spawned {
        Ok(handle) => PageTimer { tx: Some(tx), handle: Some(handle) },
        Err(_) => PageTimer { tx: None, handle: None },
    };
    let rendered = renderer.render(RenderRequest { page, kind: RequestKind::Text, scale: 1.0, ..Default::default() });
    let end = match timer.handle.take() {
        Some(handle) => {
            if let Some(sender) = timer.tx.take() {
                let _ = sender.send(());
            }
            handle.join().unwrap_or(PageEnd::Finished)
        }
        None => PageEnd::Finished,
    };
    (rendered, end)
}

/// `true` when the match cap was reached.
#[cfg(not(target_arch = "wasm32"))]
fn push_hits(path: &Path, page: usize, text: &PageText, scan: &Scan<'_>, report: &mut Report) -> Result<bool, Error> {
    let ranges = text.search(scan.needle, scan.opts).map_err(|e| Error::Pattern(e.to_string()))?;
    let page_no = u32::try_from(page.saturating_add(1)).unwrap_or(u32::MAX);
    for range in ranges {
        if report.hits.len() >= scan.limit {
            report.truncated = true;
            return Ok(true);
        }
        report.hits.push(Hit { path: path.to_path_buf(), page: page_no, snippet: snippet(text, range) });
    }
    if report.hits.len() >= scan.limit {
        report.truncated = true;
        return Ok(true);
    }
    Ok(false)
}

/// Opened, skipped, or stopped because the search was cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn search_file(
    path: &Path,
    scan: &Scan<'_>,
    files_done: usize,
    report: &mut Report,
    progress: &mut impl FnMut(&Progress, &[Hit]) -> bool,
) -> Result<Read, Error> {
    if stopped(scan.cancel) {
        return Ok(Read::Stop { opened: false });
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(Read::Skipped),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Ok(Read::Skipped);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => Arc::new(b),
        Err(_) => return Ok(Read::Skipped),
    };
    if stopped(scan.cancel) {
        return Ok(Read::Stop { opened: false });
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut renderer = PageRenderer::new(bytes, RenderConfig { cancelled: Some(Arc::clone(&stop)), ..RenderConfig::default() });
    let count = renderer.page_count();
    if count == 0 {
        return Ok(if stopped(scan.cancel) { Read::Stop { opened: false } } else { Read::Skipped });
    }
    let pages_cap = count.min(MAX_PAGES);
    let pages = u32::try_from(pages_cap).unwrap_or(u32::MAX);
    if count > MAX_PAGES {
        report.truncated = true;
    }
    for page in 0..pages_cap {
        if report.hits.len() >= scan.limit {
            report.truncated = true;
            return Ok(Read::Opened);
        }
        let state = Progress {
            files_done,
            files_total: scan.total,
            path: Some(path.to_path_buf()),
            page: u32::try_from(page.saturating_add(1)).unwrap_or(u32::MAX),
            pages,
        };
        if !tick(scan.cancel, progress, &state, &report.hits) {
            return Ok(Read::Stop { opened: true });
        }
        let (rendered, end) = read_bounded(&mut renderer, page, &stop, scan.cancel);
        if let Some(text) = rendered.text.as_deref()
            && push_hits(path, page, text, scan, report)?
        {
            return Ok(Read::Opened);
        }
        if !tick(scan.cancel, progress, &state, &report.hits) {
            return Ok(Read::Stop { opened: true });
        }
        if matches!(end, PageEnd::TimedOut) {
            report.truncated = true;
            return Ok(Read::Opened);
        }
    }
    Ok(Read::Opened)
}

#[cfg(not(target_arch = "wasm32"))]
fn walk(
    dir: &Path,
    depth: usize,
    subfolders: bool,
    basket: &mut Basket<'_>,
    cancel: Option<&Arc<AtomicBool>>,
    progress: &mut impl FnMut(&Progress, &[Hit]) -> bool,
) -> Result<(), Error> {
    if *basket.cancelled {
        return Ok(());
    }
    if *basket.truncated || basket.files.len() >= MAX_FILES {
        *basket.truncated = true;
        return Ok(());
    }
    if !tick(cancel, progress, &counting(), &[]) {
        *basket.cancelled = true;
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut entries = Vec::new();
            let mut seen = 0usize;
            for entry in rd {
                seen += 1;
                if seen.is_multiple_of(32) && !tick(cancel, progress, &counting(), &[]) {
                    *basket.cancelled = true;
                    return Ok(());
                }
                match entry {
                    Ok(entry) => entries.push(entry),
                    Err(_) => *basket.skipped += 1,
                }
            }
            entries.sort_by_key(|entry| entry.file_name());
            entries
        }
        Err(e) if depth == 0 => return Err(Error::Unreadable { path: dir.display().to_string(), source: e }),
        Err(_) => {
            *basket.skipped += 1;
            return Ok(());
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        if basket.files.len() >= MAX_FILES {
            *basket.truncated = true;
            return Ok(());
        }
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                *basket.skipped += 1;
                continue;
            }
        };
        // `file_type` does not follow links, so a linked directory is not walked.
        if ft.is_symlink() {
            *basket.skipped += 1;
            continue;
        }
        if ft.is_dir() {
            if subfolders {
                children.push(path);
            }
            continue;
        }
        if ft.is_file() && is_pdf(&path) {
            basket.files.push(path);
        }
    }
    if !subfolders || children.is_empty() {
        return Ok(());
    }
    if depth + 1 >= MAX_DEPTH {
        *basket.truncated = true;
        return Ok(());
    }
    for child in children {
        if *basket.cancelled || *basket.truncated {
            return Ok(());
        }
        walk(&child, depth + 1, true, basket, cancel, progress)?;
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
            cancel: None,
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
        assert!(!found.cancelled && !found.truncated, "{found:?}");
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

        let mut saw_total = false;
        let counted = search(&dir, &query("Page"), |state, _| {
            saw_total |= state.files_total >= 2;
            true
        })
        .unwrap();
        assert!(saw_total && !counted.cancelled && !counted.hits.is_empty(), "{counted:?}");

        let mut flagged = query("Page");
        flagged.cancel = Some(Arc::new(AtomicBool::new(true)));
        let flagged = search(&dir, &flagged, |_, _| true).unwrap();
        assert!(flagged.cancelled && flagged.hits.is_empty() && flagged.files == 0, "{flagged:?}");

        let mut calls = 0;
        let cancelled = search(&dir, &query("Page"), |_, _| {
            calls += 1;
            false
        })
        .unwrap();
        assert!(cancelled.cancelled && cancelled.hits.is_empty() && calls == 1, "{cancelled:?} calls {calls}");
    }
}
