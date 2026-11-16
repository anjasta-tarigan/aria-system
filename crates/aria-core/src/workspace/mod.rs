//! Workspace root resolution and path containment.
//!
//! Every path a Tool touches goes through this module. Containment is enforced
//! here, never in the prompt (issue 03).

use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path escapes the workspace: {0}")]
    Escapes(String),
    #[error("parent directory does not exist: {0}")]
    MissingParent(String),
    #[error("path is not valid UTF-8: {0}")]
    NotUtf8(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, PathError>;

/// A workspace root that file tools are confined to.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    canonical: PathBuf,
}

impl WorkspaceRoot {
    /// Canonicalise the root. Fails if it is not an existing directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root)?;
        if !canonical.is_dir() {
            return Err(PathError::Escapes(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        Ok(Self { canonical })
    }

    pub fn root(&self) -> &Path {
        &self.canonical
    }

    /// Resolve a path for reading. The result exists and is inside the root.
    pub fn resolve_read(&self, requested: impl AsRef<Path>) -> Result<PathBuf> {
        let candidate = self.join(requested.as_ref())?;
        let resolved = std::fs::canonicalize(&candidate)?;
        self.check_inside(&resolved)?;
        Ok(resolved)
    }

    /// Resolve a path for creation or replacement.
    ///
    /// The final component may not exist, but its parent must exist and resolve
    /// inside the root. An existing final component is canonicalised and checked
    /// too, so a symlinked file pointing outside is refused.
    pub fn resolve_create(&self, requested: impl AsRef<Path>) -> Result<PathBuf> {
        let candidate = self.join(requested.as_ref())?;
        if candidate.exists() || std::fs::symlink_metadata(&candidate).is_ok() {
            let resolved = std::fs::canonicalize(&candidate)?;
            self.check_inside(&resolved)?;
            return Ok(resolved);
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| PathError::MissingParent(candidate.display().to_string()))?;
        let parent = std::fs::canonicalize(parent)?;
        self.check_inside(&parent)?;
        let name = candidate
            .file_name()
            .ok_or_else(|| PathError::MissingParent(candidate.display().to_string()))?;
        let resolved = parent.join(name);
        // The joined name itself cannot escape: a file name has no separators.
        self.check_inside(&resolved)?;
        Ok(resolved)
    }

    /// Containment check without touching the filesystem. The path must be
    /// absolute and lexically normalised.
    pub fn check_inside(&self, path: &Path) -> Result<()> {
        if path.starts_with(&self.canonical) {
            Ok(())
        } else {
            Err(PathError::Escapes(path.display().to_string()))
        }
    }

    /// Convert an absolute resolved path back to a workspace-relative string.
    pub fn relativise(&self, absolute: &Path) -> Result<String> {
        let rel = absolute
            .strip_prefix(&self.canonical)
            .map_err(|_| PathError::Escapes(absolute.display().to_string()))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }

    /// Join a requested path onto the root, rejecting absolute paths and any
    /// `..` component before we touch the filesystem.
    fn join(&self, requested: &Path) -> Result<PathBuf> {
        if requested.is_absolute() {
            return Err(PathError::Escapes(requested.display().to_string()));
        }
        let mut out = self.canonical.clone();
        for comp in requested.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(part) => out.push(part),
                Component::ParentDir => {
                    return Err(PathError::Escapes(requested.display().to_string()));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(PathError::Escapes(requested.display().to_string()));
                }
            }
        }
        Ok(out)
    }
}

impl std::ops::Deref for WorkspaceRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.canonical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, WorkspaceRoot, tempfile::TempDir) {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("a/b")).unwrap();
        std::fs::write(ws.path().join("a/b/f.txt"), "content").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let root = WorkspaceRoot::new(ws.path()).unwrap();
        (ws, root, outside)
    }

    #[test]
    fn resolves_relative_file_for_read() {
        let (ws, root, _o) = setup();
        let p = root.resolve_read("a/b/f.txt").unwrap();
        assert_eq!(p, std::fs::canonicalize(ws.path().join("a/b/f.txt")).unwrap());
        assert_eq!(root.relativise(&p).unwrap(), "a/b/f.txt");
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_ws, root, _o) = setup();
        assert!(matches!(
            root.resolve_read("../x"),
            Err(PathError::Escapes(_))
        ));
        assert!(matches!(
            root.resolve_read("a/../../x"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn rejects_absolute_paths() {
        let (_ws, root, _o) = setup();
        assert!(matches!(
            root.resolve_read("/etc/passwd"),
            Err(PathError::Escapes(_))
        ));
        assert!(matches!(
            root.resolve_create("/etc/aria-new-file"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn rejects_symlink_pointing_outside() {
        let (ws, root, outside) = setup();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            ws.path().join("a/link.txt"),
        )
        .unwrap();
        assert!(matches!(
            root.resolve_read("a/link.txt"),
            Err(PathError::Escapes(_))
        ));
        assert!(matches!(
            root.resolve_create("a/link.txt"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn rejects_symlinked_parent_pointing_outside() {
        let (ws, root, outside) = setup();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("a/out")).unwrap();
        assert!(matches!(
            root.resolve_create("a/out/new.txt"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn allows_new_file_in_existing_nested_dir() {
        let (ws, root, _o) = setup();
        let p = root.resolve_create("a/b/new/deep.txt").map(|_| ());
        assert!(p.is_err(), "parent 'new' does not exist");
        let p = root.resolve_create("a/b/new.txt").unwrap();
        assert_eq!(p, std::fs::canonicalize(ws.path().join("a/b")).unwrap().join("new.txt"));
    }

    #[test]
    fn allows_symlink_inside_root() {
        let (ws, root, _o) = setup();
        std::os::unix::fs::symlink(ws.path().join("a/b/f.txt"), ws.path().join("link.txt"))
            .unwrap();
        assert!(root.resolve_read("link.txt").is_ok());
    }

    #[test]
    fn symlinked_root_is_canonicalised() {
        let (ws, _root, _o) = setup();
        let alias = tempfile::tempdir().unwrap();
        let alias_path = alias.path().join("ws");
        std::os::unix::fs::symlink(ws.path(), &alias_path).unwrap();
        let root = WorkspaceRoot::new(&alias_path).unwrap();
        assert_eq!(root.root(), std::fs::canonicalize(ws.path()).unwrap());
        assert!(root.resolve_read("a/b/f.txt").is_ok());
    }

    #[test]
    fn scope_workspace_contains_only_inside() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let scope = Scope::Workspace(WorkspaceRoot::new(dir.path()).unwrap());
        std::fs::write(dir.path().join("x"), "").unwrap();
        let child = std::fs::canonicalize(dir.path().join("x")).unwrap();
        assert!(scope.contains(&child));
        assert!(!scope.contains(other.path()));
    }

    #[test]
    fn scope_main_system_empty_allows_everything() {
        let base = std::env::current_dir().unwrap();
        let scope = Scope::MainSystem { roots: vec![], base };
        let abs = std::fs::canonicalize("/").unwrap();
        assert!(scope.contains(&abs));
    }

    #[test]
    fn scope_main_system_paths_restricts() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let roots = vec![std::fs::canonicalize(dir.path()).unwrap()];
        let scope = Scope::MainSystem { roots, base: std::env::current_dir().unwrap() };
        std::fs::write(dir.path().join("x"), "").unwrap();
        let child = std::fs::canonicalize(dir.path().join("x")).unwrap();
        assert!(scope.contains(&child));
        assert!(!scope.contains(other.path()));
    }

    #[test]
    fn scope_none_contains_nothing() {
        let scope = Scope::None;
        let abs = std::fs::canonicalize("/").unwrap();
        assert!(!scope.contains(&abs));
    }

    #[test]
    fn scope_main_system_base_roots_at_home_and_journals_absolute_paths() {
        let base = std::env::current_dir().unwrap();
        let scope = Scope::MainSystem { roots: vec![], base: base.clone() };
        assert_eq!(scope.root().unwrap(), base);
        let outside = Path::new("/etc/passwd");
        assert_eq!(scope.relativise(outside).unwrap(), "/etc/passwd");
    }
}

/// What part of the filesystem a conversation's Tools may touch.
///
/// `None` is the ordinary main session with no file access. `Workspace` confines
/// every path to one folder. `MainSystem` grants system-wide access, optionally
/// restricted to the given absolute roots (an empty list means unrestricted).
#[derive(Debug, Clone)]
pub enum Scope {
    None,
    Workspace(WorkspaceRoot),
    /// Whole-device access, optionally restricted to `roots`. `base` is Aria's
    /// own home directory: shell cwd, relative paths and recursive-search
    /// default. An empty `roots` list means every path is allowed.
    MainSystem { roots: Vec<PathBuf>, base: PathBuf },
}

impl Scope {
    /// The base directory for recursive searches, shell cwd and journal keys, or
    /// `None` if filesystem access is disabled. For the main system this is its
    /// home directory; allowed roots only widen what `resolve_*` accepts.
    pub fn root(&self) -> Option<PathBuf> {
        match self {
            Scope::Workspace(w) => Some(w.root().to_path_buf()),
            Scope::MainSystem { base, .. } => Some(base.clone()),
            Scope::None => None,
        }
    }

    /// All roots to walk for a recursive search: the folder root, or the
    /// permitted system paths (Aria's home when unrestricted).
    pub fn roots(&self) -> Vec<PathBuf> {
        match self {
            Scope::Workspace(w) => vec![w.root().to_path_buf()],
            Scope::MainSystem { roots, base } => {
                if !roots.is_empty() {
                    roots.clone()
                } else if let Some(home) =
                    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
                {
                    // Unrestricted: walk the user's home rather than Aria's own
                    // small scratch dir, so a bare search is device-wide.
                    vec![PathBuf::from(home)]
                } else {
                    vec![base.clone()]
                }
            }
            Scope::None => vec![],
        }
    }

    /// Whether this is a folder-scoped conversation (file tools use `folder()`).
    pub fn is_folder(&self) -> bool {
        matches!(self, Scope::Workspace(_))
    }

    /// Resolve a path for reading. The result is absolute, canonical, and inside
    /// this scope; otherwise an error. Workspace paths are folder-relative; for
    /// the main system the path is absolute or relative to Aria's home.
    pub fn resolve_read(&self, requested: impl AsRef<Path>) -> Result<PathBuf> {
        match self {
            Scope::None => Err(PathError::Escapes("no filesystem access".into())),
            Scope::Workspace(w) => w.resolve_read(requested),
            Scope::MainSystem { roots, base } => {
                let candidate = if requested.as_ref().is_absolute() {
                    requested.as_ref().to_path_buf()
                } else {
                    base.join(requested.as_ref())
                };
                let resolved = std::fs::canonicalize(&candidate)?;
                if roots.is_empty() || roots.iter().any(|p| resolved.starts_with(p)) {
                    Ok(resolved)
                } else {
                    Err(PathError::Escapes(resolved.display().to_string()))
                }
            }
        }
    }

    /// Resolve a path for creation or replacement (see [`WorkspaceRoot::resolve_create`]).
    pub fn resolve_create(&self, requested: impl AsRef<Path>) -> Result<PathBuf> {
        match self {
            Scope::None => Err(PathError::Escapes("no filesystem access".into())),
            Scope::Workspace(w) => w.resolve_create(requested),
            Scope::MainSystem { roots, base } => {
                let candidate = if requested.as_ref().is_absolute() {
                    requested.as_ref().to_path_buf()
                } else {
                    base.join(requested.as_ref())
                };
                if candidate.exists() || std::fs::symlink_metadata(&candidate).is_ok() {
                    let resolved = std::fs::canonicalize(&candidate)?;
                    if roots.is_empty() || roots.iter().any(|p| resolved.starts_with(p)) {
                        Ok(resolved)
                    } else {
                        Err(PathError::Escapes(resolved.display().to_string()))
                    }
                } else {
                    let parent = candidate
                        .parent()
                        .ok_or_else(|| PathError::MissingParent(candidate.display().to_string()))?;
                    let parent = std::fs::canonicalize(parent)?;
                    if roots.is_empty() || roots.iter().any(|p| parent.starts_with(p)) {
                        Ok(candidate)
                    } else {
                        Err(PathError::Escapes(parent.display().to_string()))
                    }
                }
            }
        }
    }

    /// Containment test against an absolute, canonical path.
    pub fn contains(&self, abs: &Path) -> bool {
        match self {
            Scope::None => false,
            Scope::Workspace(w) => w.check_inside(abs).is_ok(),
            Scope::MainSystem { roots, .. } => {
                roots.is_empty() || roots.iter().any(|p| abs.starts_with(p))
            }
        }
    }

    /// Convert an absolute resolved path back to a stable key for the edit
    /// journal. For the workspace it is folder-relative; for the main system it
    /// is relative to Aria's home. A path outside that base falls back to the
    /// absolute path, which `Path::join` accepts and keeps unambiguous.
    pub fn relativise(&self, absolute: &Path) -> Result<String> {
        match self {
            Scope::Workspace(w) => w.relativise(absolute),
            Scope::MainSystem { base, .. } => {
                let rel = absolute.strip_prefix(base).unwrap_or(absolute);
                Ok(rel.to_string_lossy().replace('\\', "/"))
            }
            Scope::None => Err(PathError::Escapes(absolute.display().to_string())),
        }
    }
}

/// Local sync folders a desktop client keeps on disk, if they exist.
///
/// Purely local discovery — no network, no cloud API. Returns absolute paths
/// that exist and are directories, newest-first by mtime when known.
pub fn cloud_sync_folders(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let candidates = [
        "Dropbox",
        "Google Drive",
        "OneDrive",
        "Nextcloud",
        "Cloud",
        "Sync",
    ];
    let mut found: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for name in candidates {
        let p = home.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.is_dir() {
                let mtime = std::fs::metadata(&p)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                found.push((p, mtime));
            }
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found.into_iter().map(|(p, _)| p).collect()
}
