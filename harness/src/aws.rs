//! Amazon Bedrock without an SDK (roadmap 4.4): credentials, Signature
//! Version 4, and the event-stream framing a streamed reply arrives in.
//!
//! A Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`) goes as a Bearer token.
//! Otherwise the access key in `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`
//! (with `AWS_SESSION_TOKEN`), or the profile in `~/.aws/credentials`,
//! signs each request; the secret never leaves bwn. Either goes only to a
//! `bedrock-runtime.REGION.amazonaws.com` endpoint (`config::cloud_hosts`).

use ring::{digest, hmac};
use std::io::Read;

/// An access key to sign with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// The access key from the environment, else from the shared credentials
/// file's profile (`AWS_PROFILE`, or `default`).
pub fn creds() -> Option<Creds> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let (Some(a), Some(s)) = (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
        return Some(Creds {
            access_key_id: a,
            secret_access_key: s,
            session_token: var("AWS_SESSION_TOKEN"),
        });
    }
    let file = var("AWS_SHARED_CREDENTIALS_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| Some(crate::config::user_home()?.join(".aws").join("credentials")))?;
    let text = std::fs::read_to_string(file).ok()?;
    profile_creds(
        &text,
        &var("AWS_PROFILE").unwrap_or_else(|| "default".into()),
    )
}

/// One profile's access key from a credentials file's text.
pub fn profile_creds(text: &str, profile: &str) -> Option<Creds> {
    let mut inside = false;
    let (mut a, mut s, mut t) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim() == profile;
            continue;
        }
        let Some((k, v)) = line.split_once('=').filter(|_| inside) else {
            continue;
        };
        let v = Some(v.trim().to_string()).filter(|v| !v.is_empty());
        match k.trim().to_ascii_lowercase().as_str() {
            "aws_access_key_id" => a = v,
            "aws_secret_access_key" => s = v,
            "aws_session_token" => t = v,
            _ => {}
        }
    }
    Some(Creds {
        access_key_id: a?,
        secret_access_key: s?,
        session_token: t,
    })
}

/// The region in a `bedrock-runtime.REGION.amazonaws.com` host.
pub fn region_of(host: &str) -> Option<&str> {
    host.strip_prefix("bedrock-runtime.")?
        .strip_suffix(".amazonaws.com")
        .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, b).as_ref())
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg)
        .as_ref()
        .to_vec()
}

/// One path segment as sent (a model id's `:` is `%3A`).
pub fn uri_encode_segment(s: &str) -> String {
    uri_encode(s, false)
}

// RFC 3986 unreserved characters stay; every other byte is %XX (and `/`
// too, unless it separates path segments).
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What a SigV4 signature covers.
pub struct Signing<'a> {
    pub method: &'a str,
    /// The path exactly as sent; it is encoded once more for the canonical
    /// request, as AWS does for every service but S3.
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    /// Headers to sign besides `x-amz-date` (and the session token): at
    /// least `host`.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

/// The headers to add to a request: `x-amz-date`, the session token when
/// there is one, and `authorization`.
pub fn sign(r: &Signing, c: &Creds) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    headers.push(("x-amz-date".into(), r.amz_date.into()));
    if let Some(t) = &c.session_token {
        headers.push(("x-amz-security-token".into(), t.clone()));
    }
    headers.sort();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let mut query: Vec<(String, String)> = r
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    query.sort();
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        r.method,
        uri_encode(r.path, true),
        query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&"),
        headers
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>(),
        signed,
        sha256_hex(r.body),
    );
    let date = &r.amz_date[..8.min(r.amz_date.len())];
    let scope = format!("{date}/{}/{}/aws4_request", r.region, r.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        r.amz_date,
        sha256_hex(canonical.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", c.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    for part in [r.region, r.service, "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    let mut out = vec![("x-amz-date".to_string(), r.amz_date.to_string())];
    if let Some(t) = &c.session_token {
        out.push(("x-amz-security-token".into(), t.clone()));
    }
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            c.access_key_id
        ),
    ));
    out
}

/// Now as SigV4's `YYYYMMDDTHHMMSSZ`.
pub fn amz_date_now() -> String {
    amz_date(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
}

/// Seconds since 1970 as SigV4's `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ── event stream ────────────────────────────────────────────────────────────

/// CRC-32 (IEEE 802.3), as the event-stream framing uses.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// One event-stream message: its string headers and its payload.
#[derive(Debug, Default, PartialEq)]
pub struct Message {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The largest message accepted; Bedrock's chunks are far smaller.
pub const MAX_MESSAGE: usize = 16 << 20;

/// The next message from `r`, None at a clean end. Both checksums are
/// checked, and a length past `MAX_MESSAGE` or one the headers overrun is
/// refused before anything is allocated for it.
pub fn read_message(r: &mut impl Read) -> Result<Option<Message>, String> {
    let mut prelude = [0u8; 12];
    match r.read(&mut prelude[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(format!("reading the stream: {e}")),
    }
    r.read_exact(&mut prelude[1..])
        .map_err(|e| format!("reading the stream: {e}"))?;
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let (total, hlen, pcrc) = (
        be(&prelude[0..4]) as usize,
        be(&prelude[4..8]) as usize,
        be(&prelude[8..12]),
    );
    if crc32(&prelude[..8]) != pcrc {
        return Err("event stream: the prelude checksum doesn't match".into());
    }
    if !(16..=MAX_MESSAGE).contains(&total) || hlen > total - 16 {
        return Err(format!(
            "event stream: a message of {total} bytes with {hlen} of headers"
        ));
    }
    let mut rest = vec![0u8; total - 12];
    r.read_exact(&mut rest)
        .map_err(|e| format!("reading the stream: {e}"))?;
    let (body, mcrc) = rest.split_at(total - 16);
    let mut all = prelude.to_vec();
    all.extend_from_slice(body);
    if crc32(&all) != be(mcrc) {
        return Err("event stream: the message checksum doesn't match".into());
    }
    let (hdrs, payload) = body.split_at(hlen);
    Ok(Some(Message {
        headers: parse_headers(hdrs)?,
        payload: payload.to_vec(),
    }))
}

fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, String)>, String> {
    let bad = || "event stream: a malformed header".to_string();
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = *b.first().ok_or_else(bad)? as usize;
        let name = b.get(1..1 + n).ok_or_else(bad)?;
        let kind = *b.get(1 + n).ok_or_else(bad)?;
        b = &b[2 + n..];
        let fixed = |len: usize, b: &mut &[u8]| -> Result<Vec<u8>, String> {
            let v = b.get(..len).ok_or_else(bad)?.to_vec();
            *b = &b[len..];
            Ok(v)
        };
        let value = match kind {
            0 => "true".to_string(),
            1 => "false".to_string(),
            2 => fixed(1, &mut b)?[0].to_string(),
            3 => i16::from_be_bytes(fixed(2, &mut b)?.try_into().map_err(|_| bad())?).to_string(),
            4 => i32::from_be_bytes(fixed(4, &mut b)?.try_into().map_err(|_| bad())?).to_string(),
            5 | 8 => {
                i64::from_be_bytes(fixed(8, &mut b)?.try_into().map_err(|_| bad())?).to_string()
            }
            6 | 7 => {
                let len =
                    u16::from_be_bytes(fixed(2, &mut b)?.try_into().map_err(|_| bad())?) as usize;
                String::from_utf8_lossy(&fixed(len, &mut b)?).into_owned()
            }
            9 => hex(&fixed(16, &mut b)?),
            _ => return Err(bad()),
        };
        out.push((String::from_utf8_lossy(name).into_owned(), value));
    }
    Ok(out)
}

/// Encode a message (string headers only), for tests and mock servers.
pub fn encode_message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (k, v) in headers {
        h.push(k.len() as u8);
        h.extend_from_slice(k.as_bytes());
        h.push(7);
        h.extend_from_slice(&(v.len() as u16).to_be_bytes());
        h.extend_from_slice(v.as_bytes());
    }
    let total = (16 + h.len() + payload.len()) as u32;
    let mut m = total.to_be_bytes().to_vec();
    m.extend_from_slice(&(h.len() as u32).to_be_bytes());
    let pcrc = crc32(&m);
    m.extend_from_slice(&pcrc.to_be_bytes());
    m.extend_from_slice(&h);
    m.extend_from_slice(payload);
    let mcrc = crc32(&m);
    m.extend_from_slice(&mcrc.to_be_bytes());
    m
}

/// Bedrock's streamed reply as the Anthropic server-sent events inside it:
/// each `chunk` event's base64 `bytes` becomes a `data:` line, and an
/// exception event an Anthropic `error` event whose message carries the
/// HTTP status it stands for (throttling is 429), so a busy model is
/// retried on the fallback like any other. A framing error ends the read.
pub struct BedrockSse<R> {
    inner: R,
    out: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> BedrockSse<R> {
    pub fn new(inner: R) -> Self {
        BedrockSse {
            inner,
            out: Vec::new(),
            pos: 0,
            done: false,
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let msg = match read_message(&mut self.inner) {
            Ok(Some(m)) => m,
            Ok(None) => {
                self.done = true;
                return Ok(());
            }
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        };
        let data = match (msg.header(":message-type"), msg.header(":event-type")) {
            (Some("event"), Some("chunk")) => {
                let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap_or_default();
                v["bytes"]
                    .as_str()
                    .and_then(crate::media::b64_decode)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            }
            (Some("exception"), _) => {
                let kind = msg.header(":exception-type").unwrap_or("exception");
                let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap_or_default();
                let code = match kind {
                    "throttlingException" => 429,
                    "serviceUnavailableException" => 503,
                    "internalServerException" => 500,
                    "modelTimeoutException" => 504,
                    _ => 400,
                };
                Some(
                    serde_json::json!({"type": "error", "error": {
                        "type": kind,
                        "message": format!("Bedrock {kind} (HTTP {code}): {}", v["message"].as_str().unwrap_or("")),
                    }})
                    .to_string(),
                )
            }
            _ => None,
        };
        self.out.clear();
        self.pos = 0;
        if let Some(d) = data {
            self.out.extend_from_slice(b"data: ");
            self.out.extend_from_slice(d.replace('\n', " ").as_bytes());
            self.out.extend_from_slice(b"\n\n");
        }
        Ok(())
    }
}

impl<R: Read> Read for BedrockSse<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.out.len() {
            if self.done {
                return Ok(0);
            }
            self.fill()?;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
