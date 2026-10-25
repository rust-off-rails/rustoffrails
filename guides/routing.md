# Routing (`config/routes.rb` → `src/routes.rs`)

> **TL;DR**
> - Every route lives in `src/routes.rs`, plus `paths::` helpers and the `ROUTES` table with an `Access` level per route (tests walk it; `#[rustfmt::skip]` keeps it aligned).
> - htmx mode uses POST-only resource routes (`/posts/{id}/delete`); inertia uses real verbs; spa exposes `/api` only.
> - axum 0.8: `/{id}`, never `/:id`; a parameter is a whole segment (`/{id}.eml` panics at startup); the body extractor goes last.
> - Middleware order matters: security headers outside panic and timeout layers, the migrations guard after the fallback.
>
> Read the rest when adding routes, middleware, or nested resources.

Every route lives in `src/routes.rs`. One file, read top to bottom, is the whole URL surface of the app.

## The resource shape

The shape depends on `FRONTEND_MODE`:

- **`htmx`:** HTML forms only send GET and POST, and the framework doesn't use method overrides. Use the table below.
- **`inertia`:** Inertia's `useForm`/`router` sends real verbs. `update` is `PATCH /posts/{id}` and `destroy` is `DELETE /posts/{id}`. Everything else is the same as below. There is no `/delete` route.
- **`spa`:** pages are client-side routes. The server exposes only `/api/...` (below) plus the SPA fallback.

`htmx` mode:

| Action  | Method + path               | Handler                 | Path helper |
|---------|-----------------------------|-------------------------|-------------|
| index   | `GET  /posts`               | `posts::index`          | `paths::posts()` |
| new     | `GET  /posts/new`           | `posts::new`            | `paths::new_post()` |
| create  | `POST /posts`               | `posts::create`         | |
| show    | `GET  /posts/{id}`          | `posts::show`           | `paths::post(id)` |
| edit    | `GET  /posts/{id}/edit`     | `posts::edit`           | `paths::edit_post(id)` |
| update  | `POST /posts/{id}`          | `posts::update`         | |
| destroy | `POST /posts/{id}/delete`   | `posts::destroy`        | `paths::delete_post(id)` |

JSON API routes (`spa` mode) use real verbs: `GET/POST /api/posts`, `GET/PATCH/DELETE /api/posts/{id}`.

Nested resources go one level deep at most: `/posts/{post_id}/comments` (index/create). Member actions are `/comments/{id}` (shallow nesting, as in Rails `shallow: true`).

Custom actions are a `POST` to a noun: `POST /posts/{id}/publication` (publish), `POST /posts/{id}/publication/delete` (unpublish). This follows the Rails "CRUD on a new resource instead of custom verbs" pattern.

## `src/routes.rs` template

Verified with axum 0.8.9 / tower-http 0.7.1 (the reference app's `routes.rs`, trimmed to one resource):

```rust
pub fn router(state: AppState) -> anyhow::Result<Router> {
    let config = state.config.clone();
    let mut app = Router::new()
        .route("/", get(home::index))
        .route("/up", get(health::up))                      // Rails 8 health check
        // posts
        .route("/posts", get(posts::index).post(posts::create))
        .route("/posts/new", get(posts::new))
        .route("/posts/{id}", get(posts::show).post(posts::update))
        .route("/posts/{id}/edit", get(posts::edit))
        .route("/posts/{id}/delete", post(posts::destroy));

    // Framework surfaces: /dev exists only in development; /admin needs an access method.
    if config.is_development() {
        app = app.merge(dev::router()).merge(kernel::mailbox::router());
    }
    if config.is_development() || config.admin_password.is_some() {
        app = app.merge(kernel::jobs::web::router());
    }

    let static_files = Router::new()
        .fallback_service(ServeDir::new("static"))
        .layer(from_fn(kernel::assets::cache_headers));     // ?v= → immutable, else no-cache
    let csrf = CsrfLayer::new()
        .add_trusted_origin(&config.app_url)?
        .with_insecure_bypass(|_, uri| kernel::webhook::is_registered(uri.path()))
        .with_rejection_response(|_| AppError::Forbidden.into_response());

    Ok(app
        .nest_service("/static", static_files)
        .fallback(errors::not_found)
        // middleware: see the next section for the full ordered list
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(from_fn(kernel::flash::middleware))
        .layer(csrf)
        .layer(from_fn_with_state(state.clone(), error::shape_errors))
        /* request id, trace, security headers, catch-panic, timeout, compression */
        .with_state(state))
}

/// Path helpers. Templates and redirects use these, never string literals.
pub mod paths {
    pub fn posts() -> String { "/posts".into() }
    pub fn new_post() -> String { "/posts/new".into() }
    pub fn post(id: i64) -> String { format!("/posts/{id}") }
    pub fn edit_post(id: i64) -> String { format!("/posts/{id}/edit") }
    pub fn delete_post(id: i64) -> String { format!("/posts/{id}/delete") }
    pub fn new_session() -> String { "/session/new".into() }   // exists from `new`; error.rs redirects here
}

/// Who may call a route. The security suite requests every route as each actor (test-standards.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access { Public, User, Admin, Dev }

/// The route table. `app routes` prints it and writes docs/routes.md. Tests assert every entry is
/// routable (not 404/405) in development and enforce its access level, so it can't drift.
#[rustfmt::skip] // keep the table aligned; cargo fmt would explode each tuple onto five lines
pub const ROUTES: &[(&str, &str, &str, Access)] = &[
    ("GET",  "/",                 "home::index",              Access::Public),
    ("GET",  "/up",               "health::up",               Access::Public),
    ("GET",  "/posts",            "posts::index",             Access::Public),
    ("POST", "/posts",            "posts::create",            Access::User),
    // …
    ("GET",  "/admin/jobs",       "kernel::jobs::web::index", Access::Admin),
    ("GET",  "/dev",              "dev::index",               Access::Dev),
];
```

- `router` returns `anyhow::Result` because `add_trusted_origin` validates `APP_URL`. `main` and `TestApp` both call it.
- **Every route goes in `ROUTES`** with its access level, framework routes included (`/admin/*` are `Admin`, `/dev/*` are `Dev`).

## axum 0.8 gotchas (agents get these wrong)

- Path params are `{id}`. Wildcards are `{*rest}`. `:id` panics at startup.
- **A param must be the whole segment.** `/files/{id}.eml` panics at startup with "Only one parameter is allowed per path segment" (axum 0.8.9; verified alone and next to `/files/{id}`). Use `/files/{id}/message.eml`, or a `{name}` param you parse yourself.
- `Path(id): Path<i64>` for one param, `Path((post_id, id)): Path<(i64, i64)>` for two.
- Body-consuming extractors (`Form`, `Json`, `Multipart`) **must be the last** handler argument.
- `Redirect::to` sends 303 See Other, which is correct after a POST.
- Route-level middleware: `.route_layer(...)`. Router-wide middleware: `.layer(...)`. Layers apply bottom-up: the last `.layer` call runs first.

## Middleware

Outermost first. In code, `.layer` calls run bottom-up, so they appear in the **reverse** of this order:

1. `SetRequestIdLayer::new(x-request-id, MakeRequestUuid)`, then `TraceLayer::new_for_http().make_span_with(...)` (span fields `method`, `path`, `request_id`), then `PropagateRequestIdLayer`
2. `kernel::security::middleware` (`from_fn_with_state(security::Hardening::new(&config), …)`): the header matrix, `no-store` for authenticated responses, production cookie hardening ([security](security.md#the-header-matrix)). It sits **outside** the panic and timeout layers, so their 500 and 503 responses are hardened too
   - then `kernel::hosts::middleware` (`from_fn_with_state(hosts::AllowedHosts::new(&config), …)`): development only, a `Host` that isn't this machine gets a 403 before anything else runs ([security](security.md#development-is-this-machine-only))
3. `CatchPanicLayer::custom(error::panic_response)` ([errors](errors-and-logging.md))
4. `axum::middleware::map_response(error::retry_after)`: a 503 without `Retry-After` gets `Retry-After: 1` ([errors](errors-and-logging.md#response-mapping))
5. `TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, 30s)`: overload is 503, never 408 (`TimeoutLayer::new` is deprecated in tower-http 0.7; `with_status_code(StatusCode, Duration)` checked against 0.7.1)
6. `CompressionLayer::new()`
7. `error::shape_errors` (`from_fn_with_state`)
8. `CsrfLayer` (tower-http `csrf` feature; [security](security.md#csrf))
9. `kernel::flash::middleware` (`htmx` and `inertia` modes)
10. `kernel::live::gate` (`from_fn_with_state`, apps with live regions): requests with `X-Live-Refresh` wait for a permit ([realtime](realtime.md#keep-re-fetches-from-starving-user-requests))
11. `DefaultBodyLimit::max(2 MB)`, raised per route for uploads

Authentication is **not** middleware. It's an extractor on each handler (`RequireUser`), so a route's protection is visible in its signature.
