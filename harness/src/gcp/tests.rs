use super::*;
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

// A token endpoint that answers `n` requests and keeps each request's body.
fn token_server(n: usize, reply: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Default::default();
    let sink = seen.clone();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((sock, _)) = listener.accept() else {
                return;
            };
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            let mut sock = sock;
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, seen)
}

fn with_token_url<T>(url: &str, f: impl FnOnce() -> T) -> T {
    TEST_TOKEN_URL.with(|t| *t.borrow_mut() = Some(url.to_string()));
    let out = f();
    TEST_TOKEN_URL.with(|t| *t.borrow_mut() = None);
    out
}

// Roadmap 4.4, IH-1: the credentials go to Google's token endpoint only.
// A key file's `token_uri` naming another host is ignored, so a planted
// file can't collect a refresh token or a signed assertion.
#[test]
fn invariant_ih1_a_key_files_token_uri_does_not_redirect_the_exchange() {
    let (planted, planted_seen) = token_server(1, r#"{"access_token":"evil","expires_in":3600}"#);
    let (google, google_seen) = token_server(1, r#"{"access_token":"tok","expires_in":3599}"#);
    let creds = json!({
        "type": "authorized_user",
        "client_id": "cid",
        "client_secret": "csecret",
        "refresh_token": "rtoken",
        "token_uri": planted,
    });
    let (token, ttl) = with_token_url(&google, || exchange(&creds, 1_700_000_000)).unwrap();
    assert_eq!((token.as_str(), ttl), ("tok", 3599));
    let got = google_seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert!(got[0].contains("grant_type=refresh_token"), "{got:?}");
    assert!(got[0].contains("refresh_token=rtoken"), "{got:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(planted_seen.lock().unwrap().is_empty());
    // Outside tests the endpoint is the constant.
    assert_eq!(token_url(), TOKEN_URL);
}

#[test]
fn a_credential_file_of_another_kind_or_missing_a_field_is_refused() {
    for creds in [
        json!({"type": "external_account", "audience": "x"}),
        json!({"type": "authorized_user", "client_id": "c", "client_secret": "s"}),
        json!({"type": "service_account", "client_email": "e@x"}),
        json!({}),
    ] {
        assert!(token_form(&creds, 0).is_err(), "{creds}");
    }
}

// The assertion is RS256 over the header and claims, for the cloud-platform
// scope, addressed to Google's token endpoint and valid for an hour. The
// key is made for this run, so no private key is committed.
#[cfg(unix)]
#[test]
fn a_service_account_signs_its_assertion_with_its_key() {
    let dir = std::env::temp_dir().join(format!("bwn-gcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pem_path = dir.join("k.pem");
    let ok = std::process::Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
        ])
        .arg(&pem_path)
        .output()
        .expect("openssl makes the test key")
        .status
        .success();
    assert!(ok, "openssl genpkey");
    let pem = std::fs::read_to_string(&pem_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let now = 1_700_000_000;
    let jwt = service_account_jwt("sa@p.iam.gserviceaccount.com", Some("kid1"), &pem, now).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let decode = |s: &str| {
        let std64: String = s
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        crate::media::b64_decode(&std64).unwrap()
    };
    let header: Value = serde_json::from_slice(&decode(parts[0])).unwrap();
    let claims: Value = serde_json::from_slice(&decode(parts[1])).unwrap();
    assert_eq!(header, json!({"alg": "RS256", "typ": "JWT", "kid": "kid1"}));
    assert_eq!(claims["iss"], "sa@p.iam.gserviceaccount.com");
    assert_eq!(claims["aud"], TOKEN_URL);
    assert_eq!(claims["scope"], SCOPE);
    assert_eq!(claims["iat"], now);
    assert_eq!(claims["exp"], now + 3600);

    let key = ring::signature::RsaKeyPair::from_pkcs8(&pem_body(&pem).unwrap()).unwrap();
    let public = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        key.public().as_ref().to_vec(),
    );
    let signed = format!("{}.{}", parts[0], parts[1]);
    assert!(public.verify(signed.as_bytes(), &decode(parts[2])).is_ok());
    // Another payload doesn't verify under that signature.
    assert!(public.verify(b"x.y", &decode(parts[2])).is_err());

    let form = token_form(
        &json!({"type": "service_account", "client_email": "sa@p", "private_key": pem}),
        now,
    )
    .unwrap();
    assert_eq!(form[0].1, "urn:ietf:params:oauth:grant-type:jwt-bearer");
    assert_eq!(form[1].0, "assertion");
}

#[test]
fn a_key_that_isnt_pem_rsa_is_refused() {
    assert!(service_account_jwt("e", None, "not a key", 0).is_err());
    assert!(service_account_jwt(
        "e",
        None,
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
        0
    )
    .is_err());
}
