//! File-oriented Tools: reading, listing, searching, editing and mutation.
//! All path arguments go through [`WorkspaceRoot`]; all writes through
//! [`Blobs`], so journaling cannot be skipped.

use std::path::Path;

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

pub fn read_file(ctx: &ToolContext, args: ReadFileArgs) -> Result<String> {
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if resolved.is_dir() {
        return Err(ToolError::Args(format!("{} is a directory", args.path)));
    }
    let bytes = std::fs::read(&resolved)?;
    let max = default_limit(args.max_bytes, 200_000);
    let text = String::from_utf8(bytes).map_err(|_| {
        let lower = args.path.to_ascii_lowercase();
        let is_image = ["png", "jpg", "jpeg", "webp", "gif", "bmp"]
            .iter()
            .any(|e| lower.ends_with(&format!(".{e}")));
        if is_image {
            ToolError::Args(format!(
                "'{}' is a binary image file. Use the 'view_image' tool to inspect and analyze it.",
                args.path
            ))
        } else {
            ToolError::Args(format!("{} is not valid UTF-8", args.path))
        }
    })?;
    Ok(truncate(text, max))
}

#[derive(Debug, Deserialize)]
pub struct ListDirArgs {
    #[serde(default = "dot")]
    pub path: String,
}

fn dot() -> String {
    ".".to_string()
}

pub fn list_dir(ctx: &ToolContext, args: ListDirArgs) -> Result<String> {
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if !resolved.is_dir() {
        return Err(ToolError::Args(format!("{} is not a directory", args.path)));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&resolved)? {
        let entry = entry?;
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
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
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

    let mut hits = Vec::new();
    let base = scope.root().unwrap_or_else(|| Path::new("."));
    let walker = ignore::WalkBuilder::new(base)
        .standard_filters(true)
        .build();
    'outer: for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = path.strip_prefix(base).unwrap_or(path);
        if let Some(m) = &glob
            && !m.is_match(rel)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // binary or unreadable
        };
        for (n, line) in text.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!("{}:{}: {}", rel.display(), n + 1, truncate(line.to_string(), 400)));
                if hits.len() >= max {
                    break 'outer;
                }
            }
        }
    }
    if hits.is_empty() {
        Ok("no matches".to_string())
    } else {
        Ok(hits.join("\n"))
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
    let current = Blobs::read_text(base, &rel)?;
    let count = current.matches(&args.search).count();
    if count != 1 {
        return Err(ToolError::Args(format!(
            "search text occurs {count} times in {}, expected exactly once",
            args.path
        )));
    }
    let updated = current.replacen(&args.search, &args.replace, 1);
    ctx.blobs.write_file(
        ctx.workspace_id.as_deref().unwrap_or(""),
        base,
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
        ctx.workspace_id.as_deref().unwrap_or(""),
        base,
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
        ctx.workspace_id.as_deref().unwrap_or(""),
        base,
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
        ctx.workspace_id.as_deref().unwrap_or(""),
        base,
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
