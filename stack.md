# The Stack (the "Gemfile")

One crate per job. Add each one with `cargo add` (and JS packages with `pnpm add`). **Never write version numbers from memory.**

## Selection rubric (how these were chosen; do not re-run it)

1. **The model knows it.** Crates with huge training-data presence produce fewer hallucinated APIs.
2. **Types over strings.** Prefer APIs where mistakes are compile errors.
3. **Portable.** The same code must run on SQLite (local, tests, small prod) and Postgres (prod).
4. **Stable API.** Crates that churn break agent-written code.
5. **Fewer crates.** If ~150 lines of generated code replace a niche crate, generate the code from a spec (see "Kernel" below).

## Blessed crates

"Added by" says when a crate enters the app: `new` adds the first group (exact commands in [`generators/new.md`](generators/new.md#workspace--dependencies)); the rest arrive with the generator or first feature that needs them. Adding everything up front costs compile time, and `object_store` brings a second TLS crypto provider (below).

| Job | Crate(s) and features | Added by | Notes |
|-----|-----------------------|----------|-------|
| Async runtime | `tokio` (`full`), `tokio-stream` (`sync`), `futures` | `new` | `tokio-stream` for SSE; `futures` for `catch_unwind` in the job worker |
| HTTP framework | `axum` (`multipart`; `ws` with the first channel room), `axum-extra` (`cookie`, `typed-header`, `form`) | `new` | axum 0.8+: path params are `/{id}`, **not** `/:id` |
| Middleware | `tower`, `tower-http` (`fs`, `trace`, `compression-gzip`, `set-header`, `timeout`, `request-id`, `catch-panic`, `csrf`) | `new` | `csrf` is tower-http's `CsrfLayer` (the Go 1.25 `Sec-Fetch-Site` scheme), so CSRF is not generated ([security](guides/security.md#csrf)) |
| ORM / Active Record | `sea-orm` (`sqlx-sqlite`, `sqlx-postgres`, `runtime-tokio-rustls`, `macros`, `with-chrono`, `with-json`, `with-uuid`) | `new` | One codebase, both backends. `DATABASE_URL` picks the backend at runtime |
| Migrations | `sea-orm-migration` (`runtime-tokio-rustls`, `sqlx-sqlite`, `sqlx-postgres`), in the `migration/` crate | `new` | Rust DSL that emits correct DDL per backend. Embedded in the binary |
| Templates (`htmx` mode, mailers, kernel pages) | `askama` | `new` | Render with `.render()?` and wrap in `axum::response::Html`. Do **not** use `askama_axum` (removed upstream) |
| Serialization | `serde` (`derive`), `serde_json` | `new` | |
| Forms with arrays/nesting | `serde_html_form` | `new` | The kernel uses it directly (the jobs dashboard's filter links; `kernel/storage.rs` re-parses multipart fields), so every app has it. `axum-extra`'s `Form` uses its own internal copy |
| Time | `chrono` (`serde`) | `new` | `DateTimeUtc` everywhere; local time only in views |
| IDs | `i64` primary keys; `uuid` (`v4`, `serde`) for public tokens and test databases | `new` | |
| Errors | `thiserror` (`AppError`), `anyhow` (`main.rs`, tasks, jobs, and the `AppError::Internal` catch-all) | `new` | |
| Logging/tracing | `tracing`, `tracing-subscriber` (`env-filter`, `json`) | `new` | |
| CLI | `clap` (`derive`, `env`) | `new` | |
| Config | `dotenvy` (dev only) + env vars parsed into a struct | `new` | No config-file crates |
| Randomness / tokens | `rand`, `base64`, `sha2` | `new` | Tokens are random; only their SHA-256 is stored. rand 0.10: `rand::random_range(a..=b)`, `rand::rng()` |
| Email | `lettre` with **`default-features = false`** and features `builder`, `smtp-transport`, `pool`, `hostname`, `tokio1-rustls-tls` | `new` | lettre's defaults include `native-tls`; with `tokio1-rustls-tls` on top that's a **compile error**. `cargo add lettre --no-default-features --features …` |
| In-process cache, rate limits | `moka` (`future`) | `new` | Shared cache lives in the DB |
| Cron expressions | `croner` | `new` | Recurring jobs only |
| Password hashing | `argon2` (default features) | `authentication` (password variants) | 0.6 makes its own salt: `Argon2::default().hash_password(pw.as_bytes())`. 0.5's `SaltString::generate(&mut OsRng)` no longer compiles ([auth](guides/auth.md#passwords-password-variants)) |
| File storage | `object_store` (`aws`), `bytes` | first `attachment` field | Local disk in dev, memory in tests, S3-compatible in prod. 0.14: `put`/`get`/`delete` live on `ObjectStoreExt` (import it) ([storage](guides/storage.md)) |
| Markdown / rich text | `pulldown-cmark`, `ammonia` | first `rich_text` field, or `new` for a content/docs site | Always sanitize with ammonia ([views](guides/views.md#rich-text)) |
| TS type export (`inertia`/`spa`/islands) | `ts-rs` (`chrono-impl`, `serde-json-impl`) | `new` (TS modes) | `#[derive(TS)]` on every struct the frontend sees. Never hand-write TS types for server data |
| Web server | `axum::serve` on **hyper** (HTTP/1.1 + HTTP/2 cleartext); `axum-server` (`tls-rustls`) + `rustls` (`aws_lc_rs`, `std`, no default features) for direct TLS (h2) | `new`: always, since live pages need h2 in dev (`bin/dev` serves HTTPS) | No app server, no reverse proxy, no Nginx: the binary is the server. TLS terminates at the platform proxy (Fly, Kamal-proxy, Render, Railway); `APP_TLS_CERT`/`APP_TLS_KEY` serve HTTPS directly (h2 via ALPN) for a single box ([deployment](guides/deployment.md#direct-tls-optional)) |
| Public REST API docs (`API: rest \| both`) | `utoipa` (`axum_extras`, `chrono`) | `new` when `API` includes rest | OpenAPI 3.1 from annotations, served at `/api/openapi.json`, committed as `docs/openapi.json`. Not `utoipa-scalar`/`utoipa-swagger-ui` ([api](guides/api.md#openapi-api-rest--both)) |
| GraphQL (`API: graphql \| both`) | `async-graphql` (`chrono`, `dataloader`), `async-graphql-axum` | `new` when `API` includes graphql | 7.x (8.0 is a release candidate). Depth ≥ 13 or introspection breaks ([graphql](guides/graphql.md)) |
| CORS (any `API`) | `tower-http` feature `cors` | `new` when `API` is set | Exact origins from `APP_CORS_ORIGINS`, never `*`, no credentials ([api](guides/api.md#cors)) |
| Testing | built-in `#[test]`/`#[tokio::test]`; dev-deps `tower` (`util`), `http-body-util`, `tempfile`, `insta` (`json`, `redactions`) | `new` | `insta` for API contract snapshots ([test-standards](guides/test-standards.md)). Runner: `cargo-nextest` |

### Verified against (template 0.7.5, 2026-11-20)

The date says when these versions were last built and tested, not when they were current. The crates the template uses are at the versions in `template/app/Cargo.lock` for 0.7.5, which `tools/ci/test-template.sh` compiles, lints (`clippy -D warnings`) and tests on SQLite and Postgres for every release. The rest (lettre, croner, tokio-tungstenite, ts-rs, async-graphql, utoipa, argon2, object_store, GraphiQL, @scalar/api-reference) are from the `bookshelf` reference app (htmx, SQLite + Postgres 18; 2026-10-12, the API demo 2026-10-13), generated from these docs with `cargo add` and checked the same way. **This table is orientation, not a pin.** Agents still `cargo add` and read `Cargo.lock`. When a resolved major/minor differs from this table, read that crate's changelog before writing code against it.

| Crate | Version | | Crate | Version |
|-------|---------|-|-------|---------|
| rustc / cargo | 1.99 | | sea-orm, sea-orm-migration, sea-orm-cli | 2.0.4 |
| axum | 0.8.9 | | sea-query (via sea-orm) | 1.0.2 |
| axum-extra | 0.12.6 | | sqlx (via sea-orm) | 0.9.1 |
| tower / tower-http | 0.5.3 / 0.7.1 | | askama | 0.16.1 |
| tokio | 1.54.1 | | lettre | 0.11.23 |
| serde / serde_json | 1.0.229 / 1.0.154 | | moka | 0.12.16 |
| chrono | 0.4.45 | | croner | 4.0.1 |
| uuid | 1.29.3 | | rand | 0.10.4 |
| clap | 4.6.8 | | base64 | 0.23.1 |
| thiserror / anyhow | 2.0.21 / 1.0.104 | | sha2 | 0.11.1 |
| tracing / tracing-subscriber | 0.1.44 / 0.3.23 | | serde_html_form | 0.4.1 |
| insta | 1.49.0 | | cargo-nextest, cargo-watch | latest via `cargo install --locked` |
| tokio-tungstenite (dev, channel tests) | 0.29.0 (match axum's; `cargo add --dev tokio-tungstenite@0.29`) | | | |
| ts-rs (inertia branch) | 12.0.1 | | | |
| async-graphql, async-graphql-axum (api demo, 2026-10-13) | 7.2.1 | | utoipa (api demo) | 6.0.0 |
| axum-server (`tls-rustls`) | 0.9.2 | | rustls (`aws_lc_rs`) | 0.23.46 |
| GraphiQL (CDN, dev page) | 4.1.2 | | @scalar/api-reference (CDN, dev page) | 1.74.0 |
| argon2 (auth branch) | 0.6.0 | | object_store / bytes (auth branch) | 0.14.2 / 1.12.1 |

Exercised by the reference app's `auth` branch (2026-10-12): `argon2` 0.6.0, `object_store` 0.14.2, `bytes` 1.12. Resolved but not yet exercised: `pulldown-cmark` 0.13, `ammonia` 4.2. (`ts-rs` is exercised on the reference app's `inertia` branch; set `TS_RS_LARGE_INT = "number"`, see [frontend-inertia](guides/frontend-inertia.md#vite-wiring-verified-with-vite-8-vitejsplugin-react-6-tailwind-4).)

### Version gotchas found while verifying

- **MSRV-aware resolver.** Edition 2024 resolves dependencies to versions compatible with the workspace's `rust-version`. `sea-orm-cli migrate init` writes `rust-version = "1.85.0"` into `migration/Cargo.toml`, which silently downgrades crates (e.g. `uuid`). Delete that line ([new.md step 3](generators/new.md#workspace--dependencies)).
- **sea-query 1.0 (SeaORM 2):** expression methods (`.add()`, `.eq()` on `Expr`) live on the `ExprTrait` trait. Importing it at module level makes `i32::max`/`min` ambiguous, so import it inside the function that needs it. `LIKE … ESCAPE` is `Column::X.like(LikeExpr::new(pattern).escape('\\'))`.
- **SeaORM 2 raw statements:** `ConnectionTrait::execute`/`query_one`/`query_all` now take a query *builder*. For a `Statement`, use `execute_raw` / `query_one_raw` / `query_all_raw` (`execute_unprepared(&str)` is unchanged).
- **SeaORM SQLite pool:** if `max_connections` isn't set, SeaORM uses **1** for SQLite. `kernel/db.rs` always sets it.
- **tower-http 0.7:** `TimeoutLayer::new` is deprecated; use `TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, d)`. The compression layer now answers 406 when a client refuses every encoding.
- **Two rustls crypto providers.** `lettre`/`sqlx` use `ring`; `object_store` (via `reqwest` 0.13) brings `aws-lc-rs` (`cargo tree -i aws-lc-rs`). A rustls client built from the *process default* provider would panic with both compiled in ("no process-level CryptoProvider"). **Checked 2026-10-12 with object_store 0.14.2 and lettre 0.11.23: no panic.** Real TLS handshakes to S3 and to SMTPS (port 465) succeeded without `install_default`, because both crates pass their provider explicitly. Re-check after upgrading either crate. If it ever panics, `cargo add rustls --no-default-features --features ring` and call `let _ = rustls::crypto::ring::default_provider().install_default();` first in `main` and `TestApp::new`. (`aws-lc-sys` also needs `cmake` and a C compiler at build time. The `rust:*-trixie` image has them.)

## Databases

| Environment | Default | Supported |
|-------------|---------|-----------|
| Development | SQLite `sqlite://storage/development.sqlite3?mode=rwc` | Postgres |
| Test        | SQLite in-memory, one DB per test | Postgres (CI matrix, required) |
| Production  | Postgres | SQLite on a persistent volume (single server, like Rails 8) |

Portability rules are in [`guides/database.md`](guides/database.md). In short: everything goes through SeaORM, backend-specific SQL is allowed only in `src/kernel/` or behind an explicit `match` on the backend, and the test suite proves both backends.

## Frontend modes

Every app has exactly one `FRONTEND_MODE`, chosen during the [interview](interview.md) and recorded in the app's `AGENTS.md`. Don't mix modes. Islands are the one sanctioned hybrid. Modes with a JS UI (`inertia`, `spa`, islands) also have one `UI_LIBRARY`: **`react`** (default) or **`svelte`** ([frontend-svelte](guides/frontend-svelte.md)). The mode decides the architecture, and the library decides only what `web/` components are written in.

| Mode | Choose when | Stack | Guide |
|------|-------------|-------|-------|
| `htmx` (default) | CRUD apps, dashboards, content sites, internal tools | Askama + **htmx** (vendored) + `sse` extension + **Tailwind standalone CLI**. No Node | [frontend-htmx](guides/frontend-htmx.md) |
| `htmx` + islands | Mostly server-rendered, with a few rich widgets (editor, canvas, drag-and-drop) | `htmx`, plus Vite-built React or Svelte islands in `web/` | [frontend-htmx#islands](guides/frontend-htmx.md#islands) |
| `inertia` | **The human wants a JS UI (React or Svelte).** App-like UI, with server routing, controllers and sessions kept Rails-style | Inertia protocol (generated `kernel/inertia.rs`) + `@inertiajs/react` or `@inertiajs/svelte` + Vite + TS | [frontend-inertia](guides/frontend-inertia.md) |
| `spa` | The UI must be a standalone single-page app (offline-ish, app-shell UX) talking to its own JSON API | Rust JSON API under `/api` + an SPA: React (Vite + React Router + TanStack Query) or Svelte (SvelteKit in SPA mode + TanStack Query) | [frontend-react](guides/frontend-react.md), [frontend-svelte](guides/frontend-svelte.md#spa-mode-sveltekit) |

| `api` | **No UI**: the product is an API for mobile apps, partners or scripts (`rails new --api`) | Rust JSON API only: REST `/api/v1` + OpenAPI and/or GraphQL (`API` header), bearer tokens. No templates, Tailwind or `web/`; kernel pages (`/dev`, `/admin/jobs`) still exist | [api](guides/api.md), [graphql](guides/graphql.md) |

If the human says "React" or "Svelte" without a reason that points to `spa`, choose `inertia` with that `UI_LIBRARY`. A public API is a separate axis, `API: none | rest | graphql | both`, which any mode can have ([api](guides/api.md)).

### Blessed JS packages (`inertia`, `spa`, islands)

Last verified 2026-10-12 (versions are for orientation only; always `pnpm add` without a version).

| Job              | Package | Verified at |
|------------------|---------|-------------|
| Runtime          | **Node 24 LTS** (≥ 22.22 is the hard floor: React Router 8, jsdom and Vitest 5 refuse older; Node 20 fails). Vite 8's own `engines` says `^20.19.0 \|\| >=22.12.0` (checked in 8.2.2): that's Vite alone, not this stack, so ignore it. The browser smokes need ≥ 22.12 (puppeteer-core 25), which any TS app's Node already meets | 24.21 |
| Package manager  | **pnpm**, pinned per app via `"packageManager"` in `web/package.json` | 12.x |
| Scaffold         | `pnpm create vite web --template react-ts`, then convert per [frontend-inertia](guides/frontend-inertia.md) "Vite wiring" (delete demo files, swap the template's oxlint for Biome) | create-vite 9 |
| Build/dev server | **Vite** + `@vitejs/plugin-react` (entry via `build.rolldownOptions.input`; `rollupOptions` is deprecated) | 8.3 / 6.1 |
| Language         | **TypeScript** as pinned by the template, `strict: true`, `noUncheckedIndexedAccess: true`. Type-check with **`tsc -b`** (project references) | 6.0 |
| UI               | **React** | 19.3 |
| Inertia adapter  | `@inertiajs/react` + `@inertiajs/core` (same version; `inertia` mode only) | 3.9 |
| Routing          | `inertia`: server routes. `spa`: **`react-router`** (data/SPA mode, `createBrowserRouter`; `RouterProvider` from `react-router/dom`). Never `react-router-dom` or framework mode | 8.4 |
| Server state     | `inertia`: page props. `spa`: **TanStack Query** (`@tanstack/react-query`) | 5.104 |
| CSS              | **Tailwind CSS** via `@tailwindcss/vite` (islands: the standalone CLI instead) | 4.3 |
| HTTP             | `inertia`: `router`/`useForm` from `@inertiajs/react` (built-in XHR client; v3 dropped axios). `spa`: `fetch` wrapped in `web/src/api/client.ts` | |
| Unit tests       | **Vitest** + jsdom + `@testing-library/react` + `@testing-library/jest-dom` (`/vitest` entry) | 5.0 / 30 / 16.3 / 7.0 |
| E2E (critical flows; required in `inertia`/`spa`) | **Playwright** (`@playwright/test`) | 1.64 |
| Lint/format      | **Biome** (`biome check --write .` locally, `biome ci .` in CI) | 2.5 |

With `UI_LIBRARY: svelte`, these rows replace their React counterparts (details: [frontend-svelte](guides/frontend-svelte.md)):

| Job              | Package | Verified at |
|------------------|---------|-------------|
| Scaffold         | `inertia`/islands: `pnpm create vite web --template svelte-ts`. `spa`: `pnpm dlx sv create web --template minimal --types ts --no-add-ons` | create-vite 9 / sv 1.1 |
| UI               | **Svelte 5** (runes only) | 5.57 |
| Build plugin     | `@sveltejs/vite-plugin-svelte` (`spa`: through `@sveltejs/kit/vite`) | 7.3 |
| Inertia adapter  | `@inertiajs/svelte` + `@inertiajs/core` | 3.9 |
| Routing (`spa`)  | **SvelteKit** in SPA mode with `@sveltejs/adapter-static` (`ssr = false`, `prerender = false`, `fallback: "index.html"`). Never SSR or server routes | 3.0 / 4.0 |
| Server state (`spa`) | **TanStack Query** (`@tanstack/svelte-query`, accessor API) | 6.3 |
| Type-check       | **`svelte-check --fail-on-warnings`** (not `tsc`, which can't see `.svelte`) | 4.7 |
| Unit tests       | Vitest + jsdom + **`@testing-library/svelte`** (with its `svelteTesting()` Vite plugin) + `@testing-library/jest-dom` | 5.4 |
| Lint/format      | **Biome** with `html.experimentalFullSupportEnabled` (template-aware; no Prettier, no ESLint) | 2.5 |

htmx mode (no Node): **htmx 2.x** (`htmx.org@2`; 4.x is not adopted) + **htmx-ext-sse 2.x**, vendored; **Tailwind v4 standalone CLI** pinned by `TAILWIND_VERSION` in `bin/setup-tailwind`. Verified: htmx 2.0.11, htmx-ext-sse 2.2.4, Tailwind 4.3.3.

## Required tooling (install once per machine)

```sh
cargo install --locked sea-orm-cli cargo-nextest cargo-watch
cargo install --locked cargo-llvm-cov cargo-mutants   # coverage gate + weekly mutation run (test-standards.md §5)
cargo install --locked cargo-deny                     # or `brew install cargo-deny` (verified 0.20.2): deny.toml gate
# gitleaks (verified 8.30.1: `brew install gitleaks`, or CI's ghcr.io/gitleaks/gitleaks:v8.30.1 image): secrets in history
# OWASP ZAP baseline: Docker image ghcr.io/zaproxy/zaproxy:stable (security.md#automated-enforcement)
# htmx mode: bin/setup downloads the Tailwind standalone binary to bin/tailwindcss
# inertia/react/islands: Node 24 LTS (fnm/nvm/mise/Homebrew), then pnpm:
#   corepack enable pnpm        # Node ≤ 24 ships corepack; Node 25+ doesn't, so there use: npm install -g pnpm
# Deploy CLIs only for the chosen target: flyctl, kamal (gem), railway, render — installed when DEPLOY_TARGET is set
# Postgres locally is optional: `docker compose up -d db` when testing against it
# Optional, any mode: Node ≥ 22.12 + an installed Chrome for .offrails/tools/browser-smoke/run.sh
#   (puppeteer-core is cached in ~/.cache/offrails, never added to the app; guides/dev-tools.md#browser-smoke)
```

## The Kernel: code you generate instead of depending on

These pieces are small, easy to get wrong in a generic crate, and central to the Rails experience. Generate them into `src/kernel/` from the specs in the guides. The specs are the framework, and the code is your rendering of them. (CSRF used to be here; tower-http 0.7 now ships the same design as `CsrfLayer`, so it's a dependency instead.)

| Kernel module          | Spec in                        | Replaces |
|------------------------|--------------------------------|----------|
| `kernel/db.rs`         | `guides/database.md`           | `database.yml`, connection setup, SQLite pragmas |
| `kernel/jobs/` (`mod.rs`, `admin.rs`, `web.rs`) | `guides/jobs.md`, `guides/jobs-dashboard.md` | Solid Queue / Active Job / Mission Control (Oban + Oban Web, in spirit) |
| `kernel/mailbox.rs`    | `guides/mailers.md`            | letter_opener_web (Swoosh's `/dev/mailbox`) |
| `kernel/cache.rs`      | `guides/caching.md`            | Solid Cache |
| `kernel/pubsub.rs`     | `guides/realtime.md`           | Solid Cable / Action Cable refresh broadcasts (signals; woken by `NOTIFY` or in-process) |
| `kernel/hub.rs`, `kernel/channels.rs` | `guides/channels.md` | Action Cable channels / Phoenix Channels + Presence: one WebSocket, topics, room actors |
| `kernel/cluster.rs`    | `guides/channels.md`           | libcluster + Horde: `cluster_nodes` heartbeats, `NOTIFY` bus, rendezvous ownership (Postgres, opt-in) |
| `kernel/session.rs`    | `guides/auth.md`               | Rails 8 authentication sessions |
| `kernel/flash.rs`      | `guides/controllers.md`        | `flash[:notice]` (+ the `_errors` cookie in `inertia` mode) |
| `kernel/security.rs`   | `guides/security.md`           | Rails' default headers + `force_ssl` cookie flags + secure_headers gem: the header matrix, `no-store`, production cookie hardening |
| `kernel/rate_limit.rs` | `guides/security.md`           | Rails 8 `rate_limit` |
| `kernel/mailer.rs`     | `guides/mailers.md`            | Action Mailer delivery (`Smtp`/`Local`/`Log`/`Memory`) |
| `kernel/storage.rs`    | `guides/storage.md`            | Active Storage service |
| `kernel/assets.rs`     | `guides/frontend-*.md`         | Propshaft digests / Vite manifest; in development (Vite modes) also serves `/dev/vite-preamble.js` ([frontend-inertia](guides/frontend-inertia.md)) |
| `kernel/inertia.rs`    | `guides/frontend-inertia.md`   | inertia-rails |

## Banned (don't add these, don't propose them)

Enforced mechanically: `deny.toml` `[bans]` fails CI if any of these appears anywhere in the dependency tree. Crates that healthy dependencies pull in transitively (`lazy_static`, `once_cell`, `async-trait`, `log`) are banned as **direct** dependencies only, which the plugin hook and `/offrails:doctor` check ([security](guides/security.md#automated-enforcement)).

| Banned                                   | Why |
|------------------------------------------|-----|
| Diesel, raw `sqlx` queries in app code   | SeaORM is the one data layer. Raw SQL breaks portability (kernel excepted) |
| Actix, Rocket, Poem, Loco, Leptos, Dioxus, Yew | One web stack. No WASM front ends |
| Tera, Handlebars, minijinja              | Runtime templates hide errors until a request hits them |
| `askama_axum`, `axum-inertia` and other niche glue crates | Removed upstream or thin. Generate the glue from the spec |
| `lazy_static`, `once_cell`               | Use `std::sync::LazyLock` / `OnceLock` |
| `async-trait` (as your own dependency)   | Native async fn in traits is stable. Exception: `sea-orm-migration`'s `MigrationTrait` requires it, so use the re-export that crate provides |
| Node/Vite in `htmx` mode (except islands) | Vendored htmx + Tailwind standalone. One sanctioned exception: the dev-only real-browser smoke package in `tests/e2e/` (`@playwright/test` only), which never touches the build, image or runtime ([test-standards §9](guides/test-standards.md#9-real-browser-smoke)) |
| Next.js, Remix/RR framework mode, SvelteKit SSR/server routes/form actions, any SSR JS server | Rust is the only server |
| npm, yarn, webpack, CRA, Jest, ESLint+Prettier | pnpm, Vite, Vitest, Biome |
| Redux, MobX, Zustand (by default)        | Server state is props (`inertia`) or TanStack Query (`spa`); UI state lives in `useState`/context |
| axios, ky                                | Inertia's router or `fetch` through `api/client.ts` |
| JWT / localStorage tokens                | Same-origin cookie sessions in every mode |
| CSS-in-JS, component kits (MUI, Chakra)  | Tailwind. Generate components into `web/src/components/` (shadcn-style: generated, owned) |
| Hand-written TS types for server data    | Always generated by `ts-rs` |
| Redis, RabbitMQ, Kafka, Elasticsearch    | The database does jobs, cache and pub/sub |
| `config`, `figment`                      | Env vars into one struct |
| `log` crate macros                       | Use `tracing` |
| `reqwest` with `native-tls`              | If you need HTTP egress, use `reqwest` with `rustls-tls` and no default features |

## Adding a crate not on this list

Only when a `product/` spec requires a capability nothing above provides (for example, PDF generation or Stripe). Then:

1. Pick the most-downloaded, most-boring option.
2. `cargo add` it.
3. Add a line to the app's `docs/decisions.md`: `YYYY-MM-DD: added <crate> for <spec>, because <reason>`.
