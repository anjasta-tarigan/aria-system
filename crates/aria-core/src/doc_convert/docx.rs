//! DOCX (WordprocessingML) → Markdown converter.
//!
//! Parses `word/document.xml` from the ZIP archive natively (no Python,
//! no pandoc). Headings become `#`/`##`…, tables become pipe tables,
//! and list items become `- ` bullets.
//!
//! Shortcuts (known limits):
//! * List numbering is not resolved from `numbering.xml`; every list item
//!   renders as a bullet. Upgrade when numbered lists must be exact.
//! * Only `word/document.xml` is converted (headers/footers ignored).
//! * Multi-paragraph table cells are joined with a space.

use std::io::{Read, Cursor};

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

/// Convert docx bytes to Markdown.
pub fn convert_docx(bytes: &[u8]) -> Result<String, String> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| e.to_string())?;
    let mut document = archive
        .by_name("word/document.xml")
        .map_err(|_| "word/document.xml not found in docx".to_string())?;
    let mut xml = String::new();
    document.read_to_string(&mut xml).map_err(|e| e.to_string())?;
    Parser::new(xml.into_bytes()).run()
}

/// Heading level derived from a `w:pStyle` value.
#[derive(Clone, Copy)]
enum Heading {
    Level(u8),
}

/// One frame on the parse stack.
enum Frame {
    /// A paragraph: collects text, heading style and list flag.
    Para {
        style: Option<Heading>,
        text: String,
        list: bool,
    },
    /// A run: collects text.
    Run { text: String },
    /// A table: collects rows of cells.
    Table { rows: Vec<Vec<String>> },
    /// A table row: collects cells.
    Tr { cells: Vec<String> },
    /// A table cell: collects its paragraph text.
    Tc { text: String },
}

struct Parser {
    reader: Reader<Cursor<Vec<u8>>>,
    buf: Vec<u8>,
    stack: Vec<Frame>,
    out: String,
    last_was_list: bool,
}

impl Parser {
    fn new(xml: Vec<u8>) -> Self {
        Self {
            reader: Reader::from_reader(Cursor::new(xml)),
            buf: Vec::new(),
            stack: Vec::new(),
            out: String::new(),
            last_was_list: false,
        }
    }

    fn run(mut self) -> Result<String, String> {
        loop {
            self.buf.clear();
            match self.reader.read_event_into(&mut self.buf).map_err(|e| e.to_string())? {
                Event::Eof => break,
                Event::Start(e) => {
                    let (name, style) = {
                        let name = local_name(e.local_name());
                        let style = if name == "pStyle" {
                            heading_level(&e)
                        } else {
                            None
                        };
                        (name, style)
                    }; // `e` (and its borrow of `self`) is dropped here
                    self.handle_start(&name, style);
                }
                Event::End(e) => {
                    let name = {
                        let name = local_name(e.local_name());
                        name
                    }; // `e` dropped here
                    self.handle_end(&name);
                }
                Event::Text(t) => {
                    let content = t.into_inner();
                    let text: String = quick_xml::escape::unescape(&content)
                        .map_err(|e| e.to_string())?
                        .into_owned();
                    self.handle_text(&text);
                }
                Event::Empty(e) => {
                    let (name, style, is_numpr) = {
                        let name = local_name(e.local_name());
                        let style = if name == "pStyle" {
                            heading_level(&e)
                        } else {
                            None
                        };
                        let is_numpr = name == "numPr";
                        (name, style, is_numpr)
                    }; // `e` dropped here
                    self.handle_empty(&name, style, is_numpr);
                }
                _ => {}
            }
        }
        Ok(self.out.trim().to_string())
    }

    fn handle_start(&mut self, name: &str, style: Option<Heading>) {
        match name {
            "p" => self.stack.push(Frame::Para {
                style: None,
                text: String::new(),
                list: false,
            }),
            "pStyle" => {
                if let Some(Frame::Para { style: s, .. }) = self.stack.last_mut() {
                    if s.is_none() {
                        *s = style;
                    }
                }
            }
            "numPr" => {
                if let Some(Frame::Para { list, .. }) = self.stack.last_mut() {
                    *list = true;
                }
            }
            "r" => self.stack.push(Frame::Run { text: String::new() }),
            "tbl" => self.stack.push(Frame::Table { rows: Vec::new() }),
            "tr" => self.stack.push(Frame::Tr { cells: Vec::new() }),
            "tc" => self.stack.push(Frame::Tc { text: String::new() }),
            _ => {}
        }
    }

    fn handle_end(&mut self, name: &str) {
        match name {
            "p" => {
                let para = self.stack.pop().expect("paragraph frame");
                if let Frame::Para { style, text, list } = para {
                    // Paragraphs inside a table cell belong to the cell, not
                    // the document body.
                    if let Some(Frame::Tc { text: acc }) = self.stack.last_mut() {
                        if !acc.is_empty() {
                            acc.push(' ');
                        }
                        acc.push_str(text.trim());
                    } else {
                        self.emit_block(render_paragraph(&text, style, list), list);
                    }
                }
            }
            "r" => {
                let run = self.stack.pop().expect("run frame");
                if let Frame::Run { text } = run {
                    self.push_text(&text);
                }
            }
            "tr" => {
                let tr = self.stack.pop().expect("table row frame");
                if let Frame::Tr { cells } = tr {
                    if let Some(Frame::Table { rows }) = self.stack.last_mut() {
                        rows.push(cells);
                    }
                }
            }
            "tc" => {
                let tc = self.stack.pop().expect("table cell frame");
                if let Frame::Tc { text } = tc {
                    if let Some(Frame::Tr { cells }) = self.stack.last_mut() {
                        cells.push(text);
                    }
                }
            }
            "tbl" => {
                let table = self.stack.pop().expect("table frame");
                if let Frame::Table { rows } = table {
                    self.emit_block(render_table(&rows), false);
                }
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        self.push_text(text);
    }

    fn handle_empty(&mut self, name: &str, style: Option<Heading>, is_numpr: bool) {
        if name == "br" || name == "tab" {
            self.push_text(" ");
            return;
        }
        if let Some(Frame::Para { style: s, list, .. }) = self.stack.last_mut() {
            if s.is_none() {
                *s = style;
            }
            if is_numpr {
                *list = true;
            }
        }
    }

    /// Append `text` to the innermost text accumulator (Run > Para > Tc).
    fn push_text(&mut self, text: &str) {
        match self.stack.last_mut() {
            Some(Frame::Run { text: acc }) => acc.push_str(text),
            Some(Frame::Para { text: acc, .. }) => acc.push_str(text),
            Some(Frame::Tc { text: acc }) => acc.push_str(text),
            _ => {}
        }
    }

    fn emit_block(&mut self, rendered: String, is_list: bool) {
        if rendered.is_empty() {
            return;
        }
        if is_list {
            if !self.last_was_list
                && !self.out.is_empty()
                && !self.out.ends_with("\n\n")
            {
                self.out.push_str("\n\n");
            }
            self.out.push_str(&rendered);
            self.out.push('\n');
            self.last_was_list = true;
        } else {
            if self.last_was_list {
                self.out.push('\n');
                self.last_was_list = false;
            }
            if !self.out.is_empty() && !self.out.ends_with("\n\n") {
                self.out.push_str("\n\n");
            }
            self.out.push_str(&rendered);
            self.out.push_str("\n\n");
        }
    }
}

/// Convert an element's `local_name()` into an owned `String`.
fn local_name(n: impl AsRef<str>) -> String {
    n.as_ref().to_string()
}

/// Map a `w:pStyle` attribute value to a heading level.
fn heading_level(e: &quick_xml::events::BytesStart<'_>) -> Option<Heading> {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.local_name()) == "val" {
            let level = match attr.value.as_ref() {
                "Title" | "Heading1" => 1,
                "Heading2" => 2,
                "Heading3" => 3,
                "Heading4" => 4,
                "Heading5" => 5,
                "Heading6" | "Heading7" | "Heading8" | "Heading9" => 6,
                _ => return None,
            };
            return Some(Heading::Level(level));
        }
    }
    None
}

/// Render one paragraph to Markdown.
fn render_paragraph(text: &str, style: Option<Heading>, list: bool) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    match style {
        Some(Heading::Level(n)) => {
            let level = n.min(6);
            format!("{} {}", "#".repeat(level as usize), text)
        }
        None => {
            if list {
                format!("- {}", text)
            } else {
                text.to_string()
            }
        }
    }
}

/// Render rows of cells as a Markdown pipe table.
fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return String::new();
    }
    let mut md = String::new();
    md.push('|');
    for cell in &rows[0] {
        md.push_str(&format!(" {} |", escape_cell(cell)));
    }
    md.push('\n');
    md.push('|');
    for _ in 0..cols {
        md.push_str(" --- |");
    }
    md.push('\n');
    for row in &rows[1..] {
        md.push('|');
        for cell in row {
            md.push_str(&format!(" {} |", escape_cell(cell)));
        }
        md.push('\n');
    }
    md
}

/// Escape a table cell for safe pipe-table rendering.
fn escape_cell(cell: &str) -> String {
    cell.trim().replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal in-memory docx (ZIP with `word/document.xml`).
    fn build_docx(xml: &str) -> Vec<u8> {
        let sink = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(sink);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    const DOCUMENT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Hello World</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Sub section</w:t></w:r></w:p>
<w:p><w:r><w:t>Plain paragraph text here.</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>Bullet item</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cell2</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>Cell3</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cell4</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body>
</w:document>"#;

    #[test]
    fn converts_headings_paragraph_list_and_table() {
        let md = convert_docx(&build_docx(DOCUMENT_XML)).unwrap();
        let expected = "# Hello World\n\n## Sub section\n\nPlain paragraph text here.\n\n- Bullet item\n\n| Cell1 | Cell2 |\n| --- | --- |\n| Cell3 | Cell4 |";
        assert_eq!(md, expected);
    }

    #[test]
    fn missing_document_xml_is_an_error() {
        let sink = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(sink);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file("other.xml", options).unwrap();
        zip.write_all(b"hello").unwrap();
        let buf = zip.finish().unwrap().into_inner();
        let result = convert_docx(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("document.xml"));
    }

    #[test]
    fn corrupt_zip_is_an_error() {
        assert!(convert_docx(b"not a zip at all").is_err());
    }

    #[test]
    fn empty_body_produces_empty_markdown() {
        let xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body></w:body></w:document>"#;
        let md = convert_docx(&build_docx(xml)).unwrap();
        assert_eq!(md, "");
    }
}
