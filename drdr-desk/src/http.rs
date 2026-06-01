//! A tiny from-scratch HTTP/1.1 client — the network half of DrDrBrowser.
//!
//! It speaks just enough HTTP to fetch a web page over a plain TCP
//! socket: build a `GET`, read the response, split the status line and
//! headers from the body, decode `chunked` transfer-encoding, and follow
//! redirects. There is no third-party HTTP crate and no TLS stack, so
//! **`https://` is not supported** (hand-rolling TLS 1.3 is a project of
//! its own) — the browser says so plainly instead of pretending. Plain
//! `http://` works against real servers.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A fetched page.
pub struct Page {
    pub final_url: String,
    pub content_type: String,
    pub status: u16,
    pub body: String,
}

/// The split-out pieces of a URL.
#[derive(Debug, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Parse an absolute URL (defaulting the scheme to http and the path to
/// `/`). Relative URLs are not handled here — the browser resolves those.
pub fn parse_url(input: &str) -> Result<Url, String> {
    let input = input.trim();
    let (scheme, rest) = match input.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("http".to_string(), input),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("empty host".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p.parse::<u16>().map_err(|_| "bad port".to_string())?;
            (h.to_string(), port)
        }
        None => (
            authority.to_string(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    Ok(Url { scheme, host, port, path: path.to_string() })
}

/// Resolve a (possibly relative) link href against the page's URL into an
/// absolute URL string the browser can navigate to.
pub fn resolve(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.contains("://") {
        return href.to_string();
    }
    let Ok(b) = parse_url(base) else { return href.to_string() };
    if let Some(rest) = href.strip_prefix("//") {
        return format!("{}://{}", b.scheme, rest);
    }
    if href.starts_with('/') {
        return format!("{}://{}:{}{}", b.scheme, b.host, b.port, href);
    }
    // Relative to the current directory of the path.
    let dir = match b.path.rfind('/') {
        Some(i) => &b.path[..=i],
        None => "/",
    };
    format!("{}://{}:{}{}{}", b.scheme, b.host, b.port, dir, href)
}

/// Fetch a URL, following up to a few redirects. `https://` returns a
/// clear "no TLS" error rather than a confusing connection failure.
pub fn fetch(url: &str) -> Result<Page, String> {
    let mut current = url.to_string();
    for _ in 0..5 {
        let u = parse_url(&current)?;
        if u.scheme == "https" {
            return Err(format!(
                "https:// needs a TLS stack DrDrOS doesn't ship yet — try an http:// site.\n(requested {current})"
            ));
        }
        if u.scheme != "http" {
            return Err(format!("unsupported scheme: {}://", u.scheme));
        }
        let raw = fetch_once(&u)?;
        let (status, headers, body) = parse_response(&raw)?;
        if (300..400).contains(&status) {
            if let Some(loc) = header_value(&headers, "location") {
                current = resolve(&current, &loc);
                continue;
            }
        }
        let content_type = header_value(&headers, "content-type").unwrap_or_default();
        let body = if header_value(&headers, "transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
        {
            dechunk(&body)
        } else {
            body
        };
        return Ok(Page {
            final_url: current,
            content_type,
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
    }
    Err("too many redirects".into())
}

/// One request/response round trip; returns the raw response bytes.
fn fetch_once(u: &Url) -> Result<Vec<u8>, String> {
    let addr = format!("{}:{}", u.host, u.port);
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(6))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(6))).ok();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: DrDrBrowser/1.0\r\nAccept: text/html,text/*\r\nConnection: close\r\n\r\n",
        u.path, u.host
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // Cap the read so a giant page can't exhaust memory in the viewer.
    const CAP: usize = 1 << 20; // 1 MiB
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= CAP {
                    break;
                }
            }
            Err(e) => {
                if buf.is_empty() {
                    return Err(format!("read: {e}"));
                }
                break; // a timeout after some data: take what we got
            }
        }
    }
    Ok(buf)
}

/// Split a raw HTTP response into (status code, header lines, body bytes).
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<String>, Vec<u8>), String> {
    let sep = find_subslice(raw, b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| find_subslice(raw, b"\n\n").map(|i| (i, 2)))
        .ok_or("no header/body separator")?;
    let head = String::from_utf8_lossy(&raw[..sep.0]);
    let body = raw[sep.0 + sep.1..].to_vec();
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty response")?;
    // "HTTP/1.1 200 OK" → 200
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("bad status line")?;
    Ok((status, lines.map(|l| l.to_string()).collect(), body))
}

/// Case-insensitive header lookup ("Header: value" → "value").
fn header_value(headers: &[String], name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    headers.iter().find_map(|h| {
        let (k, v) = h.split_once(':')?;
        if k.trim().to_ascii_lowercase() == name {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

/// Decode a `Transfer-Encoding: chunked` body.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < body.len() {
        // Chunk size line (hex), up to CRLF.
        let line_end = match find_subslice(&body[pos..], b"\r\n") {
            Some(i) => pos + i,
            None => break,
        };
        let size_str = String::from_utf8_lossy(&body[pos..line_end]);
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
        pos = line_end + 2;
        if size == 0 {
            break;
        }
        let end = (pos + size).min(body.len());
        out.extend_from_slice(&body[pos..end]);
        pos = end + 2; // skip the trailing CRLF
    }
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = parse_url("http://example.com/page").unwrap();
        assert_eq!(u, Url { scheme: "http".into(), host: "example.com".into(), port: 80, path: "/page".into() });
        let u = parse_url("example.com").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("example.com", 80, "/"));
        let u = parse_url("http://localhost:8080/a/b").unwrap();
        assert_eq!((u.port, u.path.as_str()), (8080, "/a/b"));
        assert_eq!(parse_url("https://x.com").unwrap().port, 443);
    }

    #[test]
    fn resolves_relative_and_absolute_links() {
        let base = "http://example.com/dir/page.html";
        assert_eq!(resolve(base, "https://other.com/x"), "https://other.com/x");
        assert_eq!(resolve(base, "/top"), "http://example.com:80/top");
        assert_eq!(resolve(base, "next.html"), "http://example.com:80/dir/next.html");
    }

    #[test]
    fn splits_a_response_and_reads_headers() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello";
        let (status, headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(header_value(&headers, "content-type").as_deref(), Some("text/html"));
        assert_eq!(body, b"hello");
    }

    #[test]
    fn dechunks_a_chunked_body() {
        // "Wiki" + "pedia" in two chunks, then a zero terminator.
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"Wikipedia");
    }

    // A real network round-trip — ignored by default so the normal test
    // run stays offline/deterministic. Run with `--ignored` to confirm the
    // client fetches and parses a live page end to end.
    #[test]
    #[ignore]
    fn live_fetch_example_com() {
        let page = fetch("http://example.com").expect("fetch should succeed");
        assert_eq!(page.status, 200);
        assert!(page.body.contains("Example Domain"), "body: {:.80}", page.body);
    }
}
