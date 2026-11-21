//! Document-to-Markdown conversion.
//!
//! Architecture inspired by Microsoft MarkItDown: a dispatcher over
//! per-format converters, each producing structured Markdown
//! (headings, tables, lists). Built-in converters run offline; no cloud
//! dependency and no Python runtime required.
//!
//! Formats supported:
//! * `.docx` — WordprocessingML parsed natively from the ZIP archive
//! * `.pdf` — `pdftotext` with layout preservation + heading heuristics
//! * `.txt` — passed through as-is
//!
//! Unrecognized formats return `Ok(None)`, so callers can fall back to raw
//! text extraction.

mod docx;

use std::process::Command;

/// Convert document bytes to Markdown.
///
/// Returns `Ok(Some(markdown))` when the format is supported and conversion
/// succeeded, `Ok(None)` when the format is not recognized (caller falls back
/// to raw text), and `Err` when the format is recognized but conversion failed.
pub fn convert(bytes: &[u8], mime_type: &str) -> Result<Option<String>, String> {
    match mime_type {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            docx::convert_docx(bytes).map(Some)
        }
        "application/pdf" => convert_pdf(bytes).map(Some),
        "text/plain" => Ok(Some(String::from_utf8_lossy(bytes).trim().to_string())),
        _ => Ok(None),
    }
}

/// Extract text from a PDF document's bytes by shelling out to `pdftotext`,
/// then restructure the output into Markdown: detect headings, preserve
/// paragraph breaks, and format simple tables as pipe tables.
fn convert_pdf(bytes: &[u8]) -> Result<String, String> {
    use std::io::Write;

    let tmp_path = std::env::temp_dir().join(format!("aria-pdf-{}.pdf", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
    }

    // `-layout` preserves columns and spacing so tables and headings are
    // easier to detect from the raw text.
    let output = Command::new("pdftotext")
        .arg("-layout")
        .arg(&tmp_path)
        .arg("-")
        .output()
        .map_err(|e| e.to_string())?;

    let _ = std::fs::remove_file(&tmp_path);

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    pdf_text_to_markdown(&text)
}

/// Turn `pdftotext -layout` output into Markdown.
fn pdf_text_to_markdown(text: &str) -> Result<String, String> {
    let mut md = String::new();
    let mut lines: Vec<&str> = text.lines().collect();

    // Drop leading/trailing blank lines.
    while !lines.is_empty() && lines.first().map(|l| l.trim().is_empty()).unwrap_or(true) {
        lines.remove(0);
    }
    while !lines.is_empty() && lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
        lines.pop();
    }

    let mut i = 0;
    let mut para: Vec<String> = Vec::new();
    let flush_para = |md: &mut String, para: &mut Vec<String>| {
        if !para.is_empty() {
            md.push_str(&para.join(" "));
            md.push_str("\n\n");
            para.clear();
        }
    };

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if trimmed.is_empty() {
            // Paragraph break.
            flush_para(&mut md, &mut para);
            i += 1;
            continue;
        }

        // Heuristic: a table row has 3+ space-separated fields that look like
        // data (numbers or short words). Collect consecutive table rows.
        // Checked before headings so a short header row ("Name  Age  City")
        // isn't mistaken for a heading.
        if looks_like_table_row(trimmed) {
            flush_para(&mut md, &mut para);
            let mut rows: Vec<Vec<String>> = Vec::new();
            while i < lines.len() && looks_like_table_row(lines[i].trim()) {
                rows.push(split_layout_row(lines[i].trim()));
                i += 1;
            }
            md.push_str(&render_markdown_table(&rows));
            md.push_str("\n\n");
            continue;
        }

        // Heuristic: a heading is a short line (under ~80 chars), not ending
        // in punctuation, and either all-caps or title-case. Headings in
        // -layout output are often on their own line with extra spacing.
        if is_pdf_heading(trimmed) {
            flush_para(&mut md, &mut para);
            // Determine heading level: ALL CAPS => H1, Title Case => H2.
            let level = if trimmed.chars().any(|c| c.is_ascii_lowercase()) {
                "##"
            } else {
                "#"
            };
            md.push_str(&format!("{} {}\n\n", level, trimmed));
            i += 1;
            continue;
        }

        // Regular paragraph line.
        para.push(trimmed.to_string());
        i += 1;
    }
    flush_para(&mut md, &mut para);

    Ok(md.trim().to_string())
}

/// Heuristic: is this line likely a PDF heading?
fn is_pdf_heading(line: &str) -> bool {
    if line.is_empty() || line.len() > 80 {
        return false;
    }
    // Must not end in sentence-ending punctuation.
    if matches!(
        line.chars().last(),
        Some('.') | Some('!') | Some('?') | Some(':') | Some(';') | Some(',')
    ) {
        return false;
    }
    let has_lower = line.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = line.chars().any(|c| c.is_ascii_uppercase());
    // All-caps (no lowercase, has uppercase) => strong heading signal => H1.
    if !has_lower && has_upper {
        return true;
    }
    // Title case: every word starts with uppercase, but keep it short enough
    // that ordinary sentences ("The Quick Brown Fox jumps...") are not
    // misread as headings.
    let words: Vec<&str> = line.split_whitespace().collect();
    if !words.is_empty()
        && words.len() <= 6
        && line.len() <= 60
        && words.iter().all(|w| {
            w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        })
    {
        return true;
    }
    false
}

/// Heuristic: does this trimmed line look like a table row?
/// A table row has 3+ tokens separated by runs of 2+ spaces, where tokens are
/// short (numbers, words, dates) and not sentence-like.
fn looks_like_table_row(line: &str) -> bool {
    let parts = split_layout_row(line);
    if parts.len() < 3 {
        return false;
    }
    // Most parts should be short (not long sentences).
    let short_count = parts.iter().filter(|p| p.len() < 40).count();
    short_count >= parts.len() - 1 && !line.contains('.')
}

/// Split a layout-mode table row into cells by runs of 2+ spaces.
fn split_layout_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if i - run_start >= 2 {
                cells.push(line[start..run_start].trim().to_string());
                start = i;
            }
        } else {
            i += 1;
        }
    }
    cells.push(line[start..].trim().to_string());
    cells.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Render a list of rows (each a list of cells) as a Markdown pipe table.
fn render_markdown_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count < 2 {
        return String::new();
    }

    // Pad all rows to the same column count.
    let padded: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let mut cells = r.clone();
            cells.resize(col_count, String::new());
            cells
        })
        .collect();

    let mut md = String::new();
    // Header row.
    md.push('|');
    for cell in &padded[0] {
        md.push_str(&format!(" {} |", cell));
    }
    md.push('\n');
    // Separator row.
    md.push('|');
    for _ in 0..col_count {
        md.push_str(" --- |");
    }
    md.push('\n');
    // Data rows.
    for row in &padded[1..] {
        md.push('|');
        for cell in row {
            md.push_str(&format!(" {} |", cell));
        }
        md.push('\n');
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_layout_to_markdown() {
        let input = "INTRODUCTION\n\nThis is a regular paragraph that continues here.\n\nBackground\nSome more text here.\n\nName  Age  City\nAlice  30  NY\nBob  25  Boston\n";
        let md = pdf_text_to_markdown(input).unwrap();
        let expected = "# INTRODUCTION\n\nThis is a regular paragraph that continues here.\n\n## Background\n\nSome more text here.\n\n| Name | Age | City |\n| --- | --- | --- |\n| Alice | 30 | NY |\n| Bob | 25 | Boston |";
        assert_eq!(md, expected);
    }

    #[test]
    fn pdf_all_caps_heading_is_h1() {
        let md = pdf_text_to_markdown("EXECUTIVE SUMMARY\n\nResults here.\n").unwrap();
        assert!(md.starts_with("# EXECUTIVE SUMMARY"));
    }

    #[test]
    fn pdf_blank_input_is_empty() {
        assert_eq!(pdf_text_to_markdown("   \n\n  ").unwrap(), "");
    }

    /// End-to-end: a real PDF produced by `ps2pdf`, converted by `pdftotext`.
    /// Skips silently when the external toolchain isn't installed.
    #[test]
    fn pdf_real_file_when_tools_available() {
        let base = std::env::temp_dir().join(format!("aria-pdf-test-{}", std::process::id()));
        let ps = base.with_extension("ps");
        let pdf = base.with_extension("pdf");
        std::fs::write(
            &ps,
            "%!PS\n/Helvetica-Bold findfont 20 scalefont setfont\n72 720 moveto (EXECUTIVE SUMMARY) show\n/Helvetica findfont 12 scalefont setfont\n72 690 moveto (This document shows headings and a table.) show\n72 670 moveto (Name    Age    City) show\n72 650 moveto (Alice   30     New York) show\n72 630 moveto (Bob     25     Boston) show\nshowpage\n",
        )
        .unwrap();
        let pdf_made = std::process::Command::new("ps2pdf")
            .arg(&ps)
            .arg(&pdf)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&ps);
        if !pdf_made {
            let _ = std::fs::remove_file(&pdf);
            return; // ps2pdf not installed
        }
        let bytes = std::fs::read(&pdf).unwrap();
        let _ = std::fs::remove_file(&pdf);
        let md = convert(&bytes, "application/pdf").unwrap().unwrap();
        assert!(md.contains("# EXECUTIVE SUMMARY"), "heading missing: {md}");
        assert!(md.contains("| Name | Age | City |"), "table missing: {md}");
    }
}
