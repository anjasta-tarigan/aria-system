//! Content-addressed blob storage and the undo journal.
//!
//! Before any mutate Tool changes an existing file, the file's bytes are stored
//! as a blob and a journal row is written. Undo restores the newest journal
//! entry for a path and removes the row, so repeated undos walk back through the
//! file's history (ADR-0002).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::db::{self, Db, DbError};
use crate::workspace::{PathError, WorkspaceRoot};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("path: {0}")]
    Path(#[from] PathError),
    #[error("file is not valid UTF-8: {0}")]
    NotUtf8(String),
    #[error("nothing to undo for {0}")]
    NothingToUndo(String),
    #[error("blob for {0} is missing from the store")]
    MissingBlob(String),
}

pub type Result<T> = std::result::Result<T, JournalError>;

/// Blob directory under the app data dir plus the journal writes in `Db`.
pub struct Blobs {
    dir: PathBuf,
    db: Arc<Db>,
}

impl Blobs {
    pub fn new(dir: impl Into<PathBuf>, db: Arc<Db>) -> Self {
        let this = Self {
            dir: dir.into(),
            db,
        };
        std::fs::create_dir_all(&this.dir).ok();
        this
    }

    pub fn dir(&self) -> &Path {
        // Recreate on access: a caller may hold this before the first blob and
        // pass the path elsewhere, and tests remove temp dirs between cases.
        std::fs::create_dir_all(&self.dir).ok();
        &self.dir
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        std::fs::create_dir_all(&self.dir).ok();
        self.dir.join(hash)
    }

    /// Store `bytes` and return their sha256 hex digest.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex_digest(bytes);
        let path = self.path_for(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    pub fn read(&self, hash: &str) -> Result<Vec<u8>> {
        std::fs::read(self.path_for(hash)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                JournalError::MissingBlob(hash.to_string())
            } else {
                JournalError::Io(e)
            }
        })
    }

    /// Snapshot a file that is about to change.
    ///
    /// `rel` is the workspace-relative path. A file that does not exist yet is
    /// recorded as "did not exist", so deleting a created file can be undone.
    pub fn snapshot(&self, workspace_id: &str, root: &WorkspaceRoot, rel: &str) -> Result<()> {
        let abs = root.root().join(rel);
        let (hash, existed) = if abs.exists() {
            (self.put(&std::fs::read(&abs)?)?, true)
        } else {
            (String::new(), false)
        };
        self.db.call(|c| {
            db::insert_journal_entry(c, workspace_id, rel, &hash, existed)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Restore the previous version of `rel`, one step.
    pub fn undo(&self, workspace_id: &str, root: &WorkspaceRoot, rel: &str) -> Result<()> {
        let entry = self.db.call(|c| {
            Ok(db::journal_entries_for(c, workspace_id, rel)?
                .into_iter()
                .next())
        })?;
        let entry = entry.ok_or_else(|| JournalError::NothingToUndo(rel.to_string()))?;

        let abs = root.root().join(rel);
        if entry.existed {
            let bytes = self.read(&entry.blob_hash)?;
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, bytes)?;
        } else if abs.exists() {
            std::fs::remove_file(&abs)?;
        }

        self.db
            .call(|c| db::delete_journal_entry(c, entry.id))?;
        Ok(())
    }

    /// Whether any journal entry remains for `rel`.
    pub fn can_undo(&self, workspace_id: &str, rel: &str) -> Result<bool> {
        let has = self.db.call(|c| {
            Ok(!db::journal_entries_for(c, workspace_id, rel)?.is_empty())
        })?;
        Ok(has)
    }

    /// Workspace-relative paths with at least one undo step, most recent first.
    pub fn changed_paths(&self, workspace_id: &str) -> Result<Vec<String>> {
        Ok(self.db.call(|c| db::changed_paths(c, workspace_id))?)
    }

    /// Delete blobs that no journal row references. Returns how many were removed.
    pub fn prune(&self) -> Result<usize> {
        let referenced = self
            .db
            .call(db::referenced_blobs)?
            .into_iter()
            .collect::<HashSet<_>>();
        if !self.dir.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".tmp") || !referenced.contains(&name) {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Write `content` to `rel`, snapshotting the current version first.
    ///
    /// This is the single write path used by mutate Tools, so journaling cannot
    /// be forgotten.
    pub fn write_file(
        &self,
        workspace_id: &str,
        root: &WorkspaceRoot,
        rel: &str,
        content: &str,
    ) -> Result<()> {
        self.snapshot(workspace_id, root, rel)?;
        let abs = root.root().join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, content)?;
        Ok(())
    }

    /// Delete `rel`, snapshotting it first.
    pub fn delete_file(&self, workspace_id: &str, root: &WorkspaceRoot, rel: &str) -> Result<()> {
        self.snapshot(workspace_id, root, rel)?;
        std::fs::remove_file(root.root().join(rel))?;
        Ok(())
    }

    /// Move `from` to `to` inside the workspace, snapshotting both.
    pub fn move_file(
        &self,
        workspace_id: &str,
        root: &WorkspaceRoot,
        from: &str,
        to: &str,
    ) -> Result<()> {
        self.snapshot(workspace_id, root, from)?;
        self.snapshot(workspace_id, root, to)?;
        let dest = root.root().join(to);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(root.root().join(from), dest)?;
        Ok(())
    }

    /// Read a workspace file as UTF-8, refusing binary content.
    pub fn read_text(root: &WorkspaceRoot, rel: &str) -> Result<String> {
        let abs = root.root().join(rel);
        let bytes = std::fs::read(&abs)?;
        String::from_utf8(bytes).map_err(|_| JournalError::NotUtf8(rel.to_string()))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
