//! The security header matrix, `no-store` for authenticated responses and production cookie
//! hardening, in one middleware (guides/security.md#the-header-matrix).
//!
//! It sits outside `CatchPanicLayer` and the timeout, so panic 500s and 503s carry the headers
//! too. It overwrites the matrix headers, except on sandboxed document responses (the mailbox
//! viewer's own CSP), whose policy it keeps.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, header},
    middleware::Next,
    response::Response,
};

use crate::{
    config::Config,
    kernel::{cookie, theme},
};

/// The session cookie's name in development and in production (`__Host-`: Secure, Path=/, no
/// Domain). Defined here because the header matrix needs it before accounts exist; the
/// authentication sample's `kernel::session` uses the same names.
pub const SESSION_COOKIE_DEV: &str = "session_token";
pub const SESSION_COOKIE_PROD: &str = "__Host-session_token";

fn session_cookie(production: bool) -> &'static str {
    if production {
        SESSION_COOKIE_PROD
    } else {
        SESSION_COOKIE_DEV
    }
}

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   font-src 'self'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; \
                   base-uri 'self'; form-action 'self'";
const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=(), \
                                  serial=(), bluetooth=(), hid=(), midi=(), browsing-topics=()";
const HSTS: &str = "max-age=63072000; includeSubDomains";

/// Directives a sample adds to the CSP in development only (a violation report endpoint:
/// `"report-uri /dev/csp-reports"`). Production never gets them.
pub const DEV_CSP_EXTRA: &[&str] = &[
    // samples: dev csp
];

/// What the middleware needs from the config, computed once.
#[derive(Clone)]
pub struct Hardening {
    csp: HeaderValue,
    hsts: HeaderValue,
    production: bool,
}

impl Hardening {
    /// TLS-only directives (HSTS, `upgrade-insecure-requests`) key off `APP_URL` being https in
    /// production: a production build run locally over http must not upgrade its redirects.
    pub fn new(config: &Config) -> Self {
        let tls = config.is_production() && config.app_url.starts_with("https://");
        if config.is_production() && !tls && !is_local(&config.app_url) {
            tracing::warn!(
                app_url = %config.app_url,
                "APP_URL is not https: HSTS and upgrade-insecure-requests are off"
            );
        }
        let mut csp = if tls {
            format!("{CSP}; upgrade-insecure-requests")
        } else {
            CSP.to_owned()
        };
        if config.is_development() {
            for directive in DEV_CSP_EXTRA {
                csp.push_str("; ");
                csp.push_str(directive);
            }
        }
        Self {
            csp: HeaderValue::from_str(&csp).unwrap_or(HeaderValue::from_static(CSP)),
            hsts: HeaderValue::from_static(if tls { HSTS } else { "max-age=0" }),
            production: config.is_production(),
        }
    }
}

fn is_local(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

pub async fn middleware(State(h): State<Hardening>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let had_session = cookie::get(req.headers(), session_cookie(h.production)).is_some();
    let mut response = next.run(req).await;

    let headers = response.headers_mut();
    let sandboxed = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("sandbox") && v.contains("default-src 'none'"));
    let stricter_referrer = headers
        .get(header::REFERRER_POLICY)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v, "no-referrer" | "same-origin" | "strict-origin"));
    if sandboxed {
        fill(headers, header::CONTENT_SECURITY_POLICY, h.csp.clone());
        fill(
            headers,
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        );
    } else {
        headers.insert(header::CONTENT_SECURITY_POLICY, h.csp.clone());
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    if !sandboxed && !stricter_referrer {
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        );
    } else {
        fill(
            headers,
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        );
    }
    headers.insert(header::STRICT_TRANSPORT_SECURITY, h.hsts.clone());
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );

    // Private data never lands in a shared or back/forward cache: every response to a request
    // with a session cookie (or that starts a session), and every kernel page.
    let starts_session = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|c| c.starts_with(&format!("{}=", session_cookie(h.production))) && !is_clearing(c));
    if (had_session || starts_session || is_kernel_path(&path)) && !is_public_file(&path) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }

    if h.production {
        harden_cookies(headers);
    }
    response
}

fn fill(headers: &mut HeaderMap, name: HeaderName, value: HeaderValue) {
    headers.entry(name).or_insert(value);
}

fn is_clearing(set_cookie: &str) -> bool {
    set_cookie.contains("Max-Age=0")
}

fn is_kernel_path(path: &str) -> bool {
    ["/dev", "/admin"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

fn is_public_file(path: &str) -> bool {
    path.starts_with("/static/") || matches!(path, "/favicon.ico" | "/robots.txt" | "/sitemap.xml")
}

/// Cookies a script must be able to write, so the backstop never makes them HttpOnly. Each one
/// holds a preference, never a secret.
// SECURITY: an allowlist by exact name; every other production cookie still gets HttpOnly
// (test: the_theme_cookie_stays_script_writable_in_production).
const SCRIPT_WRITABLE_COOKIES: &[&str] = &[theme::COOKIE];

/// A backstop for cookies built without config (the flash): production cookies are always
/// `Secure; HttpOnly; SameSite` (`Secure; SameSite` for `SCRIPT_WRITABLE_COOKIES`).
fn harden_cookies(headers: &mut HeaderMap) {
    let hardened: Vec<HeaderValue> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| {
            let Ok(s) = v.to_str() else { return v.clone() };
            let lower = s.to_ascii_lowercase();
            let mut out = s.to_owned();
            if !lower.contains("; secure") {
                out.push_str("; Secure");
            }
            let script_writable = SCRIPT_WRITABLE_COOKIES
                .iter()
                .any(|name| s.starts_with(&format!("{name}=")));
            if !lower.contains("; httponly") && !script_writable {
                out.push_str("; HttpOnly");
            }
            if !lower.contains("; samesite=") {
                out.push_str("; SameSite=Lax");
            }
            HeaderValue::from_str(&out).unwrap_or_else(|_| v.clone())
        })
        .collect();
    headers.remove(header::SET_COOKIE);
    for v in hardened {
        headers.append(header::SET_COOKIE, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_cookies_gain_the_missing_flags() {
        let mut h = HeaderMap::new();
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static("_flash=x; Path=/"),
        );
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static("a=b; Path=/; HttpOnly; SameSite=Lax; Secure"),
        );
        harden_cookies(&mut h);
        let all: Vec<_> = h.get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(all[0], "_flash=x; Path=/; Secure; HttpOnly; SameSite=Lax");
        assert_eq!(all[1], "a=b; Path=/; HttpOnly; SameSite=Lax; Secure");
    }

    /// The layer order `routes.rs` uses: the middleware outside the panic layer, so a 500 from a
    /// panic still carries the matrix.
    #[tokio::test]
    async fn a_panic_500_carries_the_headers() {
        use axum::{
            Router, body::Body, http::StatusCode, middleware::from_fn_with_state, routing::get,
        };
        use tower::ServiceExt;
        use tower_http::catch_panic::CatchPanicLayer;

        async fn boom() -> &'static str {
            panic!("boom")
        }
        let hardening = Hardening::new(&crate::config::Config::test());
        let app = Router::new()
            .route("/boom", get(boom))
            .layer(CatchPanicLayer::new())
            .layer(from_fn_with_state(hardening, middleware));
        let res = app
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert!(res.headers().contains_key("content-security-policy"));
    }

    #[tokio::test]
    async fn a_handler_cannot_weaken_the_csp() {
        use axum::{
            Router,
            body::Body,
            http::{StatusCode, header},
            middleware::from_fn_with_state,
            response::IntoResponse,
            routing::get,
        };
        use tower::ServiceExt;

        async fn weak() -> impl IntoResponse {
            ([(header::CONTENT_SECURITY_POLICY, "default-src *")], "ok")
        }
        let hardening = Hardening::new(&crate::config::Config::test());
        let app = Router::new()
            .route("/weak", get(weak))
            .layer(from_fn_with_state(hardening, middleware));
        let res = app
            .oneshot(Request::builder().uri("/weak").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let csp = res.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            csp.contains("default-src 'self'") && !csp.contains("default-src *"),
            "handler-supplied CSP weakened the matrix: {csp}"
        );
    }

    #[test]
    fn kernel_and_public_paths() {
        assert!(is_kernel_path("/dev") && is_kernel_path("/admin/jobs"));
        assert!(!is_kernel_path("/devices") && !is_kernel_path("/books"));
        assert!(is_public_file("/static/app.css") && is_public_file("/robots.txt"));
        assert!(!is_public_file("/books/1"));
    }
}
