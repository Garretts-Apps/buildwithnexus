//! web_search: the DuckDuckGo Lite request and the parsing of its HTML results.

// ── web search: DuckDuckGo Lite results ──────────────────────────────────

pub(super) fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

// Strip HTML for search result display. Removes script/style blocks,
// HTML tags, and collapses whitespace. Keeps meaningful text content.
pub(super) fn strip_html(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len / 3);
    let mut i = 0;
    let mut in_tag = false;
    // Tracks whether we're inside a script or style block (suppresses content).
    let mut skip_depth = 0u8;

    while i < len {
        // Check for <script or <style open tags — enter suppression mode.
        if !in_tag && skip_depth == 0 && bytes[i] == b'<' {
            let rest = &bytes[i..];
            let is_skip = |tag: &[u8]| {
                rest.len() > tag.len()
                    && rest[..tag.len()].eq_ignore_ascii_case(tag)
                    && (rest
                        .get(tag.len())
                        .is_none_or(|b| !b.is_ascii_alphanumeric()))
            };
            if is_skip(b"<script") || is_skip(b"<style") {
                skip_depth = 1;
                in_tag = true;
                i += 1;
                continue;
            }
        }
        if bytes[i] == b'<' {
            if skip_depth > 0 {
                // Inside suppressed block: look for </script> or </style>.
                let rest = &bytes[i..];
                let ends =
                    |t: &[u8]| rest.len() >= t.len() && rest[..t.len()].eq_ignore_ascii_case(t);
                if ends(b"</script>") || ends(b"</style>") {
                    skip_depth = 0;
                    // Skip past the closing tag.
                    while i < len && bytes[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            if skip_depth == 0 {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if !in_tag && skip_depth == 0 {
            out.push(bytes[i] as char);
        }
        i += 1;
    }

    // Collapse whitespace and decode HTML entities (named + numeric).
    let decoded = decode_html_entities(&out);

    let mut result = String::new();
    let mut prev_ws = true;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            result.push(c);
            prev_ws = false;
        }
    }
    result.trim().to_string()
}

// One parsed web-search result.
pub(super) struct SearchHit {
    pub(super) title: String,
    pub(super) url: String,
    pub(super) snippet: String,
}

// Percent-decode a URL-encoded string (also treats `+` as space). Invalid
// escapes are left as-is. Bytes are reassembled as UTF-8 lossily.
pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Resolve a single entity body (the text between `&` and `;`) to its character:
// the common named entities plus decimal (`#8217`) and hex (`#x2019`) numeric
// character references. Returns None for anything unrecognized.
pub(super) fn entity_char(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "rsquo" => Some('’'),
        "lsquo" => Some('‘'),
        "rdquo" => Some('”'),
        "ldquo" => Some('“'),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

// Decode HTML entities in a single left-to-right pass — named entities and
// decimal/hex numeric character references. Single-pass so `&amp;lt;` decodes to
// the literal `&lt;` (not `<`), and an unrecognized `&…;` is left untouched.
pub(super) fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // `&` and `;` are ASCII, so `semi` is a valid char boundary; the ≤12
        // bound keeps a lone `&` in prose from swallowing a distant `;`.
        if let Some(semi) = tail.find(';') {
            if semi <= 12 {
                if let Some(ch) = entity_char(&tail[1..semi]) {
                    out.push(ch);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

// Read attribute `attr` from the tag that opens at byte index `tag_start`,
// scanning only within that tag (up to its closing `>`).
pub(super) fn tag_attr(html: &str, tag_start: usize, attr: &str) -> Option<String> {
    let end = html[tag_start..].find('>').map(|e| tag_start + e)?;
    let seg = &html[tag_start..end];
    let key = format!("{attr}=");
    let after = &seg[seg.find(&key)? + key.len()..];
    let quote = after.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &after[1..];
        rest.find(quote).map(|c| rest[..c].to_string())
    } else {
        let endv = after.find(char::is_whitespace).unwrap_or(after.len());
        Some(after[..endv].to_string())
    }
}

// DuckDuckGo Lite wraps result links in a redirect: `//duckduckgo.com/l/?uddg=
// <percent-encoded-target>&rut=…`. Recover the real destination; fall back to
// normalizing a protocol-relative href.
pub(super) fn ddg_real_url(href: &str) -> String {
    let href = href.replace("&amp;", "&");
    if let Some(p) = href.find("uddg=") {
        let rest = &href[p + "uddg=".len()..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

// Parse DuckDuckGo Lite result rows into structured hits. Returns empty if the
// markup doesn't match (the caller then falls back to a stripped-text blob).
pub(super) fn parse_ddg_lite(html: &str) -> Vec<SearchHit> {
    let mut titles: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while let Some(rel) = html[i..].find("<a ") {
        let tag_start = i + rel;
        let Some(gt) = html[tag_start..].find('>').map(|e| tag_start + e) else {
            break;
        };
        if html[tag_start..gt].contains("result-link") {
            let url = ddg_real_url(&tag_attr(html, tag_start, "href").unwrap_or_default());
            let after = &html[gt + 1..];
            if let Some(close) = after.to_ascii_lowercase().find("</a") {
                let title = decode_html_entities(&strip_html(&after[..close]))
                    .trim()
                    .to_string();
                if !title.is_empty() {
                    titles.push((title, url));
                }
            }
        }
        i = gt + 1;
    }

    let mut snippets: Vec<String> = Vec::new();
    let mut j = 0;
    while let Some(rel) = html[j..].find("result-snippet") {
        let pos = j + rel;
        if let Some(gt) = html[pos..].find('>') {
            let start = pos + gt + 1;
            if let Some(lt) = html[start..].find('<') {
                snippets.push(
                    decode_html_entities(&strip_html(&html[start..start + lt]))
                        .trim()
                        .to_string(),
                );
            }
        }
        j = pos + "result-snippet".len();
    }

    titles
        .into_iter()
        .enumerate()
        .map(|(k, (title, url))| SearchHit {
            title,
            url,
            snippet: snippets.get(k).cloned().unwrap_or_default(),
        })
        .collect()
}

// Render parsed hits as compact, numbered `title / url / snippet` blocks that a
// small model can read, capped at `max` results.
pub(super) fn format_search_hits(hits: &[SearchHit], max: usize) -> String {
    let mut out = String::new();
    for (n, h) in hits.iter().take(max).enumerate() {
        out.push_str(&format!("{}. {}\n", n + 1, h.title));
        if !h.url.is_empty() {
            out.push_str(&format!("   {}\n", h.url));
        }
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}
