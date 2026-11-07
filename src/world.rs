//! The Typst world: project files from disk, unsaved editor buffers on top,
//! fonts, packages, and the date.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use typst::diag::FileResult;
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_ide::IdeWorld;
use typst_kit::datetime::Time;
use typst_kit::downloader::SystemDownloader;
use typst_kit::files::{FileLoader, FileStore, FsRoot, SystemFiles};
use typst_kit::fonts::{self, FontStore};
use typst_kit::packages::SystemPackages;

/// Loads files from disk, except paths with an unsaved buffer in the editor.
pub struct Loader {
    files: SystemFiles,
    overlay: HashMap<PathBuf, Bytes>,
}

impl FileLoader for Loader {
    fn load(&self, id: FileId) -> FileResult<Bytes> {
        if let Ok(path) = self.files.resolve(id) {
            if let Some(bytes) = self.overlay.get(&path) {
                return Ok(bytes.clone());
            }
        }
        self.files.load(id)
    }
}

pub struct TypstWorld {
    root: PathBuf,
    main: FileId,
    library: LazyHash<Library>,
    fonts: FontStore,
    store: FileStore<Loader>,
    time: Time,
}

impl TypstWorld {
    pub fn new(root: PathBuf, main: &Path) -> anyhow::Result<Self> {
        let main = id_for(&root, main)
            .ok_or_else(|| anyhow::anyhow!("{} is not inside the root {}", main.display(), root.display()))?;
        let mut fonts = FontStore::new();
        fonts.extend(fonts::system());
        fonts.extend(fonts::embedded());
        let downloader = SystemDownloader::new(concat!("rex-typst/", env!("CARGO_PKG_VERSION")));
        let files = SystemFiles::new(FsRoot::new(root.clone()), SystemPackages::new(downloader));
        Ok(Self {
            root,
            main,
            library: LazyHash::new(Library::builder().build()),
            fonts,
            store: FileStore::new(Loader { files, overlay: HashMap::new() }),
            time: Time::system(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file id for a path in the project, if it's inside the root.
    pub fn id(&self, path: &Path) -> Option<FileId> {
        id_for(&self.root, path)
    }

    /// The path on disk of a file id.
    pub fn path(&self, id: FileId) -> Option<PathBuf> {
        self.store.loader().files.resolve(id).ok()
    }

    /// Use `text` for `path` instead of what's on disk. Returns whether it
    /// changed anything.
    pub fn set_overlay(&mut self, path: PathBuf, text: String) -> bool {
        let bytes = Bytes::from_string(text);
        let overlay = &mut self.store.loader_mut().overlay;
        if overlay.get(&path).is_some_and(|b| b.as_slice() == bytes.as_slice()) {
            return false;
        }
        overlay.insert(path, bytes);
        true
    }

    /// Go back to the file on disk for `path`.
    pub fn clear_overlay(&mut self, path: &Path) -> bool {
        self.store.loader_mut().overlay.remove(path).is_some()
    }

    /// Mark every file stale before the next compile. Unchanged sources are
    /// kept, so compiling again is incremental.
    pub fn reset(&mut self) {
        self.store.reset();
        self.time.reset();
    }

    /// The project files the last compile read, as paths on disk.
    pub fn dependencies(&mut self) -> Vec<PathBuf> {
        let (loader, ids) = self.store.dependencies();
        ids.filter(|id| matches!(id.root(), VirtualRoot::Project))
            .filter_map(|id| loader.files.resolve(id).ok())
            .collect()
    }
}

fn id_for(root: &Path, path: &Path) -> Option<FileId> {
    let vpath = VirtualPath::virtualize(root, path).ok()?;
    Some(FileId::new(RootedPath::new(VirtualRoot::Project, vpath)))
}

impl World for TypstWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        self.store.source(id)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.store.file(id)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.time.today(offset)
    }
}

impl IdeWorld for TypstWorld {
    fn upcast(&self) -> &dyn World {
        self
    }
}

/// The project root for a file: $TYPST_ROOT, else the nearest directory
/// above it holding typst.toml or .git, else the file's directory.
pub fn find_root(file: &Path) -> PathBuf {
    if let Some(env) = std::env::var_os("TYPST_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(env);
    }
    let dir = file.parent().unwrap_or(Path::new("/")).to_path_buf();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut cur = Some(dir.as_path());
    while let Some(d) = cur {
        // A dotfiles repository at $HOME isn't a Typst project.
        if home.as_deref() == Some(d) {
            break;
        }
        if d.join("typst.toml").exists() || d.join(".git").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    dir
}
