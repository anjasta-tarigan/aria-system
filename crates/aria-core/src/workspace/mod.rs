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
}
