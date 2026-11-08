//! The editor pane beside the preview.
//!
//! With Neovim, rex-typst keeps one msgpack-RPC connection to its socket and
//! installs a few autocmds that push the cursor, the unsaved text of .typ
//! buffers, and saves to us as they happen, so nothing polls while you work.
//! Clicked spots go back over the same connection. Other editors get jumps
//! typed into their Rex pane, for the ones we know how to drive. It all runs
//! on a thread, so a slow or busy editor can't freeze the preview.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use rmpv::Value;

pub struct Snapshot {
    pub path: PathBuf,
    /// 1-based line.
    pub line: usize,
    /// 1-based byte column.
    pub col: usize,
    pub modified: bool,
    /// The buffer's text, when it changed since the last snapshot.
    pub text: Option<String>,
    /// The cursor is where a click just sent it, so the preview shouldn't
    /// scroll to it.
    pub jumped: bool,
}

pub enum FromEditor {
    /// Neovim's current buffer: where the cursor is, and its unsaved text.
    Snapshot(Snapshot),
    /// A buffer was saved, reloaded, or closed, maybe while another buffer
    /// was current, so the file on disk counts again.
    Clean(PathBuf),
    /// Neovim went away, and its unsaved text with it.
    Gone,
    Status(String),
}

enum Input {
    Goto(PathBuf, usize, usize),
    /// A message from Neovim, on connection `conn`.
    Rpc(u64, Value),
    /// Connection `conn` closed.
    Lost(u64),
}

/// Sends jumps to the editor thread.
pub struct EditorLink {
    tx: Sender<Input>,
}

impl EditorLink {
    /// Jump to a 1-based line and 1-based byte column.
    pub fn goto(&self, path: PathBuf, line: usize, col: usize) {
        let _ = self.tx.send(Input::Goto(path, line, col));
    }
}

/// Installed in Neovim on connect, with our channel id as its argument. It
/// removes itself once the channel is gone.
const ATTACH_LUA: &str = r#"
local chan = ...
local group = vim.api.nvim_create_augroup("rex_typst_" .. chan, { clear = true })
-- The changedtick whose text each buffer last sent.
local sent = {}
local function notify(...)
  if not pcall(vim.rpcnotify, chan, ...) then
    pcall(vim.api.nvim_del_augroup_by_id, group)
  end
end
local function typst(buf)
  return vim.bo[buf].buftype == "" and vim.api.nvim_buf_get_name(buf):match("%.typ$") ~= nil
end
local function state()
  local buf = vim.api.nvim_get_current_buf()
  if not typst(buf) then return end
  local pos = vim.api.nvim_win_get_cursor(0)
  local modified = vim.bo[buf].modified
  local text = vim.NIL
  if not modified then
    sent[buf] = nil
  else
    local tick = vim.api.nvim_buf_get_changedtick(buf)
    if sent[buf] ~= tick then
      sent[buf] = tick
      text = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
    end
  end
  notify("rex_typst_state", vim.api.nvim_buf_get_name(buf), pos[1], pos[2] + 1, modified, text)
end
local function clean(ev)
  if not typst(ev.buf) then return end
  if ev.event == "BufUnload" or not vim.bo[ev.buf].modified then
    sent[ev.buf] = nil
    notify("rex_typst_clean", vim.api.nvim_buf_get_name(ev.buf))
  end
end
vim.api.nvim_create_autocmd(
  { "CursorMoved", "CursorMovedI", "TextChanged", "TextChangedI", "TextChangedP", "BufEnter", "WinEnter" },
  { group = group, callback = function() state() end })
vim.api.nvim_create_autocmd(
  { "BufWritePost", "BufModifiedSet", "BufUnload", "FileChangedShellPost" },
  { group = group, callback = function(ev) clean(ev) end })
state()
"#;

const GOTO_LUA: &str = r#"
local path, line, col = ...
vim.cmd("drop " .. vim.fn.fnameescape(path))
pcall(vim.api.nvim_win_set_cursor, 0, { line, col - 1 })
"#;

/// A msgpack-RPC connection to Neovim.
struct Rpc {
    stream: UnixStream,
    next_id: u64,
    /// Notifications that arrived while `call` waited for an answer.
    pending: Vec<Value>,
}

impl Rpc {
    /// Connect and install the autocmds. Gives up after a second: Neovim
    /// doesn't answer while it shows a prompt, such as a swap file warning.
    fn attach(path: &str) -> Result<Self, String> {
        let stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
        let mut rpc = Self { stream, next_id: 0, pending: Vec::new() };
        let info = rpc.call("nvim_get_api_info", vec![])?;
        let chan = info.as_array().and_then(|a| a.first()).and_then(Value::as_u64).ok_or("no channel id")?;
        rpc.call("nvim_exec_lua", vec![ATTACH_LUA.into(), Value::Array(vec![chan.into()])])?;
        Ok(rpc)
    }

    fn send(&mut self, method: &str, args: Vec<Value>) -> io::Result<u64> {
        self.next_id += 1;
        let msg = Value::Array(vec![0.into(), self.next_id.into(), method.into(), Value::Array(args)]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &msg).map_err(io::Error::other)?;
        self.stream.write_all(&buf)?;
        Ok(self.next_id)
    }

    /// Call a method and wait for its answer. Only before `listen`, which
    /// hands reading to a thread.
    fn call(&mut self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        let id = self.send(method, args).map_err(|e| e.to_string())?;
        loop {
            // Unbuffered, so nothing past this message is read and lost.
            let v = rmpv::decode::read_value(&mut &self.stream).map_err(|e| e.to_string())?;
            match response(&v) {
                Some((rid, err, result)) if rid == id => {
                    return if err.is_nil() { Ok(result.clone()) } else { Err(error_text(err)) };
                }
                Some(_) => {}
                None => self.pending.push(v),
            }
        }
    }
}

/// The id, error, and result of a response.
fn response(v: &Value) -> Option<(u64, &Value, &Value)> {
    let a = v.as_array()?;
    if a.len() != 4 || a[0].as_u64() != Some(1) {
        return None;
    }
    Some((a[1].as_u64()?, &a[2], &a[3]))
}

/// The method and arguments of a notification.
fn notification(v: &Value) -> Option<(&str, &[Value])> {
    let a = v.as_array()?;
    if a.len() != 3 || a[0].as_u64() != Some(2) {
        return None;
    }
    Some((a[1].as_str()?, a[2].as_array()?.as_slice()))
}

/// Neovim sends errors as [type, message].
fn error_text(err: &Value) -> String {
    err.as_array()
        .and_then(|a| a.get(1))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string())
}

pub struct Editor {
    /// The Rex block the editor runs in.
    block: Option<String>,
    sock: Option<String>,
    /// The socket came from the command line, so don't go looking.
    fixed_sock: bool,
    /// Name of the editor's foreground process, such as nvim or hx.
    name: Option<String>,
    pid: Option<u32>,
    rpc: Option<Rpc>,
    /// Counts connections, so a late message from an old one is ignored.
    conn: u64,
    /// Focus the editor's pane after a jump.
    focus: bool,
    /// Where the last jump put the cursor, until Neovim reports it.
    jumped_to: Option<(PathBuf, usize, usize)>,
    /// Jumps Neovim hasn't answered yet.
    gotos: HashSet<u64>,
}

impl Editor {
    pub fn new(block: Option<String>, sock: Option<String>, focus: bool) -> Self {
        let fixed_sock = sock.is_some();
        let mut e = Self {
            block,
            sock,
            fixed_sock,
            name: None,
            pid: None,
            rpc: None,
            conn: 0,
            focus,
            jumped_to: None,
            gotos: HashSet::new(),
        };
        e.refresh();
        e
    }

    /// Look at what runs in the editor pane, and connect to Neovim.
    fn refresh(&mut self) {
        if let Some(fg) = self.block.as_deref().and_then(foreground) {
            if Some(fg.pid) != self.pid {
                self.pid = Some(fg.pid);
                self.name = Some(fg.name.clone());
                if !self.fixed_sock {
                    self.sock = None;
                    self.rpc = None;
                }
            }
            if !self.fixed_sock && self.sock.is_none() && fg.name == "nvim" {
                self.sock = find_nvim_socket(fg.pid);
            }
        }
        if self.rpc.is_none() {
            if let Some(sock) = self.sock.clone() {
                match Rpc::attach(&sock) {
                    Ok(rpc) => {
                        self.rpc = Some(rpc);
                        self.conn += 1;
                    }
                    Err(_) if !self.fixed_sock => self.sock = None,
                    Err(_) => {}
                }
            }
        }
    }

    /// The file open in Neovim, if any.
    pub fn buffer_path(&mut self) -> Option<PathBuf> {
        let v = self.rpc.as_mut()?.call("nvim_eval", vec!["expand('%:p')".into()]).ok()?;
        let p = v.as_str()?;
        (!p.is_empty()).then(|| PathBuf::from(p))
    }

    /// A .typ file named on the editor process's command line.
    pub fn argv_file(&self) -> Option<PathBuf> {
        let pid = self.pid?;
        let out = Command::new("ps").args(["-ww", "-o", "args=", "-p", &pid.to_string()]).output().ok()?;
        let args = String::from_utf8_lossy(&out.stdout).to_string();
        let file = args.split_whitespace().rev().find(|a| a.ends_with(".typ"))?.to_string();
        let cwd = self.block.as_deref().and_then(foreground).and_then(|f| f.cwd);
        let path = PathBuf::from(&file);
        Some(if path.is_absolute() { path } else { cwd.map(|c| c.join(&file)).unwrap_or(path) })
    }

    /// Hand reading the connection to a thread that feeds `tx`, and replay
    /// what arrived while connecting.
    fn listen(&mut self, tx: &Sender<Input>) {
        let Some(rpc) = self.rpc.as_mut() else { return };
        let conn = self.conn;
        for v in rpc.pending.drain(..) {
            let _ = tx.send(Input::Rpc(conn, v));
        }
        let reader = rpc.stream.try_clone().and_then(|s| s.set_read_timeout(None).map(|_| s));
        let Ok(stream) = reader else {
            self.rpc = None;
            return;
        };
        let tx = tx.clone();
        thread::spawn(move || {
            let mut r = BufReader::new(stream);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                if tx.send(Input::Rpc(conn, v)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Input::Lost(conn));
        });
    }

    fn goto(&mut self, path: &Path, line: usize, col: usize) -> Result<(), String> {
        let p = path.to_string_lossy();
        if let Some(rpc) = self.rpc.as_mut() {
            let args = Value::Array(vec![p.as_ref().into(), line.into(), col.into()]);
            let id = rpc.send("nvim_exec_lua", vec![GOTO_LUA.into(), args]).map_err(|e| format!("Neovim: {e}"))?;
            self.gotos.insert(id);
            self.jumped_to = Some((path.to_path_buf(), line, col));
            self.focus_pane();
            return Ok(());
        }
        let keys = match self.name.as_deref() {
            Some("vim") => format!("\x1b:exe 'drop '.fnameescape({})|call cursor({line},{col})\r", vim_str(&p)),
            Some("hx") => format!("\x1b:open {p}\r:goto {line}\r"),
            _ => return Err(format!("{}:{line}:{col}", path.display())),
        };
        let block = self.block.as_deref().ok_or("no editor pane")?;
        let args = serde_json::json!({ "data": keys }).to_string();
        let ok = rex(&["block", "call", "-b", block, "write", &args]);
        if !ok {
            return Err("couldn't type into the editor pane".into());
        }
        self.focus_pane();
        Ok(())
    }

    fn focus_pane(&self) {
        if let (true, Some(block)) = (self.focus, self.block.as_deref()) {
            rex(&["block", "focus", block]);
        }
    }

    /// Act on a message from Neovim. Returns false once the preview is gone.
    fn handle(&mut self, v: Value, out: &Sender<FromEditor>) -> bool {
        if let Some((id, err, _)) = response(&v) {
            if self.gotos.remove(&id) && !err.is_nil() {
                return out.send(FromEditor::Status(format!("→ Neovim: {}", error_text(err)))).is_ok();
            }
            return true;
        }
        let Some((method, args)) = notification(&v) else { return true };
        let path = |i: usize| args.get(i).and_then(Value::as_str).filter(|p| !p.is_empty()).map(canonical);
        let msg = match method {
            "rex_typst_state" => {
                let Some(path) = path(0) else { return true };
                let line = args.get(1).and_then(Value::as_u64).unwrap_or(1) as usize;
                let col = args.get(2).and_then(Value::as_u64).unwrap_or(1) as usize;
                let modified = args.get(3).and_then(Value::as_bool).unwrap_or(false);
                let text = args.get(4).and_then(Value::as_str).map(str::to_string);
                let here = (path, line, col);
                let jumped = self.jumped_to.as_ref() == Some(&here);
                if jumped {
                    self.jumped_to = None;
                }
                let (path, line, col) = here;
                FromEditor::Snapshot(Snapshot { path, line, col, modified, text, jumped })
            }
            "rex_typst_clean" => match path(0) {
                Some(p) => FromEditor::Clean(p),
                None => return true,
            },
            _ => return true,
        };
        out.send(msg).is_ok()
    }

    /// Run the editor link on a thread.
    pub fn spawn(mut self) -> (EditorLink, Receiver<FromEditor>) {
        let (in_tx, in_rx) = mpsc::channel::<Input>();
        let (out_tx, out_rx) = mpsc::channel::<FromEditor>();
        let link = EditorLink { tx: in_tx.clone() };
        thread::spawn(move || {
            self.listen(&in_tx);
            let mut refreshed = Instant::now();
            loop {
                // Connected, Neovim tells us everything. Else look again
                // every couple of seconds, for an editor that starts later.
                let wait = if self.rpc.is_some() { Duration::from_secs(60) } else { Duration::from_secs(2) };
                match in_rx.recv_timeout(wait) {
                    Ok(Input::Goto(path, line, col)) => {
                        if let Err(e) = self.goto(&path, line, col) {
                            if out_tx.send(FromEditor::Status(format!("→ {e}"))).is_err() {
                                return;
                            }
                        }
                    }
                    Ok(Input::Rpc(conn, v)) if conn == self.conn => {
                        if !self.handle(v, &out_tx) {
                            return;
                        }
                    }
                    Ok(Input::Lost(conn)) if conn == self.conn => {
                        if out_tx.send(FromEditor::Gone).is_err() {
                            return;
                        }
                        self.rpc = None;
                        self.gotos.clear();
                        if !self.fixed_sock {
                            self.sock = None;
                        }
                    }
                    Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                if self.rpc.is_none() && refreshed.elapsed() >= Duration::from_secs(2) {
                    refreshed = Instant::now();
                    self.refresh();
                    self.listen(&in_tx);
                }
            }
        });
        (link, out_rx)
    }
}

/// The path with symlinks resolved, as the compiler sees it.
fn canonical(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

fn rex(args: &[&str]) -> bool {
    Command::new("rex")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn vim_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub struct Foreground {
    pub pid: u32,
    pub name: String,
    pub cwd: Option<PathBuf>,
}

/// The foreground process of a Rex terminal block.
pub fn foreground(block: &str) -> Option<Foreground> {
    let out = Command::new("rex")
        .args(["block", "call", "-b", block, "process", "{}"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let fg = v.get("foreground").filter(|f| !f.is_null()).or_else(|| v.get("child"))?;
    Some(Foreground {
        pid: fg.get("pid")?.as_u64()? as u32,
        name: fg.get("name").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
        cwd: fg.get("cwd").and_then(serde_json::Value::as_str).map(PathBuf::from),
    })
}

/// pid and every process under it. Neovim's TUI is one process and the
/// editor whose socket we want is its `nvim --embed` child.
fn family(pid: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = line.split_whitespace().filter_map(|s| s.parse::<u32>().ok());
            if let (Some(c), Some(p)) = (it.next(), it.next()) {
                children.entry(p).or_default().push(c);
            }
        }
    }
    let mut all = HashSet::from([pid]);
    let mut queue = vec![pid];
    while let Some(cur) = queue.pop() {
        for &c in children.get(&cur).map(Vec::as_slice).unwrap_or(&[]) {
            if all.insert(c) {
                queue.push(c);
            }
        }
    }
    all
}

fn find_nvim_socket(pid: u32) -> Option<String> {
    let pids = family(pid);
    let mut dirs: Vec<PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .collect();
    dirs.push(PathBuf::from("/tmp"));
    for dir in dirs {
        if let Some(s) = scan_sockets(&dir, 3, &pids) {
            return Some(s);
        }
    }
    // A socket somewhere else, such as `nvim --listen /path`: ask lsof.
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let out = Command::new("lsof").args(["-a", "-U", "-p", &list.join(","), "-F", "n"]).stderr(Stdio::null()).output().ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(name) = line.strip_prefix("n/") {
            let path = format!("/{name}");
            let answers = UnixStream::connect(&path).is_ok_and(|s| {
                let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                let mut probe = Rpc { stream: s, next_id: 0, pending: Vec::new() };
                probe.call("nvim_get_api_info", vec![]).is_ok()
            });
            if answers {
                return Some(path);
            }
        }
    }
    None
}

fn scan_sockets(dir: &Path, depth: u32, pids: &HashSet<u32>) -> Option<String> {
    use std::os::unix::fs::FileTypeExt;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if ft.is_socket() {
            // nvim.<pid>.<n>
            let spid = name.strip_prefix("nvim.").and_then(|r| r.split('.').next()).and_then(|p| p.parse::<u32>().ok());
            if spid.is_some_and(|p| pids.contains(&p)) {
                return Some(entry.path().to_string_lossy().to_string());
            }
        } else if ft.is_dir() && depth > 1 {
            if let Some(s) = scan_sockets(&entry.path(), depth - 1, pids) {
                return Some(s);
            }
        }
    }
    None
}
