// Multimodal input plumbing: vision-capability detection, video parsing via
// ffmpeg/ffprobe (sampled frames + metadata for vision models), and clipboard
// image/text capture so users can paste screenshots and clips directly into
// the composer.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::Protocol;
use crate::provider::Provider;

// ── vision capability ────────────────────────────────────────────────────────

// Whether the active model can accept image parts. Attachments are gated on
// this: sending images to a text-only model either errors or silently drops
// them, both worse than telling the user up front.
pub fn model_supports_vision(p: &Provider) -> bool {
    let m = p.model.to_lowercase();
    match p.protocol {
        // Every current Anthropic chat model (Claude 3 onward) accepts images.
        Protocol::Anthropic => true,
        // OpenAI-compat endpoints front many backends — go by model name.
        Protocol::OpenAi => {
            model_name_has_vision_hint(&m)
                || m.starts_with("gpt-4o")
                || m.starts_with("gpt-4.1")
                || m.starts_with("gpt-4-turbo")
                || m.starts_with("gpt-5")
                || m.starts_with("chatgpt-4o")
                || m.starts_with("o1")
                || m.starts_with("o3")
                || m.starts_with("o4")
                || m.contains("gpt-4-vision")
                || m.contains("claude")
                || m.contains("gemini")
        }
        // Local models are mostly text-only; require an explicit vision hint.
        Protocol::OllamaNative => model_name_has_vision_hint(&m),
    }
}

// Name fragments that reliably indicate a vision-capable model across the
// open-model ecosystem (Ollama tags, HF names, OpenRouter slugs).
fn model_name_has_vision_hint(m: &str) -> bool {
    const HINTS: &[&str] = &[
        "llava",
        "vision",
        "-vl",
        "vl-",
        "2vl",
        ".5vl",
        "pixtral",
        "moondream",
        "bakllava",
        "minicpm-v",
        "internvl",
        "gemma3",
        "gemma-3",
        "llama4",
        "llama-4",
        "multimodal",
        "smolvlm",
    ];
    HINTS.iter().any(|h| m.contains(h))
}

// ── video parsing (ffmpeg / ffprobe) ─────────────────────────────────────────

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "webm", "mkv", "avi", "m4v", "gifv", "mpg", "mpeg",
];
pub const MAX_VIDEO_FRAMES: usize = 8;

pub struct VideoAttachment {
    /// Sampled frames as (media_type, base64) pairs, ready for Msg::UserImages.
    pub frames: Vec<(String, String)>,
    /// Human/model-readable metadata block (duration, resolution, fps).
    pub summary: String,
}

pub fn ffmpeg_available() -> bool {
    have("ffmpeg") && have("ffprobe")
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct Probe {
    duration_secs: f64,
    width: u64,
    height: u64,
    fps: String,
}

fn ffprobe(path: &Path) -> Option<Probe> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = v["streams"].get(0)?;
    let duration_secs = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);
    let rate = stream["avg_frame_rate"].as_str().unwrap_or("");
    let fps = match rate.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (
                n.parse::<f64>().unwrap_or(0.0),
                d.parse::<f64>().unwrap_or(1.0),
            );
            if d > 0.0 && n > 0.0 {
                format!("{:.0}", n / d)
            } else {
                "?".to_string()
            }
        }
        None => "?".to_string(),
    };
    Some(Probe {
        duration_secs,
        width: stream["width"].as_u64().unwrap_or(0),
        height: stream["height"].as_u64().unwrap_or(0),
        fps,
    })
}

fn temp_seq() -> usize {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Parses a video into a multimodal attachment: probes metadata with ffprobe,
/// then samples up to MAX_VIDEO_FRAMES evenly spaced frames with ffmpeg
/// (scaled to ≤768px wide, jpeg). Returns None when ffmpeg/ffprobe are
/// missing or the file can't be decoded.
pub fn attach_video(path: &Path) -> Option<VideoAttachment> {
    if !ffmpeg_available() {
        return None;
    }
    let probe = ffprobe(path)?;
    let dir =
        std::env::temp_dir().join(format!("bwn-frames-{}-{}", std::process::id(), temp_seq()));
    std::fs::create_dir_all(&dir).ok()?;
    // Evenly sample across the whole clip. For very short clips the fps
    // filter simply yields fewer frames, which is fine.
    let sample_fps = MAX_VIDEO_FRAMES as f64 / probe.duration_secs.max(0.5);
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-vf",
            &format!("fps={sample_fps:.6},scale='min(768,iw)':-2"),
            "-frames:v",
            &MAX_VIDEO_FRAMES.to_string(),
            "-q:v",
            "5",
        ])
        .arg(dir.join("frame_%02d.jpg"))
        .status()
        .ok()?;
    let mut frames = Vec::new();
    if status.success() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        for p in paths {
            if let Ok(bytes) = std::fs::read(&p) {
                frames.push(("image/jpeg".to_string(), b64_encode(&bytes)));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if frames.is_empty() {
        return None;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let summary = format!(
        "[video: {} — {:.1}s, {}x{}, {} fps; {} frames sampled evenly across the clip, in order]",
        name,
        probe.duration_secs,
        probe.width,
        probe.height,
        probe.fps,
        frames.len()
    );
    Some(VideoAttachment { frames, summary })
}

// ── clipboard capture ────────────────────────────────────────────────────────

/// Grabs an image from the system clipboard into a temp PNG, if one is there.
/// Best effort across Wayland (wl-paste), X11 (xclip), macOS (pngpaste /
/// osascript) and WSL (powershell.exe).
pub fn clipboard_image_to_temp() -> Option<PathBuf> {
    let dest = std::env::temp_dir().join(format!(
        "bwn-paste-{}-{}.png",
        std::process::id(),
        temp_seq()
    ));

    // Wayland
    if have_quick("wl-paste") {
        if let Ok(t) = Command::new("wl-paste").arg("--list-types").output() {
            if String::from_utf8_lossy(&t.stdout).contains("image/") {
                if let Ok(o) = Command::new("wl-paste")
                    .args(["--type", "image/png"])
                    .output()
                {
                    if o.status.success() && !o.stdout.is_empty() {
                        std::fs::write(&dest, &o.stdout).ok()?;
                        return Some(dest);
                    }
                }
            }
        }
    }
    // X11
    if have_quick("xclip") {
        if let Ok(t) = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
        {
            if String::from_utf8_lossy(&t.stdout).contains("image/png") {
                if let Ok(o) = Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", "image/png", "-o"])
                    .output()
                {
                    if o.status.success() && !o.stdout.is_empty() {
                        std::fs::write(&dest, &o.stdout).ok()?;
                        return Some(dest);
                    }
                }
            }
        }
    }
    // macOS
    #[cfg(target_os = "macos")]
    {
        if have_quick("pngpaste") {
            if let Ok(s) = Command::new("pngpaste").arg(&dest).status() {
                if s.success() && dest.exists() {
                    return Some(dest);
                }
            }
        }
        let script = format!(
            "set f to (open for access POSIX file \"{}\" with write permission)\n\
             try\n write (the clipboard as «class PNGf») to f\n end try\n\
             close access f",
            dest.display()
        );
        if let Ok(s) = Command::new("osascript").args(["-e", &script]).status() {
            if s.success() {
                if let Ok(m) = std::fs::metadata(&dest) {
                    if m.len() > 0 {
                        return Some(dest);
                    }
                }
                let _ = std::fs::remove_file(&dest);
            }
        }
    }
    // WSL: powershell can't write into the Linux fs directly — round-trip
    // the PNG bytes as base64 over stdout.
    if crate::tools::is_wsl() {
        let ps = "$img = Get-Clipboard -Format Image; if ($img) { \
                  $ms = New-Object System.IO.MemoryStream; \
                  $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); \
                  [Convert]::ToBase64String($ms.ToArray()) }";
        if let Ok(o) = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", ps])
            .output()
        {
            let b64 = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && !b64.is_empty() {
                if let Some(bytes) = b64_decode(&b64) {
                    std::fs::write(&dest, bytes).ok()?;
                    return Some(dest);
                }
            }
        }
    }
    None
}

/// Plain-text clipboard read, used as the Ctrl+V fallback when no image is
/// on the clipboard.
pub fn clipboard_text() -> Option<String> {
    let attempts: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("pbpaste", &[]),
    ];
    for (bin, args) in attempts {
        if !have_quick(bin) {
            continue;
        }
        if let Ok(o) = Command::new(bin).args(*args).output() {
            if o.status.success() && !o.stdout.is_empty() {
                return Some(String::from_utf8_lossy(&o.stdout).into_owned());
            }
        }
    }
    if crate::tools::is_wsl() {
        if let Ok(o) = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Get-Clipboard"])
            .output()
        {
            if o.status.success() && !o.stdout.is_empty() {
                return Some(String::from_utf8_lossy(&o.stdout).into_owned());
            }
        }
    }
    None
}

// `which`-style existence check that doesn't run the binary (clipboard tools
// hang without a display when run with no args).
fn have_quick(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── base64 ───────────────────────────────────────────────────────────────────

const B64_ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(B64_ALPHA[b0 >> 2] as char);
        out.push(B64_ALPHA[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHA[((b1 & 0xf) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHA[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = b64_val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with(protocol: Protocol, model: &str) -> Provider {
        Provider {
            protocol,
            base_url: String::new(),
            api_key: None,
            model: model.to_string(),
            context_tokens: 0,
            temperature: None,
            max_tokens: None,
            ollama_ctx: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn vision_detection_by_protocol_and_name() {
        // Anthropic: always vision.
        assert!(model_supports_vision(&provider_with(
            Protocol::Anthropic,
            "claude-sonnet-4-5"
        )));
        // OpenAI-compat: known vision families yes, plain text models no.
        for m in ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "o3", "gemini-2.5-pro"] {
            assert!(
                model_supports_vision(&provider_with(Protocol::OpenAi, m)),
                "{m}"
            );
        }
        for m in ["gpt-3.5-turbo", "deepseek-chat", "mistral-7b-instruct"] {
            assert!(
                !model_supports_vision(&provider_with(Protocol::OpenAi, m)),
                "{m}"
            );
        }
        // Ollama: vision only with an explicit hint in the tag.
        for m in [
            "llava:13b",
            "qwen2.5vl:7b",
            "llama3.2-vision",
            "gemma3:4b",
            "minicpm-v",
        ] {
            assert!(
                model_supports_vision(&provider_with(Protocol::OllamaNative, m)),
                "{m}"
            );
        }
        for m in ["llama3.1:8b", "qwen2.5-coder:7b", "mistral:7b", "phi3"] {
            assert!(
                !model_supports_vision(&provider_with(Protocol::OllamaNative, m)),
                "{m}"
            );
        }
    }

    #[test]
    fn b64_round_trip() {
        for case in [
            &b""[..],
            &b"a"[..],
            &b"ab"[..],
            &b"abc"[..],
            &b"hello world, base64!"[..],
            &[0u8, 255, 128, 7, 42][..],
        ] {
            let enc = b64_encode(case);
            assert_eq!(b64_decode(&enc).as_deref(), Some(case), "{enc}");
        }
        // Whitespace and padding tolerated on decode.
        assert_eq!(b64_decode("aGk=\n").as_deref(), Some(&b"hi"[..]));
        assert_eq!(b64_decode("not base64!"), None);
    }

    #[test]
    fn video_ext_list_covers_common_containers() {
        for ext in ["mp4", "mov", "webm", "mkv"] {
            assert!(VIDEO_EXTS.contains(&ext));
        }
    }

    // Full pipeline against a real generated clip. Skips (rather than fails)
    // on machines without ffmpeg so CI stays green everywhere.
    #[test]
    fn attach_video_samples_frames_when_ffmpeg_present() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("bwn-vidtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let ok = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=3:size=320x240:rate=10",
                "-y",
            ])
            .arg(&clip)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "failed to synthesize test clip");
        let att = attach_video(&clip).expect("attach_video failed");
        assert!(!att.frames.is_empty() && att.frames.len() <= MAX_VIDEO_FRAMES);
        for (mt, b64) in &att.frames {
            assert_eq!(mt, "image/jpeg");
            // Valid base64 that decodes to a JPEG (FF D8 magic).
            let bytes = b64_decode(b64).expect("frame not base64");
            assert!(bytes.starts_with(&[0xFF, 0xD8]), "frame is not a JPEG");
        }
        assert!(att.summary.contains("320x240"), "{}", att.summary);
        assert!(att.summary.contains("frames sampled"), "{}", att.summary);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
