//! Inbound webhooks: signature verification as the route's registration, not a prefix.
//!
//! A webhook route exists only through [`route`], which mounts the handler behind a
//! middleware that verifies the provider's HMAC over the raw body before the handler
//! runs. The CSRF layer bypasses exactly the paths in [`REGISTERED_PATHS`], and every
//! path mounted with [`route`] must be listed there (a test keeps the two in sync).
//! Replays are the handler's job: store the provider's event id with
//! `on_conflict_do_nothing`, so a redelivery is a no-op (guides/security.md#csrf).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    handler::Handler,
    http::{HeaderMap, StatusCode},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::post,
};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Every path mounted with [`route`]. The CSRF bypass and the CSRF-evidence check
/// consult this list and nothing else: no prefix grants a bypass.
pub const REGISTERED_PATHS: &[&str] = &[
    // APP: one entry per webhook::route mount, e.g. "/webhooks/stripe".
];

/// How long a signed timestamp stays valid, when the provider's scheme has one.
pub const MAX_SKEW_SECS: i64 = 300;

/// The largest body the verifier buffers. Providers send small JSON events.
const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scheme {
    /// `sha256=<hex>` (GitHub's `X-Hub-Signature-256`; a bare hex value also passes).
    SignedHex,
    /// `t=<unix>,v1=<hex>` over `"<t>.<body>"` (Stripe's `Stripe-Signature`).
    Timestamped,
}

/// One provider's verification rule: which header, which scheme, which secret.
/// The secret comes from `Config` (env), never from code.
#[derive(Clone)]
pub struct Webhook {
    secret: Vec<u8>,
    header: &'static str,
    scheme: Scheme,
}

impl Webhook {
    pub fn github(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            header: "x-hub-signature-256",
            scheme: Scheme::SignedHex,
        }
    }

    pub fn stripe(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            header: "stripe-signature",
            scheme: Scheme::Timestamped,
        }
    }

    /// A provider with a plain hex HMAC-SHA256 in a named header.
    pub fn signed_hex(secret: impl Into<Vec<u8>>, header: &'static str) -> Self {
        Self {
            secret: secret.into(),
            header,
            scheme: Scheme::SignedHex,
        }
    }

    /// `true` when the signature header proves `body` came from the provider.
    /// `now` is seconds since the Unix epoch (only the timestamped scheme uses it).
    pub fn verify(&self, headers: &HeaderMap, body: &[u8], now: i64) -> bool {
        let Some(raw) = headers.get(self.header).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        match self.scheme {
            Scheme::SignedHex => {
                let expected = raw.strip_prefix("sha256=").unwrap_or(raw);
                verify_hmac(&self.secret, body, expected)
            }
            Scheme::Timestamped => {
                let mut stamp: Option<i64> = None;
                let mut signatures = Vec::new();
                for part in raw.split(',') {
                    match part.trim().split_once('=') {
                        Some(("t", v)) => stamp = v.parse().ok(),
                        Some(("v1", v)) => signatures.push(v),
                        _ => {}
                    }
                }
                let Some(stamp) = stamp else { return false };
                if (now - stamp).abs() > MAX_SKEW_SECS {
                    return false;
                }
                let mut signed = format!("{stamp}.").into_bytes();
                signed.extend_from_slice(body);
                signatures
                    .iter()
                    .any(|sig| verify_hmac(&self.secret, &signed, sig))
            }
        }
    }
}

/// Whether `path` is a registered webhook path (exact match, never a prefix).
pub fn is_registered(path: &str) -> bool {
    is_registered_in(REGISTERED_PATHS, path)
}

fn is_registered_in(paths: &[&str], path: &str) -> bool {
    paths.contains(&path)
}

/// Mounts `handler` at `path` behind signature verification. The only way a webhook
/// route should be mounted; add `path` to [`REGISTERED_PATHS`] in the same change.
pub fn route<S, H, T>(path: &'static str, webhook: Webhook, handler: H) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    H: Handler<T, S>,
    T: 'static,
{
    Router::new().route(
        path,
        post(handler).route_layer(from_fn_with_state(webhook, verify_signature)),
    )
}

async fn verify_signature(State(webhook): State<Webhook>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return AppError::Unauthorized.into_response(),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    if !webhook.verify(&parts.headers, &bytes, now) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// HMAC-SHA256 (RFC 2104) over `data`, compared in constant time against a hex digest.
fn verify_hmac(secret: &[u8], data: &[u8], expected_hex: &str) -> bool {
    let Some(expected) = hex_decode(expected_hex) else {
        return false;
    };
    let actual = hmac_sha256(secret, data);
    let (da, db) = (Sha256::digest(actual), Sha256::digest(&expected));
    da.iter()
        .zip(db.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The raw HMAC-SHA256 of `data` under `secret`. Public for handlers that must verify
/// a provider whose scheme this module doesn't model yet.
pub fn hmac_sha256(secret: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend(key.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(data);
    let inner_hash = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(key.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer).into()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn hmac_matches_rfc4231() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex_encode(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_encode(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_valid_github_signature_passes_and_tampering_fails() {
        let hook = Webhook::github(b"s3cret".to_vec());
        let body = br#"{"action":"opened"}"#;
        let sig = format!("sha256={}", hex_encode(&hmac_sha256(b"s3cret", body)));
        let good = headers(&[("x-hub-signature-256", &sig)]);
        assert!(hook.verify(&good, body, 0));
        assert!(!hook.verify(&good, br#"{"action":"closed"}"#, 0));
        assert!(!hook.verify(&HeaderMap::new(), body, 0));
        let wrong = Webhook::github(b"other".to_vec());
        assert!(!wrong.verify(&good, body, 0));
    }

    #[test]
    fn stripe_signatures_expire_and_tampering_fails() {
        let hook = Webhook::stripe(b"whsec_test".to_vec());
        let body = br#"{"id":"evt_1"}"#;
        let now = 1_700_000_000;
        let mut signed = format!("{now}.").into_bytes();
        signed.extend_from_slice(body);
        let sig = format!(
            "t={now},v1={}",
            hex_encode(&hmac_sha256(b"whsec_test", &signed))
        );
        let good = headers(&[("stripe-signature", &sig)]);
        assert!(hook.verify(&good, body, now));
        assert!(!hook.verify(&good, body, now + MAX_SKEW_SECS + 1));
        assert!(!hook.verify(&good, br#"{"id":"evt_2"}"#, now));
    }

    #[test]
    fn only_exact_paths_are_registered() {
        let paths = &["/webhooks/stripe"];
        assert!(is_registered_in(paths, "/webhooks/stripe"));
        assert!(!is_registered_in(paths, "/webhooks/stripe/extra"));
        assert!(!is_registered_in(paths, "/webhooks"));
        assert!(!is_registered_in(paths, "/webhooks-evil"));
    }

    #[tokio::test]
    async fn the_middleware_rejects_bad_signatures_and_passes_the_raw_body() {
        use axum::{
            body::{Body, Bytes},
            http::Request,
        };
        use tower::ServiceExt;

        async fn echo(bytes: Bytes) -> String {
            format!("got {} bytes", bytes.len())
        }
        let app: Router<()> = Router::new().merge(route(
            "/webhooks/test",
            Webhook::github(b"s3cret".to_vec()),
            echo,
        ));
        let body = b"raw-body";
        let post = |headers: &[(&str, &str)]| {
            let mut b = Request::builder().method("POST").uri("/webhooks/test");
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            b.body(Body::from(&body[..])).unwrap()
        };
        let res = app.clone().oneshot(post(&[])).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let sig = format!("sha256={}", hex_encode(&hmac_sha256(b"s3cret", body)));
        let res = app
            .oneshot(post(&[("x-hub-signature-256", &sig)]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Every `webhook::route("…")` mounted in the router is in `REGISTERED_PATHS`,
    /// and every registered path is mounted.
    #[test]
    fn registered_paths_match_the_mounted_webhook_routes() {
        let source = std::fs::read_to_string("src/routes.rs").expect("read routes");
        let mut mounted: Vec<&str> = Vec::new();
        for (at, _) in source.match_indices("webhook::route(\"") {
            let rest = &source[at + "webhook::route(\"".len()..];
            mounted.push(&rest[..rest.find('"').expect("closing quote")]);
        }
        assert_eq!(
            mounted, REGISTERED_PATHS,
            "REGISTERED_PATHS must list exactly the webhook::route mounts in src/routes.rs"
        );
    }
}
