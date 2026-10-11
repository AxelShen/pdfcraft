//! Edit ▸ Advanced Search (Ctrl+Shift+F / ⇧⌘F).
//!
//! The panel searches either the open document or every PDF in a folder. Folder search runs off
//! the UI thread and opens a hit in place, keeping unsaved edits in a tab that already shows
//! that file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};
use pdfcraft_engine::folder_find::{self, Hit};
use pdfcraft_render::text::{FindOptions, PageText};

use crate::canvas::{DocView, Find};
use crate::theme::{self, Tokens};

/// Glyphs of context shown on each side of a match in the current document.
const CONTEXT: usize = 36;

/// Where Advanced Search looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchScope {
    #[default]
    Document,
    /// Every PDF in [`FolderSearch::folder`].
    Folder,
}

/// The folder-search form and its latest job.
#[derive(Clone, Debug)]
pub struct FolderSearch {
    pub query: String,
    pub folder: String,
    pub subfolders: bool,
    pub case_sensitive: bool,
    pub whole_words: bool,
    pub regex: bool,
    /// Focus the query field on the next frame (the shortcut just opened the panel).
    pub focus: bool,
    /// A problem with the form, shown until the query or the folder changes.
    pub error: Option<String>,
    job: Option<Arc<Mutex<FolderProgress>>>,
}

impl Default for FolderSearch {
    fn default() -> Self {
        Self {
            query: String::new(),
            folder: String::new(),
            subfolders: true,
            case_sensitive: false,
            whole_words: false,
            regex: false,
            focus: false,
            error: None,
            job: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FolderProgress {
    scanned: usize,
    total: usize,
    page: u32,
    pages: u32,
    current: String,
    hits: Vec<Hit>,
    files: usize,
    skipped: usize,
    truncated: bool,
    error: Option<String>,
    done: bool,
    cancel: bool,
    abort: Option<Arc<AtomicBool>>,
}

/// The Search side panel, including its frame. Works with no document open.
pub(crate) fn show(app: &mut crate::PdfCraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let mut close = false;
    egui::Frame::NONE
        .fill(t.panel)
        .inner_margin(egui::Margin { left: 14, right: 12, top: 12, bottom: 8 })
        .stroke(egui::Stroke::new(1.0, t.divider))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(tl!("Search")).font(theme::semibold(15.5)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::icons::button(ui, "x", 26.0, false, tl!("Close")).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(6.0);
            let mut to_document = false;
            if ui.radio_value(&mut app.search_scope, SearchScope::Document, tl!("In the current document")).changed() {
                to_document = true;
            }
            ui.radio_value(&mut app.search_scope, SearchScope::Folder, tl!("All PDF files in a folder"));
            ui.add_space(8.0);
            if to_document && let Some(i) = app.active {
                let find = app.views[i].find.get_or_insert_with(Default::default);
                find.in_panel = true;
                find.focus = true;
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match app.search_scope {
                SearchScope::Document => document_body(app, ui, &t),
                SearchScope::Folder => folder_body(app, ui, &t),
            });
        });
    if close {
        app.choose_right_panel(None);
    }
    let running = app.folder_search.job.as_ref().is_some_and(|job| job.lock().map(|g| !g.done).unwrap_or(false));
    if running {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

fn document_body(app: &mut crate::PdfCraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let Some((index, id)) = app.active_ids() else {
        ui.label(tl!("Open a document first"));
        return;
    };
    let pages = app.session.get(id).map(|d| d.info.pages.len()).unwrap_or(0);
    panel(ui, t, &mut app.views[index], pages);
}

pub(crate) fn panel(ui: &mut egui::Ui, t: &Tokens, view: &mut DocView, pages: usize) {
    let searched = view.texts.len() + view.text_failed.len();
    let Some(find) = view.find.as_mut() else { return };
    find.in_panel = true;
    let l = ui.label(egui::RichText::new(tl!("What word or phrase would you like to search for?")).color(t.text_muted));
    let r = search_input(
        ui,
        t,
        find.regex_error.is_some(),
        egui::TextEdit::singleline(&mut find.query).id(egui::Id::new("search-panel-input")).hint_text(tl!("Search")).desired_width(f32::INFINITY),
    )
    .labelled_by(l.id);
    if find.focus {
        r.request_focus();
        find.focus = false;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        crate::canvas::find_toggles(ui, find);
    });
    crate::canvas::consume_find_option_keys(ui, find);
    if find.query != find.case_query {
        view.rerun_find();
    }
    let Some(find) = view.find.as_ref() else { return };
    if let Some(err) = &find.regex_error {
        ui.label(egui::RichText::new(err).color(error_color(t)));
    }
    ui.add_space(6.0);
    let blank = if find.regex { find.query.is_empty() } else { find.query.trim().is_empty() };
    let status = if blank || find.regex_error.is_some() {
        String::new()
    } else if find.matches.is_empty() && searched < pages {
        crate::i18n::fmt(tl!("Searching… {s} of {p} pages"), &[("s", &searched.to_string()), ("p", &pages.to_string())])
    } else {
        let n = find.matches.len();
        let more = if searched < pages {
            crate::i18n::fmt(tl!(" (searching… {s} of {p} pages)"), &[("s", &searched.to_string()), ("p", &pages.to_string())])
        } else {
            String::new()
        };
        // English keeps its singular; Chinese has no plural.
        let count = if n == 1 { crate::i18n::fmt(tl!("1 instance"), &[]) } else { crate::i18n::fmt(tl!("{n} instances"), &[("n", &n.to_string())]) };
        format!("{count}{more}")
    };
    ui.label(egui::RichText::new(status).font(theme::semibold(12.5)));
    ui.add_space(4.0);
    let mut go = None;
    let mut last_page = None;
    for (i, (p, range)) in find.matches.iter().enumerate().take(2000) {
        if last_page != Some(*p) {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Page {label}"), &[("label", &(p + 1).to_string())])).small().color(t.text_faint));
            last_page = Some(*p);
        }
        let Some(text) = view.texts.get(p) else { continue };
        let n = text.glyphs.len();
        let before = text.text_of(range.start.saturating_sub(CONTEXT)..range.start.min(n));
        let hit = text.text_of(range.start.min(n)..range.end.min(n));
        let after = text.text_of(range.end.min(n)..(range.end + CONTEXT).min(n));
        let flat = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut job = LayoutJob::default();
        let font = FontId::new(12.5, egui::FontFamily::Proportional);
        let plain = TextFormat { font_id: font.clone(), color: t.text_muted, ..Default::default() };
        job.append(&format!("…{} ", flat(before)), 0.0, plain.clone());
        job.append(
            &flat(hit),
            0.0,
            TextFormat {
                font_id: theme::semibold(12.5),
                color: t.text,
                background: Color32::from_rgba_unmultiplied(255, 214, 0, 90),
                ..Default::default()
            },
        );
        job.append(&format!(" {}…", flat(after)), 0.0, plain);
        job.wrap.max_width = (ui.available_width() - 24.0).max(40.0);
        let selected = find.current == Some(i);
        let resp = result_row(ui, t, selected, job).on_hover_text(tl!("Go to this result"));
        if resp.clicked() {
            go = Some(i);
        }
    }
    if let Some(i) = go {
        view.go_to_match(i);
    }
}

fn folder_body(app: &mut crate::PdfCraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let progress = app.folder_search.job.as_ref().and_then(|job| job.lock().ok().map(|g| g.clone()));
    let running = progress.as_ref().is_some_and(|p| !p.done);
    let l = ui.label(egui::RichText::new(tl!("Search for words in the PDF files in this folder")).color(t.text_muted));
    let previous_query = app.folder_search.query.clone();
    let invalid = regex_problem(&app.folder_search).is_some();
    let response = search_input(
        ui,
        t,
        invalid,
        egui::TextEdit::singleline(&mut app.folder_search.query)
            .id(egui::Id::new("folder-search-input"))
            .hint_text(tl!("Search"))
            .desired_width(f32::INFINITY),
    )
    .labelled_by(l.id);
    if app.folder_search.focus {
        response.request_focus();
        app.folder_search.focus = false;
    }
    if app.folder_search.query != previous_query {
        app.folder_search.error = None;
    }
    let mut opts = Find {
        query: app.folder_search.query.clone(),
        case_sensitive: app.folder_search.case_sensitive,
        whole_words: app.folder_search.whole_words,
        regex: app.folder_search.regex,
        ..Default::default()
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        crate::canvas::find_toggles(ui, &mut opts);
    });
    crate::canvas::consume_find_option_keys(ui, &mut opts);
    app.folder_search.case_sensitive = opts.case_sensitive;
    app.folder_search.whole_words = opts.whole_words;
    app.folder_search.regex = opts.regex;
    let regex_error = regex_problem(&app.folder_search);
    if let Some(err) = &regex_error {
        ui.label(egui::RichText::new(err).color(error_color(t)));
    }
    ui.add_space(8.0);
    ui.label(tl!("Folder"));
    let previous_folder = app.folder_search.folder.clone();
    search_input(
        ui,
        t,
        false,
        egui::TextEdit::singleline(&mut app.folder_search.folder)
            .id(egui::Id::new("folder-search-path"))
            .hint_text(tl!("Choose a folder to search"))
            .desired_width(f32::INFINITY),
    );
    if app.folder_search.folder != previous_folder {
        app.folder_search.error = None;
    }
    #[cfg(not(target_arch = "wasm32"))]
    if crate::widgets::ghost_button(ui, "folder", tl!("Browse…")).clicked() {
        let dialog = rfd::AsyncFileDialog::new().set_title(tl!("Choose a folder to search").to_string());
        app.ask_one(crate::pickers::Ask::Folder(dialog), None, |app, dir| {
            app.folder_search.folder = dir.display().to_string();
            app.folder_search.error = None;
        });
    }
    ui.checkbox(&mut app.folder_search.subfolders, tl!("Include subfolders"));
    ui.add_space(6.0);
    if running {
        if crate::widgets::pill_button(ui, tl!("Cancel"), false).clicked()
            && let Some(job) = &app.folder_search.job
        {
            stop_job(job);
        }
    } else if crate::widgets::pill_button(ui, tl!("Search this folder"), true).clicked() {
        start_folder_search(app);
    }
    ui.add_space(6.0);
    if let Some(err) = &app.folder_search.error {
        ui.label(egui::RichText::new(err).color(error_color(t)));
    }
    let Some(progress) = progress else { return };
    if let Some(err) = &progress.error {
        ui.label(egui::RichText::new(err).color(error_color(t)));
        return;
    }
    if progress.done {
        let n = progress.hits.len().to_string();
        let files = progress.files.to_string();
        ui.label(egui::RichText::new(crate::i18n::fmt(tl!("{n} results in {f} files"), &[("n", &n), ("f", &files)])).font(theme::semibold(12.5)));
        if progress.skipped > 0 {
            let k = progress.skipped.to_string();
            ui.label(crate::i18n::fmt(tl!("{k} files could not be read."), &[("k", &k)]));
        }
        if progress.truncated {
            ui.label(crate::i18n::fmt(tl!("Showing the first {n} results."), &[("n", &n)]));
        }
    } else if progress.total == 0 {
        ui.label(egui::RichText::new(tl!("Searching…")).font(theme::semibold(12.5)));
    } else {
        let shown = progress.scanned.saturating_add(1).min(progress.total);
        let status = crate::i18n::fmt(tl!("Searching… {s} of {t} files"), &[("s", &shown.to_string()), ("t", &progress.total.to_string())]);
        ui.label(egui::RichText::new(status).font(theme::semibold(12.5)));
        if progress.pages > 0 {
            let pages =
                crate::i18n::fmt(tl!("Searching… {s} of {p} pages"), &[("s", &progress.page.to_string()), ("p", &progress.pages.to_string())]);
            ui.label(egui::RichText::new(pages).color(t.text_muted));
        }
        if !progress.current.is_empty() {
            ui.label(egui::RichText::new(&progress.current).small().color(t.text_muted));
        }
    }
    ui.add_space(4.0);
    let mut last: Option<PathBuf> = None;
    let mut chosen = None;
    for (i, hit) in progress.hits.iter().enumerate() {
        if last.as_deref() != Some(hit.path.as_path()) {
            ui.add_space(6.0);
            let name = hit.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| hit.path.display().to_string());
            ui.label(egui::RichText::new(name).font(theme::semibold(13.0))).on_hover_text(hit.path.display().to_string());
            last = Some(hit.path.clone());
        }
        ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Page {label}"), &[("label", &hit.page.to_string())])).small().color(t.text_faint));
        let text = if hit.snippet.is_empty() { "…".to_string() } else { hit.snippet.clone() };
        if result_row(ui, t, false, text).on_hover_text(tl!("Go to this result")).clicked() {
            chosen = Some(i);
        }
    }
    if let Some(i) = chosen
        && let Some(hit) = progress.hits.get(i).cloned()
    {
        open_hit(app, &hit);
    }
}

fn regex_problem(search: &FolderSearch) -> Option<String> {
    if !search.regex || search.query.is_empty() {
        return None;
    }
    let opts = FindOptions { case_sensitive: search.case_sensitive, whole_words: search.whole_words, regex: true };
    PageText::default().search(&search.query, opts).err().map(|e| e.to_string())
}

fn start_folder_search(app: &mut crate::PdfCraftApp) {
    app.folder_search.error = None;
    let text = if app.folder_search.regex { app.folder_search.query.clone() } else { app.folder_search.query.trim().to_string() };
    if text.is_empty() {
        app.folder_search.error = Some(tl!("Enter a word or phrase to search for.").to_string());
        app.folder_search.job = None;
        return;
    }
    if let Some(err) = regex_problem(&app.folder_search) {
        app.folder_search.error = Some(err);
        app.folder_search.job = None;
        return;
    }
    if app.folder_search.folder.trim().is_empty() {
        app.folder_search.error = Some(tl!("Choose a folder to search").to_string());
        app.folder_search.job = None;
        return;
    }
    if let Some(job) = &app.folder_search.job {
        stop_job(job);
    }
    let abort = Arc::new(AtomicBool::new(false));
    let query = folder_find::Query {
        text,
        subfolders: app.folder_search.subfolders,
        options: FindOptions {
            case_sensitive: app.folder_search.case_sensitive,
            whole_words: app.folder_search.whole_words,
            regex: app.folder_search.regex,
        },
        limit: folder_find::MAX_MATCHES,
        cancel: Some(Arc::clone(&abort)),
    };
    let folder = PathBuf::from(app.folder_search.folder.trim());
    let progress = Arc::new(Mutex::new(FolderProgress { abort: Some(abort), ..FolderProgress::default() }));
    let shared = progress.clone();
    let lang = crate::i18n::current();
    let work = move || {
        crate::i18n::set_current(lang);
        let report = folder_find::search(&folder, &query, |state, hits| {
            let Ok(mut guard) = shared.lock() else { return false };
            guard.scanned = state.files_done;
            guard.total = state.files_total;
            guard.page = state.page;
            guard.pages = state.pages;
            guard.current = state.path.as_ref().and_then(|path| path.file_name()).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            if guard.hits.len() != hits.len() {
                guard.hits = hits.to_vec();
            }
            !guard.cancel
        });
        if let Ok(mut guard) = shared.lock() {
            match report {
                Ok(report) => {
                    guard.hits = report.hits;
                    guard.files = report.files;
                    guard.skipped = report.skipped;
                    guard.truncated = report.truncated;
                    guard.scanned = guard.total;
                    guard.done = true;
                }
                Err(err) => {
                    guard.error = Some(explain(&err));
                    guard.done = true;
                }
            }
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    if app.run_inline {
        work();
    } else if std::thread::Builder::new().name("pdfcraft-folder-find".into()).spawn(work).is_err()
        && let Ok(mut guard) = progress.lock()
    {
        guard.done = true;
        guard.error = Some(tl!("Folder search is not available in this build.").to_string());
    }
    #[cfg(target_arch = "wasm32")]
    work();
    app.folder_search.job = Some(progress);
}

fn stop_job(job: &Mutex<FolderProgress>) {
    if let Ok(mut guard) = job.lock() {
        guard.cancel = true;
        if let Some(flag) = &guard.abort {
            flag.store(true, Ordering::Release);
        }
    }
}

fn explain(err: &folder_find::Error) -> String {
    match err {
        folder_find::Error::Empty => tl!("Enter a word or phrase to search for.").to_string(),
        folder_find::Error::Pattern(message) => message.clone(),
        folder_find::Error::NotAFolder { path } => crate::i18n::fmt(tl!("{path} is not a folder"), &[("path", path)]),
        folder_find::Error::Unreadable { path, source } => {
            crate::i18n::fmt(tl!("Couldn't read {name}: {e}"), &[("name", path), ("e", &source.to_string())])
        }
        folder_find::Error::Unavailable => tl!("Folder search is not available in this build.").to_string(),
    }
}

fn open_hit(app: &mut crate::PdfCraftApp, hit: &Hit) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, hit);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let ids: Vec<(usize, pdfcraft_engine::DocId)> = app.views.iter().enumerate().map(|(i, view)| (i, view.id)).collect();
        let existing =
            ids.into_iter().find(|(_, id)| app.session.get(*id).and_then(|doc| doc.path.clone()).is_some_and(|stored| same_file(&stored, &hit.path)));
        if let Some((index, _)) = existing {
            app.active = Some(index);
        } else {
            app.open_path(&hit.path.display().to_string());
        }
        let Some(index) = app.active else { return };
        let opened = app
            .views
            .get(index)
            .and_then(|view| app.session.get(view.id))
            .and_then(|doc| doc.path.clone())
            .is_some_and(|stored| same_file(&stored, &hit.path));
        if !opened {
            return;
        }
        let case_sensitive = app.folder_search.case_sensitive;
        let whole_words = app.folder_search.whole_words;
        let regex = app.folder_search.regex;
        let query = app.folder_search.query.clone();
        let view = &mut app.views[index];
        let page = usize::try_from(hit.page.saturating_sub(1)).unwrap_or(0);
        view.go_to_page(page);
        let find = view.find.get_or_insert_with(Default::default);
        find.query = query;
        find.case_sensitive = case_sensitive;
        find.whole_words = whole_words;
        find.regex = regex;
        find.in_panel = false;
        find.focus = false;
        view.rerun_find();
    }
}

fn same_file(stored: &str, hit: &Path) -> bool {
    let stored = Path::new(stored);
    stored == hit || matches!((stored.canonicalize(), hit.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn error_color(t: &Tokens) -> Color32 {
    if t.dark() { Color32::from_rgb(0xF0, 0x70, 0x70) } else { Color32::from_rgb(0xC4, 0x2B, 0x2B) }
}

/// The theme draws text fields without a stroke. A box keeps the click on the field.
fn search_input(ui: &mut egui::Ui, t: &Tokens, invalid: bool, edit: egui::TextEdit<'_>) -> egui::Response {
    let rest = if invalid { error_color(t) } else { t.border };
    let hot = if invalid { error_color(t) } else { t.accent };
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::new(1.0, rest);
        ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::new(1.0, hot);
        ui.visuals_mut().widgets.active.bg_stroke = egui::Stroke::new(1.0, hot);
        ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0, hot);
        ui.add(edit)
    })
    .inner
}

/// One result. Full width, with a border, so a click lands on that result.
fn result_row(ui: &mut egui::Ui, t: &Tokens, selected: bool, content: impl Into<egui::WidgetText>) -> egui::Response {
    let width = ui.available_width();
    ui.scope(|ui| {
        let stroke = egui::Stroke::new(1.0, if selected { t.accent } else { t.border });
        ui.visuals_mut().widgets.inactive.bg_stroke = stroke;
        ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::new(1.0, t.accent);
        ui.visuals_mut().widgets.active.bg_stroke = egui::Stroke::new(1.0, t.accent);
        ui.visuals_mut().widgets.inactive.weak_bg_fill = if selected { t.selected } else { t.card };
        ui.visuals_mut().widgets.hovered.weak_bg_fill = t.hover;
        ui.visuals_mut().widgets.active.weak_bg_fill = t.pressed;
        ui.add(egui::Button::selectable(selected, content).wrap().min_size(egui::vec2(width, 28.0)))
    })
    .inner
}
