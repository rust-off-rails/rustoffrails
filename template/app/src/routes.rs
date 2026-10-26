//! THE router: every URL the app serves, read top to bottom. (`config/routes.rb`)
//!
//! `htmx` mode: HTML forms only send GET and POST, so update is `POST /<things>/{id}` and
//! destroy is `POST /<things>/{id}/delete`.

use std::time::Duration;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderName, Method, StatusCode, header},
    middleware::{Next, from_fn, from_fn_with_state, map_response},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tower_http::{
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    csrf::CsrfLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::{ServeDir, ServeFile},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{
    controllers::{dev, errors, health, home, meta, migrations, theme},
    error::{self, AppError},
    kernel::{assets, flash, hosts, redact, security, webhook},
    state::AppState,
};

// Feature route files, one per feature (src/routes/<feature>.rs: its router, `paths::` helpers
// and `ROUTES` rows), each wired in by one line per spot, marked with the feature's name.
// samples: route mods

/// Who may call a route. The security suite requests every route as each actor
/// (`.offrails/guides/test-standards.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    User,
    Admin,
    Dev,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Public => "public",
            Access::User => "user",
            Access::Admin => "admin",
            Access::Dev => "dev",
        }
    }
}

async fn csrf_evidence(req: Request, next: Next) -> Response {
    let safe = matches!(
        req.method(),
        &Method::GET | &Method::HEAD | &Method::OPTIONS
    );
    let has_cookie = req.headers().contains_key(header::COOKIE);
    let has_evidence =
        req.headers().contains_key("sec-fetch-site") || req.headers().contains_key(header::ORIGIN);
    if safe || !has_cookie || has_evidence || webhook::is_registered(req.uri().path()) {
        next.run(req).await
    } else {
        AppError::Forbidden.into_response()
    }
}

pub fn router(state: AppState) -> anyhow::Result<Router> {
    let config = state.config.clone();
    #[allow(unused_mut)] // samples add routes below
    let mut app = Router::new()
        .route("/", get(home::index))
        .route("/up", get(health::up))
        .route("/robots.txt", get(meta::robots))
        .route("/sitemap.xml", get(meta::sitemap))
        // The theme switch's no-JS fallback; kernel pages (/admin/jobs too) and the app layout.
        .route("/theme", post(theme::update))
        // Browsers ask for it at the root whatever the <link> says.
        .route_service("/favicon.ico", ServeFile::new("static/favicon.ico"));
    // samples: routes

    // Development only. In production these routes don't exist (a test asserts the 404s).
    if config.is_development() {
        app = app.merge(dev::router());
        // samples: dev routers
    }

    // Digested URLs are immutable, everything else revalidates.
    let static_files = Router::new()
        .fallback_service(ServeDir::new("static"))
        .layer(from_fn_with_state(
            state.assets.clone(),
            assets::cache_headers,
        ));

    let mut csrf = CsrfLayer::new().add_trusted_origin(&config.app_url)?;
    if let Some(port) = config.dev_http_port {
        // The same app on plain http beside the TLS port (development only).
        csrf = csrf.add_trusted_origin(format!("http://localhost:{port}"))?;
    }
    let csrf = csrf
        .with_insecure_bypass(|_, uri| webhook::is_registered(uri.path()))
        .with_rejection_response(|_| AppError::Forbidden.into_response());

    let x_request_id = HeaderName::from_static("x-request-id");
    let app = app
        .nest_service("/static", static_files)
        .fallback(errors::not_found);
    // The pending-migrations guard shows a page instead of a "no such table" error. A layer
    // only wraps what exists when it is applied, so it goes after the fallback: unknown paths
    // get the page too.
    let app = if config.is_development() {
        app.layer(from_fn_with_state(state.clone(), migrations::guard))
    } else {
        app
    };
    // `.layer` calls run bottom-up: the last one added is outermost.
    Ok(app
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(from_fn(flash::middleware))
        .layer(from_fn(csrf_evidence))
        .layer(csrf)
        // A panic becomes a shaped 500, so it sits inside `shape_errors`.
        .layer(CatchPanicLayer::custom(error::panic_response))
        .layer(from_fn_with_state(state.clone(), error::shape_errors))
        .layer(CompressionLayer::new())
        // Overload is 503 + Retry-After, never 408 (errors-and-logging.md#response-mapping).
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(30),
        ))
        .layer(map_response(error::retry_after))
        // Development only: a Host that isn't this machine is refused (DNS rebinding).
        .layer(from_fn_with_state(
            hosts::AllowedHosts::new(&config),
            hosts::middleware,
        ))
        // Outside the panic and timeout layers, so a 500 or 503 carries the headers too.
        .layer(from_fn_with_state(
            security::Hardening::new(&config),
            security::middleware,
        ))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        // SECURITY: the request span logs the URI with tokens redacted (emailed links carry one in
        // the path), never the raw URI (test: tokens_in_request_uris_never_reach_the_logs).
        .layer(TraceLayer::new_for_http().make_span_with(
            |req: &axum::http::Request<axum::body::Body>| {
                tracing::debug_span!(
                    "request",
                    method = %req.method(),
                    uri = %redact::uri(req.uri()),
                    version = ?req.version(),
                )
            },
        ))
        .layer(from_fn(error::log_server_errors))
        // samples: outer layers
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .with_state(state))
}

/// URL helpers (`*_path`). Templates, redirects and mailers use these, never string literals.
pub mod paths {
    pub fn root() -> String {
        "/".into()
    }
    pub fn up() -> String {
        "/up".into()
    }
    /// Where `AppError::Unauthorized` sends a visitor; an authentication sample routes it.
    pub fn new_session() -> String {
        "/session/new".into()
    }
    pub fn dev() -> String {
        "/dev".into()
    }
    /// The theme switch (`POST`, no-JS fallback).
    pub fn theme() -> String {
        "/theme".into()
    }
    /// The component gallery (development only).
    pub fn dev_components() -> String {
        "/dev/components".into()
    }
    pub fn dev_components_modal() -> String {
        "/dev/components/modal".into()
    }
    /// A layout demo: `sidebar`, `marketing`, `auth` or `docs` (development only).
    pub fn dev_layout(name: &str) -> String {
        format!("/dev/layouts/{name}")
    }
    // samples: paths
}

/// One row of the route table: method, path pattern, handler, who may call it.
pub type Route = (&'static str, &'static str, &'static str, Access);

/// The route table: the app's own rows, then each feature's (`src/routes/<feature>.rs`), in
/// order. `app routes` prints it and writes docs/routes.md. Tests assert every entry is routable
/// (not the router fallback) and enforce its access level.
pub const ROUTES: &[Route] = &concat::<{ count(TABLES) }>(TABLES);

/// The tables `ROUTES` is made of: one line per feature.
const TABLES: &[&[Route]] = &[
    APP_ROUTES,
    // samples: route tables
];

/// The kernel's rows, and the parts'.
#[rustfmt::skip]
const APP_ROUTES: &[Route] = &[
    ("GET",  "/",                    "home::index",     Access::Public),
    ("GET",  "/up",                  "health::up",      Access::Public),
    ("GET",  "/robots.txt",          "meta::robots",    Access::Public),
    ("GET",  "/sitemap.xml",         "meta::sitemap",   Access::Public),
    ("GET",  "/favicon.ico",         "static favicon",  Access::Public),
    ("POST", "/theme",               "theme::update",   Access::Public),
    ("GET",  "/dev",                 "dev::index",      Access::Dev),
    ("GET",  "/dev/routes",          "dev::routes",     Access::Dev),
    ("GET",  "/dev/system",          "dev::system",     Access::Dev),
    ("GET",  "/dev/introspect.json", "dev::introspect", Access::Dev),
    ("GET",  "/dev/reload",          "dev::reload",     Access::Dev),
    ("POST", "/dev/migrations",      "migrations::run", Access::Dev),
    ("GET",  "/dev/components",      "gallery::index",  Access::Dev),
    ("POST", "/dev/components",      "gallery::submit", Access::Dev),
    ("GET",  "/dev/components/modal", "gallery::modal", Access::Dev),
    ("GET",  "/dev/layouts/sidebar", "gallery::sidebar", Access::Dev),
    ("GET",  "/dev/layouts/marketing", "gallery::marketing", Access::Dev),
    ("GET",  "/dev/layouts/auth",    "gallery::auth",   Access::Dev),
    ("POST", "/dev/layouts/auth",    "gallery::auth_submit", Access::Dev),
    ("GET",  "/dev/layouts/docs",    "gallery::docs",   Access::Dev),
    // samples: routes table
];

/// How many rows the tables hold (compile time).
const fn count(tables: &[&[Route]]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < tables.len() {
        n += tables[i].len();
        i += 1;
    }
    n
}

/// The tables, one after the other (compile time: `ROUTES` stays a plain `&[Route]`).
const fn concat<const N: usize>(tables: &[&[Route]]) -> [Route; N] {
    let mut out: [Route; N] = [("", "", "", Access::Public); N];
    let (mut k, mut i) = (0, 0);
    while i < tables.len() {
        let mut j = 0;
        while j < tables[i].len() {
            out[k] = tables[i][j];
            k += 1;
            j += 1;
        }
        i += 1;
    }
    out
}
