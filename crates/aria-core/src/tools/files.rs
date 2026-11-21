//! File-oriented Tools: reading, listing, searching, editing and mutation.
//! All path arguments go through [`WorkspaceRoot`]; all writes through
//! [`Blobs`], so journaling cannot be skipped.


use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{
    Result, ToolContext, ToolError, default_limit, truncate,
};
use crate::db::{self, Todo};
use crate::journal::Blobs;

#[derive(Debug, Deserialize)]
pub struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

fn invalid_utf8(path: &str) -> ToolError {
    let lower = path.to_ascii_lowercase();
    let is_image = ["png", "jpg", "jpeg", "webp", "gif", "bmp"]
        .iter()
        .any(|e| lower.ends_with(&format!(".{e}")));
    if is_image {
        ToolError::Args(format!(
            "'{path}' is a binary image file. Use the 'view_image' tool to inspect and analyze it."
        ))
    } else {
        ToolError::Args(format!("{path} is not valid UTF-8"))
    }
}

pub fn read_file(ctx: &ToolContext, args: ReadFileArgs) -> Result<String> {
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if resolved.is_dir() {
        return Err(ToolError::Args(format!("{} is a directory", args.path)));
    }
    let max = default_limit(args.max_bytes, 200_000);
    let mut file = std::fs::File::open(&resolved)?;
    let size = file.metadata()?.len();
    // Read only what can be shown, plus enough to finish a multi-byte char.
    let mut bytes = Vec::new();
    file.by_ref().take(max as u64 + 4).read_to_end(&mut bytes)?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let bytes = e.into_bytes();
            let valid = std::str::from_utf8(&bytes)
                .map(str::len)
                .unwrap_or_else(|e| e.valid_up_to());
            // Being cut mid-character past `max` is fine; invalid bytes inside
            // the part we would show are not.
            if size <= max as u64 || valid < max {
                return Err(invalid_utf8(&args.path));
            }
            String::from_utf8(bytes[..valid].to_vec()).expect("validated UTF-8 prefix")
        }
    };
    if size <= max as u64 {
        return Ok(text);
    }
    let mut cut = max.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(format!(
        "{}\n… [truncated {cut} of {size} bytes shown]",
        &text[..cut]
    ))
}

#[derive(Debug, Deserialize)]
pub struct ListDirArgs {
    #[serde(default = "dot")]
    pub path: String,
}

fn dot() -> String {
    ".".to_string()
}

/// Stop after this many entries so a directory with tens of thousands of files
/// cannot hang the Run or exhaust memory.
const MAX_DIR_ENTRIES: usize = 10_000;

/// Cap on todo list items so a run cannot bloat the database with thousands
/// of entries.
const MAX_TODO_ITEMS: usize = 500;

pub fn list_dir(ctx: &ToolContext, args: ListDirArgs) -> Result<String> {
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if !resolved.is_dir() {
        return Err(ToolError::Args(format!("{} is not a directory", args.path)));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut count = 0usize;
    for entry in std::fs::read_dir(&resolved)? {
        let entry = entry?;
        count += 1;
        if count > MAX_DIR_ENTRIES {
            return Err(ToolError::Args(format!(
                "{} has more than {MAX_DIR_ENTRIES} entries; narrow the path",
                args.path
            )));
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            dirs.push(format!("{name}/"));
        } else {
            files.push(format!("{name}\t{} bytes", meta.len()));
        }
    }
    dirs.sort();
    files.sort();
    dirs.extend(files);
    if dirs.is_empty() {
        Ok(format!("{} is empty", args.path))
    } else {
        Ok(dirs.join("\n"))
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchArgs {
    pub pattern: String,
    /// Directory to search. Defaults to the allowed roots (or the base dir).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// Stop after this many files so a search from `/` or a home directory cannot
/// hang the Run. The model can narrow the search with `path` or `glob`.
// shortcut: fixed 20k-file walk cap; make it time-based if large repos suffer.
const MAX_SCAN: usize = 20_000;

/// Search with ripgrep when it is installed. Returns `None` when `rg` is
/// missing, cannot start, or fails, so the caller falls back to the walker.
fn search_with_rg(
    pattern: &str,
    glob: Option<&str>,
    bases: &[PathBuf],
    display_root: Option<&Path>,
    max: usize,
) -> Option<String> {
    if bases.is_empty() {
        return None;
    }
    let mut command = Command::new("rg");
    command.args([
        "--line-number",
        "--no-heading",
        "--color",
        "never",
        "--null",
        "--no-messages",
    ]);
    if let Some(glob) = glob {
        command.arg("--glob").arg(glob);
    }
    command.arg("-e").arg(pattern).arg("--").args(bases);
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());

    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut hits = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        let entry = String::from_utf8_lossy(&line);
        let entry = entry.trim_end_matches(['\n', '\r']);
        // `--null` prints `path\0line:text`.
        let Some((path, rest)) = entry.split_once('\0') else {
            continue;
        };
        let shown = display_root
            .and_then(|root| Path::new(path).strip_prefix(root).ok())
            .unwrap_or_else(|| Path::new(path));
        let (number, text) = rest.split_once(':').unwrap_or(("?", rest));
        hits.push(format!(
            "{}:{}: {}",
            shown.display(),
            number,
            truncate(text.to_string(), 400)
        ));
        if hits.len() >= max {
            let _ = child.kill();
            break;
        }
    }
    let status = child.wait().ok()?;
    // Exit code 2 is an rg error (bad glob, unreadable root): let the walker
    // report the real thing instead of claiming there were no matches.
    if status.code() == Some(2) {
        return None;
    }
    if hits.is_empty() {
        Some("no matches".to_string())
    } else {
        Some(hits.join("\n"))
    }
}

pub fn search_files(ctx: &ToolContext, args: SearchArgs) -> Result<String> {
    let scope = &ctx.scope;
    let re = regex::Regex::new(&args.pattern)
        .map_err(|e| ToolError::Args(format!("invalid regex: {e}")))?;
    let glob = match &args.glob {
        Some(g) => Some(
            globset::Glob::new(g)
                .map_err(|e| ToolError::Args(format!("invalid glob: {e}")))?
                .compile_matcher(),
        ),
        None => None,
    };
    let max = default_limit(args.max_results, 200);
    let bases = match &args.path {
        Some(p) => vec![scope.resolve_read(p)?],
        None => scope.roots(),
    };
    let multi = bases.len() > 1;
    // One base: show paths relative to the conversation root, even when a
    // subdirectory was named, so hits stay identifiable.
    let display_root = if multi { None } else { scope.root() };

    if let Some(text) = search_with_rg(
        &args.pattern,
        args.glob.as_deref(),
        &bases,
        display_root.as_deref(),
        max,
    ) {
        return Ok(text);
    }

    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let mut capped = false;
    'outer: for base in &bases {
        let walker = ignore::WalkBuilder::new(base)
            .standard_filters(true)
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_SCAN {
                capped = true;
                break 'outer;
            }
            let target = path.strip_prefix(base).unwrap_or(path);
            if let Some(m) = &glob
                && !m.is_match(target)
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue; // binary or unreadable
            };
            let shown = match &display_root {
                Some(root) => path.strip_prefix(root).unwrap_or(path),
                None => path,
            };
            for (n, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{}:{}: {}", shown.display(), n + 1, truncate(line.to_string(), 400)));
                    if hits.len() >= max {
                        break 'outer;
                    }
                }
            }
        }
    }
    let note = if capped {
        format!("\n… [stopped after {MAX_SCAN} files; narrow it with `path` or `glob`]")
    } else {
        String::new()
    };
    if hits.is_empty() {
        Ok(format!("no matches{note}"))
    } else {
        Ok(format!("{}{note}", hits.join("\n")))
    }
}

#[derive(Debug, Deserialize)]
pub struct GlobArgs {
    /// Glob pattern, e.g. `**/*.rs` or `src/**/*.{ts,tsx}`.
    pub pattern: String,
    /// Directory to search. Defaults to the allowed roots (or the base dir).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// List files whose paths match a glob pattern. Respects .gitignore.
pub fn glob_files(ctx: &ToolContext, args: GlobArgs) -> Result<String> {
    let scope = &ctx.scope;
    let glob = globset::Glob::new(&args.pattern)
        .map_err(|e| ToolError::Args(format!("invalid glob: {e}")))?
        .compile_matcher();
    let max = default_limit(args.max_results, 200);
    let bases = match &args.path {
        Some(p) => vec![scope.resolve_read(p)?],
        None => scope.roots(),
    };
    let multi = bases.len() > 1;
    let display_root = if multi { None } else { scope.root() };

    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let mut capped = false;
    'outer: for base in &bases {
        let walker = ignore::WalkBuilder::new(base)
            .standard_filters(true)
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_SCAN {
                capped = true;
                break 'outer;
            }
            let target = path.strip_prefix(base).unwrap_or(path);
            if !glob.is_match(target) {
                continue;
            }
            let shown = match &display_root {
                Some(root) => path.strip_prefix(root).unwrap_or(path),
                None => path,
            };
            hits.push(shown.display().to_string());
            if hits.len() >= max {
                break 'outer;
            }
        }
    }
    let note = if capped {
        format!("\n… [stopped after {MAX_SCAN} files; narrow it with `path`]")
    } else {
        String::new()
    };
    if hits.is_empty() {
        Ok(format!("no matches{note}"))
    } else {
        Ok(format!("{}{note}", hits.join("\n")))
    }
}

#[derive(Debug, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub search: String,
    pub replace: String,
}

pub fn edit_file(ctx: &ToolContext, args: EditArgs) -> Result<String> {
    if args.search.is_empty() {
        return Err(ToolError::Args("search text must not be empty".into()));
    }
    let resolved = ctx.scope.resolve_create(&args.path)?;
    let base = ctx.scope.root().ok_or_else(|| {
        ToolError::Refused("no filesystem access".into())
    })?;
    let rel = ctx.scope.relativise(&resolved)?;
    let current = Blobs::read_text(&base, &rel)?;
    let count = current.matches(&args.search).count();
    if count != 1 {
        return Err(ToolError::Args(format!(
            "search text occurs {count} times in {}, expected exactly once",
            args.path
        )));
    }
    let updated = current.replacen(&args.search, &args.replace, 1);
    ctx.blobs.write_file(
        &ctx.journal_id,
        &base,
        &rel,
        &updated,
    )?;
    Ok(format!("edited {rel} (1 replacement)"))
}

#[derive(Debug, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
}

pub fn write_file(ctx: &ToolContext, args: WriteFileArgs) -> Result<String> {
    let resolved = ctx.scope.resolve_create(&args.path)?;
    if resolved.exists() {
        return Err(ToolError::Args(format!(
            "{} already exists; use edit_file to change it",
            args.path
        )));
    }
    let base = ctx.scope.root().ok_or_else(|| {
        ToolError::Refused("no filesystem access".into())
    })?;
    let rel = ctx.scope.relativise(&resolved)?;
    ctx.blobs.write_file(
        &ctx.journal_id,
        &base,
        &rel,
        &args.content,
    )?;
    Ok(format!("created {rel} ({} bytes)", args.content.len()))
}

#[derive(Debug, Deserialize)]
pub struct PathArg {
    pub path: String,
}

pub fn delete_file(ctx: &ToolContext, args: PathArg) -> Result<String> {
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if resolved.is_dir() {
        return Err(ToolError::Args("delete_file only removes files".into()));
    }
    let base = ctx.scope.root().ok_or_else(|| {
        ToolError::Refused("no filesystem access".into())
    })?;
    let rel = ctx.scope.relativise(&resolved)?;
    ctx.blobs.delete_file(
        &ctx.journal_id,
        &base,
        &rel,
    )?;
    Ok(format!("deleted {rel}"))
}

#[derive(Debug, Deserialize)]
pub struct MoveArgs {
    pub from: String,
    pub to: String,
}

pub fn move_file(ctx: &ToolContext, args: MoveArgs) -> Result<String> {
    let from = ctx.scope.resolve_read(&args.from)?;
    if from.is_dir() {
        return Err(ToolError::Args("move_file only moves files".into()));
    }
    let to = ctx.scope.resolve_create(&args.to)?;
    if to.exists() {
        return Err(ToolError::Args(format!("{} already exists", args.to)));
    }
    let base = ctx.scope.root().ok_or_else(|| {
        ToolError::Refused("no filesystem access".into())
    })?;
    let from_rel = ctx.scope.relativise(&from)?;
    let to_rel = ctx.scope.relativise(&to)?;
    ctx.blobs.move_file(
        &ctx.journal_id,
        &base,
        &from_rel,
        &to_rel,
    )?;
    Ok(format!("moved {from_rel} -> {to_rel}"))
}

#[derive(Debug, Deserialize)]
pub struct TodoItem {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Deserialize)]
pub struct TodosArgs {
    #[serde(default)]
    pub items: Vec<TodoItem>,
}

pub fn todos(ctx: &ToolContext, args: TodosArgs) -> Result<String> {
    if args.items.len() > MAX_TODO_ITEMS {
        return Err(ToolError::Args(format!(
            "todo list has {} items, over the {MAX_TODO_ITEMS}-item limit",
            args.items.len()
        )));
    }
    let items: Vec<(String, bool)> = args
        .items
        .into_iter()
        .map(|i| (i.text, i.done))
        .collect();
    let saved: Vec<Todo> = ctx
        .db
        .call(|c| db::replace_todos(c, &ctx.run_id, &items))?;
    if saved.is_empty() {
        return Ok("todo list cleared".to_string());
    }
    let mut out = String::from("todo list:\n");
    for t in &saved {
        out.push_str(&format!(
            "{} {}\n",
            if t.done { "[x]" } else { "[ ]" },
            t.text
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod glob_tests {
    use super::*;
    use crate::db::Db;
    use crate::journal::Blobs;
    use crate::workspace::{Scope, WorkspaceRoot};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn ctx_for(dir: &std::path::Path) -> ToolContext {
        let root = WorkspaceRoot::new(dir).unwrap();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let blobs = Arc::new(Blobs::new(dir, Arc::clone(&db)));
        ToolContext {
            scope: Scope::Workspace(root),
            root: Some(WorkspaceRoot::new(dir).unwrap()),
            workspace_id: Some("ws".into()),
            journal_id: "test".into(),
            run_id: "test".into(),
            db,
            blobs,
            shell_timeout_secs: 30,
            embed: None,
            reranker: None,
            reranker_top_k: 50,
            search: None,
            vision: None,
            background: None,
            disabled_tools: Vec::new(),
        }
    }

    #[test]
    fn glob_files_matches_pattern() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        std::fs::write(dir.path().join("b.rs"), "y").unwrap();
        std::fs::write(dir.path().join("c.txt"), "z").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("d.rs"), "w").unwrap();

        let ctx = ctx_for(dir.path());
        let out = glob_files(&ctx, GlobArgs {
            pattern: "**/*.rs".into(),
            path: None,
            max_results: None,
        }).unwrap();

        assert!(out.contains("a.rs"), "output: {out}");
        assert!(out.contains("b.rs"), "output: {out}");
        assert!(out.contains("sub/d.rs"), "output: {out}");
        assert!(!out.contains("c.txt"), "output: {out}");
    }

    #[test]
    fn glob_files_no_matches() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let ctx = ctx_for(dir.path());
        let out = glob_files(&ctx, GlobArgs {
            pattern: "**/*.rs".into(),
            path: None,
            max_results: None,
        }).unwrap();

        assert_eq!(out, "no matches");
    }

    #[test]
    fn glob_files_invalid_pattern_errors() {
        let dir = tempdir().unwrap();
        let ctx = ctx_for(dir.path());
        let err = glob_files(&ctx, GlobArgs {
            pattern: "[".into(),
            path: None,
            max_results: None,
        });
        assert!(err.is_err());
    }

    #[test]
    fn list_dir_caps_entries() {
        let dir = tempdir().unwrap();
        for i in 0..MAX_DIR_ENTRIES + 5 {
            std::fs::write(dir.path().join(format!("file_{i:05}.txt")), "x").unwrap();
        }
        let ctx = ctx_for(dir.path());
        let err = list_dir(&ctx, ListDirArgs { path: ".".into() });
        assert!(err.is_err());
    }

    #[test]
    fn todos_caps_items() {
        let dir = tempdir().unwrap();
        let ctx = ctx_for(dir.path());
        let items: Vec<TodoItem> = (0..MAX_TODO_ITEMS + 5)
            .map(|i| TodoItem { text: format!("item {i}"), done: false })
            .collect();
        let err = todos(&ctx, TodosArgs { items });
        assert!(err.is_err());
    }
}
