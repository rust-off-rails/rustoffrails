# Config, Environments & Secrets

> **TL;DR**
> - One `Config` struct parsed from env vars once (`Config::from_env`); never `std::env::var` deep in the code.
> - `APP_ENV` = development | test | production; `.env` is loaded only in development; `.env.example` lists every variable with dev defaults (no real secrets).
> - In development `APP_URL` follows `PORT`; normalize it (no trailing slash).
> - `serve` listens on `APP_BIND`: 127.0.0.1 in development, 0.0.0.0 in production; an unset `APP_ENV` (= development) warns at every boot.
> - Secrets live in the deploy platform's secret store, never in git or images.
>
> Read the rest when adding a setting, a secret, or environment-specific behavior.

| Rails | Off Rails |
|-------|-----------|
| `RAILS_ENV` | `APP_ENV` = `development` \| `test` \| `production` |
| `config/environments/*.rb` | One `match config.env` per concern, inside `Config::from_env` or the kernel module that cares |
| `database.yml` | `DATABASE_URL` (+ `DATABASE_POOL`) |
| `credentials.yml.enc` + master key | Env vars, set through the deploy platform's secret store |
| `Rails.application.config.x.foo` | A typed field on `Config` |
| `.env` (dotenv-rails) | `dotenvy`, loaded **only** in development |

## `src/config.rs`

```rust
/// The human product name: page titles, kernel UIs (/dev, /admin/jobs), the default MAIL_FROM.
/// Never derived from the crate name (env!("CARGO_PKG_NAME")): crates get renamed, products don't.
pub const APP_NAME: &str = "Bookshelf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv { Development, Test, Production }

#[derive(Debug, Clone)]
pub struct Config {
    pub env: AppEnv,
    pub database_url: String,
    pub database_pool: u32,           // DATABASE_POOL, default 4 on SQLite, 10 on Postgres
    pub worker_threads: usize,        // APP_WORKER_THREADS, default 2: Tokio workers (main builds the runtime from it)
    pub blocking_threads: usize,      // APP_BLOCKING_THREADS, default 16: cap on Tokio's blocking pool (Tokio's own: 512)
    pub app_url: String,              // APP_URL, absolute, no trailing slash: used in emails and the origin check
    pub port: u16,                    // PORT, default 3000
    pub inline_worker: bool,          // APP_INLINE_WORKER: default true in dev, and in prod on SQLite
    pub queues: Vec<(String, usize)>, // APP_QUEUES, default "default:10,mailers:5" (queue:concurrency per node)
    pub jobs_prune_days: u32,         // APP_JOBS_PRUNE_DAYS, default 7: how long finished jobs stay visible
    pub jobs_shutdown_grace: u64,     // APP_JOBS_SHUTDOWN_GRACE seconds, default 25
    pub shutdown_drain: Duration,     // APP_SHUTDOWN_DRAIN_SECS, default 5: after the signal, open connections get this long, then are dropped (realtime.md)
    pub refresh_wait: Duration,       // not an env var: 5 s (Config::test(): 100 ms); how long a live re-fetch waits at the gate before 503 (realtime.md)
    pub admin_password: Option<String>, // APP_ADMIN_PASSWORD: HTTP Basic for /admin/jobs when the app has no admin role
    pub smtp_url: Option<String>,     // SMTP_URL: production delivery (ignored in development unless MAILER=smtp)
    pub mailer: Option<String>,       // MAILER = smtp | local | log: overrides the per-environment default (mailers.md)
    pub mail_from: String,            // MAIL_FROM, default "<App> <no-reply@localhost>"
    pub trusted_proxy: bool,          // APP_TRUSTED_PROXY: read the client IP from X-Forwarded-For (required behind any proxy; rate limiting)
    pub trusted_proxy_hops: u8,       // APP_TRUSTED_PROXY_HOPS (default 1): how many proxies append to the header; the client is that many hops from the right
    pub log_format: LogFormat,        // LOG_FORMAT = pretty | json (default: json in production)
    pub revision: Option<String>,     // APP_REVISION: git sha of the build, shown on /dev/system
    pub tls: Option<TlsFiles>, // APP_TLS_CERT + APP_TLS_KEY (PEM paths): `serve` speaks HTTPS + h2 itself (deployment.md#direct-tls-optional); both or neither, one alone fails the boot.
    pub dev_http_port: Option<u16>, // APP_DEV_HTTP_PORT: development only (any other APP_ENV fails the boot); with TLS on, the same router on plain HTTP beside it (dev-tools.md#https-in-development)
                               // The development APP_URL default follows it: https://localhost:{PORT} when set (CSRF compares origins).
    pub bind: IpAddr,                 // APP_BIND: the address `serve` listens on. Default 127.0.0.1 in development (this machine only), 0.0.0.0 otherwise (security.md#development-is-this-machine-only)
    pub allowed_hosts: Vec<String>,   // APP_ALLOWED_HOSTS: extra Host names development answers (a tunnel, a LAN name); ".example.com" = its subdomains too
    pub env_defaulted: bool,          // always false: an unset APP_ENV refuses to boot (kept for parts that read it)
    // apps with an API (api.md):
    // pub cors_origins: Vec<String>, // APP_CORS_ORIGINS: exact origins, comma-separated; `*` fails at boot (config::parse_cors_origins)
    // added with the first attachment field (storage.md):
    // pub storage: StorageConfig,    // STORAGE_SERVICE, STORAGE_PATH, STORAGE_BUCKET, STORAGE_ENDPOINT, STORAGE_REGION, STORAGE_PUBLIC_HOST
    // app-specific settings: APP_* prefixed, typed, documented in .env.example
}

impl Config {
    /// Called once in main. Development: `dotenvy::dotenv().ok()` first.
    /// Collects ALL missing/invalid vars, then fails with one message listing them.
    pub fn from_env() -> anyhow::Result<Self>;
    pub fn test() -> Self;            // the test suite's config: APP_ENV=test, sqlite::memory:, never reads env
    pub fn is_production(&self) -> bool;
    pub fn is_development(&self) -> bool;
    pub fn boot_warnings(&self) -> Vec<String>; // below; `main` logs each at warn right after tracing starts
}

/// "default:10,mailers:5" → [("default", 10), ("mailers", 5)]; also used by `app worker --queues`.
pub fn parse_queues(s: &str) -> Result<Vec<(String, usize)>, String>;
```

### Where `serve` listens (`APP_BIND`, `APP_ALLOWED_HOSTS`) and the unset-`APP_ENV` warning

Development mounts `/dev` (the mailbox with every login link, introspection, the jobs dashboard) **without a login**, so it listens on this machine only, and an unset `APP_ENV` is said out loud at every boot ([security](security.md#development-is-this-machine-only)). Copy these functions and their tests into `config.rs` (from an app generated from these docs, 2026-10-16: compiled, clippy-clean, green on SQLite and Postgres):

```rust
use std::net::{IpAddr, Ipv4Addr};

/// `APP_BIND`, or the environment's default: loopback in development, every interface otherwise.
// SECURITY: development serves /dev tools without a login; on 0.0.0.0 anyone on the same Wi-Fi
// could open them. A dev container sets APP_BIND=0.0.0.0 and APP_DEV_REMOTE=1 explicitly
// (test: development_binds_loopback_unless_told_otherwise).
pub fn bind_address(env: AppEnv, raw: Option<&str>) -> Result<IpAddr, String> {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.trim_start_matches('[').trim_end_matches(']').parse().map_err(|_| {
            format!("APP_BIND (expected an IP address such as 127.0.0.1 or 0.0.0.0, got {v:?})")
        }),
        None if env == AppEnv::Development => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        None => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    }
}

/// `APP_ALLOWED_HOSTS`: comma-separated host names, lowercased.
pub fn parse_allowed_hosts(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

impl Config {
    /// Loud warnings `main` logs right after logging starts (parts add theirs here).
    pub fn boot_warnings(&self) -> Vec<String> {
        Vec::new()
    }
}
```

`APP_ENV` is **required**. `from_env` reads it; when it is unset it loads `.env` (bin/setup writes `APP_ENV=development` there) and reads again; still unset is an error naming the fix, never a default:

```rust
// SECURITY: an unset APP_ENV is an error, never a default. Development mounts /dev tools
// without a login, so "forgot to set it on a server" must not silently become development
// (test: an_unset_app_env_refuses_to_boot).
if get_env().is_none() {
    dotenvy::dotenv().ok();
}
let env = match get_env().as_deref() {
    Some("development") => AppEnv::Development,
    Some("test") => AppEnv::Test,
    Some("production") => AppEnv::Production,
    Some(other) => anyhow::bail!("invalid APP_ENV: {other} (development | test | production)"),
    None => anyhow::bail!("APP_ENV is not set. Locally: APP_ENV=development in .env (bin/setup creates it \
                           from .env.example). On every server: APP_ENV=production (the Dockerfile sets it)."),
};
// … with the other settings; a bad value joins the one "invalid env vars" message:
let bind = bind_address(env, get("APP_BIND").as_deref()).unwrap_or_else(|problem| {
    invalid.push(problem);
    IpAddr::V4(Ipv4Addr::LOCALHOST)
});
let allowed_hosts = parse_allowed_hosts(get("APP_ALLOWED_HOSTS").as_deref());
```

`Config::test()` sets `bind: IpAddr::V4(Ipv4Addr::LOCALHOST)`, `allowed_hosts: Vec::new()`, `env_defaulted: false`; the hand-written `Debug` prints all three (none is a secret). `main` runs `for w in config.boot_warnings() { tracing::warn!("{w}"); }` right after `init_tracing` (`app routes` returns before `from_env`: the route table needs no config), and `serve` binds `SocketAddr::new(config.bind, config.port)` (and `APP_DEV_HTTP_PORT` on the same `config.bind`). The boot test lives in `tests/app/kernel_test.rs` (an integration test, where `CARGO_BIN_EXE_offrails_app` exists), the rest in `config.rs`:

```rust
/// Runs the binary in a directory with no `.env`, so the fallback has nothing to load.
#[test]
fn an_unset_app_env_refuses_to_boot() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_offrails_app"))
        .arg("db").arg("status").env_remove("APP_ENV").current_dir(dir.path()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("APP_ENV is not set"));
}

#[test]
fn development_binds_loopback_unless_told_otherwise() {
    let (local, any) = (IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    assert_eq!(bind_address(AppEnv::Development, None), Ok(local));
    assert_eq!(bind_address(AppEnv::Development, Some("")), Ok(local));
    assert_eq!(bind_address(AppEnv::Development, Some("0.0.0.0")), Ok(any)); // a container opts in
    assert_eq!(bind_address(AppEnv::Development, Some("[::1]")), Ok("::1".parse().unwrap()));
    assert_eq!(bind_address(AppEnv::Production, None), Ok(any)); // behind the platform's proxy
    assert!(bind_address(AppEnv::Development, Some("localhost")).is_err());
    assert_eq!(parse_allowed_hosts(Some(" Dev.Example.com, .tunnel.example ,")), ["dev.example.com", ".tunnel.example"]);
}
```

- `Config` holds credentials, so implement `Debug` by hand **as an allowlist**: print the value only for settings that can never hold a secret (`env`, `port`, `app_url`, `database_pool`, `queues`, the numbers, durations and booleans, `bind`, `allowed_hosts`, `log_format`, `revision`, `mail_from`), the database as its backend and host only (`redact_db`), and `set` / `not set` for everything else, `smtp_url`, `admin_password` and every field added later included, until you add it to the allowlist on purpose. A denylist (`[redacted]` for three named fields) leaks the next secret someone adds (`STRIPE_SECRET_KEY`, a webhook signing key) on `/dev/introspect.json` and `app introspect`. Test `debug_shows_no_secret_values`: a `Config` with a sentinel (`"sentinel-secret"`) in every string field outside the allowlist, and `format!("{c:?}")` contains no sentinel.

- **Env var names are string literals in `config.rs`** (`var("APP_URL")`, `parse("PORT", "3000")`, never a name built with `format!`), and every `"SCREAMING_SNAKE"` literal there is an env var: `offrails-check`'s `[env]` rule reads them all and fails on any missing from `.env.example`. A literal that isn't one gets a trailing `// not an env var`.
- **Read env vars only in `config.rs`.** Nowhere else calls `std::env::var` (the AWS keys read by `object_store`'s `from_env()` are the one sanctioned exception).
- `Config` lives in `AppState` (`state.config`), wrapped in `Arc` if it gets large.
- **Fail fast in production.** Missing `DATABASE_URL`, `APP_URL`, or `SMTP_URL` (when email is enabled) aborts boot with: `missing required env vars: APP_URL, SMTP_URL (see .env.example)`.
- Development has defaults for everything except `APP_ENV` itself, so `cargo run` works once `.env` says `APP_ENV=development` (`bin/setup` writes it):
  - `DATABASE_URL=sqlite://storage/development.sqlite3?mode=rwc`
  - `APP_URL=http://localhost:{PORT}`: it follows `PORT` (`config::dev_app_url(port)`), so emailed links and the CSRF origin check still work when you run on another port. Parse `PORT` before `APP_URL`.
- `APP_ENV` unset **refuses to boot** with a message naming the fix (above): on a server an unset `APP_ENV` is a missing setting that would mount `/dev` and `/admin/jobs` without a login, and a default is not a gate. The Dockerfile sets `APP_ENV=production`, and so must every deploy config. `TestApp` builds `Config` directly (`Config::test()`) and never reads the environment.
- `serve` listens on `APP_BIND`: **127.0.0.1 in development**, 0.0.0.0 in production (the container's interface, behind the platform's proxy). A dev container, or a phone on your LAN, needs `APP_BIND=0.0.0.0`, `APP_DEV_REMOTE=1` **and** its host name in `APP_ALLOWED_HOSTS` ([security](security.md#development-is-this-machine-only)).

### Runtime threads (`APP_WORKER_THREADS`, `APP_BLOCKING_THREADS`)

`main` parses `Config` first, then builds the Tokio runtime from it (no `#[tokio::main]`): 2 worker threads and at most 16 blocking threads by default, instead of one worker per core and 512 blocking threads. Each idle thread holds a stack and an allocator arena, so a small app on a many-core box stays small. Raise `APP_WORKER_THREADS` when request handlers do real CPU work (rendering big pages, image processing) on a box with cores to spare, and `APP_BLOCKING_THREADS` when `spawn_blocking` work (password hashing, file I/O) queues under load. I/O-bound apps rarely gain past 2–4 workers. The file is [`template/app/src/main.rs`](../template/app/src/main.rs); [database](database.md#sqlite-memory) has the SQLite side and the measured numbers.

## Per-environment behavior

Put it in one place, as a `match`:

```rust
// src/state.rs: select_mailer(&config, &db). The kernel only has constructors (Mailer::smtp/local/memory, Mailer::Log).
Ok(match (c.env, c.mailer.as_deref()) {
    (AppEnv::Test, _) => Mailer::memory(),
    (_, Some("smtp")) => smtp(from)?,                           // needs SMTP_URL: try real delivery from development
    (_, Some("local")) => Mailer::local(db.clone(), from),
    (_, Some("log")) => Mailer::Log,
    (_, Some(other)) => anyhow::bail!("MAILER must be smtp, local or log (got {other:?})"),
    (AppEnv::Development, None) => Mailer::local(db.clone(), from),   // ALWAYS the dev mailbox, even with SMTP_URL set
    (AppEnv::Production, None) if c.smtp_url.is_some() => smtp(from)?,
    (AppEnv::Production, None) => Mailer::Log,                  // EMAIL: not yet (logged only)
})
```

**Rate limits are per environment too** ([security](security.md#rate-limiting-kernelrate_limitrs)): in development every limit is **1000 per window** (`rate_limit::DEV_LIMIT`), so a person or agent retrying a form locally never locks themselves out; test and production use each call site's own limit (the 11th login is refused in tests exactly as in production). `RateLimiter::new(config.env)` decides it, once; call sites never branch on the environment.

**Never use `cfg!(debug_assertions)` or `#[cfg(debug_assertions)]` for behavior.** A release build running locally must behave like development when `APP_ENV=development`.

## `.env.example`

This file is the documentation for every variable. One line each, grouped, with a comment:

```sh
# --- core ---
# Required. development | test | production; an unset APP_ENV refuses to boot (development mounts /dev with no login).
APP_ENV=development
PORT=3000
# APP_URL=https://myapp.example.com   # required in production; in development it follows PORT (http://localhost:{PORT}), so leave it unset
DATABASE_URL=sqlite://storage/development.sqlite3?mode=rwc
# DATABASE_URL=postgres://postgres:postgres@localhost:5432/myapp_development
DATABASE_POOL=10
# APP_BIND=0.0.0.0             # development listens on 127.0.0.1 only (its /dev tools have no login); a dev container opts in here. Production default: 0.0.0.0
# APP_DEV_REMOTE=1             # required with a non-loopback APP_BIND in development: everyone on that network can then read /dev and the mailbox
# APP_ALLOWED_HOSTS=dev.example.com,.lan.example   # development answers only localhost and APP_URL's host (DNS rebinding); add a tunnel or LAN name ('.' = subdomains too)
APP_INLINE_WORKER=true
# APP_TRUSTED_PROXY=true       # behind Fly, Render or any proxy: read the client IP from X-Forwarded-For (REQUIRED behind any proxy, or every user shares one rate-limit key)
# APP_TRUSTED_PROXY_HOPS=1     # proxies that append to X-Forwarded-For before the app: 1 = one platform proxy (Fly, Render), 2 = Cloudflare → Caddy → app
# --- realtime (channels.md) ---
# APP_NODE=web-1               # this process's name on /dev/sockets and in the cluster (default <hostname>:<pid>)
# APP_CLUSTER=true             # Postgres only: share rooms and presence across nodes; refused at boot on SQLite
# APP_REALTIME_REVALIDATE_SECS=60    # long-lived sockets/streams re-check their session this often (logout closes them at once)
# APP_REALTIME_TRANSPORTS=websocket,sse,longpoll   # drop `websocket` behind a proxy that mishandles it; clients fall back
# --- jobs ---
APP_QUEUES=default:10,mailers:5
APP_JOBS_PRUNE_DAYS=7
APP_JOBS_SHUTDOWN_GRACE=25
# APP_SHUTDOWN_DRAIN_SECS=5    # web: open connections get this long after SIGTERM (streams end first); keep it below the platform's kill timeout
# APP_ADMIN_PASSWORD=          # production: protects /admin/jobs when there's no admin role
# --- email: development always uses the dev mailbox (/dev/mailbox); SMTP_URL is for production ---
# SMTP_URL=smtps://user:pass@smtp.provider.com:465
# MAILER=smtp                  # only to try real delivery from development (needs SMTP_URL)
MAIL_FROM="Myapp <no-reply@localhost>"
# --- storage (added with the first attachment field) ---
# STORAGE_SERVICE=local
# STORAGE_PATH=storage/files
# STORAGE_PUBLIC_HOST=        # only when serving S3 presigned URLs (storage.md)
# APP_REVISION=               # git sha, set by the deploy (shown on /dev/system)
# --- API (apps with an API; guides/api.md) ---
# APP_CORS_ORIGINS=https://app.example,http://localhost:5173   # browser apps on other origins; exact, never *
# --- direct TLS (optional; default: plain HTTP behind the platform's TLS proxy) ---
# APP_TLS_CERT=/etc/app/tls/cert.pem
# APP_TLS_KEY=/etc/app/tls/key.pem
# APP_DEV_HTTP_PORT=3001        # development only, with TLS on: the same app on plain http (bin/dev sets it with DEV_CERT_SELF_SIGNED=1)
# --- logging ---
RUST_LOG=info,myapp=debug
LOG_FORMAT=pretty
```

`bin/setup` copies it to `.env` if `.env` is missing. `.env` is git-ignored, always.

## Secrets in production

There's no encrypted credentials file. Secrets come from the platform:

| Target | How |
|--------|-----|
| Fly.io | `fly secrets set SMTP_URL=… ` |
| Kamal | `.kamal/secrets` (reads from a password manager or env; never commit raw values) + `env.secret` in `config/deploy.yml` |
| Railway / Render | Service variables in the dashboard or CLI |

When you add an env var: update `Config`, `.env.example`, and the deploy config. **Then tell the human which secret to set and where.** You can't set platform secrets for them unless they've given you CLI access.

## Gotchas agents get wrong

- Reading `std::env::var("X")` deep inside a handler. Add a `Config` field instead.
- Calling `dotenvy::dotenv()` when `APP_ENV` is already set, so a stray `.env` in a production image overrides real config. Only call it when `APP_ENV` is unset (to find it) or `development`.
- Trailing slash on `APP_URL`, which produces `//` in email links. Normalize it in `from_env`.
- Committing real values to `.env.example`. Only dev defaults and placeholders go there.
- Deriving the product name from `env!("CARGO_PKG_NAME")`. Renaming the crate (for example, to avoid a binary-name clash with another app on the same machine) then renames the product in every page title. Use `config::APP_NAME`.
