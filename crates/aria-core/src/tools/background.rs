//! Long-running shell commands that outlive a single Run.
//!
//! `start` detaches a `sh -c` process, streams its combined output to a temp
//! log under the blob dir, and on exit records the final `BackgroundTask` row.
//! `poll` returns the row plus a tail of the log so the agent or UI can watch
//! progress. `kill` takes down the process group (grandchildren included).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::db::{self, Db};
use crate::journal::Blobs;
use crate::tools::shell;
use crate::workspace::{Scope, WorkspaceRoot};

use super::{BackgroundStatus, BackgroundTask, Result, ToolContext, ToolError};

/// Reap a background task that runs longer than this; a hung command would
/// otherwise stream into the temp log forever.
// shortcut: 8h cap reaps runaway tasks; raise it for legitimately longer jobs.
/// The temp log can still grow large within the cap (not bounded today).
const MAX_RUNTIME: Duration = Duration::from_secs(8 * 3600);

/// Owns background tasks for one conversation's workspace root.
pub struct BackgroundTasks {
    pub db: Arc<Db>,
    pub blobs: Arc<Blobs>,
    pub root: PathBuf,
    pub scope: Scope,
    pub shell_timeout_secs: u64,
}

impl BackgroundTasks {
    /// Start `cmd` detached. Returns immediately with the running task row.
    ///
    /// Refuses unless the scope is a single workspace folder (`None` and
    /// `MainSystem` are rejected: we only run in a known, contained root).
    pub fn start(&self, conversation_id: &str, cmd: &str) -> Result<BackgroundTask> {
        if !matches!(self.scope, Scope::Workspace(_)) {
            return Err(ToolError::Refused(
                "background tasks require an open workspace folder".into(),
            ));
        }
        let id = db::uuid_v7();
        let task = BackgroundTask {
            id: id.clone(),
            cmd: cmd.to_string(),
            conversation_id: conversation_id.to_string(),
            started_at: db::now_ms(),
            ended_at: None,
            exit_code: None,
            status: BackgroundStatus::Running,
            log_blob: String::new(),
        };
        self.db.call(|c| db::insert_background_task(c, &task))?;

        // Minimal context: no progress sink, a runtime cap well above the normal
        // 120s kill so only MAX_RUNTIME reaps it. The detached task needs no
        // embed/search/vision/background capability.
        let ctx = ToolContext {
            scope: self.scope.clone(),
            root: self
                .scope
                .root()
                .map(WorkspaceRoot::new)
                .transpose()?,
            workspace_id: None,
            run_id: id.clone(),
            db: Arc::clone(&self.db),
            blobs: Arc::clone(&self.blobs),
            shell_timeout_secs: self.shell_timeout_secs,
            embed: None,
            search: None,
            vision: None,
            background: None,
        };

        let db = Arc::clone(&self.db);
        let blobs = Arc::clone(&self.blobs);
        let temp = self.temp_log_path(&id);
        let pid_path = self.temp_pid_path(&id);
        let cmd = cmd.to_string();
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let (pgid_tx, mut pgid_rx) = mpsc::unbounded_channel::<u32>();
            let mut file = match tokio::fs::File::create(&temp).await {
                Ok(f) => f,
                Err(_) => return,
            };
            let writer = tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if tokio::io::AsyncWriteExt::write_all(&mut file, line.as_bytes()).await.is_err()
                        || tokio::io::AsyncWriteExt::write_all(&mut file, b"\n").await.is_err()
                    {
                        break;
                    }
                }
            });
            // Record the process-group leader pid so kill() can target it.
            let pid_saver = {
                let pid_path = pid_path.clone();
                tokio::spawn(async move {
                    if let Some(pid) = pgid_rx.recv().await {
                        let _ = std::fs::write(&pid_path, pid.to_string());
                    }
                })
            };
            // Large deadline: only MAX_RUNTIME caps a runaway task.
            let outcome = shell::execute(&ctx, &cmd, MAX_RUNTIME, Some(tx), Some(pgid_tx)).await;
            let _ = writer.await;
            let _ = pid_saver.await;

            let (exit_code, status) = match outcome {
                Ok(o) => (
                    o.exit_code,
                    if o.exit_code == Some(0) {
                        BackgroundStatus::Succeeded
                    } else if o.timed_out {
                        BackgroundStatus::Killed
                    } else {
                        BackgroundStatus::Failed
                    },
                ),
                Err(_) => (None, BackgroundStatus::Failed),
            };
            let log = std::fs::read(&temp).unwrap_or_default();
            let log_blob = blobs.put(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&temp);
            let _ = std::fs::remove_file(&pid_path);
            let _ = db.call(|c| {
                db::update_background_task(c, &id, db::now_ms(), exit_code, status, &log_blob)
            });
        });

        Ok(task)
    }

    /// Return the task row plus the last ~2KB of its log (temp file while
    /// running, the stored blob once finished).
    pub fn poll(&self, id: &str) -> Result<(BackgroundTask, String)> {
        let task = self.db.call(|c| db::get_background_task(c, id))?;
        let bytes = if task.status == BackgroundStatus::Running {
            std::fs::read(self.temp_log_path(id)).unwrap_or_default()
        } else {
            self.blobs.read(&task.log_blob).unwrap_or_default()
        };
        Ok((task, tail_text(&bytes)))
    }

    /// All tasks for a conversation, newest first.
    pub fn list(&self, conversation_id: &str) -> Result<Vec<BackgroundTask>> {
        Ok(self
            .db
            .call(|c| db::list_background_tasks(c, conversation_id))?)
    }

    /// Kill the task's process group. A `Running` task becomes `Killed`; a
    /// finished one is untouched.
    pub async fn kill(&self, id: &str) -> Result<BackgroundTask> {
        let task = self.db.call(|c| db::get_background_task(c, id))?;
        if task.status != BackgroundStatus::Running {
            return Ok(task);
        }
        // The temp pid file holds the live process-group leader (negative-pid
        // kill reaps grandchildren too).
        if let Some(pid) = std::fs::read_to_string(self.temp_pid_path(id))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            shell::kill_group(pid).await;
        }
        let _ = std::fs::remove_file(self.temp_pid_path(id));
        let _ = std::fs::remove_file(self.temp_log_path(id));
        self.db.call(|c| {
            db::update_background_task(
                c,
                id,
                db::now_ms(),
                None,
                BackgroundStatus::Killed,
                &task.log_blob,
            )
        })?;
        Ok(self.db.call(|c| db::get_background_task(c, id))?)
    }

    fn temp_log_path(&self, id: &str) -> PathBuf {
        self.blobs.dir().join(format!("bg-{id}.log"))
    }

    fn temp_pid_path(&self, id: &str) -> PathBuf {
        self.blobs.dir().join(format!("bg-{id}.pid"))
    }
}

/// Last ~2000 bytes of a log as a string, falling back to the whole thing.
fn tail_text(bytes: &[u8]) -> String {
    let tail = 2000;
    let start = bytes.len().saturating_sub(tail);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
