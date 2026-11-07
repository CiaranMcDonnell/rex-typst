//! rex-typst: a live Typst preview drawn in a Rex pane with kitty graphics.

mod editor;
mod kitty;
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
use typst::layout::{Abs, Point};
use typst::utils::{Scalar, hash128};
use typst_ide::Jump;
use typst_layout::PagedDocument;

use editor::{Editor, FromEditor, ToEditor};
use world::TypstWorld;

#[derive(Parser)]
#[command(version, about = "Live Typst preview in a Rex pane, drawn with kitty graphics")]
struct Args {
    /// The file to preview. Default: the .typ file open in the editor pane,
    /// else main.typ or the only .typ file in the current directory.
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
    /// Don't scroll the preview to the editor's cursor.
    #[arg(long)]
    no_follow: bool,
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
    /// Screen column of its left edge.
    col: u16,
    /// Pixels per point.
    scale: f64,
    /// Identifies the rendered image: the page's content, scale and colors.
    key: u128,
}

/// Gap between pages, in rows.
const GAP: usize = 1;

struct App {
    world: TypstWorld,
    main: PathBuf,
    doc: Option<PagedDocument>,
    page_hashes: Vec<u128>,
    errors: Vec<String>,
    warnings: usize,
    compile_ms: u128,
    term: Term,
    scroll: usize,
    fit: Fit,
    invert: bool,
    follow: bool,
    slots: Vec<Slot>,
    total_rows: usize,
    /// Rendered pages in the terminal's image store, by slot key.
    images: HashMap<u128, u32>,
    next_id: u32,
    placed: HashSet<u32>,
    /// Images no page uses any more. They stay on screen until the next draw
    /// has placed their replacements, so a changed page never blinks out.
    stale: Vec<u32>,
    message: Option<(String, Instant)>,
    editor_tx: mpsc::Sender<ToEditor>,
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
        self.warnings = warnings.len();
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
        self.update_watches();
        self.relayout();
        self.dirty = true;
    }

    fn describe(&self, d: &SourceDiagnostic) -> String {
        let place = d.span.id().and_then(|id| {
            let path = self.world.path(id)?;
            let range = self.world.range(d.span)?;
            let line = self.world.source(id).ok()?.lines().byte_to_line(range.start)? + 1;
            let shown = path.strip_prefix(self.world.root()).unwrap_or(&path).display().to_string();
            Some(format!("{shown}:{line}: "))
        });
        format!("{}{}", place.unwrap_or_default(), d.message)
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

    fn relayout(&mut self) {
        self.slots.clear();
        self.total_rows = 0;
        let Some(doc) = &self.doc else { return };
        let t = self.term;
        let avail_w = (t.cols.saturating_sub(2) as u32 * t.cw).max(t.cw) as f64;
        let avail_h = (self.view_rows().saturating_sub(1) as u32 * t.ch).max(t.ch) as f64;
        let max_w = doc.pages().iter().map(|p| p.frame.width().to_pt()).fold(1.0, f64::max);
        let max_h = doc.pages().iter().map(|p| p.frame.height().to_pt()).fold(1.0, f64::max);
        let scale = match self.fit {
            Fit::Width => avail_w / max_w,
            Fit::Page => (avail_w / max_w).min(avail_h / max_h),
        }
        .max(0.05);
        let mut top = 0;
        for (i, page) in doc.pages().iter().enumerate() {
            let w = (page.frame.width().to_pt() * scale).ceil() as u32;
            let h = (page.frame.height().to_pt() * scale).ceil() as u32;
            let cols = w.div_ceil(t.cw).clamp(1, t.cols as u32) as u16;
            let rows = h.div_ceil(t.ch).max(1) as u16;
            let key = hash128(&(self.page_hashes[i], scale.to_bits(), self.invert, t.cw, t.ch));
            self.slots.push(Slot { top, cols, rows, col: (t.cols - cols) / 2, scale, key });
            top += rows as usize + GAP;
        }
        self.total_rows = top.saturating_sub(GAP);
        // Images no page uses any more: draw frees them once their
        // replacements are on screen.
        let keys: HashSet<u128> = self.slots.iter().map(|s| s.key).collect();
        let stale = &mut self.stale;
        self.images.retain(|k, id| {
            let keep = keys.contains(k);
            if !keep {
                stale.push(*id);
            }
            keep
        });
        self.placed.retain(|id| self.images.values().any(|v| v == id));
        self.clamp_scroll();
    }

    fn max_scroll(&self) -> usize {
        self.total_rows.saturating_sub(self.view_rows())
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
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

    /// Scroll so a position in the document is in view, if it isn't.
    fn reveal(&mut self, pos: &PagedPosition, center: bool) {
        let Some(slot) = self.slots.get(pos.page.get() - 1) else { return };
        let row = slot.top + (pos.point.y.to_pt() * slot.scale / self.term.ch as f64) as usize;
        let view = self.view_rows();
        if center || row < self.scroll || row >= self.scroll + view {
            self.scroll = row.saturating_sub(view / 3);
            self.clamp_scroll();
            self.dirty = true;
        }
    }

    /// Render a page into the terminal's image store, if it isn't there.
    fn ensure_image(&mut self, out: &mut Vec<u8>, i: usize) -> Option<u32> {
        let slot = &self.slots[i];
        if let Some(&id) = self.images.get(&slot.key) {
            return Some(id);
        }
        let page = self.doc.as_ref()?.pages().get(i)?;
        let opts = typst_render::RenderOptions { pixel_per_pt: Scalar::new(slot.scale), render_bleed: false };
        let pixmap = typst_render::render(page, &opts);
        // Pad to whole cells, so page rows line up with terminal rows.
        let (pw, ph) = (pixmap.width(), pixmap.height());
        let w = slot.cols as u32 * self.term.cw;
        let h = slot.rows as u32 * self.term.ch;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let src = pixmap.data();
        for y in 0..ph.min(h) {
            for x in 0..pw.min(w) {
                let s = ((y * pw + x) * 4) as usize;
                let d = ((y * w + x) * 4) as usize;
                // Premultiplied alpha: put it over white, then maybe invert.
                let a = src[s + 3];
                for c in 0..3 {
                    let v = src[s + c].saturating_add(255 - a);
                    rgba[d + c] = if self.invert { 255 - v } else { v };
                }
                rgba[d + 3] = 255;
            }
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        kitty::upload(out, id, w, h, &rgba).ok()?;
        self.images.insert(slot.key, id);
        Some(id)
    }

    fn draw(&mut self) -> io::Result<()> {
        self.dirty = false;
        // Synchronized output: the terminal shows the whole frame at once.
        // Terminals without it ignore the sequence.
        let mut out = b"\x1b[?2026h".to_vec();
        let t = self.term;
        let view = self.view_rows();
        let mut shown = HashSet::new();
        for i in 0..self.slots.len() {
            let (top, rows) = (self.slots[i].top, self.slots[i].rows as usize);
            let start = top.max(self.scroll);
            let end = (top + rows).min(self.scroll + view);
            if start >= end {
                continue;
            }
            let Some(id) = self.ensure_image(&mut out, i) else { continue };
            let s = &self.slots[i];
            kitty::place(&mut out, &kitty::Placement {
                id,
                col: s.col,
                row: (start - self.scroll) as u16,
                x: 0,
                y: ((start - top) as u32) * t.ch,
                w: s.cols as u32 * t.cw,
                h: ((end - start) as u32) * t.ch,
                cols: s.cols,
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

        // Errors above the status line, then the status line. Clear the rows
        // the last draw's errors used first.
        for k in 0..self.error_rows {
            write!(out, "\x1b[{};1H\x1b[2K", (t.rows as usize).saturating_sub(1 + k))?;
        }
        self.error_rows = self.errors.len().min(3);
        for (k, e) in self.errors.iter().take(3).enumerate() {
            let row = view + k + 1;
            write!(out, "\x1b[{row};1H\x1b[2K\x1b[31m{}\x1b[0m", clip(e, t.cols as usize))?;
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
        } else if self.warnings > 0 {
            let s = format!("⚠ {}  {}ms", self.warnings, self.compile_ms);
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
        if self.invert {
            flags.push_str("  dark");
        }
        if !self.follow {
            flags.push_str("  nofollow");
        }
        let hint = "  j/k scroll  n/p page  w fit  i dark  f follow  q quit";
        let used = left_len + 2 + state_len + flags.len();
        let hint = if used + hint.len() <= cols { hint } else { "" };
        format!("\x1b[2m{left}\x1b[0m  {state}\x1b[2m{flags}{hint}\x1b[0m")
    }

    fn click(&mut self, col: u16, row: u16) {
        let Some(doc) = &self.doc else { return };
        let doc_row = self.scroll + row as usize;
        let Some(i) = self.slots.iter().position(|s| s.top <= doc_row && doc_row < s.top + s.rows as usize) else {
            return;
        };
        let s = &self.slots[i];
        if col < s.col || col >= s.col + s.cols {
            return;
        }
        let x = (((col - s.col) as u32 * self.term.cw) as f64 + self.term.cw as f64 / 2.0) / s.scale;
        let y = (((doc_row - s.top) as u32 * self.term.ch) as f64 + self.term.ch as f64 / 2.0) / s.scale;
        let pos = PagedPosition { page: NonZeroUsize::new(i + 1).unwrap(), point: Point::new(Abs::pt(x), Abs::pt(y)) };
        match typst_ide::jump_from_click(&self.world, doc, &pos) {
            Some(Jump::File(id, offset)) => {
                let (Some(path), Ok(source)) = (self.world.path(id), self.world.source(id)) else { return };
                let lines = source.lines();
                let line = lines.byte_to_line(offset).unwrap_or(0);
                let col = offset - lines.line_to_byte(line).unwrap_or(0);
                let _ = self.editor_tx.send(ToEditor::Goto(path, line + 1, col + 1));
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
            KeyCode::Char('n') | KeyCode::Char('J') | KeyCode::Right => self.goto_page(self.current_page() + 1),
            KeyCode::Char('p') | KeyCode::Char('K') | KeyCode::Left => {
                self.goto_page(self.current_page().saturating_sub(1))
            }
            KeyCode::Char('w') => {
                let page = self.current_page();
                self.fit = if self.fit == Fit::Width { Fit::Page } else { Fit::Width };
                self.relayout();
                self.goto_page(page);
            }
            KeyCode::Char('i') => {
                self.invert = !self.invert;
                self.relayout();
                self.dirty = true;
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                self.notify(if self.follow { "following the cursor" } else { "not following the cursor" });
            }
            KeyCode::Char('r') => {
                self.world.reset();
                self.compile();
            }
            _ => {}
        }
        true
    }

    fn resize(&mut self) {
        let page = self.current_page();
        let at_top = self.scroll == 0;
        self.term = Term::query();
        let mut out = Vec::new();
        let _ = kitty::delete_all(&mut out);
        let _ = io::stdout().write_all(&out);
        self.images.clear();
        self.placed.clear();
        self.stale.clear();
        let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::All));
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

fn pick_file(args: &Args, editor: &Editor) -> anyhow::Result<PathBuf> {
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
        // In a pane of its own, keep the reason on screen until a key.
        if std::io::IsTerminal::is_terminal(&io::stdin()) && args.editor_block.is_some() {
            eprintln!("Press Enter to close.");
            let _ = io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

fn start(args: &Args) -> anyhow::Result<()> {
    let editor = Editor::new(args.editor_block.clone(), args.nvim.clone());
    let main = pick_file(args, &editor)?.canonicalize().context("the Typst file doesn't exist")?;
    let root = match &args.root {
        Some(r) => r.canonicalize()?,
        None => world::find_root(&main).canonicalize()?,
    };
    let world = TypstWorld::new(root, &main)?;
    let (editor_tx, editor_rx) = editor.spawn();

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
    print!("\x1b]2;typst · {}\x1b\\", file_name(&main));

    let mut app = App {
        world,
        main,
        doc: None,
        page_hashes: Vec::new(),
        errors: Vec::new(),
        warnings: 0,
        compile_ms: 0,
        term: Term::query(),
        scroll: 0,
        fit: if args.fit_page { Fit::Page } else { Fit::Width },
        invert: args.invert,
        follow: !args.no_follow,
        slots: Vec::new(),
        total_rows: 0,
        images: HashMap::new(),
        next_id: 0,
        placed: HashSet::new(),
        stale: Vec::new(),
        message: None,
        editor_tx,
        watcher,
        watched: HashSet::new(),
        deps: HashSet::new(),
        overlaid: HashSet::new(),
        placeholder: false,
        error_rows: 0,
        dirty: true,
    };
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
                    MouseEventKind::ScrollDown => app.scroll_by(3),
                    MouseEventKind::ScrollUp => app.scroll_by(-3),
                    MouseEventKind::Down(MouseButton::Left) => app.click(m.column, m.row),
                    _ => {}
                },
                Event::Resize(..) => app.resize(),
                _ => {}
            }
        }

        // Unsaved edits and cursor moves from Neovim.
        let mut cursor = None;
        while let Ok(msg) = editor_rx.try_recv() {
            match msg {
                FromEditor::Status(s) => app.notify(s),
                FromEditor::Snapshot(s) => {
                    if s.path.extension().is_some_and(|e| e == "typ") && app.world.id(&s.path).is_some() {
                        let changed = match (s.modified, s.text) {
                            (true, Some(text)) => {
                                app.overlaid.insert(s.path.clone());
                                app.world.set_overlay(s.path.clone(), text)
                            }
                            (false, _) => app.overlaid.remove(&s.path) && app.world.clear_overlay(&s.path),
                            _ => false,
                        };
                        if changed {
                            app.world.reset();
                            app.compile();
                        }
                        cursor = Some((s.path, s.line, s.col));
                    }
                }
            }
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
    let _ = execute!(stdout, event::DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}
