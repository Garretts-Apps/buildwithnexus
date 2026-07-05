use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{config, report, tui};

const MAX_EVENTS: usize = 500;
const MAX_DETAIL_CHARS: usize = 16_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceEvent {
    pub id: u64,
    pub created_ms: u128,
    pub kind: String,
    pub title: String,
    pub detail: Value,
}

struct TraceState {
    session_id: String,
    next_id: u64,
    events: Vec<TraceEvent>,
}

fn state() -> &'static Mutex<TraceState> {
    static STATE: OnceLock<Mutex<TraceState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(TraceState {
            session_id: format!("pid-{}", std::process::id()),
            next_id: 1,
            events: Vec::new(),
        })
    })
}

pub fn set_session(id: &str) {
    if let Ok(mut s) = state().lock() {
        s.session_id = id.to_string();
        s.next_id = 1;
        s.events.clear();
    }
}

pub fn record(kind: &str, title: impl Into<String>, detail: Value) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut event = TraceEvent {
        id: 0,
        created_ms: now,
        kind: kind.to_string(),
        title: title.into(),
        detail: clip_value(detail),
    };

    let (session_id, id) = match state().lock() {
        Ok(mut s) => {
            event.id = s.next_id;
            s.next_id += 1;
            s.events.push(event.clone());
            if s.events.len() > MAX_EVENTS {
                let extra = s.events.len() - MAX_EVENTS;
                s.events.drain(0..extra);
            }
            (s.session_id.clone(), event.id)
        }
        Err(_) => return 0,
    };

    persist(&session_id, &event);
    id
}

pub fn record_visible(kind: &str, title: impl Into<String>, detail: Value) -> u64 {
    let title = title.into();
    let id = record(kind, title.clone(), detail);
    if !report::is_json() && id > 0 {
        tui::line(&format!(
            "  {} {} #{} {}",
            tui::dim("trace"),
            tui::accent(kind),
            id,
            tui::dim(&title)
        ));
    }
    id
}

pub fn recent(limit: usize) -> Vec<TraceEvent> {
    match state().lock() {
        Ok(s) => s.events.iter().rev().take(limit).cloned().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn get(id: u64) -> Option<TraceEvent> {
    state()
        .lock()
        .ok()?
        .events
        .iter()
        .find(|e| e.id == id)
        .cloned()
}

pub fn render_list(limit: usize) {
    let events = recent(limit);
    if events.is_empty() {
        tui::line(&tui::dim("  no trace events yet"));
        return;
    }
    tui::line(&tui::accent("  trace events"));
    for e in events.into_iter().rev() {
        tui::line(&format!(
            "  {} {:<14} {}",
            tui::bold(&format!("#{}", e.id)),
            e.kind,
            e.title
        ));
    }
    tui::line(&tui::dim("  use /trace <id> to inspect event details"));
}

pub fn render_detail(id: u64) {
    let Some(e) = get(id) else {
        tui::line(&tui::red(&format!("  no trace event #{id}")));
        return;
    };
    tui::line(&format!(
        "  {} {} {}",
        tui::bold(&format!("#{}", e.id)),
        tui::accent(&e.kind),
        e.title
    ));
    tui::line(&tui::dim(&format!("  created_ms: {}", e.created_ms)));
    match serde_json::to_string_pretty(&e.detail) {
        Ok(s) => {
            for l in s.lines() {
                tui::line(&format!("  {l}"));
            }
        }
        Err(_) => tui::line(&format!("  {}", e.detail)),
    }
}

pub fn preview(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn persist(session_id: &str, event: &TraceEvent) {
    config::ensure_home();
    let dir = config::home().join("traces");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{session_id}.jsonl"));
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if let Ok(line) = serde_json::to_string(event) {
        let _ = writeln!(f, "{line}");
    }
}

fn clip_value(v: Value) -> Value {
    match v {
        Value::String(s) => {
            if s.chars().count() > MAX_DETAIL_CHARS {
                json!({
                    "truncated": true,
                    "chars": s.chars().count(),
                    "preview": preview(&s, MAX_DETAIL_CHARS),
                })
            } else {
                Value::String(s)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(clip_value).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, clip_value(v))).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_marks_truncation() {
        assert_eq!(preview("abcdef", 3), "abc…");
        assert_eq!(preview("abc", 3), "abc");
    }
}
