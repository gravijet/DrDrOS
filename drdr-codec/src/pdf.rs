//! A from-scratch PDF *text* extractor — not a full renderer (PDF is an
//! enormous spec), but enough to read a document: find the content
//! streams, inflate the FlateDecode ones (reusing our DEFLATE decoder),
//! and pull the text out of the `(...) Tj` / `[...] TJ` operators, with
//! line breaks on the text-positioning operators. Honest about its
//! limits: it returns readable text for ordinary PDFs and a clear note
//! when a document is image-only or uses an unsupported encoding.

use crate::inflate::{inflate, zlib_decompress};

/// Extract the text of a PDF as a list of lines.
pub fn extract_pdf_text(data: &[u8]) -> Result<Vec<String>, String> {
    if data.len() < 5 || &data[0..5] != b"%PDF-" {
        return Err("pdf: not a PDF".into());
    }
    let mut out = String::new();
    for stream in find_streams(data) {
        // Try FlateDecode (zlib, then raw), else treat as a plain stream.
        let decoded = zlib_decompress(&stream)
            .or_else(|_| inflate(&stream))
            .unwrap_or(stream);
        // Only mine streams that actually contain a text block.
        if find_sub(&decoded, b"BT").is_some() || find_sub(&decoded, b"Tj").is_some() {
            out.push_str(&extract_text_from_content(&decoded));
        }
    }
    let lines: Vec<String> = out
        .split('\n')
        .map(|l| l.trim_end().to_string())
        .collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(vec![
            "(no extractable text — this PDF may be scanned images".into(),
            " or use an embedded font encoding we don't map yet)".into(),
        ]);
    }
    Ok(lines)
}

/// Locate every `stream … endstream` body in the file.
fn find_streams(data: &[u8]) -> Vec<Vec<u8>> {
    let mut streams = Vec::new();
    let mut i = 0;
    while let Some(rel) = find_sub(&data[i..], b"stream") {
        let mut s = i + rel + 6;
        // The keyword is followed by CRLF or LF.
        if s < data.len() && data[s] == b'\r' {
            s += 1;
        }
        if s < data.len() && data[s] == b'\n' {
            s += 1;
        }
        if let Some(erel) = find_sub(&data[s..], b"endstream") {
            let end = s + erel;
            // Trim a trailing EOL before `endstream`.
            let mut e = end;
            if e > s && data[e - 1] == b'\n' {
                e -= 1;
            }
            if e > s && data[e - 1] == b'\r' {
                e -= 1;
            }
            streams.push(data[s..e].to_vec());
            i = end + 9;
        } else {
            break;
        }
    }
    streams
}

/// Pull readable text out of a decoded content stream.
pub fn extract_text_from_content(content: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    let n = content.len();
    let mut last_token = Vec::new();
    while i < n {
        let c = content[i];
        match c {
            b'(' => {
                // literal string, depth + escape aware
                let (s, ni) = read_literal_string(content, i);
                out.push_str(&s);
                out.push(' ');
                i = ni;
                last_token.clear();
            }
            b'<' if i + 1 < n && content[i + 1] != b'<' => {
                let (s, ni) = read_hex_string(content, i);
                out.push_str(&s);
                out.push(' ');
                i = ni;
                last_token.clear();
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'*' | b'\'' | b'"' => {
                last_token.push(c);
                i += 1;
                // Text-positioning operators end a line.
                let tok = last_token.as_slice();
                if matches!(tok, b"Td" | b"TD" | b"T*" | b"'" | b"\"") {
                    out.push('\n');
                    last_token.clear();
                }
            }
            _ => {
                if !c.is_ascii_alphanumeric() {
                    last_token.clear();
                }
                i += 1;
            }
        }
    }
    out
}

/// Read a `(...)` literal string starting at `start` (the `(`). Returns
/// the decoded text and the index just past the closing `)`.
fn read_literal_string(data: &[u8], start: usize) -> (String, usize) {
    let mut s = String::new();
    let mut i = start + 1;
    let mut depth = 1;
    while i < data.len() {
        let c = data[i];
        match c {
            b'\\' => {
                i += 1;
                if i >= data.len() {
                    break;
                }
                let e = data[i];
                match e {
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    b'b' => s.push('\u{8}'),
                    b'f' => s.push('\u{c}'),
                    b'(' => s.push('('),
                    b')' => s.push(')'),
                    b'\\' => s.push('\\'),
                    b'0'..=b'7' => {
                        // octal escape, up to 3 digits
                        let mut val = (e - b'0') as u32;
                        let mut k = 0;
                        while k < 2 && i + 1 < data.len() && (b'0'..=b'7').contains(&data[i + 1]) {
                            i += 1;
                            val = val * 8 + (data[i] - b'0') as u32;
                            k += 1;
                        }
                        if let Some(ch) = char::from_u32(val) {
                            s.push(ch);
                        }
                    }
                    b'\n' => {} // line continuation
                    other => s.push(other as char),
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                s.push('(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
                s.push(')');
                i += 1;
            }
            _ => {
                s.push(c as char);
                i += 1;
            }
        }
    }
    (s, i)
}

/// Read a `<...>` hex string. Returns the decoded text and the next index.
fn read_hex_string(data: &[u8], start: usize) -> (String, usize) {
    let mut s = String::new();
    let mut i = start + 1;
    let mut hi: Option<u8> = None;
    while i < data.len() && data[i] != b'>' {
        let c = data[i];
        if let Some(d) = hex_val(c) {
            match hi {
                None => hi = Some(d),
                Some(h) => {
                    s.push(((h << 4) | d) as char);
                    hi = None;
                }
            }
        }
        i += 1;
    }
    if let Some(h) = hi {
        s.push((h << 4) as char);
    }
    (s, (i + 1).min(data.len()))
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Find the first occurrence of `needle` in `hay`.
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_from_a_content_stream() {
        let content = b"BT /F1 12 Tf 72 700 Td (Hello, DrDrOS!) Tj 0 -14 Td (Second line) Tj ET";
        let text = extract_text_from_content(content);
        assert!(text.contains("Hello, DrDrOS!"));
        assert!(text.contains("Second line"));
        // The Td between the two strings forces a line break.
        let nl = text.matches('\n').count();
        assert!(nl >= 1, "expected a newline from Td, got: {text:?}");
    }

    #[test]
    fn handles_escapes_and_hex_strings() {
        let lit = b"(A\\(B\\) C) Tj";
        assert!(extract_text_from_content(lit).contains("A(B) C"));
        let hex = b"<48656c6c6f> Tj"; // "Hello"
        assert!(extract_text_from_content(hex).contains("Hello"));
    }

    #[test]
    fn rejects_non_pdf() {
        assert!(extract_pdf_text(b"just text, no header").is_err());
    }

    #[test]
    fn finds_streams_between_keywords() {
        let pdf = b"%PDF-1.4\n1 0 obj<<>>stream\nBT (Hi) Tj ET\nendstream endobj";
        let lines = extract_pdf_text(pdf).unwrap();
        assert!(lines.iter().any(|l| l.contains("Hi")));
    }
}
