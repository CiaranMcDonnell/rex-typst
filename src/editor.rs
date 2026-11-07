//! The editor pane beside the preview.
//!
//! With Neovim, a background thread polls its cursor and unsaved buffer over
//! its RPC socket (`nvim --server … --remote-expr`) and jumps it to clicked
//! spots. Other editors get jumps typed into their Rex pane, for the ones we
//! know how to drive. The thread keeps a slow or busy editor from freezing
//! the preview.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub enum ToEditor {
    /// Jump to a 1-based line and 1-based byte column.
    Goto(PathBuf, usize, usize),
}

pub struct Snapshot {
    pub path: PathBuf,
    /// 1-based line.
    pub line: usize,
    /// 1-based byte column.
    pub col: usize,
    pub modified: bool,
    /// The buffer's text, when it changed since the last snapshot.
    pub text: Option<String>,
}

pub enum FromEditor {
    Snapshot(Snapshot),
    Status(String),
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
}

impl Editor {
    pub fn new(block: Option<String>, sock: Option<String>) -> Self {
        let fixed_sock = sock.is_some();
        let mut e = Self { block, sock, fixed_sock, name: None, pid: None };
        e.refresh();
        e
    }

    pub fn is_nvim(&self) -> bool {
        self.sock.is_some()
    }

    /// Look at what runs in the editor pane, and find Neovim's socket.
    fn refresh(&mut self) {
        let Some(fg) = self.block.as_deref().and_then(foreground) else {
            return;
        };
        if Some(fg.pid) != self.pid {
            self.pid = Some(fg.pid);
            self.name = Some(fg.name.clone());
            if !self.fixed_sock {
                self.sock = None;
            }
        }
        if !self.fixed_sock && self.sock.is_none() && fg.name == "nvim" {
            self.sock = find_nvim_socket(fg.pid);
        }
    }

    /// Evaluate a Vim expression in Neovim. Gives up after a second: Neovim
    /// doesn't answer while it shows a prompt, such as a swap file warning.
    pub fn eval(&self, expr: &str) -> Option<String> {
        use std::io::Read;
        let sock = self.sock.as_deref()?;
        let mut child = Command::new("nvim")
            .args(["--server", sock, "--remote-expr", expr])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Read on a thread, so a big buffer can't fill the pipe and stall.
        let mut stdout = child.stdout.take()?;
        let reader = thread::spawn(move || {
            let mut out = String::new();
            stdout.read_to_string(&mut out).map(|_| out)
        });
        let deadline = Instant::now() + Duration::from_millis(1000);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        let out = reader.join().ok()?.ok()?;
        status.success().then(|| out.trim().to_string())
    }

    /// The file open in Neovim, if any.
    pub fn buffer_path(&self) -> Option<PathBuf> {
        let p = self.eval("expand('%:p')")?;
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

    fn goto(&self, path: &Path, line: usize, col: usize) -> Result<(), String> {
        let p = path.to_string_lossy();
        if self.is_nvim() {
            let expr = format!(
                "execute('drop ' . fnameescape({})) . cursor({line}, {col})",
                vim_str(&p)
            );
            return self.eval(&expr).map(|_| ()).ok_or_else(|| "Neovim didn't answer".into());
        }
        let keys = match self.name.as_deref() {
            Some("vim") => format!("\x1b:exe 'drop '.fnameescape({})|call cursor({line},{col})\r", vim_str(&p)),
            Some("hx") => format!("\x1b:open {p}\r:goto {line}\r"),
            _ => return Err(format!("{}:{line}:{col}", path.display())),
        };
        let block = self.block.as_deref().ok_or("no editor pane")?;
        let args = serde_json::json!({ "data": keys }).to_string();
        let ok = Command::new("rex")
            .args(["block", "call", "-b", block, "write", &args])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok { Ok(()) } else { Err("couldn't type into the editor pane".into()) }
    }

    /// Run the editor link on a thread.
    pub fn spawn(mut self) -> (Sender<ToEditor>, Receiver<FromEditor>) {
        let (to_tx, to_rx) = mpsc::channel::<ToEditor>();
        let (from_tx, from_rx) = mpsc::channel::<FromEditor>();
        thread::spawn(move || {
            let mut last: Option<(PathBuf, usize, usize, i64, bool)> = None;
            let mut refreshed = Instant::now();
            loop {
                while let Ok(cmd) = to_rx.try_recv() {
                    match cmd {
                        ToEditor::Goto(path, line, col) => {
                            if let Err(e) = self.goto(&path, line, col) {
                                let _ = from_tx.send(FromEditor::Status(format!("→ {e}")));
                            } else if let Some(l) = last.as_mut() {
                                // Don't scroll the preview back to where it sent us.
                                *l = (path, line, col, l.3, l.4);
                            }
                        }
                    }
                }
                if refreshed.elapsed() > Duration::from_secs(2) {
                    refreshed = Instant::now();
                    let had = self.sock.is_some();
                    self.refresh();
                    if self.sock.is_some() && !had {
                        last = None;
                    }
                }
                if self.sock.is_some() {
                    match self.eval("json_encode([expand('%:p'), line('.'), col('.'), b:changedtick, &modified])") {
                        Some(s) => {
                            if let Some(snap) = parse_state(&s) {
                                let (path, line, col, tick, modified) = snap;
                                let changed_text = last.as_ref().is_none_or(|l| l.0 != path || l.3 != tick || l.4 != modified);
                                let moved = last.as_ref().is_none_or(|l| l.0 != path || l.1 != line || l.2 != col);
                                if changed_text || moved {
                                    let text = (changed_text && modified)
                                        .then(|| self.eval(r#"json_encode(join(getline(1, '$'), "\n"))"#))
                                        .flatten()
                                        .and_then(|t| serde_json::from_str::<String>(&t).ok());
                                    last = Some((path.clone(), line, col, tick, modified));
                                    let msg = Snapshot { path, line, col, modified, text };
                                    if from_tx.send(FromEditor::Snapshot(msg)).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        None if !self.fixed_sock => self.sock = None,
                        None => {}
                    }
                }
                thread::sleep(Duration::from_millis(120));
            }
        });
        (to_tx, from_rx)
    }
}

fn parse_state(s: &str) -> Option<(PathBuf, usize, usize, i64, bool)> {
    let v: Value = serde_json::from_str(s).ok()?;
    let a = v.as_array()?;
    let path = a.first()?.as_str()?;
    if path.is_empty() {
        return None;
    }
    Some((
        PathBuf::from(path),
        a.get(1)?.as_u64()? as usize,
        a.get(2)?.as_u64()? as usize,
        a.get(3)?.as_i64().unwrap_or(0),
        a.get(4)?.as_i64().unwrap_or(0) != 0,
    ))
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
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let fg = v.get("foreground").filter(|f| !f.is_null()).or_else(|| v.get("child"))?;
    Some(Foreground {
        pid: fg.get("pid")?.as_u64()? as u32,
        name: fg.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        cwd: fg.get("cwd").and_then(Value::as_str).map(PathBuf::from),
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
            let probe = Editor { block: None, sock: Some(path.clone()), fixed_sock: true, name: None, pid: None };
            if probe.eval("1").is_some() {
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
