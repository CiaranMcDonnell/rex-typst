//! Finds the document an open file belongs to, from the includes and imports
//! written in the project's files. Nothing is compiled: each file that
//! mentions `include` or `import` is parsed, which takes microseconds.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use typst::syntax::ast::{self, AstNode};
use typst::syntax::{SyntaxKind, SyntaxNode};

/// Past this many directory entries, or Typst files, the project is too big
/// to scan at startup, and the open file is previewed as it is.
const MAX_ENTRIES: usize = 100_000;
const MAX_FILES: usize = 5_000;

/// The main file of the document that `file` is part of: one that includes or
/// imports it, directly or through other files, and that no file includes.
/// When there are several, main.typ wins, then the shallowest, then by name.
/// `file` itself when nothing includes it, or the includes are computed
/// rather than written as plain strings. Both paths are canonical.
pub fn main_of(root: &Path, file: &Path) -> PathBuf {
    let Some(files) = typ_files(root) else { return file.to_path_buf() };
    // Who includes or imports each file.
    let mut parents: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        if !text.contains("include") && !text.contains("import") {
            continue;
        }
        let Ok(f) = f.canonicalize() else { continue };
        let dir = f.parent().unwrap_or(root);
        let mut deps = Vec::new();
        collect_deps(&typst::syntax::parse(&text), &mut deps);
        for dep in deps {
            // Paths starting with / are relative to the project root.
            let path = match dep.strip_prefix('/') {
                Some(rel) => root.join(rel),
                None => dir.join(&dep),
            };
            if let Ok(path) = path.canonicalize() {
                parents.entry(path).or_default().push(f.clone());
            }
        }
    }

    // Walk up the includes to the files nothing includes.
    let mut seen = HashSet::from([file.to_path_buf()]);
    let mut queue = vec![file.to_path_buf()];
    let mut tops = Vec::new();
    while let Some(f) = queue.pop() {
        match parents.get(&f) {
            None => tops.push(f),
            Some(ps) => queue.extend(ps.iter().filter(|p| seen.insert((*p).clone())).cloned()),
        }
    }
    tops.into_iter()
        .min_by_key(|p| (p.file_name() != Some(OsStr::new("main.typ")), p.components().count(), p.clone()))
        .unwrap_or_else(|| file.to_path_buf())
}

/// The paths a file imports or includes as plain strings. Packages are left
/// out, being outside the project.
fn collect_deps(node: &SyntaxNode, out: &mut Vec<String>) {
    let source = match node.kind() {
        SyntaxKind::ModuleImport => ast::ModuleImport::from_untyped(node).map(|i| i.source()),
        SyntaxKind::ModuleInclude => ast::ModuleInclude::from_untyped(node).map(|i| i.source()),
        _ => None,
    };
    if let Some(ast::Expr::Str(s)) = source {
        let s = s.get();
        if !s.starts_with('@') {
            out.push(s.to_string());
        }
    }
    for child in node.children() {
        collect_deps(child, out);
    }
}

/// Every .typ file under the root, leaving out hidden directories and build
/// output, or None if there are too many to scan.
fn typ_files(root: &Path) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries = 0;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(list) = std::fs::read_dir(&dir) else { continue };
        for e in list.flatten() {
            entries += 1;
            if entries > MAX_ENTRIES {
                return None;
            }
            let name = e.file_name();
            // Symlinks aren't followed, so a link loop can't trap the scan.
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                let n = name.to_string_lossy();
                if !n.starts_with('.') && n != "node_modules" && n != "target" {
                    dirs.push(e.path());
                }
            } else if Path::new(&name).extension().is_some_and(|x| x == "typ") {
                files.push(e.path());
                if files.len() > MAX_FILES {
                    return None;
                }
            }
        }
    }
    Some(files)
}
