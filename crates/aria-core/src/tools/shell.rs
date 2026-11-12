//! `run_shell`: non-interactive `sh -c` in the project root.
//!
//! Output is drained by two reader tasks until the pipes close; the child's exit
//! status is watched concurrently. The whole process group is killed on timeout.
//! A nonzero exit code is a normal result, not an error (issue 09).

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{Result, ToolContext, ToolError};

/// Cap on bytes fed back to the model, split between the head and tail.
const MODEL_BYTES: usize = 32_000;

#[derive(Debug, Deserialize)]
pub struct ShellArgs {
    pub cmd: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Run a shell command. `progress` receives each output line as it arrives, so
/// the UI can stream it; the final result carries the truncated text.
pub async fn run_shell(
    ctx: &ToolContext,
    args: ShellArgs,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<String> {
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(ctx.shell_timeout_secs).max(1));
    let outcome = execute(ctx, &args.cmd, timeout, progress).await?;
    Ok(format_outcome(&args.cmd, &outcome))
}

enum Line {
    Stdout(String),
    Stderr(String),
}

pub async fn execute(
    ctx: &ToolContext,
    cmd: &str,
    timeout: Duration,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<ShellOutcome> {
    let (root, _) = ctx.folder()?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(root.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Own process group so a timeout can kill grandchildren too.
        .process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| ToolError::Refused(format!("cannot start shell: {e}")))?;
    let pid = child.id().unwrap_or_default();

    let (tx, mut rx) = mpsc::unbounded_channel::<Line>();
    let mut readers = 0;
    if let Some(pipe) = child.stdout.take() {
        spawn_reader(pipe, Line::Stdout, tx.clone());
        readers += 1;
    }
    if let Some(pipe) = child.stderr.take() {
        spawn_reader(pipe, Line::Stderr, tx.clone());
        readers += 1;
    }
    drop(tx);

    let deadline = tokio::time::Instant::now() + timeout;
    let mut lines = Vec::new();
    let mut status: Option<std::process::ExitStatus> = None;
    let mut timed_out = false;

    if readers == 0 {
        let s = child
            .wait()
            .await
            .map_err(|e| ToolError::Refused(e.to_string()))?;
        return Ok(finish(s.code(), lines, false));
    }

    loop {
        if status.is_some() && readers == 0 {
            break;
        }
        tokio::select! {
            biased;
            line = rx.recv(), if readers > 0 => match line {
                Some(line) => {
                    emit(&line, &progress);
                    lines.push(line);
                }
                None => readers = 0,
            },
            exit = child.wait(), if status.is_none() => {
                status = Some(exit.map_err(|e| ToolError::Refused(e.to_string()))?);
            }
            _ = tokio::time::sleep_until(deadline) => {
                // A reader still following a pipe held open by a surviving
                // grandchild must not keep us here.
                timed_out = true;
                kill_group(pid).await;
                let _ = child.kill().await;
                status = child.wait().await.ok();
                break;
            }
        }
    }

    Ok(finish(status.and_then(|s| s.code()), lines, timed_out))
}

fn spawn_reader<R>(pipe: R, wrap: fn(String) -> Line, tx: mpsc::UnboundedSender<Line>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let text = line.trim_end_matches(['\n', '\r']).to_string();
                    if tx.send(wrap(text)).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

fn emit(line: &Line, progress: &Option<mpsc::UnboundedSender<String>>) {
    if let Some(tx) = progress {
        match line {
            Line::Stdout(l) => {
                let _ = tx.send(l.clone());
            }
            Line::Stderr(l) => {
                let _ = tx.send(format!("[stderr] {l}"));
            }
        }
    }
}

async fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

fn finish(exit_code: Option<i32>, lines: Vec<Line>, timed_out: bool) -> ShellOutcome {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for line in lines {
        match line {
            Line::Stdout(l) => {
                stdout.push_str(&l);
                stdout.push('\n');
            }
            Line::Stderr(l) => {
                stderr.push_str(&l);
                stderr.push('\n');
            }
        }
    }
    ShellOutcome {
        exit_code,
        stdout,
        stderr,
        timed_out,
    }
}

fn format_outcome(cmd: &str, out: &ShellOutcome) -> String {
    let mut text = format!("$ {cmd}\n");
    if !out.stdout.is_empty() {
        text.push_str(&head_tail(&out.stdout));
    }
    if !out.stderr.is_empty() {
        text.push_str("stderr:\n");
        text.push_str(&head_tail(&out.stderr));
    }
    match (out.timed_out, out.exit_code) {
        (true, _) => text.push_str("\n[timed out; process group killed]"),
        (false, Some(0)) => text.push_str("\nexit: 0"),
        (false, Some(code)) => text.push_str(&format!("\nexit: {code}")),
        (false, None) => text.push_str("\nexit: killed by signal"),
    }
    text
}

/// Keep the first and last half of the output so both the start of a command
/// and its summary stay visible to the model.
fn head_tail(text: &str) -> String {
    if text.len() <= MODEL_BYTES {
        return text.to_string();
    }
    let half = MODEL_BYTES / 2;
    let mut start = half;
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = text.len() - half;
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    format!(
        "{}\n… [{} bytes omitted] …\n{}",
        &text[..start],
        text.len() - start - (text.len() - end),
        &text[end..]
    )
}
