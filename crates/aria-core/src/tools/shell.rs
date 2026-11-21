//! `run_shell`: non-interactive `sh -c` in the project root.
//!
//! Output is drained by two reader tasks until the pipes close; the child's exit
//! status is watched concurrently. The whole process group is killed on timeout.
//! A nonzero exit code is a normal result, not an error (issue 09).

use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{Result, ToolContext, ToolError};

/// Programs that only read. Under `auto_approve_safe` a call whose command is
/// one of these — a single simple command, no chaining or redirection — runs
/// without asking. Everything else still asks.
const READ_ONLY: &[&str] = &[
    "ast-grep", "bat", "batcat", "cat", "date", "df", "du", "echo", "fd", "fdfind", "file",
    "grep", "head", "id", "jq", "ls", "pwd", "rg", "stat", "tail", "tree", "uname", "wc",
    "whoami", "zoxide",
];

/// Shell syntax that can chain, redirect, or substitute another command, so the
/// first word is no longer the whole story.
const SHELL_SYNTAX: &[char] = &[
    '|', '&', ';', '<', '>', '$', '`', '\\', '(', ')', '{', '}', '\n', '\r',
];

/// Options that turn a read-only program into one that writes or executes
/// something else (`find -delete`, `git --ext-diff`, `rg --pre`, ...).
fn has_option(argv: &[&str], options: &[&str]) -> bool {
    argv.iter().any(|arg| {
        options.iter().any(|opt| {
            *arg == *opt
                || (opt.starts_with("--") && arg.strip_prefix(opt).is_some_and(|r| r.starts_with('=')))
        })
    })
}

fn is_read_only_command(program: &str, argv: &[&str]) -> bool {
    match program {
        "find" => !has_option(
            argv,
            &["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fls", "-fprintf"],
        ),
        // `sg` is absent on purpose: shadow-utils' `sg` runs a command.
        "fd" | "fdfind" => !argv.iter().any(|a| {
            a.starts_with("-x") || a.starts_with("-X") || a.starts_with("--exec")
        }),
        "ast-grep" => !argv.iter().any(|a| {
            a.starts_with("-r")
                || a.starts_with("--rewrite")
                || *a == "-U"
                || a.starts_with("--update-all")
                || a.starts_with("--fix")
        }),
        "zoxide" => matches!(argv.get(1).copied(), Some("query" | "list")),
        "jq" => !has_option(argv, &["-i", "--inplace"]),
        "git" => {
            matches!(
                argv.get(1).copied(),
                Some(
                    "status"
                        | "diff"
                        | "log"
                        | "show"
                        | "rev-parse"
                        | "ls-files"
                        | "grep"
                        | "blame"
                )
            ) && !has_option(
                argv,
                &["-c", "--ext-diff", "--textconv", "--exec", "--output", "--upload-pack"],
            )
        }
        "rg" => !has_option(argv, &["--pre", "--pre-glob"]),
        "date" => !has_option(argv, &["-s", "--set"]),
        other => READ_ONLY.contains(&other),
    }
}

/// Whether `cmd` is a single read-only command that `auto_approve_safe` may run
/// without an Approval. Conservative by design: quoting, variables, globs with
/// shell syntax, pipes, redirects and unknown programs all ask.
pub fn is_safe_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || cmd.chars().any(|c| SHELL_SYNTAX.contains(&c)) {
        return false;
    }
    let argv: Vec<&str> = cmd.split_whitespace().collect();
    let Some(program) = argv.first() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    is_read_only_command(program, &argv)
}

/// Whether `cmd` is a destructive/harmful command that requires user approval
/// even under `AutoApproveSafe`. Conservative by design: only well-known
/// destructive programs and clearly destructive flags are caught; chained,
/// quoted or otherwise complex commands fall through and run without asking.
pub fn is_harmful_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || cmd.chars().any(|c| SHELL_SYNTAX.contains(&c)) {
        return false;
    }
    let argv: Vec<&str> = cmd.split_whitespace().collect();
    let Some(program) = argv.first() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    match program {
        "sudo" | "su" | "doas" => true,
        "rm" => argv.iter().any(|a| {
            a.starts_with('-') && a.chars().any(|c| c == 'r' || c == 'R')
        }) || argv.iter().any(|a| *a == "/"),
        "npm" | "npx" | "yarn" | "pnpm" => argv.iter().any(|a| {
            matches!(
                *a,
                "install" | "uninstall" | "remove" | "publish" | "add" | "rm"
            )
        }),
        "dd" | "mkfs" | "mkfs.ext4" | "mkfs.vfat" | "fdisk" | "parted" => true,
        _ => false,
    }
}

/// Cap on bytes fed back to the model, split between the head and tail.
const MODEL_BYTES: usize = 32_000;

/// The user's own bin directories, appended to the app's PATH. A desktop
/// launch never reads the shell's rc files, so `~/.cargo/bin` and friends are
/// often missing even though the user's terminal has them.
fn user_path(base: Option<&std::ffi::OsStr>, home: Option<&std::path::Path>) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = base
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    let mut extra = Vec::new();
    if let Some(home) = home {
        extra.push(home.join(".cargo/bin"));
        extra.push(home.join(".local/bin"));
        extra.push(home.join("bin"));
    }
    extra.push(PathBuf::from("/usr/local/bin"));
    extra.push(PathBuf::from("/opt/homebrew/bin"));
    for dir in extra {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// PATH for shell commands: the app's PATH plus the user's own bins.
pub(crate) fn shell_path() -> std::ffi::OsString {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    user_path(std::env::var_os("PATH").as_deref(), home.as_deref())
}

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
    let outcome = execute(ctx, &args.cmd, timeout, progress, None).await?;
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
    pgid_tx: Option<mpsc::UnboundedSender<u32>>,
) -> Result<ShellOutcome> {
    let root = ctx
        .scope
        .root()
        .ok_or_else(|| ToolError::Refused("no filesystem access for shell".into()))?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .env("PATH", shell_path())
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
    // The whole group shares the leader's pid; publishing it lets a detached
    // owner (background task) kill grandchildren via `kill_group(-pid)`.
    if let Some(tx) = pgid_tx {
        let _ = tx.send(pid);
    }

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

pub(crate) async fn kill_group(pid: u32) {
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

/// Open a URL in the system's default browser (`xdg-open` on Linux, `open` on
/// macOS, `start` on Windows). No extra browser is installed — it uses what is
/// already on the device.
#[derive(Debug, Deserialize)]
pub struct BrowserOpenArgs {
    pub url: String,
}

pub fn browser_open(_ctx: &ToolContext, args: BrowserOpenArgs) -> Result<String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "cmd"
    } else {
        "xdg-open"
    };
    let mut command = StdCommand::new(opener);
    if cfg!(windows) {
        command.args(["/c", "start", "", &args.url]);
    } else {
        command.arg(&args.url);
    }
    // Detach: the browser should outlive Aria. Ignore errors — the browser may
    // simply not be installed, which the model can learn from the silence.
    let _ = command.spawn();
    Ok(format!("opened {} in the default browser", args.url))
}

#[cfg(test)]
mod tests {
    use super::{is_harmful_command, is_safe_command, user_path};

    #[test]
    fn user_path_appends_existing_user_bins_once() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cargo/bin")).unwrap();
        let base = std::ffi::OsStr::new("/usr/bin");
        let path = user_path(Some(base), Some(home.path()));
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], std::path::PathBuf::from("/usr/bin"));
        let cargo = home.path().join(".cargo/bin");
        assert_eq!(dirs.iter().filter(|d| **d == cargo).count(), 1);
        assert!(!dirs.iter().any(|d| d.ends_with(".local/bin")));
    }

    #[test]
    fn read_only_commands_are_safe() {
        for cmd in [
            "ls -la",
            "cat notes.txt",
            "grep -rn needle .",
            "rg TODO src",
            "git status",
            "git diff --stat",
            "git log --oneline -5",
            "find . -name '*.rs'",
            "echo hello",
            "/bin/ls /tmp",
            "  wc -l file.txt  ",
            "fd -e rs .",
            "fdfind -t f .",
            "ast-grep run -p foo src",
            "jq .name package.json",
            "bat README.md",
            "zoxide query proj",
            "zoxide list",
            "git grep TODO",
        ] {
            assert!(is_safe_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn writes_chains_and_execution_are_not_safe() {
        for cmd in [
            "",
            "rm -rf /",
            "git commit -m change",
            "git branch -D main",
            "git -c core.pager=sh status",
            "git diff --ext-diff",
            "git log --output=/tmp/x",
            "ls; rm -rf /",
            "ls && rm -rf /",
            "ls | sh",
            "echo x > file",
            "cat $(rm -rf /)",
            "cat `rm -rf /`",
            "ls\nrm -rf /",
            "echo $HOME",
            "find . -delete",
            "find . -exec rm {} +",
            "rg --pre=./scan .",
            "date -s tomorrow",
            "sudo ls",
            "sh -c 'echo hi'",
            "fd -x rm",
            "fd --exec rm",
            "fd -X rm",
            "ast-grep run -p foo -r bar",
            "ast-grep run -p foo --rewrite=bar",
            "ast-grep run --update-all -p foo",
            "ast-grep scan --fix",
            "sg group -c id",
            "jq -i . f.json",
            "zoxide add /tmp",
        ] {
            assert!(!is_safe_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn harmful_commands_need_approval() {
        for cmd in [
            "sudo ls",
            "sudo rm -rf /tmp/x",
            "su -c rm -rf /",
            "rm -rf /",
            "rm -rf /tmp/x",
            "rm -R /tmp/x",
            "rm --recursive /tmp/x",
            "rm /",
            "npm install",
            "npm uninstall left-pad",
            "npm publish",
            "yarn add lodash",
            "pnpm remove lodash",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sdb1",
            "fdisk /dev/sda",
        ] {
            assert!(is_harmful_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn non_harmful_commands_run_without_approval() {
        for cmd in [
            "ls -la",
            "cat notes.txt",
            "rm notes.txt",
            "rm -f notes.txt",
            "git status",
            "git commit -m change",
            "npm run build",
            "npm version patch",
            "node script.js",
            "python3 -m http.server",
            "cargo build",
            "echo hello",
        ] {
            assert!(!is_harmful_command(cmd), "{cmd}");
        }
    }
}
