//! Request URIs as they may appear in logs: secrets in the path or the query are replaced with
//! `[redacted]`. Emailed links carry their token in the path (`/login_links/<token>`, reset and
//! verification links), and development logs every request URI (TraceLayer at debug), so without
//! this a token would sit in a log file, a terminal scrollback or a log shipper.
//!
//! Two rules: the route patterns in `TOKEN_ROUTES` (exact, a `{token}` segment is redacted), and a
//! generic one for any long random-looking segment or query value, for routes nobody listed.
//! GUIDE: .offrails/guides/errors-and-logging.md

use axum::http::Uri;
use tracing_subscriber::EnvFilter;

/// What a secret becomes in a log line.
pub const REDACTED: &str = "[redacted]";

/// APP: routes whose `{token}` segment is a secret (emailed links, one-time links). Other `{…}`
/// segments match anything and stay. Samples add theirs at the anchor.
pub const TOKEN_ROUTES: &[&str] = &[
    // samples: token routes
];

/// APP: query parameters whose value is always a secret, whatever it looks like.
pub const SECRET_PARAMS: &[&str] = &[
    "token",
    "code",
    "key",
    "secret",
    "password",
    "signature",
    "sig",
    "otp",
];

/// A segment at least this long that looks random (base64url or hex, with a digit or a capital) is
/// treated as a token even on a route nobody listed.
const RANDOM_MIN_LEN: usize = 20;

/// Targets that log a raw request URI at some level, each capped just below it. `RUST_LOG=trace`
/// can't lift the cap: these directives are added after the user's and win for their target.
// WHY: tower_http's CSRF layer traces `uri=<path>` for every request it passes.
const RAW_URI_TARGETS: &[&str] = &["tower_http::csrf=debug"];

/// The app's log filter: `directives` (RUST_LOG, or the environment's default) with the raw-URI
/// targets capped. `main.rs` builds the subscriber with it.
// SECURITY: no log line, at any level, carries an unredacted request URI
// (test: tokens_in_request_uris_never_reach_the_logs).
pub fn log_filter(directives: &str) -> EnvFilter {
    RAW_URI_TARGETS
        .iter()
        .filter_map(|d| d.parse().ok())
        .fold(EnvFilter::new(directives), EnvFilter::add_directive)
}

/// `uri` with every token-like path segment and secret query value replaced by `[redacted]`.
// SECURITY: tokens from emailed links never reach the logs, at any level: the request span and
// the 5xx log line both go through here (test: tokens_in_request_uris_never_reach_the_logs).
pub fn uri(uri: &Uri) -> String {
    let mut out = path(uri.path());
    if let Some(q) = uri.query() {
        out.push('?');
        out.push_str(&query(q));
    }
    out
}

/// The path with `TOKEN_ROUTES` tokens and random-looking segments redacted.
pub fn path(path: &str) -> String {
    path_with(path, TOKEN_ROUTES)
}

fn path_with(path: &str, routes: &[&str]) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let listed = routes.iter().find_map(|route| {
        let pattern: Vec<&str> = route.split('/').collect();
        let fits = pattern.len() == segments.len()
            && pattern
                .iter()
                .zip(&segments)
                .all(|(p, s)| (p.starts_with('{') && p.ends_with('}') && !s.is_empty()) || p == s);
        fits.then_some(pattern)
    });
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let token_slot = listed.as_ref().is_some_and(|p| p[i] == "{token}");
            if token_slot || looks_random(s) {
                REDACTED
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Free text (an error message, a job's failure reason) with secrets replaced by `[redacted]`:
/// a URL's `user:password@`, its token path segments and secret query values; `key=value` pairs
/// whose key names a secret (`password=…`, `api_key=…`); and long random-looking words (tokens).
/// For text that is stored or shown after the fact, where `uri` can't be applied up front.
// SECURITY: errors often quote the URL that failed, with its credentials; stored or shown error
// text goes through here first (test: secrets_in_free_text_are_redacted).
pub fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word = String::new();
    let mut after_bearer = false;
    for c in s.chars() {
        if c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '`'
            )
        {
            if after_bearer && !word.is_empty() {
                out.push_str(REDACTED);
            } else {
                out.push_str(&word_redacted(&word));
            }
            after_bearer = word.eq_ignore_ascii_case("bearer");
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    if after_bearer && !word.is_empty() {
        out.push_str(REDACTED);
    } else {
        out.push_str(&word_redacted(&word));
    }
    out
}

fn word_redacted(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    if let Some((scheme, rest)) = word.split_once("://") {
        let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(split);
        let host = match authority.rsplit_once('@') {
            Some((_, host)) => format!("{REDACTED}@{host}"),
            None => authority.to_owned(),
        };
        let (before_fragment, fragment) = match tail.split_once('#') {
            Some((b, f)) => (b, Some(f)),
            None => (tail, None),
        };
        let (p, q) = match before_fragment.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (before_fragment, None),
        };
        let mut out = format!(
            "{scheme}://{host}{}",
            if p.is_empty() { String::new() } else { path(p) }
        );
        if let Some(q) = q {
            out.push('?');
            out.push_str(&query(q));
        }
        if let Some(f) = fragment {
            out.push('#');
            out.push_str(&query(f));
        }
        return out;
    }
    if let Some((k, v)) = word.split_once(['=', ':'])
        && !k.is_empty()
        && !v.is_empty()
        && (is_secret_param(k.trim_start_matches(['-', '?', '&'])) || looks_random(v))
    {
        let sep = &word[k.len()..k.len() + 1];
        return format!("{k}{sep}{REDACTED}");
    }
    if looks_random(word.trim_end_matches(['.', ':'])) {
        return REDACTED.to_owned();
    }
    word.to_owned()
}

fn query(q: &str) -> String {
    q.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) if is_secret_param(k) || looks_random(v) => format!("{k}={REDACTED}"),
            Some(_) => pair.to_owned(),
            None if looks_random(pair) => REDACTED.to_owned(),
            None => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn is_secret_param(name: &str) -> bool {
    const CONCEPTS: &[&str] = &[
        "token",
        "key",
        "secret",
        "password",
        "passphrase",
        "passcode",
        "passwd",
        "pwd",
        "credential",
        "credentials",
        "signature",
        "sig",
        "otp",
        "code",
        "session",
        "auth",
        "bearer",
        "private",
    ];
    let mut normalized = String::with_capacity(name.len() + 4);
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if c == '-' || c == ' ' || c == '.' {
            normalized.push('_');
        } else if c.is_ascii_uppercase() {
            if prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
                normalized.push('_');
            }
            normalized.push(c.to_ascii_lowercase());
        } else {
            normalized.push(c.to_ascii_lowercase());
        }
        prev = Some(c);
    }
    if SECRET_PARAMS.iter().any(|p| normalized == *p) {
        return true;
    }
    normalized
        .split('_')
        .filter(|t| !t.is_empty())
        .any(|t| CONCEPTS.contains(&t))
}

/// Long, URL-safe, and not a lowercase slug: base64url tokens, hex digests, signed blobs.
fn looks_random(s: &str) -> bool {
    s.len() >= RANDOM_MIN_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'%'))
        && s.bytes()
            .any(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "q8Zr2mX0kLwP4vN7tY1sB9cD3fG6hJ5aE2uI8oQ4rTw";

    #[test]
    fn listed_routes_redact_their_token_segment_only() {
        let routes = &[
            "/login_links/{token}",
            "/passwords/{token}/edit",
            "/a/{id}/{token}",
        ];
        assert_eq!(
            path_with("/login_links/abc", routes),
            "/login_links/[redacted]"
        );
        assert_eq!(
            path_with("/passwords/short/edit", routes),
            "/passwords/[redacted]/edit"
        );
        assert_eq!(path_with("/a/42/xyz", routes), "/a/42/[redacted]");
        // Other routes and other lengths are left alone.
        assert_eq!(
            path_with("/login_links/sent/x", routes),
            "/login_links/sent/x"
        );
        assert_eq!(path_with("/passwords/new", routes), "/passwords/new");
        assert_eq!(path_with("/", routes), "/");
    }

    #[test]
    fn long_random_segments_are_redacted_anywhere() {
        assert_eq!(path(&format!("/whatever/{TOKEN}")), "/whatever/[redacted]");
        assert_eq!(
            path("/files/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
            "/files/[redacted]"
        );
        // Readable paths stay readable.
        for keep in [
            "/dev/mailbox/42",
            "/blog/a-very-long-lowercase-article-slug",
            "/admin/jobs/errors",
            "/live/lobby",
        ] {
            assert_eq!(path(keep), keep);
        }
    }

    #[test]
    fn secrets_in_free_text_are_redacted() {
        let shown = text(&format!(
            "GET https://deploy:hunter2@api.example.com/v1/hooks?token=abc123&page=2 failed: 401 (key {TOKEN}); retry with password=swordfish"
        ));
        for secret in ["deploy", "hunter2", "abc123", TOKEN, "swordfish"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert_eq!(
            shown,
            "GET https://[redacted]@api.example.com/v1/hooks?token=[redacted]&page=2 failed: 401 (key [redacted]); retry with password=[redacted]"
        );
        // Ordinary text is untouched, and redacting twice changes nothing.
        let plain =
            "charging card 42: connection refused (os error 61) at hooks.partner.invalid:443";
        assert_eq!(text(plain), plain);
        assert_eq!(text(&shown), shown);
        assert_eq!(
            text("postgres://u:p@db/app"),
            "postgres://[redacted]@db/app"
        );
    }

    #[test]
    fn secret_named_values_are_redacted_however_they_are_spelled() {
        for (line, secret) in [
            ("api-key=shortval", "shortval"),
            ("apiKey=shortval", "shortval"),
            ("access-token=shortval", "shortval"),
            ("authToken=shortval", "shortval"),
            ("clientSecret=shortval", "shortval"),
            ("passwd=shortval", "shortval"),
            ("credential=shortval", "shortval"),
        ] {
            let shown = text(&format!("connect failed: {line}"));
            assert!(!shown.contains(secret), "{secret} leaked in {shown:?}");
            assert!(shown.contains(REDACTED), "no redaction in {shown:?}");
        }
        let u: Uri = "/x?api-key=shortval&apiKey=shortval&page=2"
            .parse()
            .unwrap();
        let shown = uri(&u);
        assert!(!shown.contains("shortval"), "leaked in {shown:?}");
        assert!(shown.contains("page=2"), "plain params stay: {shown:?}");
    }

    #[test]
    fn bearer_credentials_and_plain_lowercase_tokens_are_redacted() {
        let shown = text("Authorization: Bearer abc123short retrying");
        assert!(!shown.contains("abc123short"), "leaked in {shown:?}");
        let shown = text("session=abcdefghijklmnopqrstuvwx");
        assert!(
            !shown.contains("abcdefghijklmnopqrstuvwx"),
            "leaked in {shown:?}"
        );
    }

    #[test]
    fn secret_query_values_are_redacted() {
        let u: Uri = format!("/x?token=abc&page=2&reset_token=t&q=hello&state={TOKEN}")
            .parse()
            .unwrap();
        assert_eq!(
            uri(&u),
            "/x?token=[redacted]&page=2&reset_token=[redacted]&q=hello&state=[redacted]"
        );
        let u: Uri = "/plain".parse().unwrap();
        assert_eq!(uri(&u), "/plain");
    }
}
