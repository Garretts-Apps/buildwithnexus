// Session token/cost accounting. Every completed model request (streaming or
// blocking, main loop, sub-agents, the /model probe) records the server's
// `usage` block into one process-wide ledger; `/cost`, `/context`, the footer
// meter, and the `--max-budget-usd` guard all read from it.
//
// Prices are a static snapshot (USD per million tokens) matched by the longest
// model-name prefix. An unknown model is reported as "unknown price" — tokens
// are always shown, a dollar figure is never invented.

use std::sync::Mutex;

/// Token counts for one request, normalized across protocols: `input` is the
/// uncached prompt portion, so the full prompt is
/// `input + cache_read + cache_write` (Anthropic reports the three separately;
/// OpenAI's `prompt_tokens` includes `cached_tokens`, which is split out here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    /// Everything the model read this request.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const fn price(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Price {
    Price {
        input,
        output,
        cache_read,
        cache_write,
    }
}

// Snapshot of list prices (mid-2026). Longest matching prefix wins, so the
// dated/suffixed variants ("gpt-4o-2024-11-20", "claude-sonnet-4-6-…") fall
// under their family and "gpt-4o-mini" is not priced as "gpt-4o". OpenAI has
// no cache-write premium, so cache_write equals input there.
const PRICES: &[(&str, Price)] = &[
    // Anthropic
    ("claude-fable-5-1", price(10.0, 50.0, 0.25, 12.5)),
    ("claude-fable-5", price(10.0, 50.0, 1.0, 12.5)),
    ("claude-opus-5", price(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-8", price(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-7", price(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-6", price(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-5", price(5.0, 25.0, 0.5, 6.25)),
    ("claude-opus-4-1", price(15.0, 75.0, 1.5, 18.75)),
    ("claude-opus-4", price(15.0, 75.0, 1.5, 18.75)),
    ("claude-sonnet-5", price(2.0, 10.0, 0.2, 2.5)),
    ("claude-sonnet-4-6", price(3.0, 15.0, 0.3, 3.75)),
    ("claude-sonnet-4-5", price(3.0, 15.0, 0.3, 3.75)),
    ("claude-sonnet-4", price(3.0, 15.0, 0.3, 3.75)),
    ("claude-haiku-4-5", price(1.0, 5.0, 0.1, 1.25)),
    // OpenAI
    ("gpt-4o-mini", price(0.15, 0.60, 0.075, 0.15)),
    ("gpt-4o", price(2.5, 10.0, 1.25, 2.5)),
    ("gpt-4.1-nano", price(0.1, 0.4, 0.025, 0.1)),
    ("gpt-4.1-mini", price(0.4, 1.6, 0.1, 0.4)),
    ("gpt-4.1", price(2.0, 8.0, 0.5, 2.0)),
    ("gpt-5-nano", price(0.05, 0.4, 0.005, 0.05)),
    ("gpt-5-mini", price(0.25, 2.0, 0.025, 0.25)),
    ("gpt-5", price(1.25, 10.0, 0.125, 1.25)),
    ("o3-mini", price(1.1, 4.4, 0.55, 1.1)),
    ("o3", price(2.0, 8.0, 0.5, 2.0)),
    ("o4-mini", price(1.1, 4.4, 0.275, 1.1)),
];

/// List price for a model, matched by the longest known prefix (case-insensitive).
pub fn price_for(model: &str) -> Option<Price> {
    let m = model.trim().to_ascii_lowercase();
    PRICES
        .iter()
        .filter(|(prefix, _)| m.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, p)| *p)
}

/// Estimated USD for one request; None when the model has no known price.
pub fn cost_usd(model: &str, u: &Usage) -> Option<f64> {
    let p = price_for(model)?;
    Some(
        (u.input as f64 * p.input
            + u.output as f64 * p.output
            + u.cache_read as f64 * p.cache_read
            + u.cache_write as f64 * p.cache_write)
            / 1_000_000.0,
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastRequest {
    pub model: String,
    pub usage: Usage,
    /// Some(0.0) for local providers; None when the price is unknown.
    pub cost_usd: Option<f64>,
    pub local: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub totals: Usage,
    pub requests: u64,
    /// Sum over every request with a known (or local, $0) price.
    pub cost_usd: f64,
    /// Requests against a remote model with no price entry — their cost is
    /// missing from `cost_usd`, never guessed.
    pub unpriced_requests: u64,
    pub unpriced_model: String,
    pub local_requests: u64,
    pub last: Option<LastRequest>,
}

pub struct Ledger {
    inner: Mutex<Snapshot>,
}

impl Ledger {
    pub const fn new() -> Self {
        Ledger {
            inner: Mutex::new(Snapshot {
                totals: Usage {
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                },
                requests: 0,
                cost_usd: 0.0,
                unpriced_requests: 0,
                unpriced_model: String::new(),
                local_requests: 0,
                last: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        // A poisoned lock only means a panic elsewhere; the counters are
        // still valid.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one completed request. `local` providers (Ollama, llama.cpp, LM
    /// Studio, a loopback custom endpoint) cost $0 regardless of model name.
    pub fn record(&self, model: &str, local: bool, usage: Usage) {
        let cost = if local {
            Some(0.0)
        } else {
            cost_usd(model, &usage)
        };
        let mut s = self.lock();
        s.totals.add(&usage);
        s.requests += 1;
        match cost {
            Some(c) => s.cost_usd += c,
            None => {
                s.unpriced_requests += 1;
                s.unpriced_model = model.to_string();
            }
        }
        if local {
            s.local_requests += 1;
        }
        s.last = Some(LastRequest {
            model: model.to_string(),
            usage,
            cost_usd: cost,
            local,
        });
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }

    /// Drop the last-request snapshot — after /clear, /compact, or a resume
    /// the measured prompt size no longer describes the live transcript.
    pub fn forget_last(&self) {
        self.lock().last = None;
    }
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

static SESSION: Ledger = Ledger::new();

pub fn record(model: &str, local: bool, usage: Usage) {
    SESSION.record(model, local, usage);
}

pub fn snapshot() -> Snapshot {
    SESSION.snapshot()
}

pub fn forget_last() {
    SESSION.forget_last();
}

/// Tokens the next request will carry, measured from the last one: its full
/// prompt plus the reply it produced. None until a request has reported usage.
pub fn last_context_tokens() -> Option<usize> {
    let s = snapshot();
    let last = s.last?;
    if last.usage.is_empty() {
        return None;
    }
    Some((last.usage.prompt_tokens() + last.usage.output) as usize)
}

// ── budget ─────────────────────────────────────────────────────────────────
// Bits of the f64 budget; 0 (= +0.0) doubles as "unset" — a zero budget would
// stop before the first request, which is never what anyone means.
static BUDGET_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_budget(usd: Option<f64>) {
    let bits = usd.filter(|b| *b > 0.0).map_or(0, f64::to_bits);
    BUDGET_BITS.store(bits, std::sync::atomic::Ordering::Relaxed);
}

pub fn budget() -> Option<f64> {
    match BUDGET_BITS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        bits => Some(f64::from_bits(bits)),
    }
}

/// The stop message when the session's estimated cost has passed the budget,
/// checked before each model request so the loop never dies mid-stream.
/// Unpriced requests count as $0 — the message says so when it matters.
pub fn budget_stop() -> Option<String> {
    budget_stop_msg(&snapshot(), budget()?)
}

fn budget_stop_msg(s: &Snapshot, limit: f64) -> Option<String> {
    if s.cost_usd <= limit {
        return None;
    }
    let mut msg = format!(
        "  ⛔ budget reached — estimated ${:.4} spent of the ${limit:.2} limit (--max-budget-usd); stopping before the next model request",
        s.cost_usd
    );
    if s.unpriced_requests > 0 {
        msg.push_str(&format!(
            " ({} request{} with unknown price for {} not counted)",
            s.unpriced_requests,
            if s.unpriced_requests == 1 { "" } else { "s" },
            s.unpriced_model
        ));
    }
    Some(msg)
}

fn fmt_tokens(n: u64) -> String {
    if n < 10_000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Lines for `/cost`: tokens by category, request count, and the cost line —
/// a dollar estimate, "local", or "unknown price for <model>".
pub fn render(s: &Snapshot, current_model: &str) -> Vec<String> {
    let t = &s.totals;
    let mut out = vec![
        format!(
            "  tokens   input {}  ·  output {}  ·  cache read {}  ·  cache write {}",
            fmt_tokens(t.input),
            fmt_tokens(t.output),
            fmt_tokens(t.cache_read),
            fmt_tokens(t.cache_write)
        ),
        format!(
            "  requests {}{}",
            s.requests,
            match &s.last {
                Some(l) if !l.usage.is_empty() => format!(
                    "  ·  last: {} prompt + {} output ({})",
                    fmt_tokens(l.usage.prompt_tokens()),
                    fmt_tokens(l.usage.output),
                    l.model
                ),
                _ => String::new(),
            }
        ),
    ];
    let cost_line = if s.requests == 0 {
        match price_for(current_model) {
            Some(_) => "  cost     $0.0000 (no requests yet)".to_string(),
            None => format!("  cost     unknown price for {current_model} (tokens only)"),
        }
    } else if s.requests == s.local_requests {
        "  cost     $0.00 (local)".to_string()
    } else if s.unpriced_requests == s.requests {
        format!(
            "  cost     unknown price for {} (tokens only)",
            s.unpriced_model
        )
    } else {
        let mut line = format!("  cost     ~${:.4} estimated", s.cost_usd);
        if s.unpriced_requests > 0 {
            line.push_str(&format!(
                " — {} request{} with unknown price for {} not counted",
                s.unpriced_requests,
                if s.unpriced_requests == 1 { "" } else { "s" },
                s.unpriced_model
            ));
        }
        line
    };
    out.push(cost_line);
    if let Some(b) = budget() {
        out.push(format!("  budget   ${b:.2} (max_budget_usd)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn price_matches_longest_prefix() {
        assert_eq!(
            price_for("gpt-4o-mini-2024-07-18"),
            price_for("gpt-4o-mini")
        );
        assert_ne!(price_for("gpt-4o-mini"), price_for("gpt-4o"));
        assert_eq!(price_for("GPT-4o"), price_for("gpt-4o"));
        assert_eq!(
            price_for("claude-sonnet-4-6-20260101"),
            price_for("claude-sonnet-4-6")
        );
        assert_eq!(price_for("claude-opus-4-1"), price_for("claude-opus-4"));
        assert_ne!(price_for("claude-opus-4-6"), price_for("claude-opus-4"));
        assert!(price_for("gpt-5-mini").is_some());
        assert!(price_for("o4-mini").is_some());
        assert!(price_for("o3-2025-04-16").is_some());
        assert!(price_for("claude-haiku-4-5").is_some());
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(price_for("llama3.2").is_none());
        assert!(price_for("local-model").is_none());
        assert!(price_for("").is_none());
        assert!(cost_usd("qwen2.5-coder", &u(100, 100, 0, 0)).is_none());
    }

    #[test]
    fn cost_uses_per_million_rates_by_category() {
        // sonnet-4-6: 3 / 15 / 0.3 / 3.75 per MTok
        let c = cost_usd(
            "claude-sonnet-4-6",
            &u(1_000_000, 1_000_000, 1_000_000, 1_000_000),
        )
        .unwrap();
        assert!((c - (3.0 + 15.0 + 0.3 + 3.75)).abs() < 1e-9);
        assert_eq!(cost_usd("gpt-4o", &Usage::default()), Some(0.0));
    }

    #[test]
    fn usage_prompt_tokens_sums_all_prompt_categories() {
        assert_eq!(u(10, 5, 20, 30).prompt_tokens(), 60);
        assert!(Usage::default().is_empty());
        assert!(!u(0, 1, 0, 0).is_empty());
    }

    #[test]
    fn ledger_accumulates_and_prices_requests() {
        let l = Ledger::new();
        l.record("gpt-4o", false, u(1_000_000, 0, 0, 0));
        l.record("gpt-4o", false, u(0, 1_000_000, 0, 0));
        let s = l.snapshot();
        assert_eq!(s.requests, 2);
        assert_eq!(s.totals, u(1_000_000, 1_000_000, 0, 0));
        assert!((s.cost_usd - 12.5).abs() < 1e-9);
        assert_eq!(s.unpriced_requests, 0);
        let last = s.last.unwrap();
        assert_eq!(last.usage, u(0, 1_000_000, 0, 0));
        assert_eq!(last.model, "gpt-4o");
        assert!((last.cost_usd.unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn ledger_tracks_unpriced_and_local_requests() {
        let l = Ledger::new();
        l.record("mystery-model", false, u(10, 10, 0, 0));
        l.record("llama3.2", true, u(10, 10, 0, 0));
        let s = l.snapshot();
        assert_eq!(s.requests, 2);
        assert_eq!(s.unpriced_requests, 1);
        assert_eq!(s.unpriced_model, "mystery-model");
        assert_eq!(s.local_requests, 1);
        assert_eq!(s.cost_usd, 0.0);
        assert_eq!(s.last.as_ref().unwrap().cost_usd, Some(0.0));
        assert!(s.last.as_ref().unwrap().local);
    }

    #[test]
    fn ledger_forget_last_keeps_totals() {
        let l = Ledger::new();
        l.record("gpt-4o", false, u(5, 5, 0, 0));
        l.forget_last();
        let s = l.snapshot();
        assert!(s.last.is_none());
        assert_eq!(s.requests, 1);
        assert_eq!(s.totals, u(5, 5, 0, 0));
    }

    #[test]
    fn budget_zero_or_negative_means_unset() {
        set_budget(Some(0.0));
        assert_eq!(budget(), None);
        set_budget(Some(-1.0));
        assert_eq!(budget(), None);
        set_budget(Some(2.5));
        assert_eq!(budget(), Some(2.5));
        set_budget(None);
        assert_eq!(budget(), None);
    }

    #[test]
    fn budget_stop_fires_only_above_the_limit_and_names_unpriced_requests() {
        let mut s = Snapshot {
            requests: 2,
            cost_usd: 1.0,
            ..Snapshot::default()
        };
        assert!(budget_stop_msg(&s, 1.0).is_none(), "at the limit is fine");
        let msg = budget_stop_msg(&s, 0.5).unwrap();
        assert!(msg.contains("$1.0000 spent of the $0.50 limit"), "{msg}");
        assert!(!msg.contains("unknown price"));
        s.unpriced_requests = 1;
        s.unpriced_model = "mystery".into();
        let msg = budget_stop_msg(&s, 0.5).unwrap();
        assert!(
            msg.contains("1 request with unknown price for mystery"),
            "{msg}"
        );
    }

    #[test]
    fn render_reports_tokens_requests_and_cost_states() {
        let mut s = Snapshot::default();
        // No requests yet, priced model.
        let lines = render(&s, "gpt-4o");
        assert!(lines[0].contains("input 0"));
        assert!(lines[1].starts_with("  requests 0"));
        assert!(lines[2].contains("$0.0000"));
        // No requests yet, unknown model.
        assert!(render(&s, "llama3.2")[2].contains("unknown price for llama3.2"));

        s.requests = 2;
        s.local_requests = 2;
        s.totals = u(12_345, 678, 0, 0);
        let lines = render(&s, "llama3.2");
        assert!(lines[0].contains("input 12.3k"), "{}", lines[0]);
        assert!(lines[0].contains("output 678"));
        assert!(lines[2].contains("$0.00 (local)"));

        let mut s = Snapshot {
            requests: 1,
            unpriced_requests: 1,
            unpriced_model: "mystery".into(),
            ..Snapshot::default()
        };
        assert!(render(&s, "mystery")[2].contains("unknown price for mystery"));

        s.requests = 3;
        s.cost_usd = 0.0123;
        s.last = Some(LastRequest {
            model: "gpt-4o".into(),
            usage: u(100, 20, 50, 0),
            cost_usd: Some(0.001),
            local: false,
        });
        let lines = render(&s, "gpt-4o");
        assert!(lines[1].contains("last: 150 prompt + 20 output (gpt-4o)"));
        assert!(lines[2].contains("~$0.0123 estimated"));
        assert!(lines[2].contains("1 request with unknown price for mystery"));
    }

    #[test]
    fn last_context_tokens_is_prompt_plus_output() {
        let l = Ledger::new();
        l.record("gpt-4o", false, u(100, 20, 50, 5));
        let last = l.snapshot().last.unwrap();
        assert_eq!(last.usage.prompt_tokens() + last.usage.output, 175);
        l.record("gpt-4o", false, Usage::default());
        assert!(l.snapshot().last.unwrap().usage.is_empty());
    }
}
