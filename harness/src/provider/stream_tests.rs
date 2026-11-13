//! The typed stream readers against the `serde_json::Value` readers they
//! replaced (kept here, verbatim, as the reference): every stream in a
//! corpus of well-formed and deliberately odd events must produce the same
//! reply, the same error, and the same callbacks in the same order.

use super::*;

// What a reply held, in a form that compares.
#[derive(Debug, PartialEq)]
struct Seen {
    text: String,
    calls: Vec<(String, String, Value)>,
    stop_reason: Option<String>,
    usage: [u64; 4],
    thinking_blocks: Vec<Value>,
}

#[derive(Debug, PartialEq)]
struct Outcome {
    reply: Result<Seen, String>,
    events: Vec<(char, String)>,
}

type Parser = fn(&[u8], &mut dyn FnMut(&str), &mut dyn FnMut(&str)) -> Result<Reply, String>;

fn run(parse: Parser, stream: &str) -> Outcome {
    let events = std::cell::RefCell::new(Vec::new());
    let reply = parse(
        stream.as_bytes(),
        &mut |t| events.borrow_mut().push(('t', t.to_string())),
        &mut |t| events.borrow_mut().push(('k', t.to_string())),
    )
    .map(|r| Seen {
        text: r.text,
        calls: r
            .calls
            .into_iter()
            .map(|c| (c.id, c.name, c.input))
            .collect(),
        stop_reason: r.stop_reason,
        usage: [
            r.usage.input,
            r.usage.output,
            r.usage.cache_read,
            r.usage.cache_write,
        ],
        thinking_blocks: r.thinking_blocks,
    });
    Outcome {
        reply,
        events: events.into_inner(),
    }
}

// Every way to make one field of `event` odd: each value in the tree is
// replaced in turn by null, a number, a string, an array, an object…
fn mutations(event: &Value) -> Vec<Value> {
    fn paths(v: &Value, at: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        out.push(at.clone());
        match v {
            Value::Object(m) => {
                for (k, child) in m {
                    at.push(k.clone());
                    paths(child, at, out);
                    at.pop();
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    at.push(i.to_string());
                    paths(child, at, out);
                    at.pop();
                }
            }
            _ => {}
        }
    }
    fn at<'a>(v: &'a mut Value, path: &[String]) -> &'a mut Value {
        path.iter().fold(v, |v, k| match v {
            Value::Array(a) => &mut a[k.parse::<usize>().unwrap()],
            other => &mut other[k.as_str()],
        })
    }
    let odd = [
        json!(null),
        json!(1),
        json!(-1),
        json!(1.5),
        json!(true),
        json!("s"),
        json!(""),
        json!("text_delta"),
        json!("tool_use"),
        json!([]),
        json!(["a", 2, {"type": "text_delta", "text": "x", "content": "y"}]),
        json!({}),
        json!({"type": "thinking", "text": "x", "content": "y", "name": "n", "arguments": "{}"}),
    ];
    let mut all = Vec::new();
    paths(event, &mut Vec::new(), &mut all);
    let mut out = Vec::new();
    for path in &all {
        for replacement in &odd {
            let mut mutated = event.clone();
            *at(&mut mutated, path) = replacement.clone();
            out.push(mutated);
        }
        // And the field missing altogether.
        if let Some((last, parent)) = path.split_last() {
            let mut mutated = event.clone();
            if let Value::Object(m) = at(&mut mutated, parent) {
                m.remove(last);
                out.push(mutated);
            }
        }
    }
    out
}

// Each event of `stream`, mutated every way, in place within the stream.
fn corpus(stream: &[Value]) -> Vec<String> {
    let line = |v: &Value| format!("data: {v}\n\n");
    let mut out = vec![stream.iter().map(line).collect::<String>()];
    for (i, event) in stream.iter().enumerate() {
        for mutated in mutations(event) {
            let mut s = String::new();
            for (k, e) in stream.iter().enumerate() {
                s.push_str(&line(if k == i { &mutated } else { e }));
            }
            out.push(s);
        }
    }
    out
}

fn assert_same(new: Parser, old: Parser, streams: &[String]) {
    for s in streams {
        assert_eq!(run(new, s), run(old, s), "stream:\n{s}");
    }
}

// Lines no serialiser produces: duplicate keys, escapes, non-objects,
// broken JSON, events without the blank line, CRLF.
const HAND_WRITTEN: [&str; 12] = [
    "data: null\n",
    "data: []\n",
    "data: \"text\"\n",
    "data: 17\n",
    "data: {not json\n",
    "data:\n",
    "data: {\"type\":\"content_block_delta\",\"type\":\"ping\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"dup\"}}\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\",\"text\":\"b\"}}\r\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"tab\\tquote\\\" \\u00e9 \\ud83d\\ude80 \\n\"}}\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"a\",\"content\":\"b\"}}],\"choices\":[{\"delta\":{\"content\":\"c\"}}]}\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"tab\\tquote\\\" \\u00e9 \\ud83d\\ude80 <think>hm\\n</think>after\",\"reasoning\":\"r\\n\"}}]}\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"x\\ty\",\"function\":{\"name\":\"n\\u0041\",\"arguments\":\"{\\\"a\\\":\\\"\\\\n\\\"}\"}}]}}]}\n",
];

fn with_hand_written(stream: &[Value], tail: &str) -> Vec<String> {
    let body: String = stream.iter().map(|v| format!("data: {v}\n\n")).collect();
    let mut out: Vec<String> = HAND_WRITTEN
        .iter()
        .flat_map(|l| {
            [
                format!("{l}{body}{tail}"),
                format!("{body}{l}"),
                l.to_string(),
            ]
        })
        .collect();
    out.push(format!("{}{tail}", HAND_WRITTEN.concat()));
    out
}

#[test]
fn openai_stream_reads_every_shape_as_the_value_reader_did() {
    let stream = [
        json!({"id": "x", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "", "reasoning_content": "hm"}, "finish_reason": null}], "usage": null}),
        json!({"choices": [{"index": 0, "delta": {"content": "Hel<thi"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": "nk>plan</think>lo", "reasoning": "more"}}, {"index": 1, "delta": {"content": "second choice"}}]}),
        json!({"choices": [{"delta": {"content": null, "tool_calls": [{"index": 0, "id": "c0", "type": "function", "function": {"name": "read_file", "arguments": "{\"pa"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a\"}"}}, {"index": 1, "id": "c1", "function": {"name": "finish", "arguments": ""}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 9}}),
        json!({"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 30, "prompt_tokens_details": {"cached_tokens": 100}}}),
    ];
    let mut streams = corpus(&stream);
    streams.extend(with_hand_written(&stream, "data: [DONE]\n"));
    // An error chunk part-way, and a stream cut short.
    streams.extend(corpus(&[
        stream[1].clone(),
        json!({"error": {"message": "overloaded sk-ABCDEFGHIJKLMNOP", "code": 529}}),
        stream[5].clone(),
    ]));
    streams.push(format!("data: {}\n", stream[1]));
    assert!(streams.len() > 1_000);
    assert_same(
        |r, t, k| openai_stream(r, t, k),
        |r, t, k| reference_openai_stream(r, t, k),
        &streams,
    );
}

#[test]
fn anthropic_stream_reads_every_shape_as_the_value_reader_did() {
    let stream = [
        json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 12, "cache_read_input_tokens": 7, "cache_creation_input_tokens": 3, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "think."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "c2ln"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "b3BhcXVl"}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "Hello"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": " there\n"}}),
        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "\"a\"}"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 40}}),
        json!({"type": "message_stop"}),
    ];
    let mut streams = corpus(&stream);
    streams.extend(with_hand_written(&stream, ""));
    streams.extend(corpus(&[
        stream[8].clone(),
        stream[9].clone(),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy sk-ABCDEFGHIJKLMNOP"}}),
        stream[10].clone(),
    ]));
    streams.push(format!("data: {}\n", stream[9]));
    assert!(streams.len() > 1_000);
    assert_same(
        |r, t, k| anthropic_stream(r, t, k),
        |r, t, k| reference_anthropic_stream(r, t, k),
        &streams,
    );
}

// ── the readers as they were (main at 83190bc) ─────────────────────────────

fn reference_openai_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut in_think = false;
    let mut think_carry = String::new();
    let mut stop_reason: Option<String> = None;
    let mut usage = Usage::default();
    // index → (id, name, accumulated args)
    let mut pending: Vec<(String, String, String)> = Vec::new();
    let mut stream_err: Option<String> = None;
    let stopped = for_each_sse(reader, |data| {
        if data == "[DONE]" {
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        // vLLM, LiteLLM, OpenRouter and friends report mid-stream failures as
        // an {"error": …} chunk rather than an HTTP status.
        if let Some(e) = openai_error(&v) {
            stream_err = Some(e);
            return true;
        }
        // With stream_options.include_usage the final chunk (empty choices)
        // carries the totals; some servers put usage on every chunk instead.
        if let Some(u) = openai_usage(&v) {
            usage = u;
        }
        // finish_reason rides on the last content-bearing chunk (or one after).
        if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
            stop_reason = Some(normalize_stop_reason(r));
        }
        let delta = &v["choices"][0]["delta"];
        if let Some(r) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            on_thinking(r);
        }
        if let Some(t) = delta["content"].as_str() {
            route_openai_content(
                t,
                &mut in_think,
                &mut think_carry,
                &mut text,
                on_text,
                on_thinking,
            );
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while pending.len() <= idx {
                    pending.push((String::new(), String::new(), String::new()));
                }
                let e = &mut pending[idx];
                if let Some(id) = tc["id"].as_str() {
                    if !id.is_empty() {
                        e.0 = id.to_string();
                    }
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    if !name.is_empty() {
                        e.1 = name.to_string();
                    }
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    e.2.push_str(args);
                }
            }
        }
        false
    })?;
    if let Some(e) = stream_err {
        return Err(e);
    }
    if !stopped && stop_reason.is_none() {
        return Err(STREAM_TRUNCATED.to_string());
    }
    flush_think_carry(&think_carry, in_think, &mut text, on_text);
    let calls = pending
        .into_iter()
        .filter(|e| !e.1.is_empty())
        .map(|(id, name, args)| ToolCall {
            id,
            name,
            input: parse_args(&args),
        })
        .collect();
    Ok(Reply {
        text,
        calls,
        stop_reason,
        usage,
        ..Default::default()
    })
}

fn reference_anthropic_stream(
    reader: impl Read,
    on_text: &mut dyn FnMut(&str),
    on_thinking: &mut dyn FnMut(&str),
) -> Result<Reply, String> {
    let mut text = String::new();
    let mut stop_reason: Option<String> = None;
    let mut stream_err: Option<String> = None;
    let mut usage = Usage::default();
    // index → (id, name, accumulated input JSON)
    let mut pending: Vec<(usize, String, String, String)> = Vec::new();
    // index → thinking/redacted_thinking block, assembled from its deltas so
    // it can be replayed verbatim (signature included) on the next request.
    let mut thinking: Vec<(usize, Value)> = Vec::new();
    let stopped = for_each_sse(reader, |data| {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        match v["type"].as_str() {
            Some("message_start") => {
                anthropic_usage_into(&v["message"]["usage"], &mut usage);
            }
            Some("content_block_start") => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let cb = &v["content_block"];
                match cb["type"].as_str() {
                    Some("tool_use") => pending.push((
                        idx,
                        cb["id"].as_str().unwrap_or_default().to_string(),
                        cb["name"].as_str().unwrap_or_default().to_string(),
                        String::new(),
                    )),
                    Some("thinking") => thinking.push((
                        idx,
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                    )),
                    Some("redacted_thinking") => thinking.push((idx, cb.clone())),
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let d = &v["delta"];
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().unwrap_or_default();
                        text.push_str(t);
                        on_text(t);
                    }
                    Some("thinking_delta") => {
                        // Extended thinking tokens (claude-3-7+): surfaced separately so
                        // the caller can render them as internal monologue.
                        let t = d["thinking"].as_str().unwrap_or_default();
                        if let Some((_, block)) = thinking.iter_mut().find(|e| e.0 == idx) {
                            if let Some(acc) = block["thinking"].as_str() {
                                block["thinking"] = json!(format!("{acc}{t}"));
                            }
                        }
                        on_thinking(t);
                    }
                    Some("signature_delta") => {
                        if let Some((_, block)) = thinking.iter_mut().find(|e| e.0 == idx) {
                            block["signature"] = json!(d["signature"].as_str().unwrap_or_default());
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(e) = pending.iter_mut().find(|e| e.0 == idx) {
                            e.3.push_str(d["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                // Final metadata for the turn — carries the stop reason and
                // the output token count.
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    stop_reason = Some(normalize_stop_reason(r));
                }
                anthropic_usage_into(&v["usage"], &mut usage);
            }
            Some("error") => {
                // Mid-stream server error (e.g. overloaded) — surface it so the
                // caller can retry or report instead of returning a truncated reply.
                stream_err = Some(format!(
                    "stream error: {}",
                    redact(&v["error"].to_string())
                        .chars()
                        .take(400)
                        .collect::<String>()
                ));
                return true;
            }
            Some("message_stop") => return true,
            _ => {}
        }
        false
    })?;
    if let Some(e) = stream_err {
        return Err(e);
    }
    // message_delta carries the stop reason, so a stream that got that far has
    // all its content even if message_stop itself was lost.
    if !stopped && stop_reason.is_none() {
        return Err(STREAM_TRUNCATED.to_string());
    }
    // Same INVALID_ARGS flagging as the OpenAI path: a truncated tool call must
    // reach the agent loop as malformed, not silently execute with empty input.
    let calls = pending
        .into_iter()
        .map(|(_, id, name, args)| ToolCall {
            id,
            name,
            input: parse_args(&args),
        })
        .collect();
    Ok(Reply {
        text,
        calls,
        stop_reason,
        usage,
        thinking_blocks: thinking.into_iter().map(|(_, b)| b).collect(),
    })
}
