#[test]
fn a_name_the_server_does_not_list_is_noted_and_the_list_is_short() {
    let names = vec!["tinycoder-7b-q4_k_m.gguf".to_string()];
    assert!(unlisted_note("TinyCoder-7B-Q4_K_M.gguf", &names).is_none());
    assert!(unlisted_note("anything", &[]).is_none());
    let m = unlisted_note("tinycoder:9b", &names).unwrap();
    assert!(
        m.starts_with("the server lists only tinycoder-7b-q4_k_m.gguf;"),
        "{m}"
    );
    let many: Vec<String> = (0..11).map(|i| format!("m{i}")).collect();
    assert_eq!(
        served_summary(&many),
        "m0, m1, m2, m3, m4, m5, m6, m7, and 3 more"
    );
}

use super::*;
use std::io::Cursor;

#[test]
fn server_down_msg_names_host_not_path() {
    let m = server_down_msg("http://127.0.0.1:11434/v1/chat/completions");
    assert!(m.contains("http://127.0.0.1:11434 "));
    assert!(!m.contains("/v1/"));
    assert!(m.contains("ollama serve"));
    // A model server on the local network gets the same advice.
    let m = server_down_msg("http://192.168.50.10:11434/api/chat");
    assert!(m.contains("nothing is answering at http://192.168.50.10:11434 "));
    assert!(m.contains("ollama serve"));
    // A hosted API gets the network advice instead.
    let m = server_down_msg("https://api.example.com/v1/chat/completions");
    assert!(m.contains("https://api.example.com"), "{m}");
    assert!(!m.contains("ollama serve"), "{m}");
}

fn ctx<'a>(model: &'a str, has_key: bool, ollama: bool) -> FailureContext<'a> {
    FailureContext {
        model,
        base: "http://192.168.50.10:11434",
        has_key,
        key: None,
        ollama,
        in_session: true,
    }
}

fn explained(code: u16, body: &str, c: &FailureContext) -> String {
    format!(
        "{}\n  HTTP {code}: {}",
        status_advice(code, body, c),
        server_message(body)
    )
}

#[test]
fn a_script_is_told_flags_and_variables_not_slash_commands() {
    let mut c = ctx("gpt-4o", true, false);
    c.in_session = false;
    let m = explained(401, "{}", &c);
    assert!(
        m.starts_with("the API key was rejected by the provider — `buildwithnexus login`")
            && m.contains("CUSTOM_API_KEY")
            && !m.contains("/login"),
        "{m}"
    );
    c.has_key = false;
    let m = explained(401, "{}", &c);
    assert!(
        m.contains("wants an API key") && !m.contains("/login"),
        "{m}"
    );
    let m = explained(404, r#"{"error":"model not found"}"#, &c);
    assert!(m.contains("--model <name>") && !m.contains("/model"), "{m}");
    let m = explained(
        500,
        r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
        &ctx("m", false, true),
    );
    assert!(
        m.contains("smaller model with /model"),
        "session wording stays: {m}"
    );
    let mut o = ctx("m", false, true);
    o.in_session = false;
    let m = explained(
        500,
        r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
        &o,
    );
    assert!(
        m.contains("smaller model with --model <name>") && !m.contains("with /model"),
        "{m}"
    );
    // Not in a session (the default here): the unreachable-server hint names --base-url.
    let m = server_down_msg("http://127.0.0.1:1/v1");
    assert!(
        m.contains("--base-url <url>") && !m.contains("/model"),
        "{m}"
    );
}

fn provider_for(base_url: &str, key: Option<&str>) -> Provider {
    Provider {
        protocol: Protocol::OpenAi,
        base_url: base_url.into(),
        api_key: key.map(Into::into),
        model: "m".into(),
        context_tokens: 8192,
        temperature: None,
        max_tokens: None,
        effort: crate::config::Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    }
}

#[test]
fn a_rejected_key_names_where_it_came_from() {
    let mut c = ctx("claude-sonnet-4-6", true, false);
    c.in_session = false;
    c.key = Some(KeySource::Env("ANTHROPIC_API_KEY".into()));
    let m = explained(401, "{}", &c);
    assert!(
            m.starts_with(
                "ANTHROPIC_API_KEY (from your environment) was rejected (HTTP 401). Check or unset it; `buildwithnexus login` only changes the stored key, which this variable overrides."
            ),
            "{m}"
        );
    // A key that came from the environment is not fixed by `login`, and
    // CUSTOM_API_KEY is only named when it is the variable in use.
    assert!(
        !m.contains("replaces it") && !m.contains("CUSTOM_API_KEY"),
        "{m}"
    );

    c.key = Some(KeySource::Stored("OpenAI".into()));
    let m = explained(401, "{}", &c);
    assert!(
            m.starts_with(
                "The stored API key for OpenAI was rejected (HTTP 401). `buildwithnexus login` replaces it."
            ),
            "{m}"
        );
    assert!(
        !m.contains("CUSTOM_API_KEY") && !m.contains("environment"),
        "{m}"
    );

    c.key = Some(KeySource::Env("CUSTOM_API_KEY".into()));
    let m = explained(401, "{}", &c);
    assert!(
        m.starts_with("CUSTOM_API_KEY (from your environment) was rejected"),
        "{m}"
    );

    // In a session the replacement is a slash command.
    c.in_session = true;
    c.key = Some(KeySource::Env("OPENAI_API_KEY".into()));
    let m = explained(401, "{}", &c);
    assert!(
        m.contains("Check or unset it; /login only changes the stored key"),
        "{m}"
    );
    assert!(!m.contains("buildwithnexus login"), "{m}");
    c.key = Some(KeySource::Stored("Groq".into()));
    let m = explained(401, "{}", &c);
    assert!(
        m.starts_with("The stored API key for Groq was rejected (HTTP 401). /login replaces it."),
        "{m}"
    );

    // 403 from an environment key does not send the user to `login`.
    c.in_session = false;
    c.key = Some(KeySource::Env("OPENAI_API_KEY".into()));
    let m = explained(403, "{}", &c);
    assert!(
        m.contains("change OPENAI_API_KEY (from your environment;"),
        "{m}"
    );
    assert!(!m.contains("CUSTOM_API_KEY"), "{m}");

    // The wording the setup check reads as a rejected key still does.
    for k in [
        Some(KeySource::Env("OPENAI_API_KEY".into())),
        Some(KeySource::Stored("OpenAI".into())),
    ] {
        c.key = k;
        assert_eq!(
            crate::onboarding::Fail::from_error(&explained(401, "{}", &c)),
            crate::onboarding::Fail::KeyRejected(401)
        );
    }
}

#[test]
fn the_key_source_is_the_variable_that_holds_the_key_else_the_store() {
    let _g = crate::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved: Vec<_> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CUSTOM_API_KEY"]
        .map(|n| (n, std::env::var_os(n)))
        .into();
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("CUSTOM_API_KEY");

    assert_eq!(
        key_source(&provider_for("https://api.openai.com/v1", None)),
        None
    );
    let openai = provider_for("https://api.openai.com/v1", Some("sk-stored"));
    assert_eq!(
        key_source(&openai),
        Some(KeySource::Stored("OpenAI".into()))
    );
    std::env::set_var("OPENAI_API_KEY", "sk-stored");
    assert_eq!(
        key_source(&openai),
        Some(KeySource::Env("OPENAI_API_KEY".into()))
    );
    std::env::set_var("OPENAI_API_KEY", "sk-other");
    assert_eq!(
        key_source(&openai),
        Some(KeySource::Stored("OpenAI".into()))
    );

    // A gateway: the key is CUSTOM_API_KEY's, or the one saved for it.
    let gw = provider_for("http://127.0.0.1:9000/v1", Some("sk-gw"));
    assert_eq!(
        key_source(&gw),
        Some(KeySource::Stored("http://127.0.0.1:9000".into()))
    );
    std::env::set_var("CUSTOM_API_KEY", "sk-gw");
    assert_eq!(
        key_source(&gw),
        Some(KeySource::Env("CUSTOM_API_KEY".into()))
    );

    for (n, v) in saved {
        match v {
            Some(v) => std::env::set_var(n, v),
            None => std::env::remove_var(n),
        }
    }
}

#[test]
fn statuses_map_to_what_happened_and_what_to_do() {
    let _g = crate::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("BWN_MAX_RETRIES");
    let hosted = ctx("gpt-4o", true, false);
    let m = explained(401, r#"{"error":{"message":"invalid api key"}}"#, &hosted);
    assert!(
        m.starts_with("the API key was rejected by the provider — /login to replace it"),
        "{m}"
    );
    assert!(m.ends_with("HTTP 401: invalid api key"), "{m}");
    assert!(explained(401, "{}", &ctx("m", false, false)).contains("wants an API key"));

    let ollama = ctx("tinycoder:7b", false, true);
    let m = explained(
        404,
        r#"{"error":"model \"tinycoder:7b\" not found, try pulling it first"}"#,
        &ollama,
    );
    assert!(
        m.contains("model tinycoder:7b is not installed on http://192.168.50.10:11434"),
        "{m}"
    );
    assert!(m.contains("ollama pull tinycoder:7b"), "{m}");
    let m = explained(
        404,
        r#"{"error":{"message":"The model `gpt-9` does not exist"}}"#,
        &hosted,
    );
    assert!(m.contains("does not know model gpt-4o"), "{m}");
    assert!(explained(404, "404 page not found", &hosted).contains("check base_url"));

    let m = explained(
        400,
        r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":2150,"n_ctx":2048}}"#,
        &ctx("tiny.gguf", false, false),
    );
    assert!(
            m.starts_with("the prompt needs 2,150 tokens and the server holds 2,048 — restart llama-server with a larger -c, or set context_tokens"),
            "{m}"
        );
    let m = explained(
        400,
        r#"{"error":"Trying to keep the first 56390 tokens when context the overflows. However, the model is loaded with context length of only 4096 tokens, which is not enough."}"#,
        &ctx("tiny", false, false),
    );
    assert!(
        m.starts_with("the prompt needs 56,390 tokens and the server holds 4,096"),
        "{m}"
    );
    // The server's own words stay, so the agent's overflow recovery
    // still recognises the failure.
    assert!(m.contains("context length"), "{m}");
    let m = explained(
        400,
        r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130500 tokens."}}"#,
        &hosted,
    );
    assert!(
        m.starts_with("the prompt needs 130,500 tokens and the server holds 128,000"),
        "{m}"
    );

    let m = explained(
        500,
        r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
        &ollama,
    );
    assert!(
        m.starts_with(
            "the model needs 5.6 GiB but 3.1 GiB is free — pick a smaller model with /model"
        ),
        "{m}"
    );
    let m = explained(500, r#"{"error":{"message":"upstream exploded"}}"#, &hosted);
    assert!(
        m.starts_with("the server failed after 3 retries — BWN_MAX_RETRIES"),
        "{m}"
    );
    assert!(m.ends_with("HTTP 500: upstream exploded"), "{m}");
    let m = explained(503, r#"{"error":{"message":"Loading model"}}"#, &ollama);
    assert!(
        m.starts_with("the server is still loading tinycoder:7b after 14 retries"),
        "{m}"
    );
    assert!(
        explained(429, "{}", &hosted).starts_with("rate-limited by the provider after 3 retries")
    );
}

#[test]
fn definitive_failures_are_not_retried() {
    let _g = crate::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("BWN_MAX_RETRIES");
    for (code, body) in [
        (400, "{}"),
        (401, "{}"),
        (404, r#"{"error":"model not found"}"#),
        (
            500,
            r#"{"error":"model requires more system memory (5.6 GiB) than is available (3.1 GiB)"}"#,
        ),
        (500, r#"{"error":"CUDA error: out of memory"}"#),
    ] {
        assert_eq!(retries_for(code, body), 0, "{code} {body}");
    }
    assert_eq!(retries_for(500, r#"{"error":"boom"}"#), 3);
    assert_eq!(retries_for(429, "{}"), 3);
    assert_eq!(retries_for(503, r#"{"error":"Loading model"}"#), 14);
    std::env::set_var("BWN_MAX_RETRIES", "0");
    assert_eq!(retries_for(500, r#"{"error":"boom"}"#), 0);
    assert_eq!(retries_for(503, "Loading model"), 0);
    std::env::set_var("BWN_MAX_RETRIES", "999");
    assert_eq!(retries_for(500, "{}"), MAX_RETRIES_CAP);
    std::env::remove_var("BWN_MAX_RETRIES");
}

// A one-shot proxy that answers its first request with `reply`.
fn proxy_answering(reply: &'static str) -> u16 {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(reply.as_bytes());
        }
    });
    port
}

fn through(proxy: &str, url: &str) -> String {
    let client = crate::net::Client::with_lookup(
        |k: &str| (k == "HTTPS_PROXY").then(|| proxy.to_string()),
        |b| b,
    );
    let e = client.get(url).call().unwrap_err();
    let route = client.proxy_route(url).unwrap();
    proxy_failure(url, &route, &e, io_kind(&e))
}

#[test]
fn proxy_failures_name_the_proxy_and_the_setting() {
    let url = "https://gw.example.test/v1/chat/completions";
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let m = through(&format!("http://127.0.0.1:{closed}"), url);
    assert!(
        m.starts_with(&format!(
            "the proxy in HTTPS_PROXY (http://127.0.0.1:{closed}) refused the connection"
        )),
        "{m}"
    );
    assert!(m.contains("add gw.example.test to NO_PROXY"), "{m}");

    let port =
        proxy_answering("HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n");
    let m = through(&format!("http://127.0.0.1:{port}"), url);
    assert_eq!(
        m,
        format!("the proxy wants credentials — HTTPS_PROXY=http://user:pass@127.0.0.1:{port}")
    );

    let port = proxy_answering("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    let m = through(&format!("http://127.0.0.1:{port}"), url);
    assert!(
        m.contains("would not connect to gw.example.test:443"),
        "{m}"
    );
    assert!(m.contains("NO_PROXY"), "{m}");
}

#[test]
fn a_bare_refusal_from_a_proxy_is_named_as_the_proxy() {
    let r = crate::net::ProxyRoute {
        var: "HTTP_PROXY",
        shown: "http://127.0.0.1:19109".into(),
        host_port: "127.0.0.1:19109".into(),
        has_credentials: false,
        unusable: None,
    };
    let m = proxy_status(403, "", &r, "http://vm:19100/v1").unwrap();
    assert!(
            m.starts_with("HTTP 403 from proxy http://127.0.0.1:19109 for vm:19100 — if vm is on your network, add vm to NO_PROXY"),
            "{m}"
        );
    assert!(proxy_status(403, r#"{"error":"no"}"#, &r, "http://vm:19100/v1").is_none());
    assert!(proxy_status(407, "", &r, "http://vm:19100/v1")
        .unwrap()
        .contains("HTTP_PROXY=http://user:pass@127.0.0.1:19109"));
}

#[test]
fn a_stalled_response_names_the_read_timeout_setting() {
    let p = Provider {
        protocol: Protocol::OllamaNative,
        base_url: "http://localhost:11434".into(),
        api_key: None,
        model: "tinycoder:3b".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    let m = explain_failure(
        &p,
        "stream read failed: Network Error: timed out reading response".into(),
    );
    assert!(m.contains("BWN_READ_TIMEOUT_SECS"), "{m}");
    assert!(m.contains("http://localhost:11434"), "{m}");
    // Anything that is not a status or a timeout passes through.
    assert_eq!(explain_failure(&p, "interrupted".into()), "interrupted");
}

type Captured = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

// One-shot HTTP server: answers `responses.len()` requests with the given
// (status, body) pairs, then exits. Runs the real wire path end-to-end.
fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
    let (base, handle, _) = mock_server_capture(responses);
    (base, handle)
}

// Same, also handing back each request body it received (in order) so a
// test can assert on the wire shape the client actually sent.
fn mock_server_capture(
    responses: Vec<(u16, &'static str)>,
) -> (String, std::thread::JoinHandle<()>, Captured) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured: Captured = Default::default();
    let sink = captured.clone();
    let handle = std::thread::spawn(move || {
        for (code, body) in responses {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                .unwrap();
            // Drain the request without caring about its shape.
            let mut buf = [0u8; 16384];
            let mut seen = Vec::new();
            loop {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        // Headers done and some body bytes read is enough.
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.ends_with(b"}") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body_start = seen
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(seen.len(), |i| i + 4);
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&seen[body_start..]).into_owned());
            let resp = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    (base, handle, captured)
}

#[test]
fn validate_passes_on_ok_and_reports_auth_and_connect_failures() {
    let ok_body =
        r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
    let auth_body = r#"{"error":{"message":"invalid api key"}}"#;
    let (base, handle) = mock_server(vec![(200, ok_body), (401, auth_body)]);
    let p = Provider {
        protocol: Protocol::OpenAi,
        base_url: base,
        api_key: Some("sk-test".into()),
        model: "test-model".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    assert!(validate(&p).is_ok(), "200 must validate");
    let err = validate(&p).unwrap_err();
    assert!(
        err.contains("401"),
        "auth failure must surface the status: {err}"
    );
    handle.join().unwrap();
}

fn tc(id: &str, name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    }
}

// ── redact ──────────────────────────────────────────────────────────────
#[test]
fn redact_blanks_known_key_prefixes() {
    // Prefix tokens must clear the 12-char length floor to be flagged.
    assert!(redact("error: sk-ABCDEFGHIJKLMNOP failed").contains("[redacted]"));
    assert!(redact("token AIzaSyABCDEFGHIJKL bad").contains("[redacted]"));
    assert!(redact("hf_ABCDEFGHIJKLMNOP nope").contains("[redacted]"));
}

#[test]
fn redact_blanks_long_opaque_tokens() {
    // A whitespace-delimited token of >32 alnum chars is treated as secretish.
    let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEF"; // 42 alnum
    assert!(redact(&format!("token {long} end")).contains("[redacted]"));
    // The surrounding words survive.
    let out = redact(&format!("token {long} end"));
    assert!(out.starts_with("token ") && out.ends_with(" end"));
}

#[test]
fn redact_leaves_ordinary_text_untouched() {
    let msg = "the quick brown fox jumps over the lazy dog";
    assert_eq!(redact(msg), msg);
    // Short sk- prefixed words are below the length floor and survive.
    assert_eq!(redact("sk-short"), "sk-short");
}

#[test]
fn redact_preserves_punctuation_around_secret() {
    let out = redact("(\"sk-ABCDEFGHIJKLMNOP\")");
    assert!(out.contains("[redacted]"));
    assert!(out.starts_with("(\"") && out.ends_with("\")"));
}

#[test]
fn redact_empty_string() {
    assert_eq!(redact(""), "");
}

// ── parse_args ──────────────────────────────────────────────────────────
#[test]
fn parse_args_empty_is_object() {
    assert_eq!(parse_args(""), json!({}));
    assert_eq!(parse_args("   "), json!({}));
}

#[test]
fn parse_args_valid_json() {
    assert_eq!(parse_args(r#"{"a":1}"#), json!({"a": 1}));
}

#[test]
fn parse_args_invalid_is_flagged() {
    let v = parse_args("not json");
    assert_eq!(v[crate::tools::INVALID_ARGS], json!("not json"));
}

#[test]
fn parse_args_partial_json_is_flagged() {
    let v = parse_args(r#"{"a":"#);
    assert_eq!(v[crate::tools::INVALID_ARGS], json!(r#"{"a":"#));
}

// ── retry classification ────────────────────────────────────────────────
#[test]
fn transient_status_covers_rate_limits_and_5xx() {
    for code in [429, 500, 502, 503, 504, 529] {
        assert!(is_transient_status(code), "{code} should be transient");
    }
}

#[test]
fn non_transient_status_includes_client_errors_and_501() {
    for code in [400, 401, 403, 404, 422, 501] {
        assert!(!is_transient_status(code), "{code} should not retry");
    }
}

#[test]
fn retry_after_parses_seconds_and_caps_at_60s() {
    assert_eq!(retry_after_ms(Some("2")), Some(2_000));
    assert_eq!(retry_after_ms(Some(" 30 ")), Some(30_000));
    assert_eq!(retry_after_ms(Some("300")), Some(60_000));
    // HTTP-date form and garbage fall back to exponential backoff.
    assert_eq!(retry_after_ms(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
    assert_eq!(retry_after_ms(None), None);
}

// ── cache_last_message ──────────────────────────────────────────────────
#[test]
fn cache_last_message_empty_is_noop() {
    let mut m: Vec<Value> = vec![];
    cache_last_message(&mut m);
    assert!(m.is_empty());
}

#[test]
fn cache_last_message_string_body_becomes_text_block() {
    let mut m = vec![json!({"role": "user", "content": "hi"})];
    cache_last_message(&mut m);
    let block = &m[0]["content"][0];
    assert_eq!(block["type"], "text");
    assert_eq!(block["text"], "hi");
    assert_eq!(block["cache_control"]["type"], "ephemeral");
}

#[test]
fn cache_last_message_array_body_marks_last_block() {
    let mut m = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "a"},
        {"type": "text", "text": "b"}
    ]})];
    cache_last_message(&mut m);
    assert!(m[0]["content"][0].get("cache_control").is_none());
    assert_eq!(m[0]["content"][1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn cache_last_message_only_touches_last() {
    let mut m = vec![
        json!({"role": "user", "content": "first"}),
        json!({"role": "user", "content": "second"}),
    ];
    cache_last_message(&mut m);
    assert!(m[0]["content"].is_string());
    assert_eq!(m[1]["content"][0]["cache_control"]["type"], "ephemeral");
}

// ── anthropic_body ──────────────────────────────────────────────────────
#[test]
fn anthropic_body_merges_system_blocks() {
    let msgs = vec![
        Msg::System("one".into()),
        Msg::System("two".into()),
        Msg::User("hello".into()),
    ];
    let b = anthropic_body("m", &msgs, &[], None);
    assert_eq!(b["system"][0]["text"], "one\n\ntwo");
    assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(b["max_tokens"], 8192);
}

#[test]
fn anthropic_body_max_tokens_default_and_override() {
    let msgs = vec![Msg::User("hi".into())];
    assert_eq!(anthropic_body("m", &msgs, &[], None)["max_tokens"], 8192);
    assert_eq!(
        anthropic_body("m", &msgs, &[], Some(1234))["max_tokens"],
        1234
    );
}

#[test]
fn anthropic_body_no_system_no_tools_keys() {
    let b = anthropic_body("m", &[Msg::User("hi".into())], &[], None);
    assert!(b.get("system").is_none());
    assert!(b.get("tools").is_none());
}

#[test]
fn anthropic_body_assistant_text_and_calls() {
    let msgs = vec![Msg::Assistant {
        text: "thinking".into(),
        calls: vec![tc("t1", "read_file", json!({"path": "a"}))],
    }];
    let b = anthropic_body("m", &msgs, &[], None);
    let content = &b["messages"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["id"], "t1");
    assert_eq!(content[1]["name"], "read_file");
}

#[test]
fn anthropic_body_empty_assistant_text_omits_text_block() {
    let msgs = vec![Msg::Assistant {
        text: String::new(),
        calls: vec![tc("t1", "x", json!({}))],
    }];
    let b = anthropic_body("m", &msgs, &[], None);
    assert_eq!(b["messages"][0]["content"][0]["type"], "tool_use");
}

#[test]
fn anthropic_body_tool_results() {
    let msgs = vec![Msg::Tool(vec![ToolResult {
        id: "t1".into(),
        content: "ok".into(),
        is_error: false,
        images: Vec::new(),
    }])];
    let b = anthropic_body("m", &msgs, &[], None);
    let block = &b["messages"][0]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "t1");
    assert_eq!(block["is_error"], false);
    // Text only: the content stays a plain string.
    assert_eq!(block["content"], "ok");
}

// A round where one tool returned an image and one did not, after the
// assistant turn that made both calls.
fn image_round() -> Vec<Msg> {
    vec![
        Msg::Assistant {
            text: String::new(),
            calls: vec![
                tc("c1", "read_file", json!({"path": "a.png"})),
                tc("c2", "read_file", json!({"path": "b.txt"})),
            ],
        },
        Msg::Tool(vec![
            ToolResult {
                id: "c1".into(),
                content: "image a.png".into(),
                is_error: false,
                images: vec![("image/png".into(), "QUJD".into())],
            },
            ToolResult {
                id: "c2".into(),
                content: "hello".into(),
                is_error: false,
                images: Vec::new(),
            },
        ]),
    ]
}

#[test]
fn anthropic_body_puts_tool_images_inside_the_tool_result() {
    let b = anthropic_body("m", &image_round(), &[], None);
    let results = &b["messages"][1]["content"];
    assert_eq!(
        results[0]["content"],
        json!([
            {"type": "text", "text": "image a.png"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
        ])
    );
    assert_eq!(results[1]["content"], "hello");
    // No empty text block when the tool said nothing.
    let mut bare = image_round();
    if let Msg::Tool(rs) = &mut bare[1] {
        rs[0].content.clear();
    }
    let b = anthropic_body("m", &bare, &[], None);
    assert_eq!(
        b["messages"][1]["content"][0]["content"][0]["type"],
        "image"
    );
}

#[test]
fn openai_body_follows_the_tool_messages_with_their_images() {
    let b = openai_body("m", &image_round(), &[], None, None);
    let m = b["messages"].as_array().unwrap();
    assert_eq!(m.len(), 4, "{m:?}");
    assert_eq!(m[1]["role"], "tool");
    assert_eq!(m[1]["content"], "image a.png");
    assert_eq!(m[2]["role"], "tool");
    assert_eq!(m[3]["role"], "user");
    assert_eq!(
        m[3]["content"],
        json!([
            {"type": "text", "text": "[1 image returned by the read_file call c1]"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
        ])
    );
    // A round without images adds nothing.
    let b = openai_body("m", &image_round()[..1], &[], None, None);
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn ollama_body_follows_the_tool_messages_with_their_images() {
    let b = ollama_body("m", &image_round(), &[], None, None, 8_192);
    let m = b["messages"].as_array().unwrap();
    assert_eq!(m.len(), 4, "{m:?}");
    assert_eq!(m[1]["role"], "tool");
    assert!(m[1].get("images").is_none());
    assert_eq!(m[3]["role"], "user");
    assert_eq!(
        m[3]["content"],
        "[1 image returned by the read_file call c1]"
    );
    assert_eq!(m[3]["images"], json!(["QUJD"]));
}

#[test]
fn flattened_requests_keep_tool_images_on_the_user_turn() {
    let mut msgs = vec![Msg::User("look at a.png".into())];
    msgs.extend(image_round());
    let mut b = openai_body("m", &msgs, &[], None, None);
    flatten_openai_messages(&mut b, true);
    let last = b["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    let parts = last["content"].as_array().unwrap();
    assert_eq!(
        parts[0]["image_url"]["url"], "data:image/png;base64,QUJD",
        "{last}"
    );
    let mut b = ollama_body("m", &msgs, &[], None, None, 8_192);
    flatten_ollama_messages(&mut b);
    let last = b["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["images"], json!(["QUJD"]), "{last}");
}

#[test]
fn tool_images_survive_a_session_round_trip_and_old_files_still_load() {
    let r: ToolResult =
        serde_json::from_str(r#"{"id":"a","content":"x","is_error":false}"#).unwrap();
    assert!(r.images.is_empty());
    let Msg::Tool(rs) = &image_round()[1] else {
        unreachable!()
    };
    let text = serde_json::to_string(&rs[..]).unwrap();
    assert_eq!(text.matches("images").count(), 1, "{text}");
    let back: Vec<ToolResult> = serde_json::from_str(&text).unwrap();
    assert_eq!(back[0].images, rs[0].images);
}

#[test]
fn anthropic_body_maps_tool_schemas() {
    let tools = crate::tools::defs(false);
    let b = anthropic_body("m", &[Msg::User("hi".into())], &tools, None);
    assert!(b["tools"].as_array().unwrap().len() == tools.len());
    assert!(b["tools"][0].get("input_schema").is_some());
}

// ── openai_body ─────────────────────────────────────────────────────────
#[test]
fn openai_body_roles() {
    let msgs = vec![Msg::System("sys".into()), Msg::User("u".into())];
    let b = openai_body("m", &msgs, &[], None, None);
    assert_eq!(b["messages"][0]["role"], "system");
    assert_eq!(b["messages"][1]["role"], "user");
}

#[test]
fn openai_body_assistant_serializes_tool_calls_as_string_args() {
    let msgs = vec![Msg::Assistant {
        text: "x".into(),
        calls: vec![tc("c1", "run_command", json!({"cmd": "ls"}))],
    }];
    let b = openai_body("m", &msgs, &[], None, None);
    let call = &b["messages"][0]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "run_command");
    // arguments must be a JSON *string*, per the OpenAI shape.
    assert!(call["function"]["arguments"].is_string());
    assert_eq!(
        call["function"]["arguments"],
        json!({"cmd": "ls"}).to_string()
    );
}

#[test]
fn template_role_error_detected() {
    assert!(is_template_role_error(
            "HTTP 400: Unable to generate parser for this template. Conversation roles must alternate user/assistant"
        ));
    assert!(!is_template_role_error("HTTP 500: internal error"));
    assert!(!is_template_role_error("HTTP 429: rate limited"));
}

#[test]
fn flatten_messages_produces_alternating_user_assistant() {
    // system, user, assistant(+tool_call), tool-result → must collapse to
    // strictly alternating user/assistant with no system/tool roles.
    let mut body = json!({
        "model": "gemma",
        "tools": [{"type": "function"}],
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "build it"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "write_file", "arguments": "{\"path\":\"a\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "1", "content": "wrote a"}
        ]
    });
    flatten_openai_messages(&mut body, true);
    let m = body["messages"].as_array().unwrap();
    // The empty tool-call-only assistant turn is dropped (no call-shaped
    // text for a small model to imitate), so the remaining turns are all
    // user/assistant with none of the system/tool roles.
    let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
    assert!(roles.iter().all(|r| *r == "user" || *r == "assistant"));
    let joined: String = m
        .iter()
        .map(|x| x["content"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("You are helpful"));
    assert!(joined.contains("build it"));
    assert!(joined.contains("Tool result"));
    // A synthetic "[called …]" marker must never leak into the prompt.
    assert!(!joined.contains("[called"));
    // The native tools field is dropped for the restrictive template.
    assert!(body.get("tools").is_none());
}

#[test]
fn flatten_messages_prepends_user_when_starting_on_assistant() {
    let mut body = json!({
        "messages": [{"role": "assistant", "content": "hi"}]
    });
    flatten_openai_messages(&mut body, true);
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["user", "assistant"]);
}

#[test]
fn interruptible_reader_passes_a_stream_through_unchanged() {
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let mut r = InterruptibleReader::new(Box::new(Cursor::new(data.clone())));
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    assert_eq!(out, data);
    // A worker's result comes back through `interruptible` as-is.
    assert_eq!(interruptible(|| 7).unwrap(), 7);
}

#[test]
fn flatten_openai_keeps_images_and_describes_tools_in_text() {
    let mut body = json!({
        "tools": [{"type": "function", "function": {
            "name": "write_file",
            "description": "Write a file. Overwrites it.",
            "parameters": {"type": "object", "required": ["path"],
                "properties": {"path": {}, "content": {}}}
        }}],
        "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
                {"type": "text", "text": "read this"}
            ]}
        ]
    });
    flatten_openai_messages(&mut body, true);
    let m = body["messages"].as_array().unwrap();
    assert_eq!(m.len(), 1, "system folds into the first user turn");
    let parts = m[0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAA");
    let text = parts.last().unwrap()["text"].as_str().unwrap();
    assert!(
        text.contains("- write_file(path, content?): Write a file"),
        "{text}"
    );
    assert!(text.contains(r#"{"name": "<tool>""#), "{text}");
    assert!(text.contains("Be brief.") && text.contains("read this"));
    assert!(body.get("tools").is_none());

    // A server that rejected the image gets the same turns without it.
    let mut again = json!({"messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
        {"type": "text", "text": "read this"}
    ]}]});
    flatten_openai_messages(&mut again, false);
    assert_eq!(again["messages"][0]["content"], "read this");
}

#[test]
fn flatten_ollama_folds_tool_traffic_and_keeps_system_and_images() {
    let mut body = json!({
        "tools": [{"type": "function", "function": {
            "name": "read_file", "description": "Read a file.",
            "parameters": {"type": "object", "required": ["path"],
                "properties": {"path": {}}}
        }}],
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "look", "images": ["QUJD"]},
            {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": "read_file", "arguments": {"path": "a"}}}
            ]},
            {"role": "tool", "content": "file body", "tool_name": "read_file"}
        ]
    });
    flatten_ollama_messages(&mut body);
    let m = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    let sys = m[0]["content"].as_str().unwrap();
    assert!(sys.starts_with("sys") && sys.contains("- read_file(path): Read a file"));
    assert_eq!(m[1]["images"], json!(["QUJD"]));
    assert!(m[2]["content"]
        .as_str()
        .unwrap()
        .contains(r#"read_file({"path":"a"})"#));
    assert!(m[3]["content"]
        .as_str()
        .unwrap()
        .starts_with("Tool result:\nfile body"));
    assert!(body.get("tools").is_none());
}

#[test]
fn ollama_retries_without_tools_when_the_model_has_none_and_remembers() {
    let ok = r#"{"message":{"role":"assistant","content":"y"},"done":true,"done_reason":"stop"}"#;
    let reject = r#"{"error":"registry.ollama.ai/library/gemma3:4b does not support tools"}"#;
    let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
    let p = Provider {
        protocol: Protocol::OllamaNative,
        base_url: base,
        api_key: None,
        model: "gemma3:4b".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        // Pre-seeded so no /api/show probe consumes a scripted reply.
        ollama_ctx: std::sync::OnceLock::from(Some(8_192)),
    };
    let tools = vec![ToolDef {
        name: "read_file",
        description: "Read a file.",
        schema: json!({"type": "object", "properties": {"path": {}}}),
    }];
    let msgs = [
        Msg::System("be brief".into()),
        Msg::UserImages {
            text: "what is this".into(),
            images: vec![("image/png".into(), "QUJD".into())],
        },
    ];
    complete(&p, &msgs, &tools).unwrap();
    complete(&p, &msgs, &tools).unwrap();
    handle.join().unwrap();
    let sent: Vec<Value> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(sent.len(), 3, "400 → retry, then one remembered call");
    assert!(sent[0].get("tools").is_some());
    for retry in &sent[1..] {
        assert!(retry.get("tools").is_none());
        assert_eq!(retry["messages"][1]["images"], json!(["QUJD"]));
        assert!(retry["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("- read_file(path?)"));
    }
}

#[test]
fn ollama_retries_without_thinking_when_the_model_has_none_and_remembers() {
    let ok = r#"{"message":{"role":"assistant","content":"y"},"done":true,"done_reason":"stop"}"#;
    let reject =
        r#"{"error":"registry.ollama.ai/library/llama3.2:latest does not support thinking"}"#;
    let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
    let p = Provider {
        protocol: Protocol::OllamaNative,
        base_url: base,
        api_key: None,
        model: "llama3.2:latest".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::High,
        ollama_ctx: std::sync::OnceLock::from(Some(8_192)),
    };
    let msgs = [Msg::User("hi".into())];
    assert_eq!(complete(&p, &msgs, &[]).unwrap().text, "y");
    assert_eq!(complete(&p, &msgs, &[]).unwrap().text, "y");
    handle.join().unwrap();
    let sent: Vec<Value> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(sent.len(), 3, "400 → retry, then one remembered call");
    assert_eq!(sent[0]["think"], true);
    for retry in &sent[1..] {
        assert!(retry.get("think").is_none(), "{retry}");
    }
}

#[test]
fn openai_body_tool_results_each_become_a_message() {
    let msgs = vec![Msg::Tool(vec![
        ToolResult {
            id: "a".into(),
            content: "1".into(),
            is_error: false,
            images: Vec::new(),
        },
        ToolResult {
            id: "b".into(),
            content: "2".into(),
            is_error: true,
            images: Vec::new(),
        },
    ])];
    let b = openai_body("m", &msgs, &[], None, None);
    assert_eq!(b["messages"].as_array().unwrap().len(), 2);
    assert_eq!(b["messages"][0]["role"], "tool");
    assert_eq!(b["messages"][1]["tool_call_id"], "b");
}

#[test]
fn openai_body_sets_temperature_and_max_tokens_defaults() {
    let b = openai_body("m", &[Msg::User("hi".into())], &[], None, None);
    assert_eq!(b["temperature"], 0.2);
    assert_eq!(b["max_tokens"], 4096);
}

#[test]
fn openai_body_honors_temperature_and_max_tokens_overrides() {
    let b = openai_body("m", &[Msg::User("hi".into())], &[], Some(0.7), Some(512));
    assert_eq!(b["temperature"], 0.7);
    assert_eq!(b["max_tokens"], 512);
}

#[test]
fn openai_body_tool_schema_wraps_function() {
    let tools = crate::tools::defs(false);
    let b = openai_body("m", &[Msg::User("hi".into())], &tools, None, None);
    assert_eq!(b["tools"][0]["type"], "function");
    assert!(b["tools"][0]["function"].get("parameters").is_some());
}

// ── openai_stream ───────────────────────────────────────────────────────
fn drain_openai(sse: &str) -> Reply {
    let mut out = String::new();
    openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| out.push_str(t),
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn openai_stream_extracts_reasoning_and_think_tags() {
    let sse = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"reasoning...\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"<think>deep thought\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ing</think>Hello\"}}]}\n\
                   data: [DONE]\n";
    let mut text_out = String::new();
    let mut think_out = String::new();
    openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| text_out.push_str(t),
        &mut |t| think_out.push_str(t),
    )
    .unwrap();
    assert_eq!(text_out, "Hello");
    assert_eq!(think_out, "reasoning...deep thoughting");
}

#[test]
fn openai_stream_extracts_think_tag_when_single_chunk_contains_answer() {
    let sse =
        "data: {\"choices\":[{\"delta\":{\"content\":\"<think>inspect first</think>Done\"}}]}\n\
                   data: [DONE]\n";
    let mut text_out = String::new();
    let mut think_out = String::new();
    openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| text_out.push_str(t),
        &mut |t| think_out.push_str(t),
    )
    .unwrap();
    assert_eq!(text_out, "Done");
    assert_eq!(think_out, "inspect first");
}

#[test]
fn split_trailing_partial_holds_tag_prefix() {
    assert_eq!(
        split_trailing_partial("hello <thi", "<think>"),
        ("hello ", "<thi")
    );
    assert_eq!(
        split_trailing_partial("reason</thi", "</think>"),
        ("reason", "</thi")
    );
    // No trailing partial → nothing held.
    assert_eq!(
        split_trailing_partial("plain text", "<think>"),
        ("plain text", "")
    );
    // A complete tag ends with `>`, not a tag prefix, so nothing is held —
    // the caller's find() handles complete tags first.
    assert_eq!(
        split_trailing_partial("a<think>", "<think>"),
        ("a<think>", "")
    );
}

#[test]
fn openai_stream_handles_think_open_tag_split_across_chunks() {
    // "<think>" is split as "<thi" | "nk>".
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello <thi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"nk>reason</think>done\"}}]}\n\
                   data: [DONE]\n";
    let mut text_out = String::new();
    let mut think_out = String::new();
    openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| text_out.push_str(t),
        &mut |t| think_out.push_str(t),
    )
    .unwrap();
    assert_eq!(text_out, "hello done");
    assert_eq!(think_out, "reason");
}

#[test]
fn openai_stream_handles_think_close_tag_split_across_chunks() {
    // "</think>" is split as "</thi" | "nk>".
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>reason</thi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"nk>done\"}}]}\n\
                   data: [DONE]\n";
    let mut text_out = String::new();
    let mut think_out = String::new();
    openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| text_out.push_str(t),
        &mut |t| think_out.push_str(t),
    )
    .unwrap();
    assert_eq!(text_out, "done");
    assert_eq!(think_out, "reason");
}

#[test]
fn openai_stream_flushes_false_partial_at_stream_end() {
    // Content that merely ends with a tag-prefix and then stops must not be
    // swallowed — it wasn't a real tag.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"answer<thi\"}}]}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.text, "answer<thi");
}

#[test]
fn openai_stream_accumulates_text_and_stops_on_done() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\
                   data: [DONE]\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"AFTER\"}}]}\n";
    let r = drain_openai(sse);
    assert_eq!(r.text, "Hello");
}

#[test]
fn openai_stream_assembles_fragmented_tool_call() {
    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]}}]}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].id, "c1");
    assert_eq!(r.calls[0].name, "edit_file");
    assert_eq!(r.calls[0].input, json!({"path": "a"}));
}

#[test]
fn openai_stream_multiple_tool_calls_by_index() {
    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f0\",\"arguments\":\"{}\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"f1\",\"arguments\":\"{}\"}}]}}]}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.calls.len(), 2);
    assert_eq!(r.calls[0].name, "f0");
    assert_eq!(r.calls[1].name, "f1");
}

#[test]
fn openai_stream_ignores_blank_and_malformed_lines() {
    let sse = "\n\
                   data: not-json\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                   : comment\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.text, "ok");
}

#[test]
fn openai_stream_invalid_args_flagged() {
    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"{bad\"}}]}}]}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{bad"));
}

#[test]
fn openai_stream_done_only_is_empty_reply() {
    let r = drain_openai("data: [DONE]\n");
    assert!(r.text.is_empty());
    assert!(r.calls.is_empty());
}

#[test]
fn stream_eof_without_terminal_event_is_err() {
    let run_openai = |sse: &str| {
        openai_stream(
            Cursor::new(sse.as_bytes().to_vec()),
            &mut |_| {},
            &mut |_| {},
        )
    };
    assert_eq!(run_openai("").unwrap_err(), STREAM_TRUNCATED);
    let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"Hal\"}}]}\n";
    assert_eq!(run_openai(partial).unwrap_err(), STREAM_TRUNCATED);
    // A finish_reason is as good as [DONE]: the content is all there.
    let finished =
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n";
    assert_eq!(run_openai(finished).unwrap().text, "Hi");

    let a = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hal\"}}\n";
    let r = anthropic_stream(Cursor::new(a.as_bytes().to_vec()), &mut |_| {}, &mut |_| {});
    assert_eq!(r.unwrap_err(), STREAM_TRUNCATED);

    let o = "{\"message\":{\"content\":\"Hal\"},\"done\":false}\n";
    let r = ollama_stream(Cursor::new(o.as_bytes().to_vec()), &mut |_| {}, &mut |_| {});
    assert_eq!(r.unwrap_err(), STREAM_TRUNCATED);
}

#[test]
fn openai_error_chunk_and_error_body_surface_as_err() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\
                   data: {\"error\":{\"message\":\"upstream overloaded, key sk-abcdefghijklmnop\"}}\n\
                   data: [DONE]\n";
    let err = openai_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |_| {},
        &mut |_| {},
    )
    .unwrap_err();
    assert!(err.contains("upstream overloaded"), "got: {err}");
    assert!(
        !err.contains("sk-abcdefghijklmnop"),
        "must be redacted: {err}"
    );

    let err = openai_parse(json!({"error": {"message": "model not loaded"}})).unwrap_err();
    assert!(err.contains("model not loaded"), "got: {err}");
    let long = "x ".repeat(1000);
    let err = openai_parse(json!({ "error": long })).unwrap_err();
    assert!(err.len() < 500, "truncated: {}", err.len());
    // A null error field is not an error.
    assert!(openai_parse(json!({"error": null, "choices": []})).is_ok());
}

// ── transport: timeouts, retries, redirects ────────────────────────────
#[test]
fn shared_agent_has_no_overall_deadline_and_follows_no_redirects() {
    let dbg = format!("{:?}", agent().agent_for("https://api.example.com"));
    assert!(dbg.contains("timeout: None"), "{dbg}");
    assert!(dbg.contains("timeout_read: Some("), "{dbg}");
    assert!(dbg.contains("timeout_connect: Some(15s)"), "{dbg}");
    assert!(dbg.contains("redirects: 0"), "{dbg}");
}

// Accepts connections for `window`, reading each request but never
// answering. Returns how many connections arrived.
fn silent_server(window: std::time::Duration) -> (String, std::thread::JoinHandle<usize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let end = std::time::Instant::now() + window;
        let mut held = Vec::new();
        while std::time::Instant::now() < end {
            match listener.accept() {
                Ok((sock, _)) => held.push(sock),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        held.len()
    });
    (base, handle)
}

#[test]
fn read_timeout_after_post_is_not_retried() {
    let (base, handle) = silent_server(std::time::Duration::from_millis(2_000));
    let req = agent()
        .post(&format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_millis(300));
    let err = send_raw(req, json!({"model": "m"})).unwrap_err();
    assert!(err.contains("BWN_READ_TIMEOUT_SECS"), "got: {err}");
    assert_eq!(handle.join().unwrap(), 1, "the POST must be sent once");
}

#[test]
fn connect_failure_is_retried() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let e = agent().post(&url).send_json(json!({})).unwrap_err();
    assert!(transport_retryable(&e), "{e}");
    // The local-server hint is only given on the second refused attempt,
    // so seeing it proves send_raw retried.
    let err = send_raw(agent().post(&url), json!({})).unwrap_err();
    assert!(err.contains("nothing is answering"), "got: {err}");
}

#[test]
fn redirect_is_refused_and_key_never_reaches_the_other_host() {
    use std::io::{Read, Write};
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_addr = other.local_addr().unwrap();
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", first.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = first.accept().unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        let mut buf = [0u8; 16384];
        let _ = sock.read(&mut buf);
        let resp = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{other_addr}/v1/messages\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
        let _ = sock.write_all(resp.as_bytes());
    });
    let p = Provider {
        protocol: Protocol::Anthropic,
        base_url: base,
        api_key: Some("sk-ant-test-key".into()),
        model: "claude-test".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    let err = complete(&p, &[Msg::User("hi".into())], &[]).unwrap_err();
    handle.join().unwrap();
    assert!(err.contains("redirected"), "got: {err}");
    other.set_nonblocking(true).unwrap();
    assert!(
        other.accept().is_err(),
        "redirect target must not be contacted"
    );
}

#[test]
fn jitter_stays_within_twenty_percent() {
    for _ in 0..50 {
        let j = jitter(1_000);
        assert!((800..=1_200).contains(&j), "{j}");
    }
}

// ── anthropic_stream ────────────────────────────────────────────────────
fn drain_anthropic(sse: &str) -> Reply {
    let mut out = String::new();
    let mut _thinking = String::new();
    anthropic_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |t| out.push_str(t),
        &mut |t| _thinking.push_str(t),
    )
    .unwrap()
}

#[test]
fn anthropic_stream_text_deltas() {
    let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(r.text, "Hi there");
}

#[test]
fn anthropic_stream_assembles_input_json_delta() {
    let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read_file\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"x\\\"}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].id, "t1");
    assert_eq!(r.calls[0].name, "read_file");
    assert_eq!(r.calls[0].input, json!({"path": "x"}));
}

#[test]
fn anthropic_stream_bad_input_json_is_flagged_invalid() {
    // A truncated/malformed streamed tool call must surface as INVALID_ARGS
    // so the agent loop reports it to the model instead of executing the
    // tool with a silently-empty input.
    let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{not"));
}

#[test]
fn anthropic_stream_captures_stop_reason_from_message_delta() {
    let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn anthropic_stream_error_event_is_err() {
    let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\
                   data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
    let r = anthropic_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |_| {},
        &mut |_| {},
    );
    let err = r.unwrap_err();
    assert!(err.contains("stream error"), "got: {err}");
    assert!(err.contains("Overloaded"), "got: {err}");
}

// Yields its buffered bytes, then fails instead of reporting EOF —
// simulates a connection dropped mid-stream.
struct BrokenPipe(Cursor<Vec<u8>>);
impl Read for BrokenPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.read(buf)? {
            0 => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "connection reset",
            )),
            n => Ok(n),
        }
    }
}

#[test]
fn stream_read_error_before_terminal_event_is_err() {
    // No message_stop / [DONE] — the transport dies first.
    let a = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n";
    let r = anthropic_stream(
        BrokenPipe(Cursor::new(a.as_bytes().to_vec())),
        &mut |_| {},
        &mut |_| {},
    );
    assert!(r.unwrap_err().contains("stream read failed"));

    let o = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n";
    let r = openai_stream(
        BrokenPipe(Cursor::new(o.as_bytes().to_vec())),
        &mut |_| {},
        &mut |_| {},
    );
    assert!(r.unwrap_err().contains("stream read failed"));
}

#[test]
fn stream_read_error_after_terminal_event_is_ok() {
    // The terminal event stops reading before the transport failure is seen.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\
                   data: [DONE]\n";
    let r = openai_stream(
        BrokenPipe(Cursor::new(sse.as_bytes().to_vec())),
        &mut |_| {},
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(r.text, "Hi");
}

#[test]
fn anthropic_stream_interleaved_text_and_tool() {
    let sse = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"go\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(r.text, "go");
    assert_eq!(r.calls.len(), 1);
}

#[test]
fn anthropic_stream_ignores_unknown_events() {
    let sse = "data: {\"type\":\"ping\"}\n\
                   data: {\"type\":\"message_start\",\"message\":{}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert!(r.text.is_empty());
    assert!(r.calls.is_empty());
}

#[test]
fn anthropic_parse_extracts_text_and_calls() {
    let v = json!({"content": [
        {"type": "text", "text": "hello"},
        {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "a"}}
    ]});
    let r = anthropic_parse(v).unwrap();
    assert_eq!(r.text, "hello");
    assert_eq!(r.calls[0].name, "read_file");
}

#[test]
fn openai_parse_handles_missing_tool_calls() {
    let v = json!({"choices": [{"message": {"content": "hi"}}]});
    let r = openai_parse(v).unwrap();
    assert_eq!(r.text, "hi");
    assert!(r.calls.is_empty());
}

#[test]
fn openai_parse_defaults_missing_args_to_empty_object() {
    let v = json!({"choices": [{"message": {"content": "", "tool_calls": [
        {"id": "c1", "function": {"name": "f"}}
    ]}}]});
    let r = openai_parse(v).unwrap();
    assert_eq!(r.calls[0].input, json!({}));
}

// ── think-tag stripping ─────────────────────────────────────────────────
#[test]
fn strip_think_removes_blocks_and_keeps_answer() {
    assert_eq!(
        strip_think("<think>let me reason</think>The answer is 42."),
        "The answer is 42."
    );
    // Multiple blocks, with real content between them.
    assert_eq!(strip_think("<think>a</think>X<think>b</think>Y"), "XY");
    // No tags → untouched.
    assert_eq!(strip_think("plain answer"), "plain answer");
}

#[test]
fn strip_think_drops_unclosed_reasoning() {
    // An unterminated <think> means the model never emitted a final answer;
    // the leaked reasoning must not become the answer.
    assert_eq!(strip_think("intro <think>reasoning with no close"), "intro");
}

#[test]
fn openai_parse_strips_think_from_content() {
    let v = json!({"choices": [{"message": {"content": "<think>plan</think>done"}}]});
    let r = openai_parse(v).unwrap();
    assert_eq!(r.text, "done");
}

#[test]
fn ollama_parse_strips_think_from_content() {
    let v = json!({"message": {"content": "<think>hmm</think>result"}, "done_reason": "stop"});
    let r = ollama_parse(v).unwrap();
    assert_eq!(r.text, "result");
}

// ── stop_reason ─────────────────────────────────────────────────────────
#[test]
fn anthropic_parse_captures_stop_reason() {
    let v = json!({"content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn"});
    let r = anthropic_parse(v).unwrap();
    assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
    // Absent → None, never fabricated.
    let r = anthropic_parse(json!({"content": []})).unwrap();
    assert_eq!(r.stop_reason, None);
}

#[test]
fn openai_parse_normalizes_finish_reason() {
    let with =
        |reason: &str| json!({"choices": [{"message": {"content": "x"}, "finish_reason": reason}]});
    assert_eq!(
        openai_parse(with("length")).unwrap().stop_reason.as_deref(),
        Some("max_tokens")
    );
    assert_eq!(
        openai_parse(with("tool_calls"))
            .unwrap()
            .stop_reason
            .as_deref(),
        Some("tool_use")
    );
    assert_eq!(
        openai_parse(with("stop")).unwrap().stop_reason.as_deref(),
        Some("stop")
    );
}

#[test]
fn openai_stream_captures_finish_reason() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.text, "Hi");
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
}

// ── ollama_root ─────────────────────────────────────────────────────────
#[test]
fn ollama_root_trims_v1_and_slashes() {
    assert_eq!(
        ollama_root("http://localhost:11434"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_root("http://localhost:11434/"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_root("http://localhost:11434/v1"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_root("http://localhost:11434/v1/"),
        "http://localhost:11434"
    );
}

// ── ollama_body ─────────────────────────────────────────────────────────
#[test]
fn ollama_body_sets_options_and_stream_off() {
    let b = ollama_body("m", &[Msg::User("hi".into())], &[], None, None, 16_384);
    assert_eq!(b["stream"], false);
    assert_eq!(b["options"]["num_ctx"], 16_384);
    assert_eq!(b["options"]["repeat_penalty"], 1.0);
    assert_eq!(b["options"]["temperature"], 0.2);
    assert_eq!(b["options"]["num_predict"], 4096);
    // Sampling knobs live under "options", never at the top level.
    assert!(b.get("temperature").is_none());
    assert!(b.get("max_tokens").is_none());
}

#[test]
fn ollama_body_honors_temperature_and_max_tokens_overrides() {
    let b = ollama_body(
        "m",
        &[Msg::User("hi".into())],
        &[],
        Some(0.7),
        Some(512),
        8_192,
    );
    assert_eq!(b["options"]["temperature"], 0.7);
    assert_eq!(b["options"]["num_predict"], 512);
}

#[test]
fn ollama_body_merges_system_and_maps_roles() {
    let msgs = vec![
        Msg::System("one".into()),
        Msg::System("two".into()),
        Msg::User("u".into()),
    ];
    let b = ollama_body("m", &msgs, &[], None, None, 8_192);
    assert_eq!(b["messages"][0]["role"], "system");
    assert_eq!(b["messages"][0]["content"], "one\n\ntwo");
    assert_eq!(b["messages"][1]["role"], "user");
}

#[test]
fn ollama_body_assistant_tool_call_args_are_objects() {
    let msgs = vec![Msg::Assistant {
        text: "x".into(),
        calls: vec![tc("c1", "run_command", json!({"cmd": "ls"}))],
    }];
    let b = ollama_body("m", &msgs, &[], None, None, 8_192);
    let call = &b["messages"][0]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "run_command");
    // arguments must be a JSON *object*, unlike the OpenAI string form.
    assert_eq!(call["function"]["arguments"], json!({"cmd": "ls"}));
}

#[test]
fn ollama_body_tool_results_carry_tool_name_when_resolvable() {
    let msgs = vec![
        Msg::Assistant {
            text: String::new(),
            calls: vec![tc("c1", "read_file", json!({"path": "a"}))],
        },
        Msg::Tool(vec![
            ToolResult {
                id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                images: Vec::new(),
            },
            ToolResult {
                id: "unknown".into(),
                content: "?".into(),
                is_error: true,
                images: Vec::new(),
            },
        ]),
    ];
    let b = ollama_body("m", &msgs, &[], None, None, 8_192);
    let first = &b["messages"][1];
    assert_eq!(first["role"], "tool");
    assert_eq!(first["content"], "ok");
    assert_eq!(first["tool_name"], "read_file");
    // Unresolvable id → no fabricated tool_name.
    assert!(b["messages"][2].get("tool_name").is_none());
}

#[test]
fn ollama_body_images_are_bare_base64_array() {
    let msgs = vec![Msg::UserImages {
        text: "what is this".into(),
        images: vec![("image/png".into(), "aGk=".into())],
    }];
    let b = ollama_body("m", &msgs, &[], None, None, 8_192);
    let msg = &b["messages"][0];
    assert_eq!(msg["content"], "what is this");
    assert_eq!(msg["images"], json!(["aGk="]));
}

#[test]
fn ollama_body_tool_schema_wraps_function() {
    let tools = crate::tools::defs(false);
    let b = ollama_body("m", &[Msg::User("hi".into())], &tools, None, None, 8_192);
    assert_eq!(b["tools"][0]["type"], "function");
    assert!(b["tools"][0]["function"].get("parameters").is_some());
    // No tools → no key.
    let b = ollama_body("m", &[Msg::User("hi".into())], &[], None, None, 8_192);
    assert!(b.get("tools").is_none());
}

// ── ollama_stream ───────────────────────────────────────────────────────
fn drain_ollama(ndjson: &str) -> Reply {
    let mut out = String::new();
    ollama_stream(
        Cursor::new(ndjson.as_bytes().to_vec()),
        &mut |t| out.push_str(t),
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn ollama_stream_accumulates_content_and_stops_on_done() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"AFTER\"},\"done\":false}\n";
    let r = drain_ollama(ndjson);
    assert_eq!(r.text, "Hello");
    assert_eq!(r.stop_reason.as_deref(), Some("stop"));
}

#[test]
fn ollama_stream_maps_length_done_reason_to_max_tokens() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"length\"}\n";
    let r = drain_ollama(ndjson);
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn ollama_stream_collects_whole_tool_calls_with_object_args() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}}]},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"list_dir\",\"arguments\":{}}}]},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n";
    let r = drain_ollama(ndjson);
    assert_eq!(r.calls.len(), 2);
    assert_eq!(r.calls[0].name, "read_file");
    assert_eq!(r.calls[0].input, json!({"path": "a"}));
    // Synthesized, position-stable ids (Ollama sends none).
    assert_eq!(r.calls[0].id, "ollama-call-0");
    assert_eq!(r.calls[1].id, "ollama-call-1");
}

#[test]
fn ollama_stream_string_args_go_through_invalid_args_guard() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"f\",\"arguments\":\"{bad\"}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
    let r = drain_ollama(ndjson);
    assert_eq!(r.calls[0].input[crate::tools::INVALID_ARGS], json!("{bad"));
}

#[test]
fn ollama_stream_routes_thinking_field_and_think_tags() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"thinking\":\"hmm \"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"<think>deep</think>Hi\"},\"done\":true,\"done_reason\":\"stop\"}\n";
    let mut text_out = String::new();
    let mut think_out = String::new();
    ollama_stream(
        Cursor::new(ndjson.as_bytes().to_vec()),
        &mut |t| text_out.push_str(t),
        &mut |t| think_out.push_str(t),
    )
    .unwrap();
    assert_eq!(text_out, "Hi");
    assert_eq!(think_out, "hmm deep");
}

#[test]
fn ollama_stream_error_line_is_err() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"par\"},\"done\":false}\n\
                      {\"error\":\"model runner has unexpectedly stopped\"}\n";
    let r = ollama_stream(
        Cursor::new(ndjson.as_bytes().to_vec()),
        &mut |_| {},
        &mut |_| {},
    );
    let err = r.unwrap_err();
    assert!(err.contains("stream error"), "got: {err}");
    assert!(err.contains("unexpectedly stopped"), "got: {err}");
}

#[test]
fn ollama_stream_read_error_before_done_is_err() {
    let ndjson = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n";
    let r = ollama_stream(
        BrokenPipe(Cursor::new(ndjson.as_bytes().to_vec())),
        &mut |_| {},
        &mut |_| {},
    );
    assert!(r.unwrap_err().contains("stream read failed"));
}

#[test]
fn ollama_stream_ignores_blank_and_malformed_lines() {
    let ndjson = "\n\
                      not-json\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":true,\"done_reason\":\"stop\"}\n";
    let r = drain_ollama(ndjson);
    assert_eq!(r.text, "ok");
}

// ── ollama_parse (non-stream) ───────────────────────────────────────────
#[test]
fn ollama_parse_extracts_text_calls_and_done_reason() {
    let v = json!({
        "message": {
            "role": "assistant",
            "content": "hello",
            "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a"}}}]
        },
        "done": true,
        "done_reason": "length"
    });
    let r = ollama_parse(v).unwrap();
    assert_eq!(r.text, "hello");
    assert_eq!(r.calls[0].name, "read_file");
    assert_eq!(r.calls[0].input, json!({"path": "a"}));
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
    // Absent done_reason → None, never fabricated.
    let r = ollama_parse(json!({"message": {"content": "x"}})).unwrap();
    assert_eq!(r.stop_reason, None);
}

// ── /api/show parsing + num_ctx sizing ──────────────────────────────────
#[test]
fn show_context_length_finds_arch_prefixed_key() {
    let v = json!({"model_info": {
        "general.architecture": "qwen2",
        "qwen2.embedding_length": 5120,
        "qwen2.context_length": 131_072
    }});
    assert_eq!(show_context_length(&v), Some(131_072));
    // No matching key or no model_info → None.
    assert_eq!(show_context_length(&json!({"model_info": {"x": 1}})), None);
    assert_eq!(show_context_length(&json!({})), None);
}

#[test]
fn show_num_ctx_parses_parameters_blob() {
    let v = json!({"parameters": "stop    \"<|im_end|>\"\nnum_ctx    16384\ntemperature 0.7"});
    assert_eq!(show_num_ctx(&v), Some(16_384));
    assert_eq!(show_num_ctx(&json!({"parameters": "stop \"x\""})), None);
    assert_eq!(show_num_ctx(&json!({})), None);
}

#[test]
fn ollama_pick_ctx_caps_floors_and_honors_modelfile() {
    // Known model max: capped at 32k.
    assert_eq!(ollama_pick_ctx(Some(131_072), None), 32_768);
    // Small model max wins over the cap.
    assert_eq!(ollama_pick_ctx(Some(16_384), None), 16_384);
    // Unknown max → 8k floor.
    assert_eq!(ollama_pick_ctx(None, None), 8_192);
    // Modelfile num_ctx wins (the user tuned it), even past the cap…
    assert_eq!(ollama_pick_ctx(Some(131_072), Some(65_536)), 65_536);
    assert_eq!(ollama_pick_ctx(None, Some(4_096)), 4_096);
    // …but never exceeds a known model max.
    assert_eq!(ollama_pick_ctx(Some(8_192), Some(65_536)), 8_192);
}

fn ollama_at(base: String, model: &str) -> Provider {
    Provider {
        protocol: Protocol::OllamaNative,
        base_url: base,
        api_key: None,
        model: model.into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    }
}

#[test]
fn served_windows_come_from_what_each_server_reports() {
    let served = |base: &str, model: &str| served_model(&format!("{base}/v1"), model);
    // llama.cpp: /props, per-slot n_ctx and its modalities.
    // total_slots: how many requests it answers at once.
    let props = r#"{"default_generation_settings":{"n_ctx":2048},"modalities":{"vision":false},"total_slots":2}"#;
    let (base, handle) = mock_server(vec![(200, props)]);
    assert_eq!(
        served(&base, "tiny.gguf"),
        Served {
            window: Some(2_048),
            vision: Some(false),
            slots: Some(2),
        }
    );
    handle.join().unwrap();
    // LM Studio: no /props; this model's loaded length and type.
    let lms = r#"{"data":[{"id":"other","loaded_context_length":32768},{"id":"tinycoder-7b-instruct","type":"vlm","state":"loaded","max_context_length":32768,"loaded_context_length":4096}]}"#;
    let (base, handle) = mock_server(vec![(404, "{}"), (200, lms)]);
    assert_eq!(
        served(&base, "tinycoder-7b-instruct"),
        Served {
            window: Some(4_096),
            vision: Some(true),
            slots: None,
        }
    );
    handle.join().unwrap();
    // vLLM: max_model_len on /v1/models.
    let vllm = r#"{"data":[{"id":"qwen","max_model_len":16384}]}"#;
    let (base, handle) = mock_server(vec![(404, "{}"), (404, "{}"), (200, vllm)]);
    assert_eq!(served(&base, "qwen").window, Some(16_384));
    handle.join().unwrap();
    // Nothing said: the caller keeps its guess.
    let (base, handle) = mock_server(vec![(404, "{}"), (404, "{}"), (200, r#"{"data":[]}"#)]);
    assert_eq!(served(&base, "qwen"), Served::default());
    handle.join().unwrap();
}

#[test]
fn a_message_bigger_than_a_known_window_is_refused_before_sending() {
    let mut p = ollama_at("http://127.0.0.1:1".into(), "tiny");
    p.protocol = Protocol::OpenAi;
    p.context_tokens = 4_096;
    let cwd = std::env::current_dir().unwrap();
    let text = format!(
        "summarize it\n\n[attached files]\n[file: {}]\n{}",
        cwd.join("big.txt").display(),
        "x".repeat(36_000)
    );
    let msgs = [Msg::System("s".into()), Msg::User(text)];
    // A guessed window never refuses anything.
    assert!(oversized_message(&p, &msgs).is_none());
    remember_window(&p);
    let why = oversized_message(&p, &msgs).unwrap();
    assert_eq!(
            why,
            "big.txt is about 9.0k tokens and the server holds 4.1k — attach a range such as @big.txt:1-200"
        );
    // Without an attachment, the message itself is named.
    let msgs = [Msg::User("y".repeat(20_000))];
    assert!(oversized_message(&p, &msgs)
        .unwrap()
        .starts_with("this message is about 5.0k tokens and the server holds 4.1k"));
    // Small messages, and tool results, pass.
    assert!(oversized_message(&p, &[Msg::User("hi".into())]).is_none());
    assert!(oversized_message(&p, &[Msg::Tool(vec![])]).is_none());
    // complete() refuses it without a connection (port 1 would fail).
    let e = complete(&p, &[Msg::User("z".repeat(20_000))], &[]).unwrap_err();
    assert!(e.starts_with("this message is about"), "{e}");
}

#[test]
fn a_failed_show_probe_is_tried_again_on_the_next_request() {
    let show =
        r#"{"model_info":{"llama.context_length":16384},"capabilities":["completion","tools"]}"#;
    let (base, handle) = mock_server(vec![(500, r#"{"error":"busy"}"#), (200, show)]);
    let p = ollama_at(base, "tinycoder:3b");
    // Not answered yet: the native path with the floor, nothing cached.
    assert_eq!(ollama_window(&p), Ok(Some(OLLAMA_CTX_FLOOR)));
    assert!(p.ollama_ctx.get().is_none(), "a failure is not remembered");
    // Answered now: the model's own window, remembered.
    assert_eq!(ollama_window(&p), Ok(Some(16_384)));
    assert_eq!(p.ollama_ctx.get(), Some(&Some(16_384)));
    assert_eq!(ollama_ctx(&p), Some(16_384));
    handle.join().unwrap();
}

#[test]
fn a_missing_ollama_model_is_named_with_what_is_installed() {
    let missing = r#"{"error":"model 'tinycoder:7b' not found"}"#;
    let tags = r#"{"models":[{"name":"tinycoder:3b"},{"name":"plainchat:2b"}]}"#;
    let (base, handle) = mock_server(vec![(404, missing), (200, tags)]);
    let p = ollama_at(base.clone(), "tinycoder:7b");
    let e = ollama_window(&p).unwrap_err();
    handle.join().unwrap();
    assert!(
        e.starts_with(&format!("model tinycoder:7b is not installed on {base}")),
        "{e}"
    );
    assert!(e.contains("installed: tinycoder:3b, plainchat:2b"), "{e}");
    assert!(
        e.contains("ollama pull tinycoder:7b, or --model tinycoder:3b"),
        "{e}"
    );
    assert!(!e.contains("/api/show"), "{e}");
    assert!(p.ollama_ctx.get().is_none(), "asked again after a pull");

    // A server with no /api/show at all is an old Ollama: /v1, remembered.
    let (base, handle) = mock_server(vec![(404, "404 page not found")]);
    let p = ollama_at(base, "tinycoder:3b");
    assert_eq!(ollama_window(&p), Ok(None));
    assert_eq!(p.ollama_ctx.get(), Some(&None));
    handle.join().unwrap();
}

#[test]
fn nobody_listening_is_said_once_without_falling_back() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let p = ollama_at(format!("http://127.0.0.1:{port}"), "tinycoder:3b");
    let e = ollama_window(&p).unwrap_err();
    assert!(
        e.starts_with("nothing is answering at http://127.0.0.1:"),
        "{e}"
    );
    assert!(p.ollama_ctx.get().is_none());
}

// ── usage parsing ────────────────────────────────────────────────────────
#[test]
fn anthropic_parse_reads_usage_and_keeps_thinking_blocks() {
    let r = anthropic_parse(json!({
        "content": [
            {"type": "thinking", "thinking": "hmm", "signature": "sig1"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "output_tokens": 7,
                  "cache_creation_input_tokens": 20, "cache_read_input_tokens": 300}
    }))
    .unwrap();
    assert_eq!(
        r.usage,
        Usage {
            input: 100,
            output: 7,
            cache_read: 300,
            cache_write: 20
        }
    );
    assert_eq!(r.thinking_blocks.len(), 2);
    assert_eq!(r.thinking_blocks[0]["signature"], "sig1");
    assert_eq!(r.thinking_blocks[1]["type"], "redacted_thinking");
    assert_eq!(r.text, "hi");
    assert_eq!(r.calls.len(), 1);
}

#[test]
fn anthropic_stream_reads_usage_from_message_start_and_delta() {
    let sse = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5,\"output_tokens\":1}}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let r = drain_anthropic(sse);
    assert_eq!(
        r.usage,
        Usage {
            input: 50,
            output: 42,
            cache_read: 10,
            cache_write: 5
        }
    );
    assert_eq!(r.text, "ok");
}

#[test]
fn anthropic_stream_assembles_thinking_blocks_with_signature() {
    let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"see\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"XYZ\"}}\n\
                   data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"read_file\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\
                   data: {\"type\":\"message_stop\"}\n";
    let mut thinking = String::new();
    let r = anthropic_stream(
        Cursor::new(sse.as_bytes().to_vec()),
        &mut |_| {},
        &mut |t| thinking.push_str(t),
    )
    .unwrap();
    assert_eq!(thinking, "let me see");
    assert_eq!(r.thinking_blocks.len(), 2);
    assert_eq!(r.thinking_blocks[0]["thinking"], "let me see");
    assert_eq!(r.thinking_blocks[0]["signature"], "SIG");
    assert_eq!(r.thinking_blocks[1]["data"], "XYZ");
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].id, "c1");
}

#[test]
fn usage_missing_fields_are_zero_never_error() {
    assert!(anthropic_parse(json!({"content": []}))
        .unwrap()
        .usage
        .is_empty());
    assert!(openai_parse(json!({"choices": []}))
        .unwrap()
        .usage
        .is_empty());
    assert!(openai_parse(json!({"choices": [], "usage": null}))
        .unwrap()
        .usage
        .is_empty());
    assert!(ollama_parse(json!({"message": {}}))
        .unwrap()
        .usage
        .is_empty());
    assert!(drain_openai("data: [DONE]\n").usage.is_empty());
    assert!(drain_ollama("{\"done\":true}\n").usage.is_empty());
    // Wrong types are ignored, not fatal.
    let r = openai_parse(json!({"usage": {"prompt_tokens": "lots"}})).unwrap();
    assert!(r.usage.is_empty());
}

#[test]
fn openai_parse_splits_cached_tokens_out_of_prompt_tokens() {
    let r = openai_parse(json!({
        "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 120, "completion_tokens": 9,
                  "prompt_tokens_details": {"cached_tokens": 100}}
    }))
    .unwrap();
    assert_eq!(
        r.usage,
        Usage {
            input: 20,
            output: 9,
            cache_read: 100,
            cache_write: 0
        }
    );
    assert_eq!(r.usage.prompt_tokens(), 120);
}

#[test]
fn openai_stream_reads_usage_from_final_chunk() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":2}}\n\
                   data: [DONE]\n";
    let r = drain_openai(sse);
    assert_eq!(r.text, "hi");
    assert_eq!(r.stop_reason.as_deref(), Some("stop"));
    assert_eq!(
        r.usage,
        Usage {
            input: 30,
            output: 2,
            cache_read: 0,
            cache_write: 0
        }
    );
}

#[test]
fn mark_streaming_requests_usage_in_the_final_chunk() {
    let mut body = json!({"model": "m"});
    mark_streaming(&mut body);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn ollama_parse_and_stream_read_eval_counts() {
    let r = ollama_parse(json!({
        "message": {"content": "x"}, "done": true, "done_reason": "stop",
        "prompt_eval_count": 77, "eval_count": 11
    }))
    .unwrap();
    assert_eq!(r.usage.input, 77);
    assert_eq!(r.usage.output, 11);
    let r = drain_ollama(
            "{\"message\":{\"content\":\"a\"},\"done\":false}\n\
             {\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":5,\"eval_count\":3}\n",
        );
    assert_eq!(r.usage.input, 5);
    assert_eq!(r.usage.output, 3);
    assert_eq!(r.usage.cache_read, 0);
}

#[test]
fn request_records_every_completed_reply_into_the_ledger() {
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1234,"completion_tokens":56}}"#;
    let (base, handle) = mock_server(vec![(200, body)]);
    let p = Provider {
        protocol: Protocol::OpenAi,
        base_url: base,
        api_key: None,
        model: "ledger-test-model".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    let before = crate::usage::snapshot();
    complete(&p, &[Msg::User("ping".into())], &[]).unwrap();
    handle.join().unwrap();
    let after = crate::usage::snapshot();
    // Other tests may record concurrently, so assert on lower bounds.
    assert!(after.requests > before.requests);
    assert!(after.totals.input >= before.totals.input + 1234);
    assert!(after.totals.output >= before.totals.output + 56);
    // A loopback base URL is a local provider: booked at $0, never unpriced.
    assert!(after.local_requests > before.local_requests);
}

// ── reasoning controls ───────────────────────────────────────────────────
#[test]
fn claude_version_parses_both_id_layouts() {
    assert_eq!(claude_version("claude-opus-4-6"), Some((4, 6)));
    assert_eq!(claude_version("claude-sonnet-4-6-20260101"), Some((4, 6)));
    assert_eq!(claude_version("claude-opus-5"), Some((5, 0)));
    assert_eq!(claude_version("claude-fable-5-1"), Some((5, 1)));
    assert_eq!(claude_version("claude-haiku-4-5"), Some((4, 5)));
    assert_eq!(claude_version("claude-sonnet-4-20250514"), Some((4, 0)));
    assert_eq!(claude_version("claude-3-7-sonnet-20250219"), Some((3, 7)));
    assert_eq!(claude_version("gpt-4o"), None);
    assert_eq!(claude_version("claude-proxy"), None);
}

#[test]
fn anthropic_effort_off_leaves_body_untouched() {
    let mut body = anthropic_body("claude-opus-4-6", &[Msg::User("hi".into())], &[], None);
    let before = body.clone();
    apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Off);
    assert_eq!(body, before);
}

#[test]
fn anthropic_effort_uses_adaptive_thinking_on_claude_4_6_and_later() {
    for (model, wants_display) in [
        ("claude-sonnet-4-6", false),
        ("claude-opus-4-8", true),
        ("claude-opus-5", true),
    ] {
        let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], None);
        apply_anthropic_effort(&mut body, model, Effort::Medium);
        assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
        assert!(body["thinking"].get("budget_tokens").is_none(), "{model}");
        assert_eq!(body["output_config"]["effort"], "medium", "{model}");
        assert_eq!(body["max_tokens"], 8192, "{model}: max_tokens untouched");
        assert_eq!(
            body["thinking"].get("display").is_some(),
            wants_display,
            "{model}"
        );
    }
}

#[test]
fn anthropic_effort_uses_budget_tokens_before_4_6_and_raises_max_tokens() {
    let model = "claude-haiku-4-5";
    for (effort, budget) in [
        (Effort::Low, 2048),
        (Effort::Medium, 8192),
        (Effort::High, 16384),
    ] {
        let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], None);
        apply_anthropic_effort(&mut body, model, effort);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], budget);
        assert!(body.get("output_config").is_none());
        let max_tokens = body["max_tokens"].as_u64().unwrap();
        assert!(max_tokens > budget, "{effort}: {max_tokens} <= {budget}");
    }
    // An explicit max_tokens already above the budget is kept.
    let mut body = anthropic_body(model, &[Msg::User("hi".into())], &[], Some(30_000));
    apply_anthropic_effort(&mut body, model, Effort::High);
    assert_eq!(body["max_tokens"], 30_000);
    // Unversioned ids (a proxy alias) take the classic form too.
    let mut body = anthropic_body("my-alias", &[Msg::User("hi".into())], &[], None);
    apply_anthropic_effort(&mut body, "my-alias", Effort::Low);
    assert_eq!(body["thinking"]["budget_tokens"], 2048);
}

#[test]
fn anthropic_effort_replays_recorded_thinking_before_tool_use() {
    // Memory is process-wide; a unique call id keeps this test isolated.
    let call_id = format!("replay-{}", std::process::id());
    remember_thinking(&Reply {
        calls: vec![tc(&call_id, "read_file", json!({}))],
        thinking_blocks: vec![json!({"type": "thinking", "thinking": "t", "signature": "s"})],
        ..Default::default()
    });
    let msgs = vec![
        Msg::User("go".into()),
        Msg::Assistant {
            text: "on it".into(),
            calls: vec![tc(&call_id, "read_file", json!({}))],
        },
        Msg::Tool(vec![ToolResult {
            id: call_id.clone(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        }]),
    ];
    let mut body = anthropic_body("claude-opus-4-6", &msgs, &[], None);
    apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Low);
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "s");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[2]["type"], "tool_use");
    // Idempotent: a second pass doesn't double the block.
    replay_thinking_blocks(&mut body);
    assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 3);
    // Off never touches the transcript, even with blocks on record.
    let mut body = anthropic_body("claude-opus-4-6", &msgs, &[], None);
    apply_anthropic_effort(&mut body, "claude-opus-4-6", Effort::Off);
    assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn remember_thinking_ignores_replies_without_calls_or_blocks() {
    let id = format!("noreplay-{}", std::process::id());
    remember_thinking(&Reply {
        calls: vec![tc(&id, "x", json!({}))],
        ..Default::default()
    });
    assert!(thinking_for(&id).is_none());
    remember_thinking(&Reply {
        thinking_blocks: vec![json!({"type": "thinking"})],
        ..Default::default()
    });
    assert!(thinking_for(&id).is_none());
}

#[test]
fn openai_effort_only_for_reasoning_models() {
    assert!(is_openai_reasoning_model("o3"));
    assert!(is_openai_reasoning_model("o4-mini"));
    assert!(is_openai_reasoning_model("O1-preview"));
    assert!(is_openai_reasoning_model("gpt-5-mini"));
    assert!(is_openai_reasoning_model("gpt-5.1"));
    assert!(!is_openai_reasoning_model("gpt-4o"));
    assert!(!is_openai_reasoning_model("gpt-4.1"));
    assert!(!is_openai_reasoning_model("gpt-oss-20b"));
    assert!(!is_openai_reasoning_model("qwen3:8b"));
    assert!(!is_openai_reasoning_model("local-model"));
    assert!(!is_openai_reasoning_model("o10-something"));

    let mut body = json!({"model": "o3"});
    apply_openai_effort(&mut body, "o3", Effort::High);
    assert_eq!(body["reasoning_effort"], "high");
    let mut body = json!({"model": "o3"});
    apply_openai_effort(&mut body, "o3", Effort::Off);
    assert!(body.get("reasoning_effort").is_none());
    // Local OpenAI-compatible servers never see it unless the model name matches.
    let mut body = json!({"model": "qwen3:8b"});
    apply_openai_effort(&mut body, "qwen3:8b", Effort::High);
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn ollama_effort_sets_think_for_any_level_but_off() {
    let mut body = json!({"model": "m"});
    apply_ollama_effort(&mut body, Effort::Off);
    assert!(body.get("think").is_none());
    for e in [Effort::Low, Effort::Medium, Effort::High] {
        let mut body = json!({"model": "m"});
        apply_ollama_effort(&mut body, e);
        assert_eq!(body["think"], true);
    }
}

// ── native tools on local OpenAI-compatible servers ──────────────────────
#[test]
fn needs_flatten_only_for_400s_naming_tools_roles_or_template() {
    assert!(needs_flatten("HTTP 400: tools are not supported"));
    assert!(needs_flatten("HTTP 400: unknown field tool_choice"));
    assert!(needs_flatten(
            "HTTP 400: Unable to generate parser for this template. Conversation roles must alternate user/assistant"
        ));
    assert!(!needs_flatten("HTTP 404: model not found"));
    assert!(!needs_flatten("HTTP 500: tools exploded"));
    assert!(!needs_flatten("HTTP 400: max_tokens must be positive"));
    assert!(!needs_flatten("connection failed: refused"));
    // Sticky only when the 400 names tools/template/roles; a capability
    // rejection retries once without poisoning the endpoint.
    assert!(flatten_is_sticky("HTTP 400: tools are not supported"));
    assert!(flatten_is_sticky(
        "HTTP 400: conversation roles must alternate"
    ));
    assert!(needs_flatten(
        "HTTP 400: this model does not support images"
    ));
    assert!(!flatten_is_sticky(
        "HTTP 400: this model does not support images"
    ));
    assert!(!flatten_is_sticky("HTTP 500: tools exploded"));
}

#[test]
fn loopback_sends_native_tools_first_then_flattens_after_400_and_remembers() {
    let ok =
        r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
    let reject = r#"{"error":{"message":"this template does not support tools"}}"#;
    let (base, handle, bodies) = mock_server_capture(vec![(400, reject), (200, ok), (200, ok)]);
    let p = Provider {
        protocol: Protocol::OpenAi,
        base_url: base,
        api_key: None,
        model: "local-model".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    let tools = vec![ToolDef {
        name: "read_file",
        description: "read",
        schema: json!({"type": "object"}),
    }];
    let msgs = [Msg::System("be brief".into()), Msg::User("hi".into())];
    complete(&p, &msgs, &tools).unwrap();
    complete(&p, &msgs, &tools).unwrap();
    handle.join().unwrap();
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3, "400 → retry, then one remembered call");
    let sent: Vec<Value> = bodies
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    // First attempt: native tools and the real system role, even on loopback.
    assert!(sent[0].get("tools").is_some());
    assert_eq!(sent[0]["messages"][0]["role"], "system");
    // Retry after the 400: flattened, tool-less.
    assert!(sent[1].get("tools").is_none());
    assert!(sent[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["role"] != "system"));
    // Second exchange goes straight to the flattened shape — no failed
    // native attempt first.
    assert!(sent[2].get("tools").is_none());
    assert!(flatten_remembered(&format!(
        "{}/chat/completions",
        p.base_url
    )));
}

#[test]
fn loopback_keeps_native_tools_when_the_server_accepts_them() {
    let ok =
        r#"{"choices":[{"message":{"role":"assistant","content":"y"},"finish_reason":"stop"}]}"#;
    let (base, handle, bodies) = mock_server_capture(vec![(200, ok), (200, ok)]);
    let p = Provider {
        protocol: Protocol::OpenAi,
        base_url: base,
        api_key: None,
        model: "local-model".into(),
        context_tokens: 8_192,
        temperature: None,
        max_tokens: None,
        effort: Effort::Off,
        ollama_ctx: std::sync::OnceLock::new(),
    };
    let tools = vec![ToolDef {
        name: "read_file",
        description: "read",
        schema: json!({"type": "object"}),
    }];
    complete(&p, &[Msg::User("hi".into())], &tools).unwrap();
    complete(&p, &[Msg::User("hi".into())], &tools).unwrap();
    handle.join().unwrap();
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    for b in bodies.iter() {
        let v: Value = serde_json::from_str(b).unwrap();
        assert!(v.get("tools").is_some(), "native tools stay on: {b}");
    }
    // A 404 (or any non-400) must not poison the endpoint either.
    assert!(!flatten_remembered(&format!(
        "{}/chat/completions",
        p.base_url
    )));
}
