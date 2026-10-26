# Security

> **TL;DR**
> - 13 **non-negotiables** (CSP, headers, CSRF/Origin, escaping, authorization, tenant scoping, rate limits, tokens, session revocation, cookies, secrets, dependencies, development reachable from this machine only) are never weakened without a decision entry **and** the human's explicit OK.
> - Development listens on 127.0.0.1 (`APP_BIND`) and refuses any `Host` that isn't this machine (`kernel/hosts.rs`): `/dev` has no login, and DNS rebinding reaches it same-origin, past CSRF.
> - CSRF is tower-http's `CsrfLayer` (Fetch Metadata + Origin, no tokens); the header matrix applies to every response class, tested by `headers_test.rs`.
> - No raw HTML except ammonia output; no SQL via `format!`; no mass assignment (owner/role fields never in `Params`); uploads validated and served safely.
> - CI enforces cargo-deny, gitleaks and a ZAP baseline; security-relevant changes run `/offrails:security` and add attack tests.
>
> Read the rest when touching auth, headers, uploads, admin or dev surfaces, WebSockets, or secrets.

**Security is the gold standard of this framework.** Rails ships defenses on by default. Off Rails ships them as specs you generate on day one (`generators/new.md`), enforced by tests, CI scanners and plugin hooks. Agents go hard here: assume every input is hostile, every client is scripted, and every proxy lies. Never remove a defense to make something work. Fix the something.

## Non-negotiables

You must **never weaken** any of these without **both** a `docs/decisions.md` entry and the human's **explicit approval in conversation**. "It was blocking me" is not a reason. If a fix seems to need it, stop and ask.

1. **CSP**: the strict policy in [the header matrix](#the-header-matrix). No `unsafe-inline`, no `unsafe-eval`, no wildcard sources, no inline `<script>` or `on*=` handlers.
2. **Security headers** on every response class, verified by `tests/app/headers_test.rs`.
3. **CSRF and Origin checks**, including the WebSocket handshake. Safe methods never change state.
4. **Escaping**: Askama autoescape, React and Svelte text rendering. Raw HTML only from ammonia-sanitized helpers.
5. **Authorization on every route and every socket join and push**: `RequireUser` (or stronger) plus a policy or an owner-scoped query.
6. **Tenant scoping** on every query in multi-tenant apps, with the isolation suite ([test-standards](test-standards.md) §4).
7. **Rate limits** on login, sign-up, password reset, magic links, API tokens, anything that sends email or costs money, and socket frames.
8. **Tokens**: random (≥ 32 bytes), **only digests stored**, expiring, single-use where they grant access, minted inside jobs (never in job args).
9. **Session revocation**: logout, password change and reset end the server session, and **every** session on change or reset, including open sockets and streams ([channels](channels.md)).
10. **Cookies**: `HttpOnly`, `SameSite=Lax`, `Secure` in production; the session cookie is `__Host-` prefixed in production.
11. **Secrets hygiene**: env vars only. Never in code, logs, job args, error pages, fixtures or git history (gitleaks in CI).
12. **Dependencies**: cargo-deny green (no known advisories, permissive licenses, no banned crates, crates.io only).
13. **Development is this machine only**: `serve` binds 127.0.0.1 in development unless `APP_BIND` says otherwise, and `kernel::hosts` refuses a `Host` that isn't localhost, `APP_URL`'s or an `APP_ALLOWED_HOSTS` entry ([below](#development-is-this-machine-only)).

The plugin's hooks flag the common ways these get weakened as you type (`|safe`, `{@html}`, `dangerouslySetInnerHTML`, inline scripts, `unsafe-inline`/`unsafe-eval`, permissive CORS, SQL built with `format!`, cookies with `Secure`/`HttpOnly` turned off). For a full review run **`/offrails:security`**: threat model, scanners, live header check, the checklist, and attack tests.

## CSRF

No tokens. Modern browsers say where a request came from, and sessions use `SameSite=Lax` cookies. This is **tower-http's `CsrfLayer`** (feature `csrf`, tower-http ≥ 0.7), a port of Go 1.25's cross-origin protection. It used to be a generated `kernel/csrf.rs` with the same rules. Safe methods (`GET`, `HEAD`, `OPTIONS`) always pass. An unsafe method passes if any of these hold:

1. `Origin` exactly matches a trusted origin. We trust `APP_URL`.
2. `Sec-Fetch-Site` is `same-origin` or `none`.
3. Neither `Sec-Fetch-Site` nor `Origin` is present, **and the request carries no cookie** (a non-browser client; a cookie-authenticated request with neither header gets 403 from the `csrf_evidence` middleware in `routes.rs`).
4. `Origin`'s host:port equals the request's `Host`.

Everything else gets 403.

```rust
let csrf = CsrfLayer::new()
    .add_trusted_origin(&config.app_url)?                                   // validated: returns an error for a bad APP_URL
    .with_insecure_bypass(|_, uri| kernel::webhook::is_registered(uri.path()))   // exactly the registered webhook paths; each verifies its signature (below)
    .with_rejection_response(|_| AppError::Forbidden.into_response());     // shaped like every other 403 (errors-and-logging.md)
```

- Safe methods **must not change state**. This whole design depends on it. A guard test (`get_handlers_do_not_write_to_the_database`) scans every `GET` handler's body for database writes; exempt a line only with `// writes: <reason>` when the write is the point.
- **Every webhook route verifies the provider's signature, or it doesn't exist.** Mount it with `kernel::webhook::route(path, webhook::Webhook::stripe(secret), handler)` (or `::github(secret)`) and add the exact path to `webhook::REGISTERED_PATHS` in the same change; a test keeps mounts and the registry in sync. The bypass covers exactly those paths, never a prefix, and the registration runs the verifier before the handler, so a handler there cannot forget the check. The verifier reads the **raw body**, computes the provider's HMAC (Stripe: `t=…,v1=…` over `"{t}.{body}"`; GitHub: `X-Hub-Signature-256`) with the secret from `Config`, compares in constant time, and refuses a timestamp older than 5 minutes. Inside the handler: store the event id and `on_conflict_do_nothing` so a replayed delivery is a no-op; return 2xx fast and do the work in a job. Test: a valid signature is accepted, a tampered body, a wrong secret, a stale timestamp and a missing header are all 401, and the same event id twice creates one record.
- Behind a proxy, `Host` and `Origin` must reach the app unchanged (Fly, Kamal-proxy, Render and Railway do this by default). Otherwise only `Sec-Fetch-Site` protects you.
- Tests: a `Sec-Fetch-Site: cross-site` POST and a foreign-`Origin` POST both get 403. `TestApp::post_form` sends `Sec-Fetch-Site: same-origin`, like a browser.

## The header matrix

One middleware, `kernel/security.rs` (`security::middleware`), sets the headers. It sits **outside** `CatchPanicLayer` and the timeout in `routes.rs`, so panic 500s and timeout 503s get them too ([routing](routing.md#middleware)). It overwrites the matrix headers a handler set, so no handler can weaken them. The one exception is a sandboxed document response (the mailbox viewer's own CSP: `sandbox` with `default-src 'none'`), whose policy it keeps.

| Header | Value | Notes |
|--------|-------|-------|
| `Content-Security-Policy` | `default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'` | Plus `; upgrade-insecure-requests` when served over https (below). Add a storage host to `img-src` only if blobs are served from another origin |
| `Strict-Transport-Security` | `max-age=63072000; includeSubDomains` | Production **with an `https://` `APP_URL`** only; `max-age=0` otherwise, so localhost is never pinned. `preload` is opt-in per decision (it's hard to undo) |
| `X-Content-Type-Options` | `nosniff` | |
| `Referrer-Policy` | `strict-origin-when-cross-origin` | |
| `Permissions-Policy` | `camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), bluetooth=(), hid=(), midi=(), browsing-topics=()` | Open a feature per spec. Don't list retired features (`interest-cohort`) |
| `Cross-Origin-Opener-Policy` | `same-origin` | Isolates the window from cross-origin openers |
| `Cross-Origin-Resource-Policy` | `same-origin` | Other sites can't embed your responses. Crawlers (OG images) aren't affected |
| `X-Frame-Options` | `DENY` | Legacy twin of `frame-ancestors 'none'` |
| `Cache-Control` | `no-store` | On **every response to a request with a session cookie** (except `/static/*`, `/favicon.ico`, `/robots.txt`), and on all kernel pages (`/dev/*`, `/admin/*`). Private data never lands in a shared or back/forward cache |
| `Cross-Origin-Embedder-Policy` | not set | Only needed for cross-origin isolation (`SharedArrayBuffer`). `require-corp` breaks third-party embeds such as video players. Set it per spec |

By response class (all verified by `tests/app/headers_test.rs`, in development and production):

| Class | Example | Required |
|-------|---------|----------|
| HTML page | `GET /` | full matrix |
| Authenticated page | `GET /books/1` logged in | full matrix + `no-store` |
| JSON | `GET /api/…`, `/dev/mailbox.json` | full matrix (+ `no-store` when authenticated or kernel) |
| SSE stream | `GET /streams/{stream}` | full matrix, `text/event-stream` |
| WebSocket handshake | `GET /socket` → 101, and the cross-origin 403 | `nosniff`, CORP, CSP on both |
| Kernel page | `/admin/jobs`, `/dev` | full matrix + `no-store` |
| Errors | 404, 403 (CSRF), 422, 500 (panic) | full matrix |
| Static asset | `/static/app.js?v=…` | full matrix, cacheable (`immutable` when digested) |
| Redirect | `303` to login | full matrix |

**Cookies.** Every cookie the app sets is `HttpOnly; SameSite=Lax`, plus `Secure` in production. The session cookie is named **`__Host-session_token`** in production (`kernel::session::name(secure)`): browsers refuse a `__Host-` cookie unless it's `Secure`, `Path=/` and has no `Domain`, so a sibling subdomain can't plant a session (session fixation, login CSRF). Production reads **only** that name. In production the middleware also adds any missing `Secure`/`HttpOnly`/`SameSite` to every `Set-Cookie`, as a backstop for cookies built without config (the flash).

**TLS-only directives key off `APP_URL`.** `upgrade-insecure-requests` and HSTS are sent only when `APP_ENV=production` **and** `APP_URL` starts with `https://`, which is always the case when deployed behind TLS. Verified in Chrome: on a production build run locally over plain http, `upgrade-insecure-requests` makes the browser upgrade same-origin **redirects** (not the initial requests) to `https://localhost`, and every redirect flow fails with `ERR_SSL_PROTOCOL_ERROR`. Production with a non-https `APP_URL` on a non-local host logs a loud warning at boot.

**Verified in current Chrome (2026-10-12):**
- `connect-src 'self'` covers same-origin `ws://` and `wss://` (CSP3). The book-room socket joins and presence renders under this exact CSP.
- `Secure` and `__Host-` cookies work on `http://localhost` (a secure context), so the production cookie path is testable locally.
- The Permissions-Policy above logs no console warnings.

**Development adjustments**, for `inertia` and `spa` only (`APP_ENV=development`; islands build with `vite build --watch` and keep the production CSP, [frontend-htmx](frontend-htmx.md#islands)):
- add `http://localhost:5173` to `script-src`, `style-src` and `connect-src`;
- add `ws://localhost:5173` to `connect-src`;
- add `'unsafe-inline'` to `style-src`.

The React refresh preamble is a same-origin file, `/dev/vite-preamble.js` ([frontend-inertia](frontend-inertia.md)), so it needs no nonce. SvelteKit SPA's boot script is allowed by **sha256 hash** ([frontend-svelte](frontend-svelte.md)). Production never gets `unsafe-inline` or `unsafe-eval`.

The kernel pages (`/admin/jobs`, `/dev/*`) are covered by the same CSP. They use vendored htmx with `allowEval: false` and no inline scripts. The mailbox's HTML-body route sets its own, stricter headers ([mailers](mailers.md)).

## Rate limiting (`kernel/rate_limit.rs`)

Rails 8 `rate_limit to: 10, within: 3.minutes` equivalent:

- `RateLimiter` (in `AppState.rate_limit`, built with `RateLimiter::new(config.env)`) wraps a `moka::future::Cache<String, Window>`. `Window` is `{ hits: Arc<AtomicU32>, ttl }`, and a `moka::Expiry` impl expires each entry `ttl` after creation, so every call site can use its own window. The key is `"{route}:{client_ip}"`. Two methods: `check(key, limit, window)` counts a hit and errors with `TooManyRequests` past the limit; `exhausted(key, limit)` only peeks. Use the pair for failure-only limits (peek first, `check` after a failure), as `AdminAccess` does.
- **A lockout never grows on retries.** The window is fixed from the **first counted attempt**: implement only `Expiry::expire_after_create`, never `expire_after_read`/`expire_after_update`, never `time_to_idle`, and never re-insert the entry on a hit. **Refused attempts are not counted**: `check` refuses when `hits.load(Relaxed) >= limit` **before** incrementing, and only then `fetch_add(1)` (refuse if the returned value was already `>= limit`, for concurrent hits). Not `fetch_update`: it's deprecated on current Rust and fails `clippy -D warnings`. A person hammering "Log in" while locked out is let back in exactly `window` after their first attempt, not `window` after their last. (A sliding window over attempt timestamps is also fine; a window that restarts or extends on each refused attempt is not.)
- **Development: 1000 per window.** `RateLimiter::new(AppEnv::Development)` replaces every call site's limit with `DEV_LIMIT` (1000), so local retries and agent loops don't lock anyone out; `Test` and `Production` use the call site's limit unchanged. `limit(configured) -> u32` is the one place that decides, `check` and `exhausted` both go through it, and `kernel_test.rs` pins it: development gives 1000 for a configured 10, test and production give 10.
- `state.rate_limit.check(&key, limit, window).await -> Result<(), AppError>`. Over the limit → `AppError::TooManyRequests`. Forms a human submits (login, link request) catch it and **re-render the form with status 429** and the alert "Try again later.", so the person sees why. Other endpoints let the error map to a plain 429 (JSON on `/api`). Limited requests never reach argon2.
- Client IP comes from the `ClientIp(Option<IpAddr>)` extractor:
  - normally the socket address. This needs `serve` to use `into_make_service_with_connect_info::<SocketAddr>()`, and it's `None` in tests;
  - the `X-Forwarded-For` hop `APP_TRUSTED_PROXY_HOPS` from the right (default 1, the rightmost), only when `APP_TRUSTED_PROXY=true`.

  `ClientIp::key(route)` builds the key.
- **Behind any proxy, `APP_TRUSTED_PROXY=true` is required**, and `APP_TRUSTED_PROXY_HOPS` must equal the number of proxies that append to the header (one platform proxy such as Fly or Render: 1; Cloudflare → Caddy → app: 2). Without it every user shares the proxy's address as their key, so ten failed logins by anyone lock login, reset and magic links for everyone. With too many hops a client forges its address. Every deploy snippet in [deployment](deployment.md) sets it; the checklist there asks for it.
- **Login is limited per IP and per account.** The per-IP limit alone lets a botnet guess one account's password from many addresses; the per-account throttle (`auth_throttle`, by email) alone lets anyone pause a victim. Keep both, and never count refused attempts.
- It's in-process, so the limit is per instance. That's acceptable, and documented in decisions. If a spec needs global limits, use the escape hatch to the `cache_entries` table.
- Applied to: login, password reset request, magic-link request (by IP **and** by email: `format!("login_links#create:{email}")`), sign-up, and any endpoint that sends email or costs money.
- Tests run without a socket, so every request shares the `"<route>:unknown"` key. One `TestApp` can exhaust a limit in a loop (the 11th attempt → 429) without any IP spoofing. To test a **per-email limit across IPs**, build the `TestApp` with `APP_TRUSTED_PROXY=true` and send a different `X-Forwarded-For` on each request: the IP keys differ, the email key is shared, and the 11th request still gets 429.

## XSS

- Askama autoescapes. `|safe` is allowed **only** on output of `views::helpers::markdown()` (ammonia-sanitized).
- React escapes. `dangerouslySetInnerHTML` is allowed only with server-sanitized HTML from the same helper, delivered in a field named `*_html`.
- Never build HTML with `format!` in Rust.
- Inline scripts and `on*=` attributes are blocked by CSP. Use `static/app.js` or React.

## Injection

- SQL: SeaORM builder with bound values only. Raw SQL only through the escape hatch with binds (`database.md`). Never `format!` user input into SQL, shell commands, or file paths.
- Dynamic sort/filter columns: map an enum from the query string onto `Column` variants with a `match`. Never trust a column name string.
- Shell: don't shell out from request handlers. If a job must, pass arguments as a vector (`Command::new(x).arg(y)`), never through a shell.

## Mass assignment

`<Model>Params` structs contain only user-editable fields. **Never** `user_id`, `author_id`, `account_id`, `role`, `admin`, `price`, `status` (unless the spec makes it user-editable). Ownership fields come from the extractor (`RequireUser`, `CurrentAccount`), not from the body.

## File uploads

See `storage.md`. In short: enforce a size limit per route (`DefaultBodyLimit`), use an allowlist of content types checked against magic bytes (not the client header), use random storage keys (never the user's filename as a path), serve with `Content-Disposition: attachment` unless it's an allowlisted image, and never serve uploads from the app origin with `text/html`.

## Admin & dev surfaces

- `/admin/jobs` ([jobs-dashboard](jobs-dashboard.md)) is protected by the `AdminAccess` extractor: `RequireAdmin` when roles exist, otherwise HTTP Basic with `APP_ADMIN_PASSWORD`. In production with neither, it isn't mounted. Any future `/admin/*` screen uses the same extractor.
- `/dev/*` ([dev-tools](dev-tools.md)) exists only in development. It's not hidden, it's not routed. A test proves it 404s with a production config.
- Admin actions are POSTs (CSRF applies), are logged with the acting user, and send `Cache-Control: no-store`.

## Development is this machine only

`/dev` has no login, by design: the mailbox shows every magic link and reset link, introspection shows the app's state, and `/admin/jobs` is open in development. That's safe only if nobody but you can reach it. Two holes, found by an independent review of generated apps (fixed in 0.7.0):

- **The LAN.** `serve` used to bind `0.0.0.0` in every environment, so anyone on the same Wi-Fi could open `http://<your-ip>:3000/dev/mailbox` and sign in as you.
- **DNS rebinding.** A page on `evil.example` re-points its own name at `127.0.0.1`. Your browser then sends `Host: evil.example:3000` to your dev server **same-origin**: CORS doesn't apply, and Fetch Metadata says `same-origin`, so the `CsrfLayer` lets its POSTs through too. The `Host` header is the one thing the attacker can't set to `localhost`.

So, in development only:

1. **Bind loopback.** `serve` listens on `config.bind` (`APP_BIND`, default `127.0.0.1` in development, `0.0.0.0` in production: [config](config.md#where-serve-listens-app_bind-app_allowed_hosts-and-the-unset-app_env-warning)). A non-loopback bind in development is refused unless `APP_DEV_REMOTE=1`: a `Host` header is not authentication, and a LAN client can set `Host: localhost` by hand. A dev container sets both `APP_BIND=0.0.0.0` and `APP_DEV_REMOTE=1` so the published port works.
2. **Check `Host`** (Rails' `HostAuthorization`). `kernel/hosts.rs` answers only `localhost`, `127.0.0.1`, `[::1]`, `*.localhost` (RFC 6761), `APP_URL`'s host and `APP_ALLOWED_HOSTS` entries (a leading `.` allows subdomains). Anything else gets a 403 page that says how to allow it. It runs **inside** the header-matrix middleware (the 403 carries the headers) and outside everything else, so no handler and no `/dev` page runs for a refused host ([routing](routing.md#middleware)). A request with no `Host` passes: a browser always sends one, and rebinding needs a browser. Production and test aren't checked: the platform's proxy routes by host, and an app answering any `Host` there exposes nothing that isn't public.
3. **Refuse an unset `APP_ENV`.** `Config::from_env` reads `.env` first (bin/setup writes `APP_ENV=development` there), and if `APP_ENV` is still unset it fails with a message naming the fix. On a server an unset `APP_ENV` is a missing setting that would mount `/dev` and `/admin/jobs` without a login; a default is not a gate.

To open the dev server from a phone or another machine, set `APP_BIND=0.0.0.0`, `APP_DEV_REMOTE=1` and `APP_ALLOWED_HOSTS=<the name or IP you'll type>`, and know that everyone on that network can then read your mailbox. A tunnel (`cloudflared`, `ngrok`) only needs `APP_ALLOWED_HOSTS=<tunnel host>` (or set `APP_URL` to it).

`src/kernel/hosts.rs`, whole (from an app generated from these docs, 2026-10-16; axum 0.8.9):

```rust
//! Development-only Host authorization (Rails' `HostAuthorization`): a request whose `Host` isn't
//! this machine, `APP_URL`'s host or an `APP_ALLOWED_HOSTS` entry gets a 403 page that says how to
//! allow it. Why: development serves /dev without a login, and DNS rebinding sends
//! `Host: evil.example` to this server same-origin, past CORS and the CSRF layer.
//! Production is not checked: the platform's proxy routes by host.
//! GUIDE: .offrails/guides/security.md#development-is-this-machine-only

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::config::Config;

/// The hosts development answers. `None` outside development (no check).
#[derive(Clone, Debug)]
pub struct AllowedHosts(Option<Vec<String>>);

impl AllowedHosts {
    pub fn new(config: &Config) -> Self {
        if !config.is_development() {
            return Self(None);
        }
        // APP: names development always answers; add yours with APP_ALLOWED_HOSTS, not here.
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].iter().map(|h| (*h).to_owned()).collect();
        if let Some(h) = host_of_url(&config.app_url) {
            hosts.push(h);
        }
        hosts.extend(config.allowed_hosts.iter().map(|h| normalize(h)));
        Self(Some(hosts))
    }

    /// `Host` values this allows (`host` or `host:port`, any case). A missing `Host` (an
    /// HTTP/1.0 client, a test request) is allowed: a rebinding attack needs a browser.
    pub fn allows(&self, host: Option<&str>) -> bool {
        let Some(list) = &self.0 else { return true };
        let Some(host) = host else { return true };
        let host = normalize(host);
        // `*.localhost` resolves to this machine by definition (RFC 6761).
        host.ends_with(".localhost")
            || list.iter().any(|allowed| {
                allowed == &host
                    || allowed
                        .strip_prefix('.')
                        .is_some_and(|domain| host == domain || host.ends_with(allowed.as_str()))
            })
    }
}

/// `Host: Dev.Example.com:3000` → `dev.example.com`; `[::1]:3000` → `::1`.
fn normalize(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default().to_owned();
    }
    if host.matches(':').count() > 1 {
        return host; // a bare IPv6 address has no port
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name.to_owned(),
        _ => host,
    }
}

fn host_of_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = normalize(authority);
    (!host.is_empty()).then_some(host)
}

/// The request's host: the URI authority (HTTP/2's `:authority`), else the `Host` header.
fn request_host(req: &Request) -> Option<String> {
    if let Some(authority) = req.uri().authority() {
        return Some(authority.as_str().to_owned());
    }
    req.headers().get(header::HOST).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

// SECURITY: in development, a request for a Host that isn't this machine (DNS rebinding, or a
// LAN visitor when APP_BIND opens the port) is refused before any handler or /dev tool runs
// (test: development_refuses_a_rebound_host_name).
pub async fn middleware(State(hosts): State<AllowedHosts>, req: Request, next: Next) -> Response {
    let host = request_host(&req);
    if hosts.allows(host.as_deref()) {
        return next.run(req).await;
    }
    let shown = normalize(host.as_deref().unwrap_or_default());
    tracing::warn!(host = %shown, "blocked a request for a host development doesn't serve (APP_ALLOWED_HOSTS)");
    refused(&shown)
}

fn refused(host: &str) -> Response {
    let host = escape(host);
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Blocked host</title></head>\n<body><main>\n<h1>Blocked host: {host}</h1>\n<p>In development this app answers only requests addressed to this machine (localhost, 127.0.0.1, [::1]) or to the host in <code>APP_URL</code>. That stops a web page from re-pointing its own domain at your machine (DNS rebinding) and reading your development tools.</p>\n<p>If you meant to open it through <strong>{host}</strong> (a tunnel or a name on your network), allow it and restart:</p>\n<pre>APP_ALLOWED_HOSTS={host}</pre>\n<p>A leading dot allows every subdomain (<code>APP_ALLOWED_HOSTS=.{host}</code>). To reach the app from another device at all, it must also listen beyond this machine: <code>APP_BIND=0.0.0.0</code>.</p>\n</main></body></html>\n"
    );
    let mut response = (StatusCode::FORBIDDEN, body).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    response
}

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '&' => "&amp;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppEnv;

    fn dev(app_url: &str, allowed: &[&str]) -> AllowedHosts {
        AllowedHosts::new(&Config {
            env: AppEnv::Development,
            app_url: app_url.into(),
            allowed_hosts: allowed.iter().map(|h| (*h).to_owned()).collect(),
            ..Config::test()
        })
    }

    #[test]
    fn hosts_match_by_name_without_port_or_case() {
        let hosts = dev("https://dev.example.com", &["tunnel.example", ".lan.example"]);
        for ok in ["localhost:3000", "LOCALHOST", "127.0.0.1:3000", "[::1]:3000", "app.localhost:3000",
                   "dev.example.com", "tunnel.example:443", "lan.example", "box.lan.example:3000"] {
            assert!(hosts.allows(Some(ok)), "{ok} should be allowed");
        }
        for refused in ["evil.example", "localhost.evil.example", "127.0.0.1.evil.example",
                        "evillan.example", "tunnel.example.evil.example", ""] {
            assert!(!hosts.allows(Some(refused)), "{refused} should be refused");
        }
        assert!(hosts.allows(None));
    }

    #[test]
    fn the_refusal_page_escapes_the_host() {
        assert_eq!(escape("<script>"), "&lt;script&gt;");
    }
}
```

`routes.rs` mounts it right inside the security middleware:

```rust
        .layer(map_response(error::retry_after))
        // Development only: a Host that isn't this machine is refused (DNS rebinding).
        .layer(from_fn_with_state(hosts::AllowedHosts::new(&config), hosts::middleware))
        // Outside the panic and timeout layers, so a 500 or 503 carries the headers too.
        .layer(from_fn_with_state(security::Hardening::new(&config), security::middleware))
```

The request tests, in `tests/app/security_test.rs` (`get_with_headers` and `post_form_with_headers` are on `TestApp`: [testing](testing.md#testapp-spec-testsappsupportrs)):

```rust
fn development() -> Config { Config { env: AppEnv::Development, ..Config::test() } }

/// DNS rebinding: a page on `evil.example` re-points its name at 127.0.0.1 and reads /dev
/// same-origin. Development refuses any Host that isn't this machine, before any handler runs.
#[tokio::test]
async fn development_refuses_a_rebound_host_name() {
    let app = TestApp::with_config(development()).await;
    for path in ["/dev", "/", "/up"] {
        let res = app.get_with_headers(path, &[("host", "evil.example:3000")]).await;
        res.assert_status(StatusCode::FORBIDDEN)
            .assert_contains("Blocked host: evil.example")
            .assert_contains("APP_ALLOWED_HOSTS=evil.example");
        assert!(res.headers.contains_key("content-security-policy"), "the refusal carries the header matrix");
    }
    // A same-origin POST from the rebound page (Fetch Metadata and Origin agree): the CSRF layer
    // alone would pass it. Use any POST route the app has.
    app.post_form_with_headers("/dev/jobs/test", &[], &[("host", "evil.example:3000"), ("origin", "http://evil.example:3000")])
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Markup in the Host header is escaped on the page.
    app.get_with_headers("/", &[("host", "a<b>.example")]).await
        .assert_status(StatusCode::FORBIDDEN)
        .assert_not_contains("a<b>");
}

#[tokio::test]
async fn development_answers_local_and_configured_hosts() {
    let app = TestApp::with_config(Config {
        app_url: "https://dev.example.com".into(),
        allowed_hosts: vec!["tunnel.example".into(), ".lan.example".into()],
        ..development()
    })
    .await;
    for host in ["localhost:3000", "127.0.0.1:3000", "[::1]:3000", "dev.example.com", "tunnel.example", "box.lan.example:3000"] {
        app.get_with_headers("/up", &[("host", host)]).await.assert_ok();
    }
    app.get("/up").await.assert_ok(); // no Host at all (HTTP/1.0, a probe): nothing to rebind
}

#[tokio::test]
async fn production_and_test_answer_any_host() {
    for config in [production(), Config::test()] {   // production(): the suite's production Config
        let app = TestApp::with_config(config).await;
        app.get_with_headers("/up", &[("host", "evil.example")]).await.assert_ok();
    }
}
```

Prove it on the running app: `curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: evil.test' localhost:$PORT/dev` prints `403` (and the usual status without the header), `lsof -nP -iTCP:$PORT -sTCP:LISTEN` shows `127.0.0.1:$PORT`, not `*:$PORT`, and `curl http://<this machine's LAN IP>:$PORT/up` is refused (curl exit 7). Verified 2026-10-16 on macOS against an app generated from these docs, along with the boot warning when `APP_ENV` is unset.

## WebSockets

`/socket` ([channels](channels.md)) has its own rules, because CSRF protection doesn't cover WebSocket handshakes, and browsers send cookies on cross-site handshakes:

- **Origin check (CSWSH):** the handshake is refused (403) unless `Origin` exactly equals APP_URL's origin. A **missing** `Origin` is refused too.
- **Auth at upgrade:** the session cookie decides the member, and logged out → 401. Every topic join is then authorized with the same policy as the page (`Channels::authorize`). Private topics (`user:{id}`) accept only their owner.
- **Limits:**
  - frames over 4 KiB close the socket;
  - more than 100 frames per 10s close it with 1008;
  - malformed frames close it with 1002;
  - silence for 60s closes it;
  - per-person limits for app events (e.g. 10 messages per 10s) live in the room, so they hold across tabs and nodes.
- **Output:** room payloads carry raw user text. The client renders it with `textContent`, and the server-rendered page escapes it.
- **For the connection's whole life** ([channels#security](channels.md#security)): logout, a password change or revocation closes that session's sockets **and** SSE streams at once (4401 / `bye`), in every process and node; periodic revalidation closes expired sessions; every push is re-authorized against the database; per-user caps (8 connections, 20 topics, 30 joins/min); presence carries minimal metadata, never emails; the SSE and long-poll fallbacks refuse cross-site requests and bind relays to member + token.

## Secrets

- Only in env vars (`config.md`). `.env` is git-ignored, and `.env.example` has placeholders only.
- Never log secrets, tokens, passwords, or full request bodies of auth endpoints. `tracing` fields for these must be omitted, not redacted later.
- Errors shown to users never include internals (`errors-and-logging.md`).

## Automated enforcement

Every app gets these on day one ([new](../generators/new.md)). They run in CI on every push, and locally through `/offrails:check` and `/offrails:security`.

| Gate | Tool | Config (committed) | Fails on |
|------|------|--------------------|----------|
| Header matrix | `tests/app/headers_test.rs` | — | any response class missing a header; unprefixed or non-`Secure` production cookies; a planted unprefixed session cookie logging someone in |
| Dependencies | `cargo deny check` (cargo-deny 0.20) | `deny.toml` | RustSec advisories and yanked crates; unmaintained crates; non-permissive licenses; **banned crates** (the "Banned" list in [stack](../stack.md), generated into `[bans]`); git or non-crates.io sources |
| Secrets | `gitleaks` (8.30) over **all git history** | `.gitleaks.toml` | any secret in any commit |
| Dynamic scan | **OWASP ZAP baseline** (`ghcr.io/zaproxy/zaproxy:stable`) against the production Docker image | `.zap/rules.tsv` | any WARN not triaged in the rules file |
| JS dependencies | `pnpm audit --prod` (TS modes) | — | high or critical advisories |
| Updates | Dependabot, weekly and grouped | `.github/dependabot.yml` | — (opens PRs; CI gates them) |

The reference `deny.toml` (verified with cargo-deny 0.20.2: `advisories ok, bans ok, licenses ok, sources ok`):

```toml
# cargo-deny: run in CI and by /offrails:check (`cargo deny check`).
# Generated from stack.md "Banned". Never loosen without a docs/decisions.md entry + human approval.

[graph]
all-features = false

[advisories]
# RustSec advisory DB. A vulnerable or yanked crate fails the build; unmaintained crates are
# reported for the whole tree (they're the ones nobody will fix).
version = 2
yanked = "deny"
unmaintained = "all"
ignore = [
  # Triaged exceptions only, each with a reason and a review date:
  # { id = "RUSTSEC-YYYY-NNNN", reason = "…, not reachable because …; recheck 2026-12-01" },
]

[licenses]
version = 2
confidence-threshold = 0.9
# Permissive licenses only. A copyleft or unknown license fails the build.
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Zlib", "0BSD", "BSL-1.0", "CC0-1.0", "Unicode-3.0", "CDLA-Permissive-2.0", "MPL-2.0",
]

# The app and its migration crate are `publish = false`; they're ours, not a dependency.
[licenses.private]
ignore = true

[bans]
multiple-versions = "warn"
wildcards = "deny"
highlight = "all"
# Never anywhere in the tree (stack.md "Banned"). Direct-only bans (lazy_static, once_cell,
# async-trait, log macros) are checked against Cargo.toml by the plugin hook and /offrails:doctor,
# because healthy crates still pull them in transitively.
deny = [
  { crate = "diesel", reason = "SeaORM is the one data layer (stack.md)" },
  { crate = "actix-web", reason = "one web stack: axum (stack.md)" },
  { crate = "rocket", reason = "one web stack: axum (stack.md)" },
  { crate = "poem", reason = "one web stack: axum (stack.md)" },
  { crate = "loco-rs", reason = "one web stack: axum (stack.md)" },
  { crate = "leptos", reason = "no WASM front ends (stack.md)" },
  { crate = "dioxus", reason = "no WASM front ends (stack.md)" },
  { crate = "yew", reason = "no WASM front ends (stack.md)" },
  { crate = "tera", reason = "runtime templates hide errors (stack.md)" },
  { crate = "handlebars", reason = "runtime templates hide errors (stack.md)" },
  { crate = "minijinja", reason = "runtime templates hide errors (stack.md)" },
  { crate = "askama_axum", reason = "removed upstream (stack.md)" },
  { crate = "axum-inertia", reason = "generate the adapter from the spec (stack.md)" },
  { crate = "redis", reason = "the database does jobs, cache and pub/sub (stack.md)" },
  { crate = "openssl", reason = "rustls only (security.md)" },
  { crate = "openssl-sys", reason = "rustls only (security.md)" },
  { crate = "native-tls", reason = "rustls only (security.md)" },
]

[sources]
# Only crates.io. A git or alternative-registry dependency must be justified and added here.
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```

Details:
- **`deny.toml`:**
  - set `publish = false` on the app and migration crates and ignore private crates' licenses (`[licenses.private] ignore = true`), so only dependencies are judged;
  - crates that healthy dependencies pull in transitively (`lazy_static`, `once_cell`, `async-trait`, `log`) are banned as **direct** dependencies only, which the plugin hook and `/offrails:doctor` check against `Cargo.toml`;
  - everything else on the banned list is denied anywhere in the tree, plus `openssl`, `openssl-sys` and `native-tls` (rustls only).
- **Advisory ignores** in `deny.toml` need an id, a reason ("not reachable because …") and a recheck date.
- **gitleaks:**
  - the allowlist covers build and runtime directories only (`target/`, `storage/`, `node_modules/`, `web/dist/`, `static/vendor/`);
  - a known-public value (an RFC test vector) gets `// gitleaks:allow` **on the same line**, so move it into a one-line `const`.
- **ZAP:**
  - CI boots the production image and runs `zap-baseline.py -c rules.tsv` **without** `-I`, so every untriaged WARN fails;
  - each `IGNORE`/`INFO` line in `rules.tsv` states why, for example: htmx's `eval` (disabled by `allowEval:false` and the CSP), anti-CSRF tokens (by design: Fetch Metadata plus Origin), COEP (see the matrix);
  - with a rules file, ZAP's informational detectors (10049 cacheability, 10111 login form found, 10112 session cookie found) are reported as WARN, so mark them `INFO` with a note.

## Checklist

The "brakeman" pass. Run through this before finishing any work touching auth, payments, uploads, admin, or multi-tenancy. Report the result in your final message:

- [ ] Every state-changing route uses an unsafe method and passes CSRF middleware (no GET writes).
- [ ] Every non-public handler has `RequireUser` (or stronger) **and** a policy check or owner-scoped query.
- [ ] Tenant data: every query filters by `account_id`, and a cross-tenant test exists.
- [ ] No entity `Model` is serialized to a client. Only `*Json`/props structs.
- [ ] `/admin/*` requires `AdminAccess`; `/dev/*` is absent in production (test exists).
- [ ] Development listens on `config.bind` (127.0.0.1 unless `APP_BIND`) and `kernel::hosts` refuses foreign `Host` names (`development_refuses_a_rebound_host_name`); no `[0, 0, 0, 0]` bind in `src/` (`offrails check` `[bind]`).
- [ ] Job args carry no secrets or tokens (they're visible in `/admin/jobs`). Auth emails create their token **inside** the job ([auth](auth.md#password-reset-passwordsrs-new-create-edit-update)).
- [ ] No `|safe` / `dangerouslySetInnerHTML` except sanitized `*_html` fields.
- [ ] No raw SQL outside the kernel or a logged escape hatch, and all values bound.
- [ ] Params structs contain no ownership/privilege fields.
- [ ] Sensitive endpoints are rate limited.
- [ ] Tokens are random (≥ 32 bytes) and stored hashed, with expiries.
- [ ] Redirect targets from user input are relative and same-origin (return-to rejects `//host` and `/\host`).
- [ ] Auth responses don't reveal whether an email exists (same status, location and flash), and magic links are consumed by POST only.
- [ ] The dev mailbox hint and `/dev/*` links render only when `APP_ENV=development` (test with production config).
- [ ] Uploads: size limit, type allowlist, random keys, safe `Content-Disposition`.
- [ ] WebSockets: Origin checked (missing = refused), auth at upgrade, every join authorized, frame size and rate limits, tests for each ([channels](channels.md#testing)).
- [ ] No secrets in code, logs, fixtures or `.env.example`. gitleaks is green over the full history.
- [ ] `headers_test.rs` covers every response class the change adds (a new kind of endpoint means a new row).
- [ ] `cargo deny check` is green. Any new advisory ignore has a reason and a recheck date.
- [ ] The ZAP baseline is green. Any new `rules.tsv` line explains its triage.
- [ ] The feature spec's `## Security` section lists the threats this change touches, each with its control and the test that proves it. A category that can't apply is a row `| <Category> | n/a: <why> | — | — |`: the reason is required, and `offrails-check` fails a bare `n/a` or a named threat without a test.
- [ ] No Non-negotiable was weakened, or there's a decision entry **and** the human's explicit approval.

## Gotchas agents get wrong

- **ZAP "Suspicious Comments" (10027) reads words in served JS and CSS comments** (`from`, `select`, `query`, `admin`, `todo`, `bug`…) as hints of SQL or debug leftovers, and each match is a WARN that fails the 0-WARN gate. Keep comments in `static/` free of those words (or don't ship comments: minify), rather than triaging the rule away.

Each of these was a real finding in the reference app, fixed test-first:
- **Cookies built without config miss `Secure`.** The flash cookie is set from `IntoResponseParts`, which has no config, so in production it went out without `Secure`. The middleware now hardens every `Set-Cookie`, and the header test fails if any production cookie lacks `Secure` or `HttpOnly`.
- **Authenticated pages were cacheable.** Only kernel pages sent `no-store`, so a logged-in book page could be kept by a shared cache or the back/forward cache. Now any request carrying a session cookie gets `no-store`, static assets excepted.
- **Headers set inside `CatchPanicLayer` vanish on panics.** A layer only decorates responses produced *inside* it, so the hardening middleware sits outside the panic and timeout layers.
- **`upgrade-insecure-requests` over plain http breaks redirects.** See the matrix: TLS-only directives key off an `https://` `APP_URL`.
- **Testing headers on endless responses:** use `TestApp::request_head`, which returns status and headers without reading the body, for SSE. Use a raw TCP handshake against `support::serve` for the WebSocket 101.
