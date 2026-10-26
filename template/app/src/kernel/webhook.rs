//! Inbound webhooks: signature verification as the route's registration, not a prefix.
//!
//! A webhook route exists only through [`route`], which mounts the handler behind a
//! middleware that verifies the provider's HMAC over the raw body before the handler
//! runs. The CSRF layer bypasses exactly the paths in [`REGISTERED_PATHS`], and every
//! path mounted with [`route`] must be listed there (a test keeps the two in sync).
//! Replays are the handler's job: store the provider's event id with
//! `on_conflict_do_nothing`, so a redelivery is a no-op (guides/security.md#csrf).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{error::AppError, kernel::crypto};
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
                let Some((raw_stamp, stamp, signatures)) = parse_timestamped(raw) else {
                    return false;
                };
                // SECURITY: checked arithmetic: a crafted `t` can't overflow the window check
                // (test: stripe_timestamps_are_digits_and_the_window_cannot_overflow)
                match now.checked_sub(stamp) {
                    Some(age) if age.abs() <= MAX_SKEW_SECS => {}
                    _ => return false,
                }
                // WHY: the HMAC covers `t` exactly as sent, like Stripe's own libraries.
                let mut signed = format!("{raw_stamp}.").into_bytes();
                signed.extend_from_slice(body);
                // Every candidate is compared; none short-circuits.
                signatures.iter().fold(false, |ok, sig| {
                    verify_hmac(&self.secret, &signed, sig) | ok
                })
            }
        }
    }
}

/// `t=<unix>,v1=<hex>[,v1=<hex>]`: the timestamp as sent and as a number, and every `v1`. A
/// missing or repeated `t`, or one that isn't 1 to 18 ASCII digits (a sign, a space), is refused.
fn parse_timestamped(raw: &str) -> Option<(&str, i64, Vec<&str>)> {
    let mut stamp: Option<(&str, i64)> = None;
    let mut signatures = Vec::new();
    for part in raw.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => {
                if stamp.is_some()
                    || v.is_empty()
                    || v.len() > 18
                    || !v.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                stamp = Some((v, v.parse().ok()?));
            }
            Some(("v1", v)) => signatures.push(v),
            _ => {}
        }
    }
    let (raw_stamp, stamp) = stamp?;
    (!signatures.is_empty()).then_some((raw_stamp, stamp, signatures))
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
    let path = parts.uri.path().to_owned();
    // SECURITY: at most MAX_BODY bytes are buffered, and only then is the signature checked
    // (test: the_middleware_rejects_bad_signatures_and_passes_the_raw_body)
    let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
        tracing::warn!(path, reason = "body_too_large", "webhook refused");
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    if !webhook.verify(&parts.headers, &bytes, now) {
        // One answer for every failed check; the log says which route, never the secret or
        // the signature.
        tracing::warn!(path, reason = "bad_signature", "webhook refused");
        return AppError::Unauthorized.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// HMAC-SHA256 over `data`, compared in constant time against a hex digest.
fn verify_hmac(secret: &[u8], data: &[u8], expected_hex: &str) -> bool {
    crypto::from_hex(expected_hex).is_some_and(|expected| {
        crypto::constant_time_eq(&crypto::hmac_sha256(secret, data), &expected)
    })
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
    fn a_valid_github_signature_passes_and_tampering_fails() {
        let hook = Webhook::github(b"s3cret".to_vec());
        let body = br#"{"action":"opened"}"#;
        let sig = format!(
            "sha256={}",
            crypto::hex(&crypto::hmac_sha256(b"s3cret", body))
        );
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
            crypto::hex(&crypto::hmac_sha256(b"whsec_test", &signed))
        );
        let good = headers(&[("stripe-signature", &sig)]);
        assert!(hook.verify(&good, body, now));
        assert!(!hook.verify(&good, body, now + MAX_SKEW_SECS + 1));
        assert!(!hook.verify(&good, br#"{"id":"evt_2"}"#, now));
    }

    #[test]
    fn stripe_timestamps_are_digits_and_the_window_cannot_overflow() {
        let hook = Webhook::stripe(b"whsec_test".to_vec());
        let body = b"{}";
        let sign = |t: &str| {
            let mut signed = format!("{t}.").into_bytes();
            signed.extend_from_slice(body);
            format!(
                "t={t},v1={}",
                crypto::hex(&crypto::hmac_sha256(b"whsec_test", &signed))
            )
        };
        let now = 1_700_000_000;
        // Signed over `t` as sent: a zero-padded stamp still verifies.
        assert!(hook.verify(
            &headers(&[("stripe-signature", &sign("01700000000"))]),
            body,
            now
        ));
        for t in [
            "-9223372036854775808",
            "+1700000000",
            " 1700000000",
            "1234567890123456789",
            "",
        ] {
            let h = headers(&[("stripe-signature", &sign(t))]);
            assert!(!hook.verify(&h, body, now), "{t:?} was accepted");
        }
        let twice = format!("{},t=1", sign("1700000000"));
        assert!(!hook.verify(&headers(&[("stripe-signature", &twice)]), body, now));
        // A far-past or far-future stamp at the edges of i64 never panics.
        assert!(!hook.verify(
            &headers(&[("stripe-signature", &sign("999999999999999999"))]),
            body,
            i64::MIN
        ));
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
        let sig = format!(
            "sha256={}",
            crypto::hex(&crypto::hmac_sha256(b"s3cret", body))
        );
        let res = app
            .oneshot(post(&[("x-hub-signature-256", &sig)]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Every `webhook::route("…")` mounted in the router (`src/routes.rs` and the feature route
    /// files in `src/routes/`) is in `REGISTERED_PATHS`, and every registered path is mounted.
    #[test]
    fn registered_paths_match_the_mounted_webhook_routes() {
        let mut files = vec![std::path::PathBuf::from("src/routes.rs")];
        if let Ok(dir) = std::fs::read_dir("src/routes") {
            files.extend(
                dir.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "rs")),
            );
        }
        let mut mounted: Vec<String> = Vec::new();
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read a routes file");
            for (at, _) in source.match_indices("webhook::route(\"") {
                let rest = &source[at + "webhook::route(\"".len()..];
                mounted.push(rest[..rest.find('"').expect("closing quote")].to_owned());
            }
        }
        mounted.sort();
        let mut registered: Vec<String> =
            REGISTERED_PATHS.iter().map(|p| (*p).to_owned()).collect();
        registered.sort();
        assert_eq!(
            mounted, registered,
            "REGISTERED_PATHS must list exactly the webhook::route mounts in src/routes.rs and src/routes/"
        );
    }
}
