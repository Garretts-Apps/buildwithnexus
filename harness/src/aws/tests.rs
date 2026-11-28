use super::*;

// AWS's published SigV4 suite (awslabs/aws-c-auth,
// tests/aws-signing-test-suite/v4). The example secret is AWS's own; it is
// put together here so no key-shaped literal sits in the source.
fn example() -> Creds {
    Creds {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: format!("{}{}", "wJalrXUtnFEMI/K7MDENG+bPxRfiCY", "EXAMPLEKEY"),
        session_token: None,
    }
}

fn sig(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
    let out = sign(
        &Signing {
            method,
            path,
            query: &[],
            headers,
            body,
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
        },
        &example(),
    );
    out.into_iter()
        .find(|(k, _)| k == "authorization")
        .unwrap()
        .1
}

#[test]
fn sigv4_matches_the_aws_test_suite() {
    let host = [("Host", "example.amazonaws.com")];
    let want = |s: &str| {
        format!("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature={s}")
    };
    assert_eq!(
        sig("GET", "/", &host, b""),
        want("5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31")
    );
    assert_eq!(
        sig("POST", "/", &host, b""),
        want("5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b")
    );
    assert_eq!(
        sig("GET", "/ሴ", &host, b""),
        want("8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85")
    );
    assert_eq!(
        sig("GET", "/example space/", &host, b""),
        want("652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741")
    );
    let body = b"Param1=value1";
    let hash = sha256_hex(body);
    assert_eq!(
        hash,
        "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e"
    );
    assert_eq!(
        sig(
            "POST",
            "/",
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Host", "example.amazonaws.com"),
                ("Content-Length", "13"),
                ("x-amz-content-sha256", &hash),
            ],
            body
        ),
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-length;content-type;host;x-amz-content-sha256;x-amz-date, Signature=d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b"
    );
}

// A session token is sent and signed; the secret appears nowhere.
#[test]
fn a_session_token_is_signed_and_the_secret_is_not_sent() {
    let mut c = example();
    c.session_token = Some("tok".into());
    let out = sign(
        &Signing {
            method: "POST",
            path: "/model/anthropic.claude-x-v1%3A0/invoke",
            query: &[],
            headers: &[("host", "bedrock-runtime.us-east-1.amazonaws.com")],
            body: b"{}",
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20150830T123600Z",
        },
        &c,
    );
    assert!(out.contains(&("x-amz-security-token".into(), "tok".into())));
    let auth = &out.iter().find(|(k, _)| k == "authorization").unwrap().1;
    assert!(
        auth.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
        "{auth}"
    );
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
    for (_, v) in &out {
        assert!(!v.contains(&c.secret_access_key));
    }
}

#[test]
fn the_canonical_path_encodes_the_sent_path_again() {
    // Bedrock's model ids hold a colon: sent as %3A, signed as %253A.
    assert_eq!(
        uri_encode("/model/a-v1%3A0/invoke", true),
        "/model/a-v1%253A0/invoke"
    );
    assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
}

#[test]
fn amz_dates_and_regions() {
    assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
    assert_eq!(amz_date(0), "19700101T000000Z");
    assert_eq!(amz_date(951_782_400), "20000229T000000Z");
    assert_eq!(
        region_of("bedrock-runtime.us-west-2.amazonaws.com"),
        Some("us-west-2")
    );
    assert_eq!(region_of("bedrock-runtime..amazonaws.com"), None);
    assert_eq!(region_of("bedrock.us-west-2.amazonaws.com"), None);
}

#[test]
fn a_profile_is_read_from_the_credentials_file() {
    let text = "[default]\naws_access_key_id = A1\naws_secret_access_key = S1\n\n[work]\naws_access_key_id=A2\naws_secret_access_key=S2\naws_session_token=T2\n";
    assert_eq!(profile_creds(text, "default").unwrap().access_key_id, "A1");
    let w = profile_creds(text, "work").unwrap();
    assert_eq!(
        (w.secret_access_key.as_str(), w.session_token.as_deref()),
        ("S2", Some("T2"))
    );
    assert!(profile_creds(text, "none").is_none());
    assert!(profile_creds("[default]\naws_access_key_id=A\n", "default").is_none());
}

#[test]
fn crc32_is_ieee() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

// Roadmap 4.4: a frame whose checksums don't match, or whose lengths don't
// add up, is refused; nothing is allocated for a length past the cap.
#[test]
fn invariant_a_corrupt_or_oversized_event_stream_frame_is_refused() {
    let good = encode_message(
        &[(":message-type", "event"), (":event-type", "chunk")],
        b"{}",
    );
    let m = read_message(&mut &good[..]).unwrap().unwrap();
    assert_eq!(m.header(":event-type"), Some("chunk"));
    assert_eq!(m.payload, b"{}");
    assert!(read_message(&mut &b""[..]).unwrap().is_none());

    let mut bad_prelude = good.clone();
    bad_prelude[9] ^= 1;
    assert!(read_message(&mut &bad_prelude[..])
        .unwrap_err()
        .contains("prelude checksum"));

    let mut bad_body = good.clone();
    let i = bad_body.len() - 6;
    bad_body[i] ^= 1;
    assert!(read_message(&mut &bad_body[..])
        .unwrap_err()
        .contains("message checksum"));

    // A prelude claiming 4 GB, with a valid prelude checksum.
    let mut huge = u32::MAX.to_be_bytes().to_vec();
    huge.extend_from_slice(&0u32.to_be_bytes());
    let c = crc32(&huge);
    huge.extend_from_slice(&c.to_be_bytes());
    assert!(read_message(&mut &huge[..])
        .unwrap_err()
        .contains("bytes with"));

    // Headers longer than the message.
    let mut over = 16u32.to_be_bytes().to_vec();
    over.extend_from_slice(&5u32.to_be_bytes());
    let c = crc32(&over);
    over.extend_from_slice(&c.to_be_bytes());
    over.extend_from_slice(&[0; 4]);
    assert!(read_message(&mut &over[..]).is_err());

    // A frame cut short.
    assert!(read_message(&mut &good[..good.len() - 1]).is_err());
}

#[test]
fn bedrock_events_become_anthropic_server_sent_events() {
    let chunk = |json: &str| {
        let payload =
            serde_json::json!({"bytes": crate::media::b64_encode(json.as_bytes())}).to_string();
        encode_message(
            &[(":message-type", "event"), (":event-type", "chunk")],
            payload.as_bytes(),
        )
    };
    let mut wire = chunk(r#"{"type":"message_start","message":{"usage":{"input_tokens":3}}}"#);
    wire.extend(chunk(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    ));
    wire.extend(encode_message(
        &[
            (":message-type", "exception"),
            (":exception-type", "throttlingException"),
        ],
        br#"{"message":"Too many requests"}"#,
    ));
    let mut text = String::new();
    BedrockSse::new(&wire[..])
        .read_to_string(&mut text)
        .unwrap();
    let events: Vec<&str> = text.split("\n\n").filter(|e| !e.is_empty()).collect();
    assert_eq!(events.len(), 3, "{text}");
    assert!(events[1].contains("\"text\":\"hi\""), "{text}");
    let err: serde_json::Value =
        serde_json::from_str(events[2].strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(err["type"], "error");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("(HTTP 429)"),
        "{err}"
    );

    // A corrupt frame ends the read with an error, not with silence.
    let mut corrupt = chunk("{}");
    corrupt[20] ^= 1;
    assert!(BedrockSse::new(&corrupt[..])
        .read_to_string(&mut String::new())
        .is_err());
}
