//! The create_docx writer: light Markdown to a minimal Word document in a stored zip.

// ── Word documents (create_docx) ─────────────────────────────────────────

pub(super) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(super) fn docx_paragraph(text: &str, style: Option<&str>) -> String {
    let ppr = style
        .map(|s| format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>"))
        .unwrap_or_default();
    format!("<w:p>{ppr}{}</w:p>", docx_runs(text))
}

// Emit one Word run for `text` with the active inline formatting. Empty text
// produces nothing (an all-empty paragraph is still valid OOXML).
pub(super) fn docx_push_run(out: &mut String, text: &str, bold: bool, italic: bool, code: bool) {
    if text.is_empty() {
        return;
    }
    let mut rpr = String::new();
    if bold {
        rpr.push_str("<w:b/>");
    }
    if italic {
        rpr.push_str("<w:i/>");
    }
    if code {
        rpr.push_str("<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>");
    }
    if !rpr.is_empty() {
        rpr = format!("<w:rPr>{rpr}</w:rPr>");
    }
    out.push_str("<w:r>");
    out.push_str(&rpr);
    out.push_str(&format!(
        "<w:t xml:space=\"preserve\">{}</w:t></w:r>",
        xml_escape(text)
    ));
}

// Light-markdown inline formatting → Word runs: `**bold**`, `*italic*`, and
// `` `code` `` (verbatim). Emphasis markers only toggle when they flank
// non-space text, so arithmetic like `5 * 3` and glob patterns stay literal;
// underscores are deliberately left alone so `snake_case` isn't italicized.
pub(super) fn docx_runs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut buf = String::new();
    let (mut bold, mut italic, mut code) = (false, false, false);
    let mut i = 0;
    while i < n {
        let c = chars[i];
        // Inside a code span everything is literal until the closing backtick.
        if code {
            if c == '`' {
                docx_push_run(&mut out, &buf, bold, italic, code);
                buf.clear();
                code = false;
            } else {
                buf.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '`' => {
                docx_push_run(&mut out, &buf, bold, italic, code);
                buf.clear();
                code = true;
                i += 1;
            }
            '*' if i + 1 < n && chars[i + 1] == '*' => {
                // Opening `**` must precede non-space; closing must follow it.
                let flanks = if bold {
                    i > 0 && !chars[i - 1].is_whitespace()
                } else {
                    i + 2 < n && !chars[i + 2].is_whitespace()
                };
                if flanks {
                    docx_push_run(&mut out, &buf, bold, italic, code);
                    buf.clear();
                    bold = !bold;
                } else {
                    buf.push('*');
                    buf.push('*');
                }
                i += 2;
            }
            '*' => {
                let flanks = if italic {
                    i > 0 && !chars[i - 1].is_whitespace()
                } else {
                    i + 1 < n && !chars[i + 1].is_whitespace()
                };
                if flanks {
                    docx_push_run(&mut out, &buf, bold, italic, code);
                    buf.clear();
                    italic = !italic;
                } else {
                    buf.push('*');
                }
                i += 1;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    docx_push_run(&mut out, &buf, bold, italic, code);
    out
}

// Split a markdown table line into trimmed cells, dropping the outer pipes.
pub(super) fn parse_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

// A markdown table row: trimmed, starts with `|`, and has at least two pipes.
pub(super) fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

// The `|---|:--:|` alignment row: every cell is only dashes/colons.
pub(super) fn is_table_separator(line: &str) -> bool {
    let cells = parse_table_cells(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'))
}

// One table cell. Header cells are a single bold run; body cells get full
// inline markdown formatting.
pub(super) fn docx_table_cell(text: &str, header: bool) -> String {
    let runs = if header {
        let mut s = String::new();
        docx_push_run(&mut s, text, true, false, false);
        s
    } else {
        docx_runs(text)
    };
    format!("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p>{runs}</w:p></w:tc>")
}

// A bordered Word table. The first row is treated as the (bold) header.
pub(super) fn docx_table(rows: &[Vec<String>]) -> String {
    let mut out = String::from(
        "<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders>\
<w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
</w:tblBorders></w:tblPr>",
    );
    for (r, row) in rows.iter().enumerate() {
        out.push_str("<w:tr>");
        for cell in row {
            out.push_str(&docx_table_cell(cell, r == 0));
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl>");
    out
}

pub(super) fn docx_document(title: &str, body: &str) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    out.push_str(&docx_paragraph(title, Some("Title")));
    let lines: Vec<&str> = body.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        // A run of consecutive `|`-delimited rows becomes one Word table; the
        // alignment separator row is dropped and the first row is the header.
        if is_table_row(lines[idx]) {
            let mut rows: Vec<Vec<String>> = Vec::new();
            while idx < lines.len() && is_table_row(lines[idx]) {
                if !is_table_separator(lines[idx]) {
                    rows.push(parse_table_cells(lines[idx]));
                }
                idx += 1;
            }
            if !rows.is_empty() {
                out.push_str(&docx_table(&rows));
            }
            continue;
        }
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() {
            out.push_str("<w:p/>");
        } else if let Some(h) = trimmed.strip_prefix("### ") {
            out.push_str(&docx_paragraph(h, Some("Heading3")));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push_str(&docx_paragraph(h, Some("Heading2")));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            out.push_str(&docx_paragraph(h, Some("Heading1")));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            out.push_str(&docx_paragraph(&format!("• {item}"), None));
        } else {
            out.push_str(&docx_paragraph(trimmed, None));
        }
        idx += 1;
    }
    out.push_str(r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#);
    out
}

pub(super) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub(super) fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(super) fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(super) fn stored_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        write_u32(&mut out, 0x0403_4b50);
        write_u16(&mut out, 20);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u32(&mut out, crc);
        write_u32(&mut out, data.len() as u32);
        write_u32(&mut out, data.len() as u32);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        write_u32(&mut central, 0x0201_4b50);
        write_u16(&mut central, 20);
        write_u16(&mut central, 20);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, crc);
        write_u32(&mut central, data.len() as u32);
        write_u32(&mut central, data.len() as u32);
        write_u16(&mut central, name.len() as u16);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u16(&mut central, 0);
        write_u32(&mut central, 0);
        write_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    write_u32(&mut out, 0x0605_4b50);
    write_u16(&mut out, 0);
    write_u16(&mut out, 0);
    write_u16(&mut out, files.len() as u16);
    write_u16(&mut out, files.len() as u16);
    write_u32(&mut out, central.len() as u32);
    write_u32(&mut out, central_offset);
    write_u16(&mut out, 0);
    out
}

pub(super) fn docx_bytes(title: &str, body: &str) -> Vec<u8> {
    let content_types = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_vec();
    let rels = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec();
    let styles = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/></w:style></w:styles>"#.to_vec();
    stored_zip(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels),
        ("word/document.xml", docx_document(title, body).into_bytes()),
        ("word/styles.xml", styles),
    ])
}
