# Dev Tools (`/dev`, a small Phoenix LiveDashboard)

> **TL;DR**
> - Every app gets `/dev` (index, routes, system, mailbox, mailer previews) and `/admin/jobs`, built by `new`, never stubbed; mounted only in development (`/admin/jobs` needs admin access in production), which listens on 127.0.0.1 and refuses foreign `Host` names (no login on `/dev`).
> - Kernel pages have their own layout: every link into them carries `hx-boost="false"`, every response is `Cache-Control: no-store`, and they use no `confirm()` dialogs.
> - Pending migrations: in development every HTML request shows a page with a Run migrations button; `/up` is 503 in production while any are pending.
> - Agents read email via `/dev/mailbox/latest.json?to=…`; the kernel-page browser smoke is `.offrails/tools/browser-smoke/run.sh`.
> - **Ask the running app, don't guess:** `GET /dev/introspect.json` (or `app introspect`) answers routes, redacted config, migrations, jobs, streams and subscribers, assets and CSS staleness. Development only; a test proves it's absent in production.
> - **HTTPS in development only with a locally trusted certificate** (mkcert). Chrome drops `Set-Cookie` on a self-signed one even after "Proceed (unsafe)", so login bounces. No mkcert: `bin/dev` serves plain HTTP and says why. Smokes never pass `--ignore-certificate-errors`.
> - **Hot reload:** `bin/dev` rebuilds and restarts on Rust/template edits, Tailwind watches CSS, and `static/dev-reload.js` reloads open tabs (a 1 s poll of `/dev/reload`, never a held stream).
>
> Read the rest when changing a kernel page, debugging pending migrations, setting up dev HTTPS, or running the browser smoke.

**Use the part.** The `dev-dashboard` (and `dev-mailbox`) [parts](samples.md#the-samples) are this guide built, tested on SQLite and Postgres and security-reviewed, with more on top: `/dev/dashboard`, a LiveDashboard: live charts, requests per route, trace waterfalls, SQL fingerprints with N+1 detection and EXPLAIN, a live log tail, a routes explorer and a schema browser. When the product needs this, add the part rather than writing it from this guide: `cargo offrails add-part dev-dashboard dev-mailbox` (dependencies come first; [workflow](../workflow.md#when-the-product-grows-a-need-add-the-part)). Keep its `// SECURITY:` markers and the tests its CHECKS.md cites. This guide stays the spec: hand-write from it only when no part fits, or the human declined the part.

In development the app serves a toolbox at `/dev`. It's for the human clicking around and for the agent curling around. Keep it modest: it shows what the app already knows, with no metrics pipeline and no profiler.

**`generators/new.md` builds every page below in every app.** None is optional, stubbed or "added later".

| Route | What | Spec |
|-------|------|------|
| `GET /dev` | Index: tiles with live numbers. Mailbox (unread / total), Jobs (executing / available / retryable / discarded), Mailer previews (count), Routes (count), System (pending migrations). **Try it** buttons (Send a test email, Enqueue a test job). The agent curl cheat sheet below | here |
| `POST /dev/jobs/test` | Enqueue a harmless `CleanupJob`; redirect to `/admin/jobs/list` with "Enqueued test job N." | here |
| `POST /dev/migrations` | The pending-migrations page's **Run migrations** button | [below](#pending-migrations) |
| `/dev/mailbox` (+ Previews tab at `/dev/mailbox/previews`) | Every email the app sent, rendered; mailer previews in the same viewer; JSON for agents | [mailers](mailers.md#the-dev-mailbox-kernelmailboxrs-swooshs-local-mailbox) |
| `/admin/jobs` | The jobs dashboard (also in production, behind admin access) | [jobs-dashboard](jobs-dashboard.md) |
| `GET /dev/routes?q=` | The `ROUTES` table: method, path, handler, access badge. One GET search box over all three columns | [routing](routing.md) |
| `GET /dev/system` | `APP_ENV`, database backend + redacted host/file, `FRONTEND_MODE`, queues, inline worker, `SMTP_URL` configured or not, crate version + git sha (`APP_REVISION`), uptime; migrations applied and **pending** (`Migrator::get_applied_migrations` / `get_pending_migrations_read_only`, sea-orm-migration 2.0: the read-only variant never creates `seaql_migrations`) | here |
| `GET /dev/introspect.json` | The app's state as JSON, for agents | [below](#introspection) |
| `GET /dev/reload` | `{"version": "<boot id>:<asset digest>"}`, polled by `static/dev-reload.js` | [below](#hot-reload) |
| `GET /dev/mailers` | 301 → `/dev/mailbox/previews` (previews live in the mailbox now). Build it as `(StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/dev/mailbox/previews")])`: axum's `Redirect::permanent` sends **308** | [mailers](mailers.md#previews) |

## Files

```
src/controllers/dev.rs          router(): /dev, /dev/routes, /dev/system, /dev/sockets (with channels), /dev/introspect.json, /dev/reload, /dev/mailers (redirect), POST /dev/jobs/test, POST /dev/migrations
src/kernel/introspect.rs        snapshot(state, frontend_mode, live), BOOT_ID, reload_version() (the full file is below)
src/kernel/css.rs               class_lists, missing_classes (template classes with no rule in static/app.css), css_escape, has_selector
static/dev-reload.js            the reload poll (htmx mode; TS modes use Vite's own reload: islands rebuild with --watch and the poll sees the new digest)
src/controllers/migrations.rs   pending(), the development guard middleware, the Run button handler
src/kernel/mailbox.rs           router(PREVIEWS): /dev/mailbox/…
src/kernel/ui.rs                shared chrome (with the jobs dashboard), BOOTED_AT for uptime
templates/kernel/dev/           index.html, routes.html, system.html, pending.html, sockets.html + _sockets.html (polled every 2s)
templates/kernel/mailbox/       index.html, _list.html, _message.html
```

`controllers/dev.rs` holds a `pub const FRONTEND_MODE: &str` matching the app's `AGENTS.md` header. `Config` gains `revision: Option<String>` (`APP_REVISION`).

## Rules

- **Mounted only when `config.env == AppEnv::Development`**, in `routes.rs`:

  ```rust
  if config.is_development() {
      app = app
          .merge(dev::router())
          .merge(mailbox::router(crate::mailers::previews::PREVIEWS));
  }
  ```

  In production the routes **don't exist** (404, not hidden). Every `/dev` route is in `ROUTES` with `Access::Dev`, and `security_test.rs` requests each one with a production `Config` and expects 404.
- **Reachable from this machine only.** `/dev` has no login, so development binds `127.0.0.1` (`APP_BIND`) and `kernel::hosts` refuses any `Host` that isn't localhost, `APP_URL`'s or an `APP_ALLOWED_HOSTS` entry (DNS rebinding would otherwise read the mailbox same-origin). Opening it from a phone or a dev container takes `APP_BIND=0.0.0.0`, `APP_DEV_REMOTE=1` and `APP_ALLOWED_HOSTS`, and exposes the mailbox to that network ([security](security.md#development-is-this-machine-only)).
- Server-rendered Askama + vendored htmx + `static/kernel/admin.css` + `static/kernel/admin.js` in every `FRONTEND_MODE`, sharing the jobs dashboard's header (tabs: Overview · Jobs · Cron · Dev · Mailbox · Sockets · Routes) and its flash banner.
- **Every link from an app page into a kernel page carries `hx-boost="false"`**: the "Dev tools" link, the auth screens' "Check your mailbox" hint (`paths::dev_mailbox()`), any nav item into `/admin/jobs`. The htmx-mode app layout has `<body hx-boost="true">`, and a boosted navigation swaps only `<body>`, so a kernel page (its own `<head>` and stylesheet) renders **unstyled** until a hard refresh. Verified in Chrome. `kernel_web_test.rs::app_links_into_kernel_pages_are_not_boosted` scans an app page and fails on any boosted `/dev` or `/admin` link. (Inertia/react apps: use a plain `<a href>`, never `<Link>`/router navigation, for kernel pages.)
- **Never cached, never stale on Back:** every `/dev`, `/dev/mailbox` and `/admin/jobs` router ends with `.layer(kernel::ui::no_store())`, so all their responses (pages, fragments, JSON, downloads, redirects) are `Cache-Control: no-store`. The kernel layout sets `historyCacheSize: 0`, and `admin.js` reloads pages restored from the back/forward cache. URLs of things that no longer exist redirect with a flash instead of a bare 404 (deleted message → inbox, deleted job → job list).
- Never display secrets: the database shows backend and host (or file path) only, never credentials (`redact_db`, unit-tested), and `SMTP_URL` shows "configured" or "not set (dev mailbox)".
- In development the app layout's footer (htmx) or `Layout` component (inertia/spa) shows a small "Dev tools" link to `/dev`. Render it only when the server says `env == development` (`Ctx.dev` / a shared prop).
- `bin/dev` migrates first, then prints the exact URL with its scheme, `→ https://localhost:${PORT}` (or `http://` on the fallback, below) and `…/dev (mailbox, jobs, routes)`, and once more as `✓ ready: <url> (HTTP/2|HTTP/1.1)` when the port accepts connections, so it isn't buried under build output.

## HTTPS in development

Live pages need HTTP/2, and browsers only speak HTTP/2 over TLS ([realtime](realtime.md#many-tabs-stall-http11-vs-http2)). **But a certificate the browser doesn't trust breaks login:** after the user clicks "Proceed to localhost (unsafe)", Chrome silently drops `Set-Cookie`. The server answers 303 with a valid cookie, the account is created, and the browser bounces back to the login page ("email doesn't work"). Verified by a peer in a real Kanban; the same flow over `http://localhost` works.

So: **dev serves HTTPS only with a locally trusted certificate.**

| Situation | `bin/dev-cert` | `bin/dev` serves |
|---|---|---|
| mkcert installed and its CA installed (`mkcert -install`, once per machine) | writes `storage/dev-certs/{cert,key}.pem`, exit 0 | **HTTPS, HTTP/2**: `→ https://localhost:3000` |
| mkcert installed, CA not installed (mkcert prints "the local CA is not installed in the system trust store") | refuses, deletes the files, says `mkcert -install`, exit 3 | plain HTTP/1.1, with the message below |
| no mkcert | refuses (no self-signed fallback), says how to install mkcert, exit 3 | plain HTTP/1.1, with the message below |
| `DEV_CERT_SELF_SIGNED=1` | an openssl self-signed certificate, with a warning that browsers drop cookies on it | **both, from one server:** HTTPS + h2 on `PORT` (for `curl -k` and scripted h2 checks) and plain HTTP/1.1 on `APP_DEV_HTTP_PORT` (default `PORT+1`) for the browser. `bin/dev` prints both URLs and says to open the http one |
| `APP_TLS=off` | not run | plain HTTP/1.1, no message |

The fallback message says: no locally trusted certificate, so plain HTTP/1.1 at `http://localhost:PORT`; login and everything work; live updates stall past about 6 tabs (HTTP/1.1 allows ~6 connections per origin); install mkcert (`brew install mkcert` · `apt install mkcert libnss3-tools`), run `mkcert -install` once, restart `bin/dev`; self-signed certificates are refused because Chrome drops login cookies on them. `bin/dev` runs `bin/dev-cert` on every start (mkcert is instant), so a stale 0.6.0 self-signed certificate in `storage/dev-certs/` is replaced or refused, never reused. The scripts are in [new](../generators/new.md#scripts--ops-files) and [realtime](realtime.md#many-tabs-stall-http11-vs-http2).

### The `bin/dev` script

htmx mode (islands add `pnpm --dir web exec vite build --watch --mode development &`; `inertia` adds Vite; `spa` serves the Rust app over http behind Vite's proxy). Copy it; verified in the reference app in all three certificate situations. The PID file and process-tree stop were verified on macOS with a mock tree started from a non-interactive shell (the caller survives, every descendant and the PID file are gone, a second start refuses); `--watch=always --minify` with Tailwind 4.3.3 (a rebuild after a template edit stays minified):

```bash
#!/usr/bin/env bash
# Everything for development in one terminal: server (rebuilt on change) + Tailwind watcher.
set -euo pipefail
cd "$(dirname "$0")/.."
port="${PORT:-3000}"

# One PID file per server: stop exactly this bin/dev with `kill "$(cat storage/pids/dev-$PORT.pid)"`.
pidfile="storage/pids/dev-$port.pid"
mkdir -p storage/pids
if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  echo "bin/dev is already running on port $port (PID $(cat "$pidfile")). Stop it: kill \$(cat $pidfile)" >&2
  exit 1
fi
if command -v lsof >/dev/null && lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null; then
  echo "port $port is taken by another process (lsof -nP -iTCP:$port -sTCP:LISTEN); pick another: PORT=… bin/dev" >&2
  exit 1
fi
echo $$ > "$pidfile"
# On exit (Ctrl-C, `kill <pid>`, an error) stop our own process tree, leaves first, and nothing else.
# Never `kill 0`: started from an agent's shell, bin/dev shares that shell's process group, and
# `kill 0` would kill the caller too.
children() { ps -A -o pid= -o ppid= | awk -v p="$1" '$2 == p { print $1 }'; }
stop_tree() { local c; for c in $(children "$1"); do stop_tree "$c"; done; kill "$1" 2>/dev/null || true; }
cleanup() { trap - EXIT INT TERM; rm -f "$pidfile"; for c in $(children $$); do stop_tree "$c"; done; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# HTTPS + HTTP/2 only with a locally trusted certificate (bin/dev-cert: mkcert + `mkcert -install`).
# Without one, plain HTTP/1.1: login works, live pages stall past ~6 tabs. APP_TLS=off forces http.
scheme=http
open_url=""   # set when the URL a person should open differs from the main one
if [ "${APP_TLS:-on}" != "off" ]; then
  if [ "${DEV_CERT_SELF_SIGNED:-}" = 1 ] && [ -f storage/dev-certs/cert.pem ] || bin/dev-cert; then
    export APP_TLS_CERT=storage/dev-certs/cert.pem APP_TLS_KEY=storage/dev-certs/key.pem
    scheme=https
    if [ "${DEV_CERT_SELF_SIGNED:-}" = 1 ]; then
      # Untrusted certificate: browsers drop cookies on it. Same server, plain HTTP beside it.
      export APP_DEV_HTTP_PORT="${APP_DEV_HTTP_PORT:-$((port + 1))}"
      open_url="http://localhost:$APP_DEV_HTTP_PORT"
    fi
  else
    cat <<MSG
!
! No locally trusted certificate, so bin/dev serves plain HTTP/1.1 at http://localhost:$port
!   Login and everything else work. Live updates stall past about 6 tabs (HTTP/1.1 allows ~6
!   connections per origin; HTTP/2 needs HTTPS).
!   For HTTPS + HTTP/2: install mkcert (brew install mkcert · apt install mkcert libnss3-tools),
!   run 'mkcert -install' once, then restart bin/dev. Self-signed certificates are refused:
!   Chrome drops login cookies on them even after "Proceed (unsafe)". APP_TLS=off hides this.
!
MSG
  fi
fi

echo "Compiling and migrating (the first build takes several minutes)…"
cargo run -q -- db migrate

# Rebuild CSS on every template change, so a new class is never missing from static/app.css.
# --minify as in the production build: without it the watcher overwrites a minified file with an
# unminified one, and any check of the built CSS sees a different file in development.
bin/tailwindcss -i assets/app.css -o static/app.css --watch=always --minify &
# Migrate on every rebuild too, so a new migration never meets a stale database.
cargo watch -q -w src -w templates -w migration -x 'run -q -- db migrate' -x 'run -- serve' &

# The exact URL, scheme included (a bare localhost:$port is http:// to the browser), again once the
# server accepts connections, so it isn't buried under the build output.
if [ -n "$open_url" ]; then
  echo "→ open in your browser: $open_url (HTTP/1.1; the certificate on :$port is self-signed and browsers drop cookies on it)"
  echo "→ for scripts and h2 checks: $scheme://localhost:$port (curl -k; never a browser login)"
else
  echo "→ $scheme://localhost:$port"
fi
echo "→ ${open_url:-$scheme://localhost:$port}/dev (mailbox, jobs, routes)"
(until (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; do sleep 1; done
 echo "✓ ready: ${open_url:-$scheme://localhost:$port} ($([ "$scheme" = https ] && [ -z "$open_url" ] && echo "HTTP/2" || echo "HTTP/1.1"))") &
wait
```

**`APP_DEV_HTTP_PORT`** (development only; `Config` refuses it in any other environment, and it does nothing without TLS): `serve` binds a second, plain listener and serves the **same router and `AppState`** on it, so streams, presence and sessions are shared with the TLS port. The CSRF layer also trusts `http://localhost:<port>`. Dev session cookies aren't `Secure`, so the browser keeps them there. Verified by a peer (dev-92) in a stock Chrome profile and in the reference app: login sticks and the page goes live on the http port, while the self-signed TLS port fails the login smoke's preflight. `bin/dev` in that mode:

```
→ open in your browser: http://localhost:3001 (HTTP/1.1; the certificate on :3000 is self-signed and browsers drop cookies on it)
→ for scripts and h2 checks: https://localhost:3000 (curl -k; never a browser login)
```

**A bare `localhost:3000`** typed into the address bar is `http://` to the browser. Against a TLS listener that used to show `ERR_INVALID_HTTP_RESPONSE`. `serve`'s TLS branch now sniffs the first byte of each connection (a TLS handshake starts with `0x16`) and answers anything else with a **307** to `https://<Host><path>` (`kernel/tls.rs`, [deployment](deployment.md#direct-tls-optional)). 307, not 308: a cached permanent redirect would break `APP_TLS=off` on the same port later.

**Never** tell a user to click through the warning, and never run a browser check with `--ignore-certificate-errors` or `acceptInsecureCerts`: that's exactly the setting under which the bug is invisible. The framework's smokes launch Chrome with no certificate overrides and fail on an untrusted certificate ([below](#browser-smoke)).

## Hot reload

Templates are compiled (Askama), so "hot reload" is the boring loop, all inside `bin/dev`:

1. **Rust and templates:** `cargo watch -w src -w templates -w migration` rebuilds, migrates and restarts `serve`.
2. **CSS:** `bin/tailwindcss -i assets/app.css -o static/app.css --watch=always --minify` rewrites the CSS when a template or `assets/app.css` changes. In development the asset catalog rescans per request, so the next page gets the new digest.
3. **The browser:** the layout loads `static/dev-reload.js` when `ctx.dev`. It polls `GET /dev/reload` once a second while the tab is visible and calls `location.reload()` when `version` changes: a new `BOOT_ID` (the server restarted) or a newer file under `static/` (`version` is `BOOT_ID:<newest mtime>:<file count>`; mtimes, not digests, because every open tab asks once a second). While the server is down mid-rebuild, the fetch fails and it keeps polling.

Why a poll and not the signal stream or a socket: on the plain-HTTP fallback every held connection costs one of the ~6 per origin, and a reload stream in every tab would halve the tabs a live page survives. One short request a second costs nothing. Measured in the reference app: a CSS-only edit reached an open tab in 0.6 s; a template edit in about 17 s, of which ~9 s is the incremental build inside the watcher (Tailwind rebuilding alongside), ~1.7 s startup, up to 1 s the poll, the rest the page load.

**Faster rebuilds:** the skeleton `Cargo.toml` has

```toml
[profile.dev]
debug = "line-tables-only"   # backtraces keep file:line; lldb shows no locals
```

and `.cargo/config.toml` adds `-C split-debuginfo=unpacked` on Linux (macOS's default already; Windows MSVC refuses it, which is why it left `[profile.dev]` in 0.7.2). More build-speed settings, and what was measured: [testing](testing.md#build-speed).

A peer measured a template edit going from 16–23 s to 3.6–5.9 s and a Rust edit from 11–12 s to 4.4–5.3 s (a Kanban, M2). In the smaller reference app on a newer machine incremental builds were ~5 s with or without it (not reproduced there; it costs nothing). For a debugging session with locals: `CARGO_PROFILE_DEV_DEBUG=true cargo run`. A page with unsaved form input loses it on reload; that's development.

```js
// static/dev-reload.js: development only (the layout loads it when ctx.dev)
(() => {
  let seen = null;
  const tick = async () => {
    if (document.visibilityState === "visible") {
      try {
        const r = await fetch("/dev/reload", { cache: "no-store", headers: { Accept: "application/json" } });
        const v = r.ok && r.headers.get("content-type")?.includes("json") ? (await r.json()).version : null;
        if (v && seen && v !== seen) return location.reload();
        if (v) seen = v;
      } catch {
        // the server is restarting: keep polling
      }
    }
    setTimeout(tick, 1000);
  };
  tick();
})();
```

The layout line is `{% if ctx.dev %}<script src="/static/dev-reload.js" defer></script>{% endif %}`. `/dev/reload` and `/dev/introspect.json` pass the pending-migrations guard (like `/up`), so they answer while migrations are pending. TS modes: inertia/spa reload through Vite's own dev server or `vite build --watch` (islands), which changes the digest the poll sees.

## Introspection

One way for an agent to ask the running app about its state, instead of grepping logs and guessing. `GET /dev/introspect.json` (development only: it's in `dev::router()`, so it **doesn't exist** in production or test) and `app introspect` (the same snapshot from the CLI; refuses unless `APP_ENV=development`; without the live parts, which only a serving process knows):

| Key | What |
|---|---|
| `app` | name, version, `frontend_mode`, `boot_id`, `booted_at` |
| `url` | `app_url`, `tls`, the browser protocol (`h2`, or `http/1.1` and its ~6-tab limit) |
| `config` | the `Debug` of `Config`: an allowlist, values only for never-secret settings and `set`/`not set` for the rest ([config](config.md#srcconfigrs); never a raw env dump) |
| `routes` | every `ROUTES` row: method, path, handler, access |
| `migrations` | applied count, `pending` names, `current` |
| `jobs` | counts by state and by queue (`admin::stats`); `null` while the table doesn't exist |
| `realtime` (live) | open signal subscriptions, connections per member and transport, channel topics with subscribers and presence, frame counters, node(s). With per-stream pub/sub ([realtime](realtime.md#kernel-spec-srckernelpubsubrs)) it lists each stream and its subscriber count |
| `assets` | the layout's digested asset URLs |
| `css` | `built`, `stale`, and up to 50 `missing_classes` (template classes with no rule in `static/app.css`, from `kernel::css::missing_classes`) |

The whole file. Copy it, then change only the lines marked `// APP:`. It's the reference implementation an upgrading app wrote from this spec (axum 0.8.9, sea-orm 2.0.4, 2026-10-13), with the app-specific lines generalized:

```rust
//! Ask the running app, don't guess (.offrails/guides/dev-tools.md#introspection): the snapshot behind
//! `GET /dev/introspect.json` and `app introspect`, and the version `GET /dev/reload` hands to
//! `static/dev-reload.js`. Development only: the routes live in `controllers::dev::router()`, which
//! production and test never mount, and `app introspect` refuses outside development.

use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

use migration::{MigrationStatus, Migrator, MigratorTrait}; // migration/src/lib.rs: `pub use sea_orm_migration::MigrationStatus;`
use serde_json::{Map, Value, json};

use crate::kernel::ui::BOOTED_AT;
use crate::kernel::{css, jobs::admin};
use crate::routes::ROUTES;
use crate::state::AppState;

/// One id per process: a restart changes it, so open tabs reload.
pub static BOOT_ID: LazyLock<String> =
    LazyLock::new(|| format!("{}-{}", std::process::id(), BOOTED_AT.timestamp_micros()));

/// The most missing classes listed (the count is always exact).
const MAX_MISSING: usize = 50;

/// `BOOT_ID:<newest mtime under static/ in ms>:<file count>`. Mtimes, not digests: every open tab
/// asks once a second.
pub fn reload_version() -> String {
    let (mut newest, mut count) = (0u128, 0usize);
    walk(Path::new("static"), &mut newest, &mut count);
    format!("{}:{newest}:{count}", *BOOT_ID)
}

fn walk(dir: &Path, newest: &mut u128, count: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, newest, count);
        } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            *count += 1;
            let ms = modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            *newest = (*newest).max(ms);
        }
    }
}

/// The app's state as JSON. `live` (the HTTP handler) adds what only a serving process knows.
pub async fn snapshot(state: &AppState, frontend_mode: &str, live: bool) -> anyhow::Result<Value> {
    let c = &state.config;
    let tls = c.tls.is_some();
    let migrations = Migrator::get_migration_with_status(&state.db).await?;
    let applied = migrations.iter().filter(|m| matches!(m.status(), MigrationStatus::Applied)).count();
    let pending: Vec<String> = migrations
        .iter()
        .filter(|m| matches!(m.status(), MigrationStatus::Pending))
        .map(|m| m.name().to_owned())
        .collect();
    // null while the jobs table doesn't exist yet (a fresh database before `db migrate`).
    let jobs = match admin::stats(&state.db).await {
        Ok(s) => json!({
            "by_state": s.by_state.iter().map(|(st, n)| (state_name(st), json!(n))).collect::<Map<_, _>>(),
            "by_queue": s.by_queue.iter().map(|(q, counts)| {
                (q.clone(), Value::Object(counts.iter().map(|(st, n)| (state_name(st), json!(n))).collect()))
            }).collect::<Map<_, _>>(),
        }),
        Err(_) => Value::Null,
    };
    let css = match css::missing_classes() {
        None => json!({ "built": false, "stale": false, "missing_count": 0, "missing_classes": [] }),
        Some(missing) => json!({
            "built": true,
            "stale": !missing.is_empty(),
            "missing_count": missing.len(),
            "missing_classes": missing.iter().take(MAX_MISSING).collect::<Vec<_>>(),
        }),
    };
    let assets = state.assets.assets();
    Ok(json!({
        "app": {
            "name": crate::config::APP_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "framework": fs::read_to_string(".offrails/VERSION").ok().and_then(|v| v.lines().next().map(str::to_owned)),
            "frontend_mode": frontend_mode,
            "env": format!("{:?}", c.env).to_lowercase(),
            "boot_id": *BOOT_ID,
            "booted_at": BOOTED_AT.to_rfc3339(),
        },
        "url": {
            "app_url": c.app_url,
            "tls": tls,
            "dev_http_port": c.dev_http_port,
            "protocol": if tls { "h2" } else { "http/1.1" },
            "note": if tls {
                "HTTPS with ALPN h2 (a locally trusted certificate)"
            } else {
                "plain HTTP/1.1: browsers hold ~6 connections per origin, so live pages stall past ~6 tabs"
            },
        },
        "config": format!("{c:?}"), // Config's hand-written Debug redacts credentials (config.md)
        "routes": ROUTES.iter().map(|(method, path, handler, access)| json!({
            "method": method, "path": path, "handler": handler, "access": format!("{access:?}"),
        })).collect::<Vec<_>>(),
        "migrations": { "applied": applied, "pending": pending, "current": pending.is_empty() },
        "jobs": jobs,
        "realtime": if live { realtime(state) } else { Value::Null },
        "live": live,
        "assets": { "css": assets.css, "app_js": assets.app_js, "htmx": assets.htmx, "htmx_sse": assets.htmx_sse },
        "css": css,
    }))
}

fn state_name(st: &impl std::fmt::Debug) -> String {
    format!("{st:?}").to_lowercase()
}

/// APP: what only the serving process knows about live connections. No realtime: `Value::Null`.
fn realtime(state: &AppState) -> Value {
    json!({
        // pubsub.streams(): (stream, receiver count) per live per-stream sender (realtime.md)
        "streams": state.pubsub.streams().into_iter().map(|(s, n)| json!({ "stream": s, "subscribers": n })).collect::<Vec<_>>(),
        "topic_count": state.pubsub.topic_count(),
        // with channels: the /dev/sockets snapshot (connections per member and transport, topics, presence,
        // frame counters, nodes). `Snapshot` derives Serialize.
        // "channels": serde_json::to_value(state.hub.snapshot()).unwrap_or(Value::Null),
        // with SSE presence: the label per room
        // "presence": state.presence.labels(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_version_starts_with_the_boot_id() {
        let v = reload_version();
        assert!(v.starts_with(&format!("{}:", *BOOT_ID)), "{v}");
        assert_eq!(v.split(':').count(), 3, "{v}");
    }
}
```

The handlers (`controllers/dev.rs`, both in `router()` and in `ROUTES` with `Access::Dev`) and the CLI arm:

```rust
/// `GET /dev/introspect.json`: the app's state for agents.
pub async fn introspect(State(s): State<AppState>) -> Result<Response, AppError> {
    Ok(Json(kernel::introspect::snapshot(&s, FRONTEND_MODE, true).await?).into_response())
}

/// `GET /dev/reload`: polled once a second by `static/dev-reload.js`.
pub async fn reload() -> Response {
    Json(serde_json::json!({ "version": kernel::introspect::reload_version() })).into_response()
}

// main.rs, `Command::Introspect` (needs the database, not the listener):
anyhow::ensure!(config.is_development(), "app introspect is development only (APP_ENV is {:?})", config.env);
let state = AppState::new(config, db)?;
println!("{}", serde_json::to_string_pretty(&kernel::introspect::snapshot(&state, controllers::dev::FRONTEND_MODE, false).await?)?);
```

The router's `.layer(kernel::ui::no_store())` already covers both responses. Apps without the jobs kernel drop `jobs` (and its import); `api` mode has no `css` or `assets` (drop both keys and the `css` import). Add `pub fn streams(&self) -> Vec<(String, usize)>` to `PubSub` (under the map lock: each stream and its `receiver_count()`) when the app has signals.

Tests (`kernel_web_test.rs`): in development it answers JSON with every key above, lists itself in `routes`, and contains none of a configured admin password or SMTP credentials; with a production **and** a test `Config`, `/dev/introspect.json` and `/dev/reload` are 404 and pages don't load `dev-reload.js`. Add both paths to `ROUTES` with `Access::Dev`.

**The development CSS banner** uses the same check. Every UI app has one: if `Ctx` has no `css_warning` field yet, add it (`Option<String>`, set by the `Ctx` extractor only when `config.is_development()`, rendered as the first plain-text line in `<main>`; [frontend-htmx](frontend-htmx.md#assets--cache-busting-kernelassetsrs)). An mtime check can't do this: Tailwind 4 doesn't rewrite unchanged output.

```rust
// src/kernel/css.rs (next to missing_classes)
/// The development CSS banner: the CSS is missing, or stale (template classes with no rule).
pub fn css_warning() -> Option<String> {
    if let Some(problem) = crate::kernel::assets::app_css_problem() {
        return Some(problem);
    }
    let missing = missing_classes()?;
    let first = missing.first()?;
    Some(format!(
        "{CSS_BUILT} is stale: {} template class(es) have no CSS rule (first: {first}). bin/dev rebuilds it.",
        missing.len()
    ))
}
// the Ctx extractor:
css_warning: if s.config.is_development() { crate::kernel::css::css_warning() } else { None },
```

## No `confirm()` dialogs in kernel pages

Kernel pages aren't boosted, so `hx-confirm` never fires there, and a native `confirm()` dialog would block the browser smoke tool. Destructive kernel actions are either cheap and dev-only (mailbox **Clear all**, deleting a job: no prompt), or go through a **server-rendered confirmation page** ("Retry 1,234 jobs?", which posts back with `confirmed=1`). If an app page needs a quick confirm, use `data-confirm="…"` on the form, handled in `static/app.js` (`submit` listener calling `confirm()`), and make browser tests accept dialogs (`page.on('dialog', d => d.accept())`).

## Pending migrations

Like Rails' pending-migration error page and Phoenix's "There are pending migrations" screen: a database behind the code shows a clear page, not a "no such table" 500.

`src/controllers/migrations.rs` (generated by `new`):

```rust
/// Read-only: never creates seaql_migrations (a fresh database reports every migration pending).
pub async fn pending(db: &DatabaseConnection) -> Result<Vec<String>, DbErr> {
    Ok(Migrator::get_pending_migrations_read_only(db).await?.iter().map(|m| m.name().to_owned()).collect())
}
pub async fn guard(State(s): State<AppState>, OriginalUri(uri): OriginalUri, req: Request, next: Next) -> Response;
pub async fn run(State(s): State<AppState>, Form(f): Form<RunForm>) -> Result<Response, AppError>;   // POST /dev/migrations
```

| Environment | Behavior |
|-------------|----------|
| development | `routes.rs` adds `app.layer(from_fn_with_state(state.clone(), migrations::guard))` **after** the routes and the `.fallback(...)` are added (a layer only wraps what exists when it's applied, so a fallback added later would skip the guard and unknown paths would 404 instead of showing the page). Every request except `/static/*`, `/up` and `POST /dev/migrations` gets a **503 "N pending migrations" page** (kernel layout, so it works before any app table exists) listing them, with a **Run migrations** button: a CSRF-protected form posting `return_to=<current path>` to `/dev/migrations`, which runs `Migrator::up`, flashes "Ran N migrations." and redirects back (only to same-site paths) |
| production | **Never auto-migrates**: the entrypoint runs `app db migrate` before `serve` ([deployment](deployment.md)). The guard and `/dev/migrations` aren't mounted. `GET /up` returns **503 "pending migrations"** while any is pending, so a deploy that skipped migrating never goes healthy |
| worker (`app worker`, inline worker) | Before starting, checks `pending()`. If any are pending it logs **once** `pending migrations: run \`app db migrate\`; worker waiting`, rechecks every 5s (stopping cleanly on shutdown), then logs `migrations applied; worker starting`. It never spams errors against missing tables |

**Nothing in `serve` may read an app table before the listener is up.** The guard renders from the kernel layout and `seaql_migrations` alone; everything else that touches app tables at boot (realtime's pub/sub and cluster, the inline worker, a cache warm-up) waits for `pending()` to be empty ([realtime](realtime.md#kernel-spec-srckernelpubsubrs)). **Fresh-database smoke**, after any change to `serve` or to what starts at boot (`bin/dev` migrates first, so it never shows this):

```sh
mkdir -p storage/pids && rm -f storage/fresh.sqlite3
PORT=4321 DATABASE_URL='sqlite://storage/fresh.sqlite3?mode=rwc' sh -c 'echo $$ > storage/pids/serve-4321.pid; exec cargo run -q -- serve' & sleep 5
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:4321/          # 503: the pending-migrations page, not a crash
curl -s -o /dev/null -w '%{http_code}\n' -X POST -H 'Sec-Fetch-Site: same-origin' -d return_to=/ http://localhost:4321/dev/migrations   # 303
sleep 6; curl -s -o /dev/null -w '%{http_code}\n' http://localhost:4321/up   # 200; the log says "migrations applied; realtime starting"
kill "$(cat storage/pids/serve-4321.pid)"; rm -f storage/pids/serve-4321.pid storage/fresh.sqlite3*
```

`bin/dev` runs `cargo run -- db migrate` before serving, and again on every rebuild (`cargo watch -x 'run -q -- db migrate' -x 'run -- serve'`), so the page is mostly seen after a `git pull` with the server stopped, or after a rollback. Tests (`kernel_web_test.rs`): a fully rolled-back dev DB shows the page (with the migration names, the button and `return_to`), `/up` is 503, the kernel stylesheet still loads, the button applies all and redirects back with the flash, no open redirect; production with one pending: `/up` 503, `/` unaffected, `/dev/migrations` 404.

## Browser smoke

HTTP tests can't see boosted navigation, `<head>` swaps, the back/forward cache or CSP violations. After changing kernel pages (or the app layout's links into them), drive a real browser:

1. **Preferred: the chrome-devtools MCP** (navigate, click, screenshot, `list_console_messages`), in an isolated context.
2. **When it can't attach** (Chrome's profile lock, CI, a script): the framework ships a ready smoke that launches the installed Chrome headless with a throwaway profile:

   ```sh
   BASE=http://localhost:3000 bash .offrails/tools/browser-smoke/run.sh
   # Linux: CHROME=/usr/bin/google-chrome BASE=… bash .offrails/tools/browser-smoke/run.sh
   ```

   It opens `/dev` directly (styled?; an app's home need not link to it) → seeds 3 mails with the **Send a test email** button (each lands on its message, `/dev/mailbox/<id>`, with the flash "Sent a test email to your mailbox."; the list then shows at least 3) → opens one (styled, sandboxed iframe) → **Clear all** → Back (none of the cleared messages comes back) → Forward (judged by the cleared messages' ids, not an empty list: other smokes `verify --full` runs alongside sign in, and an auth part's "New sign-in" email may arrive meanwhile) → (when `/dev` has an **Enqueue a test job** button) opens and deletes the job → Back (redirected to the list, flash shown), and fails on any CSP violation. Every step waits with `waitForNavigation`/`waitForSelector`, with no fixed sleeps, and seeds only through the dev-only buttons, so it needs no app routes.

3. **Multi-tab live updates** (any app with live pages): open N tabs in one profile on a live page, change something once, and assert every tab updates. It catches the HTTP/1.1 connection pool and an undersized per-user cap ([realtime](realtime.md#many-tabs-stall-http11-vs-http2)). The app supplies a small scenario module, `tests/e2e/live-tabs.scenario.mjs`: `export const path` (the live page), optional `login(page, base)`, `mutate(page, base)` returning a token, and `seen(page, token)` returning whether a tab shows it (the full contract is the header of `.offrails/tools/browser-smoke/smoke-live-tabs.mjs`):

   ```sh
   SCENARIO=tests/e2e/live-tabs.scenario.mjs BASE=https://localhost:3000 TABS=12 \
     bash .offrails/tools/browser-smoke/run.sh live-tabs
   ```

   It exits 1 if any tab misses the update, and says whether the page was on HTTP/1.1. Run it with 12 and 40 tabs after any realtime change.

4. **Login sticks** (every app with accounts; part of the UI check in the Definition of Done): a real browser with **no certificate overrides** fills and submits the real login (or sign-up) form, then proves a fresh page load is still logged in. It catches the dev-TLS trap above and any cookie a browser refuses (`Secure` on http, `__Host-` with a `Domain`). The scenario exports `login(page, base)` and `loggedIn(page)`:

   ```sh
   SCENARIO=tests/e2e/smoke.scenario.mjs BASE=<the URL bin/dev printed> bash .offrails/tools/browser-smoke/run.sh login
   ```

5. **axe in both color schemes:** axe-core (WCAG 2.2 AA) on each path with `prefers-color-scheme` emulated light, then dark; logged-out paths from `PATHS` or the scenario's `a11yPaths`, and `a11yAuthPaths` after `login`. It finds a `bg-white` card left light under a body that turned dark. The app's Playwright suite ([test-standards §10](test-standards.md#10-accessibility-wcag-22-aa-in-every-theme)) stays the CI gate; this is the framework's own check, and it needs no Node in the app:

   ```sh
   SCENARIO=tests/e2e/smoke.scenario.mjs BASE=… bash .offrails/tools/browser-smoke/run.sh a11y
   ```

6. **Every page after a boosted link** (every htmx app): each GET page in `docs/routes.md` on a full load, then reached through a boosted link from `/`; the same buttons must react, with no console errors, and no page may load its own scripts in `<head>`. Signed out, then signed in as the demo account. No npm install (Node ≥ 18 and the installed Chrome); `verify --full` runs it when `BASE` is set ([testing](testing.md#browser-smoke-boosted-navigation)):

   ```sh
   BASE=<the URL bin/dev printed> node .offrails/tools/browser-smoke/smoke-boosted.mjs   # from the app root
   ```

**Every smoke launches Chrome without certificate overrides** and starts with a preflight: an untrusted HTTPS certificate fails it with the explanation above, before any check runs. Verified in the reference app: against a forced self-signed certificate (`DEV_CERT_SELF_SIGNED=1`) `login` and `live-tabs` fail at the preflight; against the plain-HTTP fallback and against a trusted certificate both pass, and `live-tabs` reaches 12/12 tabs over h2.

**Why it lives in `.offrails/tools/` and not in the app:** htmx-mode apps have no Node toolchain or `package.json`, and this check isn't part of the Definition of Done or CI. So `run.sh` installs `puppeteer-core` (pinned, verified 2026-10-12: 25.13.0, which needs **Node ≥ 22.12**; Node 24 LTS recommended) and `axe-core` (4.13.0) once into `~/.cache/offrails/browser-smoke` and runs a copy of the script from there. The app's dependency tree, lockfiles and stack rules are untouched, and inertia/spa apps use the same script. It's optional tooling for agents and humans; the guaranteed checks remain the Rust tests above. Verified against the bookshelf app: 11/11 checks pass. Removing `hx-boost="false"` from the dev link, or omitting `includeIndicatorStyles:false`, makes it fail.

## For agents

These endpoints are your eyes on the running app. Use them in the "ran the app and exercised it" step of the [Definition of Done](../workflow.md#definition-of-done). **Start with introspection**; it answers most "why doesn't X work" questions before you read code:

```sh
curl -s $URL/dev/introspect.json | jq '.migrations, .css, .realtime, .url'  # $URL: what bin/dev printed (-k never needed)
cargo run -q -- introspect | jq '.routes[] | select(.path | test("books"))'
curl -s "$URL/dev/mailbox/latest.json?to=ada@example.com"  # did the email go out? follow .links[0]
cargo run -- jobs list --state discarded --json                      # did a job fail?
cargo run -- jobs stats                                              # is anything stuck?
curl -s $URL/dev/system | grep -i pending                       # forgot to migrate?
```
