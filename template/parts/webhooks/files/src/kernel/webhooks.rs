//! Inbound webhook signatures: one `Provider` per scheme, each with the HMAC the provider
//! documents. Pure functions over headers and bytes (no database, no `AppState`), so every rule
//! has a unit test here and the controller only orders the steps.
//! GUIDE: .offrails/guides/security.md#csrf
//!
//! Adding a provider (a third scheme) is a variant, its place in `ALL`, and one arm in each of
//! `slug`, `verify`, `event` and `sign` below, plus its secret in `.env.example`:
//!
//! 1. `slug`: the URL segment (`/webhooks/<slug>`), and the env var is derived from it.
//! 2. `verify`: the provider's documented signature over the raw bytes, compared in constant
//!    time, with the timestamp window when the scheme has a timestamp.
//! 3. `event`: where the event id and type live (a header, or the JSON).
//! 4. `sign`: the same signature, for tests and for the README's curl helper.

use axum::http::HeaderMap;
use constant_time_eq::constant_time_eq;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// APP: how old a signed timestamp may be (Stripe's own default is 5 minutes). Older is
/// refused: a captured delivery can't be replayed later.
pub const TOLERANCE_SECS: i64 = 300;
/// APP: how far in the future a timestamp may be, for clock skew between the sender and us.
pub const FUTURE_SKEW_SECS: i64 = 60;

/// The longest event id or type we store (ids are `evt_...` or a GUID; anything longer is not one).
const MAX_ID_LEN: usize = 128;

/// Which scheme a `/webhooks/<slug>` request is checked with.
// APP: add a variant per provider (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// `X-Hub-Signature-256: sha256=<hex>` over the raw body; no timestamp.
    GitHub,
    /// `Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>]` over `"{t}.{body}"`.
    Stripe,
}

/// Why a delivery was refused. The HTTP answer is the same for all of them (a bare 401, so the
/// sender learns nothing about which check failed); the log line says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    MissingHeader,
    MalformedHeader,
    /// The timestamp is older than `TOLERANCE_SECS`.
    Stale,
    /// The timestamp is more than `FUTURE_SKEW_SECS` ahead of our clock.
    FromTheFuture,
    BadSignature,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingHeader => "missing_header",
            Self::MalformedHeader => "malformed_header",
            Self::Stale => "stale_timestamp",
            Self::FromTheFuture => "future_timestamp",
            Self::BadSignature => "bad_signature",
        }
    }
}

/// A delivery whose signature checked out. The only way to get one is `Provider::verify`, and
/// the only way to read the body's meaning (`event`, `payload`) is through it: parsing before
/// verifying doesn't compile.
// SECURITY: nothing parses the body before the signature is verified; the parsing functions
// live on this type (test: malformed_json_with_a_valid_signature_is_stored_not_a_500).
pub struct Verified<'a> {
    provider: Provider,
    headers: &'a HeaderMap,
    body: &'a [u8],
}

// WHY: by hand, not derived: the headers hold the signature and the body may hold personal data,
// so a `?verified` in a log line shows neither.
impl std::fmt::Debug for Verified<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verified")
            .field("provider", &self.provider)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// What a verified delivery says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInfo {
    /// Unique per provider: what a replay repeats, so the dedupe key. It is always derived from
    /// signed bytes (Stripe's JSON `id`, or the body's SHA-256).
    pub id: String,
    /// The provider's delivery id when it is sent outside the signature (GitHub's
    /// `X-GitHub-Delivery`): kept for display and support requests, never used to dedupe.
    pub delivery: Option<String>,
    pub kind: String,
}

impl Provider {
    /// Every provider, in the order the config reads them.
    pub const ALL: [Provider; 2] = [Provider::GitHub, Provider::Stripe];

    pub fn slug(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Stripe => "stripe",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.slug() == slug)
    }

    /// `APP_WEBHOOK_GITHUB_SECRET`: the provider's signing secret. Unset means the route doesn't
    /// exist.
    pub fn env_var(self) -> String {
        format!("APP_WEBHOOK_{}_SECRET", self.slug().to_ascii_uppercase())
    }

    /// Checks the provider's signature over the raw `body`. `now` is unix seconds.
    // SECURITY: the signature is the only authentication on /webhooks/...: the HMAC the provider
    // documents, keyed with the configured secret, compared with constant_time_eq, never `==`
    // (test: a_one_bit_different_signature_is_rejected).
    pub fn verify<'a>(
        self,
        secret: &[u8],
        headers: &'a HeaderMap,
        body: &'a [u8],
        now: i64,
    ) -> Result<Verified<'a>, Rejection> {
        match self {
            Self::GitHub => verify_github(secret, headers, body)?,
            Self::Stripe => verify_stripe(secret, headers, body, now)?,
        }
        Ok(Verified {
            provider: self,
            headers,
            body,
        })
    }

    /// The signature header a sender would add for `body`, as `(name, value)`. Tests and the
    /// README's examples use it; `at` is unix seconds and only schemes with a timestamp use it.
    pub fn sign(self, secret: &[u8], body: &[u8], at: i64) -> (&'static str, String) {
        match self {
            Self::GitHub => {
                let mac = hmac_sha256(secret, &[body]).unwrap_or_default();
                (GITHUB_SIGNATURE, format!("sha256={}", to_hex(&mac)))
            }
            Self::Stripe => {
                let stamp = at.to_string();
                let mac = hmac_sha256(secret, &[stamp.as_bytes(), b".", body]).unwrap_or_default();
                (STRIPE_SIGNATURE, format!("t={stamp},v1={}", to_hex(&mac)))
            }
        }
    }
}

impl Verified<'_> {
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The event's id and type. Never fails: a body that isn't the JSON we expect still has an id
    /// (its SHA-256, so a replay of the same bytes is still a duplicate) and the type `unknown`.
    pub fn event(&self) -> EventInfo {
        let body_id = || format!("sha256-{}", to_hex(&Sha256::digest(self.body)));
        match self.provider {
            // SECURITY: GitHub signs the body only, so `X-GitHub-Delivery` is not proof of
            // anything: dedupe on the body's hash, or a captured body resent with a new GUID would
            // be stored and handled again. GitHub's redeliveries resend the same bytes, so they
            // still dedupe (test: a_github_body_resent_with_a_new_delivery_id_is_a_duplicate).
            // `X-GitHub-Event` is unsigned too: handlers check the payload, not the type alone.
            Provider::GitHub => EventInfo {
                id: body_id(),
                delivery: header_token(self.headers, GITHUB_DELIVERY),
                kind: header_token(self.headers, GITHUB_EVENT)
                    .unwrap_or_else(|| UNKNOWN.to_owned()),
            },
            Provider::Stripe => {
                // WHY: parsed only now, after the signature: `Verified` is the proof.
                let json: Value = serde_json::from_slice(self.body).unwrap_or(Value::Null);
                EventInfo {
                    id: json_token(&json, "id").unwrap_or_else(body_id),
                    delivery: None,
                    kind: json_token(&json, "type").unwrap_or_else(|| UNKNOWN.to_owned()),
                }
            }
        }
    }

    /// The body as stored: text. Invalid UTF-8 can't be JSON, and is replaced, not rejected, so a
    /// verified delivery is always recorded. A NUL is replaced too: Postgres text can't hold one.
    pub fn payload(&self) -> String {
        String::from_utf8_lossy(self.body).replace('\0', "\u{FFFD}")
    }
}

const UNKNOWN: &str = "unknown";
const GITHUB_SIGNATURE: &str = "x-hub-signature-256";
const GITHUB_DELIVERY: &str = "x-github-delivery";
const GITHUB_EVENT: &str = "x-github-event";
const STRIPE_SIGNATURE: &str = "stripe-signature";

fn verify_github(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> Result<(), Rejection> {
    let value = headers
        .get(GITHUB_SIGNATURE)
        .ok_or(Rejection::MissingHeader)?
        .to_str()
        .map_err(|_| Rejection::MalformedHeader)?;
    let sent = value
        .strip_prefix("sha256=")
        .and_then(from_hex)
        .ok_or(Rejection::MalformedHeader)?;
    let expected = hmac_sha256(secret, &[body]).ok_or(Rejection::BadSignature)?;
    if constant_time_eq(&expected, &sent) {
        Ok(())
    } else {
        Err(Rejection::BadSignature)
    }
}

fn verify_stripe(
    secret: &[u8],
    headers: &HeaderMap,
    body: &[u8],
    now: i64,
) -> Result<(), Rejection> {
    let value = headers
        .get(STRIPE_SIGNATURE)
        .ok_or(Rejection::MissingHeader)?
        .to_str()
        .map_err(|_| Rejection::MalformedHeader)?;
    let (raw_stamp, stamp, candidates) =
        parse_stripe_header(value).ok_or(Rejection::MalformedHeader)?;
    // SECURITY: a timestamp outside the window is refused before any HMAC work, so a captured
    // delivery can't be replayed after 5 minutes, and a forged "future" one doesn't stay valid
    // (test: a_stale_timestamp_is_401, a_timestamp_from_the_future_is_401).
    if now.saturating_sub(stamp) > TOLERANCE_SECS {
        return Err(Rejection::Stale);
    }
    if stamp.saturating_sub(now) > FUTURE_SKEW_SECS {
        return Err(Rejection::FromTheFuture);
    }
    // WHY: the HMAC covers the `t` exactly as sent, like Stripe's own libraries.
    let expected =
        hmac_sha256(secret, &[raw_stamp.as_bytes(), b".", body]).ok_or(Rejection::BadSignature)?;
    // Any `v1` may match (the sender signs with two secrets while one is being rolled). Every
    // candidate is compared, none short-circuits.
    let mut matched = false;
    for candidate in candidates {
        matched |= constant_time_eq(&expected, &candidate);
    }
    if matched {
        Ok(())
    } else {
        Err(Rejection::BadSignature)
    }
}

/// `t=<unix>,v1=<hex>[,v1=<hex>]` -> the timestamp (as sent, and as a number) and every decodable
/// `v1` signature. Other schemes (`v0`) are ignored; a missing, repeated or non-digit `t` (a sign,
/// a space, more than 18 digits), or no `v1` at all, is malformed.
fn parse_stripe_header(value: &str) -> Option<(&str, i64, Vec<Vec<u8>>)> {
    let mut stamp: Option<(&str, i64)> = None;
    let mut signatures = Vec::new();
    let mut v1_seen = 0;
    for part in value.split(',') {
        let (key, val) = part.trim().split_once('=')?;
        match key {
            "t" => {
                if stamp.is_some() {
                    return None;
                }
                // WHY: digits only, at most 18 of them: `parse` alone would take `+5` and `-5`,
                // and 18 digits always fit an i64, so nothing below can overflow.
                if val.is_empty() || val.len() > 18 || !val.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                stamp = Some((val, val.parse::<i64>().ok()?));
            }
            "v1" => {
                v1_seen += 1;
                // WHY: a v1 that isn't hex can't match; it is skipped, not fatal, so one bad
                // entry doesn't hide a good one.
                if let Some(bytes) = from_hex(val) {
                    signatures.push(bytes);
                }
            }
            _ => {}
        }
    }
    let (raw, stamp) = stamp?;
    (v1_seen > 0).then_some((raw, stamp, signatures))
}

fn hmac_sha256(secret: &[u8], parts: &[&[u8]]) -> Option<[u8; 32]> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).ok()?;
    for part in parts {
        mac.update(part);
    }
    Some(mac.finalize().into_bytes().into())
}

/// A header value that is a plain token (ids, event names): ASCII letters, digits and `._:-`.
fn header_token(headers: &HeaderMap, name: &str) -> Option<String> {
    token(headers.get(name)?.to_str().ok()?)
}

fn json_token(json: &Value, key: &str) -> Option<String> {
    token(json.get(key)?.as_str()?)
}

fn token(raw: &str) -> Option<String> {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_ID_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    ok.then(|| raw.to_owned())
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(raw: &str) -> Option<Vec<u8>> {
    if raw.is_empty() || !raw.len().is_multiple_of(2) || !raw.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    raw.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    // GitHub's documented example (docs.github.com, "Validating webhook deliveries").
    const GITHUB_SECRET: &[u8] = b"It's a Secret to Everybody";
    const GITHUB_BODY: &[u8] = b"Hello, World!";
    const GITHUB_SIGNATURE_HEX: &str =
        "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).expect("a header value"));
        }
        map
    }

    #[test]
    fn githubs_documented_example_verifies() {
        let signature = format!("sha256={GITHUB_SIGNATURE_HEX}");
        let map = headers(&[(GITHUB_SIGNATURE, &signature)]);
        assert!(
            Provider::GitHub
                .verify(GITHUB_SECRET, &map, GITHUB_BODY, 0)
                .is_ok()
        );
        // And `sign` produces exactly that header.
        assert_eq!(
            Provider::GitHub.sign(GITHUB_SECRET, GITHUB_BODY, 0),
            (GITHUB_SIGNATURE, signature)
        );
    }

    #[test]
    fn a_one_bit_different_signature_is_rejected() {
        // Flip each of the 256 bits of the right signature in turn: every one is refused.
        let right = from_hex(GITHUB_SIGNATURE_HEX).expect("hex");
        for bit in 0..right.len() * 8 {
            let mut wrong = right.clone();
            wrong[bit / 8] ^= 1 << (bit % 8);
            let map = headers(&[(GITHUB_SIGNATURE, &format!("sha256={}", to_hex(&wrong)))]);
            assert_eq!(
                Provider::GitHub
                    .verify(GITHUB_SECRET, &map, GITHUB_BODY, 0)
                    .err(),
                Some(Rejection::BadSignature),
                "bit {bit} was accepted"
            );
        }
        // The same for the Stripe scheme.
        let (name, good) = Provider::Stripe.sign(b"whsec_test_secret_0123456789", b"{}", 1_000);
        let (stamp, _, sigs) = parse_stripe_header(&good).expect("parses");
        let mut wrong = sigs[0].clone();
        wrong[31] ^= 1;
        let map = headers(&[(name, &format!("t={stamp},v1={}", to_hex(&wrong)))]);
        assert_eq!(
            Provider::Stripe
                .verify(b"whsec_test_secret_0123456789", &map, b"{}", 1_000)
                .err(),
            Some(Rejection::BadSignature)
        );
    }

    #[test]
    fn signatures_are_compared_with_constant_time_eq_never_equals() {
        let source = include_str!("webhooks.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("the code before the tests");
        assert!(code.contains("constant_time_eq(&expected"));
        for line in code.lines().filter(|l| !l.trim_start().starts_with("//")) {
            assert!(
                !(line.contains("expected ==") || line.contains("== expected")),
                "a signature compared with ==: {line}"
            );
        }
    }

    #[test]
    fn stripe_signs_the_timestamp_a_dot_and_the_raw_body() {
        // Computed independently: printf '1700000000.{"id":"evt_1"}' | openssl dgst -sha256 -hmac ...
        let (_, header) = Provider::Stripe.sign(
            b"whsec_test_secret_0123456789",
            br#"{"id":"evt_1"}"#,
            1_700_000_000,
        );
        assert_eq!(
            header,
            "t=1700000000,v1=7e9a82a20098cf01f04d9c09f5a7459669bfecf51392ce3adb1445906fceff61"
        );
    }

    #[test]
    fn stripe_accepts_any_v1_and_ignores_other_schemes() {
        let secret = b"whsec_test_secret_0123456789";
        let (name, good) = Provider::Stripe.sign(secret, b"{}", 5_000);
        let v1 = good.split("v1=").nth(1).expect("v1");
        let both = format!("t=5000,v1={},v0=abc,v1={v1}", "00".repeat(32));
        let map = headers(&[(name, &both)]);
        assert!(Provider::Stripe.verify(secret, &map, b"{}", 5_000).is_ok());
    }

    #[test]
    fn stripe_headers_without_a_timestamp_or_v1_are_malformed() {
        let secret = b"whsec_test_secret_0123456789";
        for value in [
            "",
            "garbage",
            "v1=abcd",
            "t=100",
            "t=abc,v1=abcd",
            "t=+100,v1=abcd",
            "t=-100,v1=abcd",
            "t= 100,v1=abcd",
            "t=1234567890123456789,v1=abcd",
            "t=100,t=101,v1=abcd",
        ] {
            let map = headers(&[(STRIPE_SIGNATURE, value)]);
            assert_eq!(
                Provider::Stripe.verify(secret, &map, b"{}", 100).err(),
                Some(Rejection::MalformedHeader),
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_timestamp_window_is_five_minutes_back_and_a_minute_ahead() {
        let secret = b"whsec_test_secret_0123456789";
        let now = 1_700_000_000;
        let check = |at: i64| {
            let (name, value) = Provider::Stripe.sign(secret, b"{}", at);
            let map = headers(&[(name, &value)]);
            Provider::Stripe.verify(secret, &map, b"{}", now).err()
        };
        assert_eq!(check(now - TOLERANCE_SECS), None);
        assert_eq!(check(now - TOLERANCE_SECS - 1), Some(Rejection::Stale));
        assert_eq!(check(now + FUTURE_SKEW_SECS), None);
        assert_eq!(
            check(now + FUTURE_SKEW_SECS + 1),
            Some(Rejection::FromTheFuture)
        );
        assert_eq!(check(0), Some(Rejection::Stale));
        assert_eq!(
            check(999_999_999_999_999_999),
            Some(Rejection::FromTheFuture)
        );
        // A negative or 19-digit `t` never reaches the arithmetic.
        assert_eq!(check(-1), Some(Rejection::MalformedHeader));
        assert_eq!(check(i64::MIN), Some(Rejection::MalformedHeader));
        assert_eq!(check(i64::MAX), Some(Rejection::MalformedHeader));
    }

    #[test]
    fn ids_and_types_are_plain_tokens_or_fall_back() {
        let secret = b"whsec_test_secret_0123456789";
        let body = br#"{"id":"evt_123","type":"invoice.paid"}"#;
        let (name, value) = Provider::Stripe.sign(secret, body, 10);
        let map = headers(&[(name, &value)]);
        let verified = Provider::Stripe.verify(secret, &map, body, 10).expect("ok");
        assert_eq!(
            verified.event(),
            EventInfo {
                id: "evt_123".into(),
                delivery: None,
                kind: "invoice.paid".into()
            }
        );

        // Not JSON, or an id that isn't a token: the body's hash stands in, the type is unknown.
        for body in [&b"not json"[..], br#"{"id":"a b<script>","type":7}"#] {
            let (name, value) = Provider::Stripe.sign(secret, body, 10);
            let map = headers(&[(name, &value)]);
            let event = Provider::Stripe
                .verify(secret, &map, body, 10)
                .expect("ok")
                .event();
            assert!(event.id.starts_with("sha256-") && event.id.len() == 7 + 64);
            assert_eq!(event.kind, "unknown");
        }

        // GitHub: the id is the body's hash whatever the unsigned headers say; the delivery GUID
        // is kept beside it when it is a plain token.
        let (name, value) = Provider::GitHub.sign(GITHUB_SECRET, GITHUB_BODY, 0);
        let event = |delivery: &str| {
            let map = headers(&[
                (name, &value),
                (GITHUB_DELIVERY, delivery),
                (GITHUB_EVENT, "push"),
            ]);
            Provider::GitHub
                .verify(GITHUB_SECRET, &map, GITHUB_BODY, 0)
                .expect("ok")
                .event()
        };
        let first = event("72d3162e-cc78-11e3-81ab-4c9367dc0958");
        let second = event("another-guid");
        assert_eq!(first.id, second.id);
        assert_eq!(
            first.id,
            format!("sha256-{}", to_hex(&Sha256::digest(GITHUB_BODY)))
        );
        assert_eq!(
            first.delivery.as_deref(),
            Some("72d3162e-cc78-11e3-81ab-4c9367dc0958")
        );
        assert_eq!(event("a b<script>").delivery, None);
        assert_eq!(first.kind, "push");
    }

    #[test]
    fn the_stored_payload_has_no_nul_and_no_invalid_utf8() {
        let secret = b"whsec_test_secret_0123456789";
        let body = b"{\"a\":\"\xff\x00\"}";
        let (name, value) = Provider::Stripe.sign(secret, body, 10);
        let map = headers(&[(name, &value)]);
        let payload = Provider::Stripe
            .verify(secret, &map, body, 10)
            .expect("ok")
            .payload();
        assert_eq!(payload, "{\"a\":\"\u{FFFD}\u{FFFD}\"}");
    }

    #[test]
    fn verified_debug_shows_neither_the_signature_nor_the_body() {
        let (name, value) = Provider::GitHub.sign(GITHUB_SECRET, GITHUB_BODY, 0);
        let map = headers(&[(name, &value)]);
        let verified = Provider::GitHub
            .verify(GITHUB_SECRET, &map, GITHUB_BODY, 0)
            .expect("ok");
        let printed = format!("{verified:?}");
        assert!(!printed.contains(GITHUB_SIGNATURE_HEX), "{printed}");
        assert!(!printed.contains("Hello"), "{printed}");
        assert!(printed.contains("GitHub"), "{printed}");
    }

    #[test]
    fn slugs_round_trip_and_name_their_env_var() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_slug(provider.slug()), Some(provider));
        }
        assert_eq!(Provider::from_slug("GitHub"), None);
        assert_eq!(Provider::from_slug(""), None);
        assert_eq!(Provider::GitHub.env_var(), "APP_WEBHOOK_GITHUB_SECRET");
        assert_eq!(Provider::Stripe.env_var(), "APP_WEBHOOK_STRIPE_SECRET");
    }

    #[test]
    fn hex_decoding_is_strict() {
        assert_eq!(from_hex("00ff"), Some(vec![0, 255]));
        assert_eq!(from_hex("00FF"), Some(vec![0, 255]));
        for bad in ["", "0", "zz", "+1", "0x", "ééé"] {
            assert_eq!(from_hex(bad), None, "{bad:?}");
        }
    }
}
