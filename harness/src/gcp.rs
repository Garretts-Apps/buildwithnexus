//! Google Cloud sign-in for Vertex AI (roadmap 4.4), in-tree.
//!
//! Application Default Credentials: the file `GOOGLE_APPLICATION_CREDENTIALS`
//! names, or the one `gcloud auth application-default login` writes. A
//! service-account key signs a JWT (RS256, with ring) that is exchanged for
//! an access token; a user credential trades its refresh token. Either way
//! the request goes to Google's token endpoint and nowhere else: the
//! `token_uri` a key file names is ignored, so a planted file can't send a
//! signed assertion or a refresh token to a host of its choosing. The token
//! is kept in memory only, and renewed before it expires.

use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The only place a credential is exchanged for a token.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[cfg(test)]
thread_local! {
    // A mock token endpoint for this crate's tests; nothing outside them can
    // reach it.
    static TEST_TOKEN_URL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn token_url() -> String {
    #[cfg(test)]
    if let Some(u) = TEST_TOKEN_URL.with(|t| t.borrow().clone()) {
        return u;
    }
    TOKEN_URL.to_string()
}

/// Where the credentials are read from, in the order Google's libraries
/// use; None when neither is there.
pub fn credentials_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let adc = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("gcloud")
    } else {
        crate::config::user_home()?.join(".config").join("gcloud")
    }
    .join("application_default_credentials.json");
    adc.is_file().then_some(adc)
}

static TOKEN: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);

/// An access token for Vertex AI, from the cache while it has more than
/// five minutes left.
pub fn access_token() -> Result<String, String> {
    let mut slot = TOKEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, until)) = slot.as_ref() {
        if Instant::now() + Duration::from_secs(300) < *until {
            return Ok(t.clone());
        }
    }
    let path = credentials_path().ok_or(
        "no Google credentials: set GOOGLE_APPLICATION_CREDENTIALS to a service-account key, or run `gcloud auth application-default login`",
    )?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let creds: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} isn't JSON: {e}", path.display()))?;
    let (token, ttl) = exchange(&creds, now_secs())?;
    *slot = Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
    Ok(token)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The form a credential file sends to the token endpoint.
pub(crate) fn token_form(creds: &Value, now: u64) -> Result<Vec<(&'static str, String)>, String> {
    let field = |k: &str| {
        creds[k]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("the Google credentials have no {k}"))
    };
    match creds["type"].as_str() {
        Some("service_account") => Ok(vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            ),
            (
                "assertion",
                service_account_jwt(
                    &field("client_email")?,
                    creds["private_key_id"].as_str(),
                    &field("private_key")?,
                    now,
                )?,
            ),
        ]),
        Some("authorized_user") => Ok(vec![
            ("grant_type", "refresh_token".into()),
            ("client_id", field("client_id")?),
            ("client_secret", field("client_secret")?),
            ("refresh_token", field("refresh_token")?),
        ]),
        other => Err(format!(
            "Google credentials of type {} aren't supported; use a service-account key or `gcloud auth application-default login`",
            other.unwrap_or("(none)")
        )),
    }
}

fn exchange(creds: &Value, now: u64) -> Result<(String, u64), String> {
    let form = token_form(creds, now)?;
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let url = token_url();
    let http = crate::net::Client::new(|b| b.timeout(Duration::from_secs(30)).redirects(0));
    let (status, v) = crate::mcp_auth::json_reply(
        &url,
        http.post(&url)
            .set("Accept", "application/json")
            .send_form(&pairs),
    )?;
    match v["access_token"].as_str().filter(|t| !t.is_empty()) {
        Some(t) if (200..300).contains(&status) => {
            Ok((t.to_string(), v["expires_in"].as_u64().unwrap_or(3600)))
        }
        _ => Err(format!(
            "Google's token endpoint refused the credentials (HTTP {status}): {}",
            crate::provider::redact(
                &v["error_description"]
                    .as_str()
                    .or(v["error"].as_str())
                    .unwrap_or("no reason given")
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        )),
    }
}

/// A signed RS256 JWT asserting `email` for the cloud-platform scope, valid
/// for an hour from `now`, addressed to Google's token endpoint.
pub(crate) fn service_account_jwt(
    email: &str,
    key_id: Option<&str>,
    pem: &str,
    now: u64,
) -> Result<String, String> {
    let der = pem_body(pem).ok_or("the service-account private_key isn't a PEM key")?;
    let key = ring::signature::RsaKeyPair::from_pkcs8(&der)
        .map_err(|e| format!("the service-account private_key isn't an RSA key: {e}"))?;
    let mut header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    if let Some(k) = key_id {
        header["kid"] = k.into();
    }
    let claims = serde_json::json!({
        "iss": email,
        "scope": SCOPE,
        "aud": TOKEN_URL,
        "iat": now,
        "exp": now + 3600,
    });
    let b64 = |b: &[u8]| crate::mcp_auth::base64(b, true);
    let signing = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing.as_bytes(),
        &mut sig,
    )
    .map_err(|_| "signing the assertion failed")?;
    Ok(format!("{signing}.{}", b64(&sig)))
}

// The DER inside a `-----BEGIN PRIVATE KEY-----` block.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    crate::media::b64_decode(&body).filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests;
