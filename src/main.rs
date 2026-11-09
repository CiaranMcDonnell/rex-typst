//! rex-typst: a live Typst preview drawn in a Rex pane with kitty graphics.

mod editor;
mod kitty;
mod project;
mod world;

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::{cursor, execute, terminal};
use notify::{RecursiveMode, Watcher};
use typst::World;
use typst::WorldExt;
use typst::diag::{Severity, SourceDiagnostic, Warned};
use typst::introspection::PagedPosition;
use typst::layout::{Abs, Frame, FrameItem, Point, Size, Transform};
use typst::utils::{Scalar, hash128};
use typst_ide::Jump;
use typst_layout::PagedDocument;

use editor::{Editor, EditorLink, FromEditor};
use world::TypstWorld;

#[derive(Parser)]
#[command(version, about = "Live Typst preview in a Rex pane, drawn with kitty graphics")]
struct Args {
    /// The file to preview. Default: the document that includes the .typ
    /// file open in the editor pane, else that file, else main.typ or the
    /// only .typ file in the current directory.
    file: Option<PathBuf>,
    /// The project root. Default: $TYPST_ROOT, else the nearest directory
    /// with typst.toml or .git, else the file's directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// The Rex block the editor runs in, to follow it and jump it to clicks.
    #[arg(long)]
    editor_block: Option<String>,
    /// Neovim's RPC socket, if it can't be found from the editor block.
    #[arg(long)]
    nvim: Option<String>,
    /// Start fitting the whole page instead of the width.
    #[arg(long)]
    fit_page: bool,
    /// Start with inverted colors, for a dark preview.
    #[arg(long)]
    invert: bool,
    /// Invert images too, when inverting. By default they keep their colors.
    #[arg(long)]
    invert_images: bool,
    /// Don't scroll the preview to the editor's cursor.
    #[arg(long)]
    no_follow: bool,
    /// Leave the focus on the preview after a click jumps the editor.
    #[arg(long)]
    no_focus_editor: bool,
    /// Keep the editor's cursor in the middle of the preview.
    #[arg(long)]
    center: bool,
    /// Megabytes of rendered pages to keep in the terminal. Pages further
    /// from the view are freed past this, and rendered again when they come
    /// back. Pages on screen are always kept.
    #[arg(long, default_value_t = 40, value_name = "MB")]
    image_mb: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Width,
    Page,
}

#[derive(Clone, Copy)]
struct Term {
    cols: u16,
    rows: u16,
    cw: u32,
    ch: u32,
}

impl Term {
    fn query() -> Self {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let (mut cw, mut ch) = (0, 0);
        if let Ok(ws) = terminal::window_size() {
            if ws.width > 0 && ws.columns > 0 {
                cw = ws.width as u32 / ws.columns as u32;
                ch = ws.height as u32 / ws.rows.max(1) as u32;
            }
        }
        if cw == 0 || ch == 0 {
            (cw, ch) = rex_cell_size().unwrap_or((8, 16));
        }
        Self { cols: cols.max(1), rows: rows.max(2), cw, ch }
    }
}

/// Cell size from Rex, for terminals that don't report pixels in the ioctl.
fn rex_cell_size() -> Option<(u32, u32)> {
    let block = std::env::var("REX_BLOCK").ok()?;
    let out = std::process::Command::new("rex").args(["block", "call", "-b", &block, "size", "{}"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((v.get("cell_width_px")?.as_u64()? as u32, v.get("cell_height_px")?.as_u64()? as u32))
}

/// Where a page sits in the scrolling column of pages.
struct Slot {
    /// First row of the page, counting rows from the top of the document.
    top: usize,
    /// Size on screen in cells.
    cols: u16,
    rows: u16,
    /// Column of its left edge, counting from the left of the widest page.
    col: u16,
    /// Pixels per point.
    scale: f64,
    /// Identifies the rendered image: the page's content, scale and colors.
    key: u128,
}

/// A compile error or warning, and where it is.
struct Problem {
    text: String,
    /// File, 1-based line, and 1-based byte column.
    at: Option<(PathBuf, usize, usize)>,
}

/// A rendered page in the terminal's image store.
struct Image {
    id: u32,
    /// Raw size, which is what the terminal keeps.
    bytes: usize,
}

/// Gap between pages, in rows.
const GAP: usize = 1;

/// Zoom limits, relative to the fit. The widest page is also kept under
/// MAX_PAGE_PX wide, so a zoomed page stays a sensible image to upload.
const MIN_ZOOM: f64 = 0.25;
const MAX_ZOOM: f64 = 4.0;
const ZOOM_STEP: f64 = 1.25;
const MAX_PAGE_PX: f64 = 6000.0;

struct App {
    world: TypstWorld,
    main: PathBuf,
    doc: Option<PagedDocument>,
    page_hashes: Vec<u128>,
    errors: Vec<Problem>,
    warnings: Vec<Problem>,
    /// The error or warning `e` goes to next.
    next_problem: usize,
    compile_ms: u128,
    term: Term,
    scroll: usize,
    /// Columns scrolled right, when zoomed pages are wider than the pane.
    hscroll: usize,
    fit: Fit,
    /// Scale relative to the fit.
    zoom: f64,
    invert: bool,
    /// Invert images along with the rest, when inverting.
    invert_images: bool,
    follow: bool,
    /// Keep the editor's cursor in the middle of the view.
    center: bool,
    slots: Vec<Slot>,
    total_rows: usize,
    /// Width of the widest page, in cells.
    content_cols: usize,
    /// Rendered pages in the terminal's image store, by slot key.
    images: HashMap<u128, Image>,
    /// Bytes of images to keep in the store, past the ones on screen.
    image_budget: usize,
    next_id: u32,
    placed: HashSet<u32>,
    /// Images no page uses any more. They stay on screen until the next draw
    /// has placed their replacements, so a changed page never blinks out.
    stale: Vec<u32>,
    message: Option<(String, Instant)>,
    editor: EditorLink,
    watcher: notify::RecommendedWatcher,
    watched: HashSet<PathBuf>,
    deps: HashSet<PathBuf>,
    /// Paths that have an unsaved buffer in the compiler.
    overlaid: HashSet<PathBuf>,
    /// "Compiling…" is on screen.
    placeholder: bool,
    /// Rows above the status line that hold error text.
    error_rows: usize,
    dirty: bool,
}

impl App {
    fn view_rows(&self) -> usize {
        let errs = self.errors.len().min(3);
        (self.term.rows as usize).saturating_sub(1 + errs).max(1)
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    fn compile(&mut self) {
        program_status("working", &format!("Compiling {}", file_name(&self.main)));
        let start = Instant::now();
        let Warned { output, warnings } = typst::compile::<PagedDocument>(&self.world);
        self.compile_ms = start.elapsed().as_millis();
        let old: Vec<String> = self.errors.iter().chain(&self.warnings).map(|p| p.text.clone()).collect();
        self.warnings = warnings.iter().map(|d| self.describe(d)).collect();
        match output {
            Ok(doc) => {
                self.page_hashes = doc.pages().iter().map(hash128).collect();
                self.doc = Some(doc);
                self.errors.clear();
                let n = self.page_hashes.len();
                program_status("idle", &format!("{} · {n} page{}", file_name(&self.main), if n == 1 { "" } else { "s" }));
            }
            Err(errs) => {
                self.errors = errs.iter().filter(|d| d.severity == Severity::Error).map(|d| self.describe(d)).collect();
                program_status("error", &format!("Compile error in {}", file_name(&self.main)));
            }
        }
        // Start over at the first one when they change.
        if !self.errors.iter().chain(&self.warnings).map(|p| &p.text).eq(old.iter()) {
            self.next_problem = 0;
        }
        self.update_watches();
        self.relayout();
        self.dirty = true;
    }

    fn describe(&self, d: &SourceDiagnostic) -> Problem {
        let at = d.span.id().and_then(|id| {
            let path = self.world.path(id)?;
            let range = self.world.range(d.span)?;
            let source = self.world.source(id).ok()?;
            let lines = source.lines();
            let line = lines.byte_to_line(range.start)?;
            let col = range.start - lines.line_to_byte(line)?;
            Some((path, line + 1, col + 1))
        });
        let place = at.as_ref().map(|(path, line, _)| {
            let shown = path.strip_prefix(self.world.root()).unwrap_or(path).display().to_string();
            format!("{shown}:{line}: ")
        });
        Problem { text: format!("{}{}", place.unwrap_or_default(), d.message), at }
    }

    /// Jump the editor to the next error, or warning when there are no
    /// errors, going round.
    fn goto_problem(&mut self) {
        let list = if self.errors.is_empty() { &self.warnings } else { &self.errors };
        let located: Vec<(&(PathBuf, usize, usize), &str)> =
            list.iter().filter_map(|p| Some((p.at.as_ref()?, p.text.as_str()))).collect();
        if located.is_empty() {
            let msg = if list.is_empty() { "no errors or warnings" } else { "the errors aren't in a file" };
            self.notify(msg);
            return;
        }
        let i = self.next_problem % located.len();
        let ((path, line, col), text) = (located[i].0.clone(), located[i].1.to_string());
        let n = located.len();
        self.next_problem = i + 1;
        self.editor.goto(path, line, col);
        self.notify(format!("{}/{n} {text}", i + 1));
    }

    /// Watch the directories of the files the last compile read.
    fn update_watches(&mut self) {
        let deps: HashSet<PathBuf> = self.world.dependencies().into_iter().chain([self.main.clone()]).collect();
        let dirs: HashSet<PathBuf> = deps.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
        for d in dirs.difference(&self.watched) {
            let _ = self.watcher.watch(d, RecursiveMode::NonRecursive);
        }
        for d in self.watched.difference(&dirs) {
            let _ = self.watcher.unwatch(d);
        }
        self.watched = dirs;
        self.deps = deps;
    }

    /// Pixels per point that fit the pages in the pane, and the most the
    /// zoom can be.
    fn fit_scale(&self) -> Option<(f64, f64)> {
        let doc = self.doc.as_ref()?;
        let t = self.term;
        let avail_w = (t.cols.saturating_sub(2) as u32 * t.cw).max(t.cw) as f64;
        let avail_h = (self.view_rows().saturating_sub(1) as u32 * t.ch).max(t.ch) as f64;
        let max_w = doc.pages().iter().map(|p| p.frame.width().to_pt()).fold(1.0, f64::max);
        let max_h = doc.pages().iter().map(|p| p.frame.height().to_pt()).fold(1.0, f64::max);
        let fit = match self.fit {
            Fit::Width => avail_w / max_w,
            Fit::Page => (avail_w / max_w).min(avail_h / max_h),
        }
        .max(0.05);
        let max_zoom = MAX_ZOOM.min(MAX_PAGE_PX / (fit * max_w)).max(1.0);
        Some((fit, max_zoom))
    }

    fn relayout(&mut self) {
        self.slots.clear();
        self.total_rows = 0;
        self.content_cols = 0;
        let Some((fit, max_zoom)) = self.fit_scale() else { return };
        self.zoom = self.zoom.clamp(MIN_ZOOM, max_zoom);
        let scale = fit * self.zoom;
        let Some(doc) = &self.doc else { return };
        let t = self.term;
        let mut top = 0;
        for (i, page) in doc.pages().iter().enumerate() {
            let w = (page.frame.width().to_pt() * scale).ceil() as u32;
            let h = (page.frame.height().to_pt() * scale).ceil() as u32;
            let cols = w.div_ceil(t.cw).clamp(1, u16::MAX as u32) as u16;
            let rows = h.div_ceil(t.ch).clamp(1, u16::MAX as u32) as u16;
            let key = hash128(&(self.page_hashes[i], scale.to_bits(), self.invert, self.invert_images, t.cw, t.ch));
            self.slots.push(Slot { top, cols, rows, col: 0, scale, key });
            top += rows as usize + GAP;
        }
        self.total_rows = top.saturating_sub(GAP);
        // Center each page in the column of pages.
        self.content_cols = self.slots.iter().map(|s| s.cols as usize).max().unwrap_or(0);
        for s in &mut self.slots {
            s.col = ((self.content_cols - s.cols as usize) / 2) as u16;
        }
        // Images no page uses any more: draw frees them once their
        // replacements are on screen.
        let keys: HashSet<u128> = self.slots.iter().map(|s| s.key).collect();
        let stale = &mut self.stale;
        self.images.retain(|k, img| {
            let keep = keys.contains(k);
            if !keep {
                stale.push(img.id);
            }
            keep
        });
        self.placed.retain(|id| self.images.values().any(|v| v.id == *id));
        self.clamp_scroll();
    }

    fn max_scroll(&self) -> usize {
        self.total_rows.saturating_sub(self.view_rows())
    }

    fn max_hscroll(&self) -> usize {
        self.content_cols.saturating_sub(self.term.cols as usize)
    }

    /// Screen column of the left edge of the column of pages: centered when
    /// the pages fit, else scrolled.
    fn x_origin(&self) -> isize {
        let cols = self.term.cols as usize;
        if self.content_cols <= cols { ((cols - self.content_cols) / 2) as isize } else { -(self.hscroll as isize) }
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
        self.hscroll = self.hscroll.min(self.max_hscroll());
    }

    fn hscroll_by(&mut self, delta: isize) {
        self.hscroll = self.hscroll.saturating_add_signed(delta);
        self.clamp_scroll();
        self.dirty = true;
    }

    /// Zoom, keeping the middle of the view where it is.
    fn set_zoom(&mut self, zoom: f64) {
        let Some((_, max_zoom)) = self.fit_scale() else { return };
        let zoom = zoom.clamp(MIN_ZOOM, max_zoom);
        if (zoom - self.zoom).abs() < 1e-6 {
            self.notify(format!("zoom {}%", (self.zoom * 100.0).round()));
            return;
        }
        let view = self.view_rows();
        let cols = self.term.cols as usize;
        let mid = self.scroll + view / 2;
        let anchor = self.slots.iter().rposition(|s| s.top <= mid).map(|i| {
            let s = &self.slots[i];
            (i, ((mid - s.top) as f64 / s.rows as f64).min(1.0))
        });
        let fx = if self.content_cols > cols {
            (self.hscroll + cols / 2) as f64 / self.content_cols as f64
        } else {
            0.5
        };
        self.zoom = zoom;
        self.relayout();
        if let Some((s, fy)) = anchor.and_then(|(i, fy)| Some((self.slots.get(i)?, fy))) {
            self.scroll = (s.top as f64 + fy * s.rows as f64 - (view / 2) as f64).max(0.0) as usize;
        }
        self.hscroll = (fx * self.content_cols as f64 - (cols / 2) as f64).max(0.0) as usize;
        self.clamp_scroll();
        self.notify(format!("zoom {}%", (self.zoom * 100.0).round()));
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
        self.dirty = true;
    }

    /// The page in view, 0-based: the last one starting above a third of the
    /// way down the view.
    fn current_page(&self) -> usize {
        let probe = self.scroll + self.view_rows() / 3;
        self.slots.iter().rposition(|s| s.top <= probe).unwrap_or(0)
    }

    fn goto_page(&mut self, page: usize) {
        if let Some(s) = self.slots.get(page) {
            self.scroll = s.top;
            self.clamp_scroll();
            self.dirty = true;
        }
    }

    /// Scroll so a position in the document is in view, if it isn't, or
    /// always with `force`. In center mode it goes to the middle of the view.
    fn reveal(&mut self, pos: &PagedPosition, force: bool) {
        let Some(slot) = self.slots.get(pos.page.get() - 1) else { return };
        let row = slot.top + (pos.point.y.to_pt() * slot.scale / self.term.ch as f64) as usize;
        let col = slot.col as usize + (pos.point.x.to_pt() * slot.scale / self.term.cw as f64) as usize;
        let force = force || self.center;
        let view = self.view_rows();
        if force || row < self.scroll || row >= self.scroll + view {
            let anchor = if self.center { view / 2 } else { view / 3 };
            self.scroll = row.saturating_sub(anchor);
            self.dirty = true;
        }
        let cols = self.term.cols as usize;
        if self.content_cols > cols && (force || col < self.hscroll || col >= self.hscroll + cols) {
            self.hscroll = col.saturating_sub(cols / 2);
            self.dirty = true;
        }
        self.clamp_scroll();
    }

    /// Render a page into the terminal's image store, if it isn't there.
    fn ensure_image(&mut self, out: &mut Vec<u8>, i: usize) -> Option<u32> {
        let slot = &self.slots[i];
        if let Some(img) = self.images.get(&slot.key) {
            return Some(img.id);
        }
        let page = self.doc.as_ref()?.pages().get(i)?;
        let opts = typst_render::RenderOptions { pixel_per_pt: Scalar::new(slot.scale), render_bleed: false };
        let pixmap = typst_render::render(page, &opts);
        // Pad to whole cells, so page rows line up with terminal rows.
        let (pw, ph) = (pixmap.width(), pixmap.height());
        let w = slot.cols as u32 * self.term.cw;
        let h = slot.rows as u32 * self.term.ch;
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        let src = pixmap.data();
        // Pixels under images, which keep their colors in a dark preview.
        let keep = (self.invert && !self.invert_images).then(|| image_mask(&page.frame, slot.scale, pw, ph)).flatten();
        for y in 0..ph.min(h) {
            for x in 0..pw.min(w) {
                let s = ((y * pw + x) * 4) as usize;
                let d = ((y * w + x) * 3) as usize;
                let invert = self.invert && !keep.as_ref().is_some_and(|m| m[(y * pw + x) as usize]);
                // Premultiplied alpha: put it over white, then maybe invert.
                let a = src[s + 3];
                for c in 0..3 {
                    let v = src[s + c].saturating_add(255 - a);
                    rgb[d + c] = if invert { 255 - v } else { v };
                }
            }
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        kitty::upload(out, id, w, h, &rgb).ok()?;
        self.images.insert(slot.key, Image { id, bytes: rgb.len() });
        Some(id)
    }

    /// Free the pages furthest from the view while the store holds more than
    /// the budget. Terminals limit how much they keep, Rex's iPad app among
    /// them, so a long document can't hold every page it has shown.
    fn evict(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let mut total: usize = self.images.values().map(|i| i.bytes).sum();
        if total <= self.image_budget {
            return Ok(());
        }
        let (top, bottom) = (self.scroll, self.scroll + self.view_rows());
        let mut far: Vec<(usize, u128)> = self
            .slots
            .iter()
            .filter(|s| self.images.get(&s.key).is_some_and(|i| !self.placed.contains(&i.id)))
            .map(|s| {
                let end = s.top + s.rows as usize;
                (if end <= top { top - end } else { s.top.saturating_sub(bottom) }, s.key)
            })
            .collect();
        far.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, key) in far {
            if total <= self.image_budget {
                break;
            }
            if let Some(img) = self.images.remove(&key) {
                kitty::delete(out, img.id)?;
                total -= img.bytes;
            }
        }
        Ok(())
    }

    fn draw(&mut self) -> io::Result<()> {
        self.dirty = false;
        // Synchronized output: the terminal shows the whole frame at once.
        // Terminals without it ignore the sequence.
        let mut out = b"\x1b[?2026h".to_vec();
        let t = self.term;
        let view = self.view_rows();
        let origin = self.x_origin();
        let mut shown = HashSet::new();
        for i in 0..self.slots.len() {
            let (top, rows) = (self.slots[i].top, self.slots[i].rows as usize);
            let start = top.max(self.scroll);
            let end = (top + rows).min(self.scroll + view);
            // Screen columns the page covers, cut to the pane.
            let left = origin + self.slots[i].col as isize;
            let right = left + self.slots[i].cols as isize;
            let (vl, vr) = (left.max(0), right.min(t.cols as isize));
            if start >= end || vl >= vr {
                continue;
            }
            let Some(id) = self.ensure_image(&mut out, i) else { continue };
            kitty::place(&mut out, &kitty::Placement {
                id,
                col: vl as u16,
                row: (start - self.scroll) as u16,
                x: ((vl - left) as u32) * t.cw,
                y: ((start - top) as u32) * t.ch,
                w: ((vr - vl) as u32) * t.cw,
                h: ((end - start) as u32) * t.ch,
                cols: (vr - vl) as u16,
                rows: (end - start) as u16,
            })?;
            shown.insert(id);
        }
        for id in self.placed.difference(&shown) {
            kitty::hide(&mut out, *id)?;
        }
        self.placed = shown;
        // The new pages are placed, so the old ones can go.
        for id in self.stale.drain(..) {
            kitty::delete(&mut out, id)?;
        }
        self.evict(&mut out)?;

        // Errors above the status line, then the status line. Clear the rows
        // the last draw's errors used first.
        for k in 0..self.error_rows {
            write!(out, "\x1b[{};1H\x1b[2K", (t.rows as usize).saturating_sub(1 + k))?;
        }
        self.error_rows = self.errors.len().min(3);
        for (k, e) in self.errors.iter().take(3).enumerate() {
            let row = view + k + 1;
            write!(out, "\x1b[{row};1H\x1b[2K\x1b[31m{}\x1b[0m", clip(&e.text, t.cols as usize))?;
        }
        let status = self.status_line();
        write!(out, "\x1b[{};1H\x1b[2K{status}", t.rows)?;
        if self.doc.is_none() && self.errors.is_empty() {
            write!(out, "\x1b[1;1H\x1b[2mCompiling…\x1b[0m")?;
            self.placeholder = true;
        } else if self.placeholder {
            write!(out, "\x1b[1;1H\x1b[2K")?;
            self.placeholder = false;
        }
        out.extend_from_slice(b"\x1b[?2026l");
        let mut stdout = io::stdout().lock();
        stdout.write_all(&out)?;
        stdout.flush()
    }

    fn status_line(&mut self) -> String {
        let cols = self.term.cols as usize;
        let n = self.slots.len();
        let left = if n > 0 {
            format!(" {}  {}/{n}", file_name(&self.main), self.current_page() + 1)
        } else {
            format!(" {}", file_name(&self.main))
        };
        let left_len = left.chars().count();
        if let Some((msg, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(4) {
                return format!("\x1b[2m{left}\x1b[0m  {}", clip(msg, cols.saturating_sub(left_len + 2)));
            }
            self.message = None;
        }
        let (state, state_len) = if !self.errors.is_empty() {
            let s = format!("✗ {} error{}", self.errors.len(), if self.errors.len() == 1 { "" } else { "s" });
            let l = s.chars().count();
            (format!("\x1b[31m{s}\x1b[0m"), l)
        } else if !self.warnings.is_empty() {
            let s = format!("⚠ {}  {}ms", self.warnings.len(), self.compile_ms);
            let l = s.chars().count();
            (format!("\x1b[33m{s}\x1b[0m"), l)
        } else {
            let s = format!("✓ {}ms", self.compile_ms);
            let l = s.chars().count();
            (format!("\x1b[32m{s}\x1b[0m"), l)
        };
        let mut flags = String::new();
        if self.fit == Fit::Page {
            flags.push_str("  page");
        }
        if (self.zoom - 1.0).abs() > 1e-6 {
            flags.push_str(&format!("  {}%", (self.zoom * 100.0).round()));
        }
        if self.center {
            flags.push_str("  center");
        }
        if self.invert {
            flags.push_str("  dark");
        }
        if !self.follow {
            flags.push_str("  nofollow");
        }
        let used = left_len + 2 + state_len + flags.chars().count();
        let hint = [
            "  j/k scroll  +/- zoom  n/p page  w fit  c center  i dark  f follow  q quit",
            "  j/k scroll  +/- zoom  n/p page  q quit",
            "",
        ]
        .into_iter()
        .find(|h| used + h.len() <= cols)
        .unwrap_or("");
        format!("\x1b[2m{left}\x1b[0m  {state}\x1b[2m{flags}{hint}\x1b[0m")
    }

    fn click(&mut self, col: u16, row: u16) {
        let Some(doc) = &self.doc else { return };
        let doc_row = self.scroll + row as usize;
        let doc_col = col as isize - self.x_origin();
        let Some(i) = self.slots.iter().position(|s| s.top <= doc_row && doc_row < s.top + s.rows as usize) else {
            return;
        };
        let s = &self.slots[i];
        let page_col = doc_col - s.col as isize;
        if page_col < 0 || page_col >= s.cols as isize {
            return;
        }
        let x = ((page_col as u32 * self.term.cw) as f64 + self.term.cw as f64 / 2.0) / s.scale;
        let y = (((doc_row - s.top) as u32 * self.term.ch) as f64 + self.term.ch as f64 / 2.0) / s.scale;
        let pos = PagedPosition { page: NonZeroUsize::new(i + 1).unwrap(), point: Point::new(Abs::pt(x), Abs::pt(y)) };
        match typst_ide::jump_from_click(&self.world, doc, &pos) {
            Some(Jump::File(id, offset)) => {
                let (Some(path), Ok(source)) = (self.world.path(id), self.world.source(id)) else { return };
                let lines = source.lines();
                let line = lines.byte_to_line(offset).unwrap_or(0);
                let col = offset - lines.line_to_byte(line).unwrap_or(0);
                self.editor.goto(path, line + 1, col + 1);
            }
            Some(Jump::Position(p)) => self.reveal(&p, true),
            Some(Jump::Url(url)) => self.notify(format!("link: {}", url.as_str())),
            None => {}
        }
    }

    /// Scroll to where the editor's cursor is in the document.
    fn on_cursor(&mut self, path: &Path, line: usize, col: usize) {
        let (Some(doc), Some(id)) = (&self.doc, self.world.id(path)) else { return };
        let Ok(source) = self.world.source(id) else { return };
        let lines = source.lines();
        let Some(start) = lines.line_to_byte(line.saturating_sub(1)) else { return };
        let byte = (start + col.saturating_sub(1)).min(source.text().len());
        if let Some(pos) = typst_ide::jump_from_cursor(doc, &source, byte).into_iter().next() {
            self.reveal(&pos, false);
        }
    }

    /// Returns false to quit.
    fn key(&mut self, k: KeyEvent) -> bool {
        if k.kind == KeyEventKind::Release {
            return true;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.view_rows() / 2).max(1) as isize;
        let full = self.view_rows().saturating_sub(2).max(1) as isize;
        let hstep = (self.term.cols as isize / 8).max(2);
        let wide = self.content_cols > self.term.cols as usize;
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Char('d') if ctrl => self.scroll_by(half),
            KeyCode::Char('u') if ctrl => self.scroll_by(-half),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(3),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-3),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_by(full),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_by(-full),
            KeyCode::Char('g') | KeyCode::Home => self.goto_page(0),
            KeyCode::Char('G') | KeyCode::End => {
                self.scroll = self.max_scroll();
                self.dirty = true;
            }
            // Left and right scroll sideways when zoomed pages are wider
            // than the pane, else turn pages.
            KeyCode::Char('h') => self.hscroll_by(-hstep),
            KeyCode::Char('l') => self.hscroll_by(hstep),
            KeyCode::Left if wide => self.hscroll_by(-hstep),
            KeyCode::Right if wide => self.hscroll_by(hstep),
            KeyCode::Char('n') | KeyCode::Char('J') | KeyCode::Right => self.goto_page(self.current_page() + 1),
            KeyCode::Char('p') | KeyCode::Char('K') | KeyCode::Left => {
                self.goto_page(self.current_page().saturating_sub(1))
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.set_zoom(self.zoom * ZOOM_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.set_zoom(self.zoom / ZOOM_STEP),
            KeyCode::Char('0') => self.set_zoom(1.0),
            KeyCode::Char('w') => {
                let page = self.current_page();
                self.fit = if self.fit == Fit::Width { Fit::Page } else { Fit::Width };
                self.zoom = 1.0;
                self.hscroll = 0;
                self.relayout();
                self.goto_page(page);
            }
            KeyCode::Char('e') => self.goto_problem(),
            KeyCode::Char('c') => {
                self.center = !self.center;
                self.notify(if self.center { "keeping the cursor centered" } else { "not centering the cursor" });
            }
            KeyCode::Char('i') => {
                self.invert = !self.invert;
                self.relayout();
                self.dirty = true;
            }
            KeyCode::Char('I') => {
                self.invert_images = !self.invert_images;
                if self.invert {
                    self.relayout();
                }
                self.notify(if self.invert_images {
                    "inverting images in the dark preview"
                } else {
                    "images keep their colors in the dark preview"
                });
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                self.notify(if self.follow { "following the cursor" } else { "not following the cursor" });
            }
            // Recompile, and send every page to the terminal again, in case
            // it dropped some.
            KeyCode::Char('r') => {
                self.drop_images();
                self.world.reset();
                self.compile();
            }
            _ => {}
        }
        true
    }

    /// Forget every page in the terminal's image store and clear the screen,
    /// so the next draw renders and uploads them again.
    fn drop_images(&mut self) {
        let mut out = Vec::new();
        let _ = kitty::delete_all(&mut out);
        let _ = io::stdout().write_all(&out);
        self.images.clear();
        self.placed.clear();
        self.stale.clear();
        self.error_rows = 0;
        let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::All));
        self.dirty = true;
    }

    fn resize(&mut self) {
        let page = self.current_page();
        let at_top = self.scroll == 0;
        self.term = Term::query();
        self.drop_images();
        self.relayout();
        if !at_top {
            self.goto_page(page);
        }
        self.dirty = true;
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Rex's program status (OSC 7501): shows whether the document compiles.
fn program_status(state: &str, msg: &str) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(msg);
    let mut out = io::stdout().lock();
    let _ = write!(out, "\x1b]7501;state={state}:app=typst:msg={b64}\x1b\\");
    let _ = out.flush();
}

/// A rectangle on the page in points: left, top, right, bottom.
type Rect = [f64; 4];

/// Which pixels of a page rendered at `scale` lie under an image, or None if
/// it has no images.
fn image_mask(frame: &Frame, scale: f64, pw: u32, ph: u32) -> Option<Vec<bool>> {
    let mut rects = Vec::new();
    image_rects(frame, Transform::identity(), None, &mut rects);
    if rects.is_empty() {
        return None;
    }
    let mut mask = vec![false; (pw * ph) as usize];
    for [l, t, r, b] in rects {
        let px = |v: f64, max: u32| ((v * scale).round().max(0.0) as u32).min(max);
        let (x0, x1, y0, y1) = (px(l, pw), px(r, pw), px(t, ph), px(b, ph));
        for y in y0..y1 {
            mask[(y * pw + x0) as usize..(y * pw + x1) as usize].fill(true);
        }
    }
    Some(mask)
}

/// The bounding boxes of a frame's images on the page, cut to the clipped
/// groups they're in. A rotated image counts as its whole bounding box.
fn image_rects(frame: &Frame, ts: Transform, clip: Option<Rect>, out: &mut Vec<Rect>) {
    let bbox = |ts: Transform, size: Size| -> Rect {
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(fx, fy)| {
            Point::new(size.x * fx, size.y * fy).transform(ts)
        });
        let xs = corners.map(|p| p.x.to_pt());
        let ys = corners.map(|p| p.y.to_pt());
        let fold = |v: [f64; 4], f: fn(f64, f64) -> f64| v.into_iter().reduce(f).unwrap_or(0.0);
        [fold(xs, f64::min), fold(ys, f64::min), fold(xs, f64::max), fold(ys, f64::max)]
    };
    let cut = |r: Rect, c: Option<Rect>| match c {
        Some(c) => [r[0].max(c[0]), r[1].max(c[1]), r[2].min(c[2]), r[3].min(c[3])],
        None => r,
    };
    for (pos, item) in frame.items() {
        let at = ts.pre_concat(Transform::translate(pos.x, pos.y));
        match item {
            FrameItem::Group(group) => {
                let inner = at.pre_concat(group.transform);
                let clip = match group.clip {
                    Some(_) => Some(cut(bbox(inner, group.frame.size()), clip)),
                    None => clip,
                };
                image_rects(&group.frame, inner, clip, out);
            }
            FrameItem::Image(_, size, _) => {
                let r = cut(bbox(at, *size), clip);
                if r[0] < r[2] && r[1] < r[3] {
                    out.push(r);
                }
            }
            _ => {}
        }
    }
}

fn pick_file(args: &Args, editor: &mut Editor) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Some(f) = &args.file {
        return Ok(cwd.join(f));
    }
    if let Some(p) = editor.buffer_path().filter(|p| p.extension().is_some_and(|e| e == "typ")) {
        return Ok(p);
    }
    if let Some(p) = editor.argv_file() {
        return Ok(p);
    }
    if cwd.join("main.typ").is_file() {
        return Ok(cwd.join("main.typ"));
    }
    let typs: Vec<PathBuf> = std::fs::read_dir(&cwd)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "typ") && p.is_file())
        .collect();
    match typs.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("No Typst file found in {}. Open one in the editor, or pass a file.", cwd.display()),
        _ => bail!("Several .typ files in {} and no main.typ. Pass the file to preview.", cwd.display()),
    }
}

fn main() {
    let args = Args::parse();
    if let Err(err) = start(&args) {
        restore();
        eprintln!("\x1b[31mrex-typst: {err:#}\x1b[0m");
        // In a pane of its own, keep the reason on screen until a key. Any
        // key, so the q that typst_close sends closes it too.
        if std::io::IsTerminal::is_terminal(&io::stdin()) && args.editor_block.is_some() {
            eprintln!("Press any key to close.");
            if terminal::enable_raw_mode().is_ok() {
                while let Ok(ev) = event::read() {
                    if matches!(ev, Event::Key(k) if k.kind != KeyEventKind::Release) {
                        break;
                    }
                }
                let _ = terminal::disable_raw_mode();
            }
        }
        std::process::exit(1);
    }
}

fn start(args: &Args) -> anyhow::Result<()> {
    let mut editor = Editor::new(args.editor_block.clone(), args.nvim.clone(), !args.no_focus_editor);
    let open = pick_file(args, &mut editor)?.canonicalize().context("the Typst file doesn't exist")?;
    let root = match &args.root {
        Some(r) => r.canonicalize()?,
        None => world::find_root(&open).canonicalize()?,
    };
    // A chapter previews the whole document it's in, unless asked for itself.
    let main = if args.file.is_some() { open.clone() } else { project::main_of(&root, &open) };
    let world = TypstWorld::new(root, &main)?;
    let (editor, editor_rx) = editor.spawn();

    let (fs_tx, fs_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let watcher = notify::recommended_watcher(fs_tx)?;

    // Leave the terminal as we found it, even on a panic.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide, event::EnableMouseCapture)?;
    // No autowrap: a line that runs past the last column of the last row
    // would scroll the screen, and the pages with it.
    print!("\x1b[?7l\x1b]2;typst · {}\x1b\\", file_name(&main));

    let mut app = App {
        world,
        main,
        doc: None,
        page_hashes: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        next_problem: 0,
        compile_ms: 0,
        term: Term::query(),
        scroll: 0,
        hscroll: 0,
        fit: if args.fit_page { Fit::Page } else { Fit::Width },
        zoom: 1.0,
        invert: args.invert,
        invert_images: args.invert_images,
        follow: !args.no_follow,
        center: args.center,
        slots: Vec::new(),
        total_rows: 0,
        content_cols: 0,
        images: HashMap::new(),
        image_budget: args.image_mb.saturating_mul(1 << 20),
        next_id: 0,
        placed: HashSet::new(),
        stale: Vec::new(),
        message: None,
        editor,
        watcher,
        watched: HashSet::new(),
        deps: HashSet::new(),
        overlaid: HashSet::new(),
        placeholder: false,
        error_rows: 0,
        dirty: true,
    };
    if app.main != open {
        app.notify(format!("previewing {}, which includes {}", file_name(&app.main), file_name(&open)));
    }
    let result = run(&mut app, &editor_rx, &fs_rx);
    restore();
    program_status("clear", "");
    result
}

fn run(
    app: &mut App,
    editor_rx: &Receiver<FromEditor>,
    fs_rx: &Receiver<notify::Result<notify::Event>>,
) -> anyhow::Result<()> {
    app.draw()?;
    app.compile();
    let mut changed_at: Option<Instant> = None;
    loop {
        if event::poll(Duration::from_millis(25))? {
            match event::read()? {
                Event::Key(k) => {
                    if !app.key(k) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => match m.kind {
                    // Ctrl-wheel zooms; shift-wheel scrolls sideways.
                    MouseEventKind::ScrollUp if m.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.set_zoom(app.zoom * ZOOM_STEP)
                    }
                    MouseEventKind::ScrollDown if m.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.set_zoom(app.zoom / ZOOM_STEP)
                    }
                    MouseEventKind::ScrollUp if m.modifiers.contains(KeyModifiers::SHIFT) => app.hscroll_by(-3),
                    MouseEventKind::ScrollDown if m.modifiers.contains(KeyModifiers::SHIFT) => app.hscroll_by(3),
                    MouseEventKind::ScrollLeft => app.hscroll_by(-3),
                    MouseEventKind::ScrollRight => app.hscroll_by(3),
                    MouseEventKind::ScrollDown => app.scroll_by(3),
                    MouseEventKind::ScrollUp => app.scroll_by(-3),
                    MouseEventKind::Down(MouseButton::Left) => app.click(m.column, m.row),
                    _ => {}
                },
                Event::Resize(..) => app.resize(),
                _ => {}
            }
        }

        // Unsaved edits, saves, and cursor moves from Neovim. Take them all,
        // then compile once.
        let mut cursor = None;
        let mut edited = false;
        while let Ok(msg) = editor_rx.try_recv() {
            match msg {
                FromEditor::Status(s) => app.notify(s),
                FromEditor::Clean(path) => edited |= app.overlaid.remove(&path) && app.world.clear_overlay(&path),
                FromEditor::Gone => {
                    for path in std::mem::take(&mut app.overlaid) {
                        edited |= app.world.clear_overlay(&path);
                    }
                }
                FromEditor::Snapshot(s) => {
                    if s.path.extension().is_some_and(|e| e == "typ") && app.world.id(&s.path).is_some() {
                        edited |= match (s.modified, s.text) {
                            (true, Some(text)) => {
                                app.overlaid.insert(s.path.clone());
                                app.world.set_overlay(s.path.clone(), text)
                            }
                            (false, _) => app.overlaid.remove(&s.path) && app.world.clear_overlay(&s.path),
                            _ => false,
                        };
                        // Don't scroll the preview back to where it sent us.
                        cursor = (!s.jumped).then_some((s.path, s.line, s.col));
                    }
                }
            }
        }
        if edited {
            app.world.reset();
            app.compile();
        }
        if let Some((path, line, col)) = cursor {
            if app.follow {
                app.on_cursor(&path, line, col);
            }
        }

        // Saved files.
        while let Ok(ev) = fs_rx.try_recv() {
            if let Ok(ev) = ev {
                if ev.paths.iter().any(|p| app.deps.contains(p)) {
                    changed_at = Some(Instant::now());
                }
            }
        }
        // Let a burst of writes settle before compiling.
        if changed_at.is_some_and(|t| t.elapsed() > Duration::from_millis(40)) {
            changed_at = None;
            app.world.reset();
            app.compile();
        }

        if app.message.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4)) {
            app.dirty = true;
        }
        if app.dirty {
            app.draw()?;
        }
    }
}

fn restore() {
    let mut out = Vec::new();
    let _ = kitty::delete_all(&mut out);
    let mut stdout = io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.write_all(b"\x1b[?7h");
    let _ = execute!(stdout, event::DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}
