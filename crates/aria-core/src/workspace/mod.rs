//! Workspace root resolution and path containment.
//!
//! Every path a Tool touches goes through this module. Containment is enforced
//! here, never in the prompt (issue 03).

use std::env;
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
        let scope = Scope::MainSystem(vec![]);
        let abs = std::fs::canonicalize("/").unwrap();
        assert!(scope.contains(&abs));
    }

    #[test]
    fn scope_main_system_paths_restricts() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let scope = Scope::MainSystem(vec![std::fs::canonicalize(dir.path()).unwrap()]);
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
    MainSystem(Vec<PathBuf>),
}

impl Scope {
    /// The base directory for recursive searches, or `None` if filesystem access
    /// is disabled. For the main system this is the first allowed root, or the
    /// working dir when unrestricted.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Scope::Workspace(w) => Some(w.root()),
            Scope::MainSystem(paths) => paths.first().map(|p| p.as_path()),
            Scope::None => None,
        }
    }

    /// All roots to walk for a recursive search: the folder root, or the
    /// permitted system paths (current directory if unrestricted).
    pub fn roots(&self) -> Vec<PathBuf> {
        match self {
            Scope::Workspace(w) => vec![w.root().to_path_buf()],
            Scope::MainSystem(paths) => {
                if paths.is_empty() {
                    vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
                } else {
                    paths.clone()
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
    /// the main system the path must be absolute or relative to the working dir.
    pub fn resolve_read(&self, requested: impl AsRef<Path>) -> Result<PathBuf> {
        match self {
            Scope::None => Err(PathError::Escapes("no filesystem access".into())),
            Scope::Workspace(w) => w.resolve_read(requested),
            Scope::MainSystem(paths) => {
                let candidate = if requested.as_ref().is_absolute() {
                    requested.as_ref().to_path_buf()
                } else {
                    env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(requested.as_ref())
                };
                let resolved = std::fs::canonicalize(&candidate)?;
                if paths.is_empty() || paths.iter().any(|p| resolved.starts_with(p)) {
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
            Scope::MainSystem(paths) => {
                let candidate = if requested.as_ref().is_absolute() {
                    requested.as_ref().to_path_buf()
                } else {
                    env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(requested.as_ref())
                };
                if candidate.exists() || std::fs::symlink_metadata(&candidate).is_ok() {
                    let resolved = std::fs::canonicalize(&candidate)?;
                    if paths.is_empty() || paths.iter().any(|p| resolved.starts_with(p)) {
                        Ok(resolved)
                    } else {
                        Err(PathError::Escapes(resolved.display().to_string()))
                    }
                } else {
                    let parent = candidate
                        .parent()
                        .ok_or_else(|| PathError::MissingParent(candidate.display().to_string()))?;
                    let parent = std::fs::canonicalize(parent)?;
                    if paths.is_empty() || paths.iter().any(|p| parent.starts_with(p)) {
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
            Scope::MainSystem(paths) => {
                paths.is_empty() || paths.iter().any(|p| abs.starts_with(p))
            }
        }
    }

    /// Convert an absolute resolved path back to a stable relative key for the
    /// edit journal. For the workspace it is folder-relative; for the main system
    /// it is relative to the first allowed root (or the working dir when unrestricted).
    pub fn relativise(&self, absolute: &Path) -> Result<String> {
        match self {
            Scope::Workspace(w) => w.relativise(absolute),
            Scope::MainSystem(paths) => {
                let base = if paths.is_empty() {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                } else {
                    paths[0].clone()
                };
                let rel = absolute
                    .strip_prefix(&base)
                    .map_err(|_| PathError::Escapes(absolute.display().to_string()))?;
                Ok(rel.to_string_lossy().replace('\\', "/"))
            }
            Scope::None => Err(PathError::Escapes(absolute.display().to_string())),
        }
    }
}
