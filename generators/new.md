# Generator: `new` (`rails new`)

> **TL;DR**
> - Run only after the interview and a confirmed plan (which includes the parts: the template plus the samples that fit, proposed by you, confirmed by the human).
> - **Generate from the template, never retype it:** `cargo run -q --manifest-path "$FW/tools/offrails-gen/Cargo.toml" --target-dir <dir>/target/offrails-gen -- new <dir> <crate_name> "<Display Name>" --pitch "<one line>" --parts <a,b>`. It copies [`template/app`](../template/README.md), renames it, vendors `.offrails/`, adds the parts and `git init`s, in seconds, on Linux, macOS and Windows.
> - Then fill the `AGENTS.md` header and `product/` from the interview, prove it green on SQLite and Postgres, run it, and make the one baseline commit. Write only the app's own code after that.
>
> Read it all, in order, whenever you create an app.

The framework ships the app skeleton as real files: [`template/app/`](../template/README.md) is a complete, compiling app (htmx) whose kernel, layout, error pages, assets, scripts, CI, Docker setup and tests were built from these guides and proven on SQLite and Postgres. Retyping it from Markdown cost an agent 50+ minutes and still drifted, so you copy it. From the moment it's copied, the code is the app's: you read it, adapt it and own it like code you wrote; humans still write none of it ([doctrine §7](../doctrine.md#7-markdown-is-the-spec-the-template-is-its-rendering)).

**Lean base, parts on demand.** The base has no job queue, mailer, accounts or realtime. Those are [parts](../template/README.md#choosing-parts), the tested [samples](../guides/samples.md), and the app gets the ones its product needs: at generation with `--parts`, or any time later with `cargo offrails add-part <name>`, by the same code path. A blog without background jobs doesn't carry a job queue. `dev-dashboard` fits every app.

## 0. Preconditions

- [ ] [`interview.md`](../interview.md) Rounds 1–2 are done (or the human said "you decide"), and the plan is confirmed, **including the parts** ([interview](../interview.md#then-confirm-the-plan-once-before-generating)).
- [ ] You know: the crate name (snake_case; make it distinctive, since other apps on the machine may be stopped by binary name), the product name (`config::APP_NAME`, human-readable), a one-line pitch, `FRONTEND_MODE`, `UI_LIBRARY`, `ISLANDS`, `API`, `PROD_DATABASE`, `DEPLOY_TARGET`, `ACCOUNTS`, `EMAIL`, and the parts.
- [ ] Tools: `rustc`/`cargo` stable, `cargo-nextest` and `cargo-watch` (`cargo install --locked <name>` for any that are missing), `sea-orm-cli` (its major must match `sea-orm`'s) for later migrations. `sqlite3` is used by `app db console`.
- [ ] TS modes (`inertia`/`spa`, or `ISLANDS: yes`): **Node 24 LTS** (22.22 is the hard floor) and pnpm via `corepack enable pnpm` ([stack](../stack.md#blessed-js-packages-inertia-spa-islands)). Any mode, for the optional browser smokes: Node ≥ 22.12.
- [ ] Docker is optional. It's only needed to test Postgres locally and to check the image builds.

## 1. Generate

`$FW` is the framework you're reading: the plugin root when run through `/offrails:new`, `~/.offrails/current` after the one-line install, or the path the human gave you. **Vendor a release, never a moving working tree.** A release install (the plugin, `~/.offrails/current`, a tag export) is copied as it is. When `$FW` is a git checkout, `new` (and `vendor`) copy **HEAD's committed files** (`git archive HEAD`, the way the release tarball is built), never the working tree, say so, and record the commit as `.offrails/VERSION`'s third line: `from git v0.7.3 (<sha>)` at the release tag, or `from git <sha> (unreleased: HEAD is not the v0.7.3 tag; VERSION names the last release)` on a branch before tagging, where `VERSION` still names the previous release (the first line, and so the `FRAMEWORK` header, say that version; the third line is the truth: say it in your final message). To vendor a release from a checkout, check out its tag first. `--worktree` copies the working tree as it is, uncommitted changes included (the framework's own CI; never for an app you hand over).

```sh
cargo run -q --manifest-path "$FW/tools/offrails-gen/Cargo.toml" --target-dir <dir>/target/offrails-gen -- \
  new <dir> <crate_name> "<Display Name>" --pitch "<one-line pitch>" --parts <comma-separated parts, or none>
# options: --prod-db postgres|sqlite (default postgres), --mode htmx|inertia|spa|api (§4), --no-git, --offline,
#          --invite (ACCOUNTS: … invite: DEFAULT_SIGNUP = Signup::Invite; needs auth-password or auth-magic-link)
```

`--target-dir` keeps the generator's build out of `$FW` (the plugin's files are compared by content, and a build there reads as drift). On Linux, macOS or Git Bash, `sh "$FW/tools/new-app.sh" <dir> <crate_name> "<Display Name>" …` is the same command.

What it does, so you can check it: copies `template/app/` into `<dir>` (refusing a non-empty directory), replaces the template's tokens (`offrails_app`, `offrails-app`, `Offrails App`, the pitch), vendors `$FW` into `<dir>/.offrails/` without `/.git`, `/.claude-plugin`, `/skills`, `/hooks`, `/evals`, `/brand`, `/.in_use`, `/media` and `/.sha256`, writes `.offrails/VERSION` (version, then `vendored <UTC date>`), writes `AGENTS.md` and `product/README.md` from the blueprints, logs `app generated from the … template` in `docs/decisions.md`, adds each part with its dependencies (`cargo add` per part's crate; that needs the crates.io index), runs `cargo fmt --all`, and `git init`s without committing. It prints the next commands.

The plugin's `skills/` and `hooks/` stay out of the app: they're loaded from the installed plugin, which works for every app at once. `hooks/fingerprint.sh` compares `.offrails/` with the plugin by content using the same exclusions.

## 2. Agent & product files

The template already has `CLAUDE.md` (`@AGENTS.md`), `.cursor/rules/offrails.mdc`, `.github/copilot-instructions.md` and `docs/decisions.md` (the template's own decisions, which the app inherits). Fill in:

| File | From |
|------|------|
| `AGENTS.md` | Written from [`blueprints/AGENTS.md`](../blueprints/AGENTS.md) with the template's defaults (`htmx`, `API: none`, `ACCOUNTS: none`, `TENANCY: none`, and `EMAIL`/`ACCOUNTS` set by the parts). Set every header line from the interview; never one the code doesn't match yet (`TENANCY` changes when the tenancy suites exist: the first owner-scoped `cargo offrails g resource` writes them, [authentication](authentication.md#2-what-the-parts-leave-to-you)) |
| `product/README.md` | Written from [`blueprints/product-README.md`](../blueprints/product-README.md) with the name filled; write the rest from the interview |
| `product/features/` | one spec per feature from the confirmed plan ([`blueprints/feature-spec.md`](../blueprints/feature-spec.md)). They're built after step 5 |
| `static/` brand assets | `new` renders them from `brand.toml` (`cargo offrails brand`: headless Chrome, else `rsvg-convert`; with neither the PNGs keep the template's look and it says so: log that in `docs/decisions.md`). Set the colors, fonts and logo in `brand.toml` and re-run |

Design: follow any references from the interview. Otherwise keep the template's clean, neutral Tailwind design.

## 3. Parts

The parts were chosen in the plan ([template](../template/README.md#choosing-parts)). If you generated with `--parts`, they're in. Otherwise, or for one the plan added later:

```sh
cargo offrails parts                       # installed vs available, what each adds
cargo offrails add <name> [<name>…]        # dependencies first; prints what it changed and what to run next
```

Then `cargo run -q -- db migrate` (parts with tables; docs/routes.md is already refreshed), and read each part's `README.md` ("After installing", "Adapting it") and `CHECKS.md` in `.offrails/template/parts/<name>/`. Adapt the `// APP:` lines (names, copy, limits) to this app; keep every `// SECURITY:` and `// INVARIANT:` line and the tests they name ([samples](../guides/samples.md#rules)).

`ACCOUNTS` isn't `none`: accounts are the auth parts (`auth-core` with `auth-password` and/or `auth-magic-link`, `auth-totp` for two-factor; invite-only is `new --invite` or `DEFAULT_SIGNUP`, not a part), added here like any other. [`authentication.md`](authentication.md) is what they leave to you: `APP_SECRET_KEY`, `paths::after_sign_in()`, your scoped handlers, the browser smoke's scenario, roles and teams.

## 4. Frontend mode

- **`htmx`** (default): nothing to do; the template is htmx.
- **`api`**, **`inertia`**, **`spa`**: `new --mode <mode>` generated the htmx template (the kernel, `/dev`, tests and CI every mode shares) and logged the requested mode. Convert it per [template → other modes](../template/README.md#other-modes) and the reference below ([other modes](#other-modes)), and set the header once the code matches.

## 5. Verify & report

1. `bin/setup` (creates `.env`, builds the CSS, migrates and seeds the dev DB). Windows: [template → Windows](../template/README.md#windows).
2. `cargo offrails verify --full` is green: the [Definition of Done](../workflow.md#definition-of-done), with `offrails-check`, on SQLite **and** Postgres at once. Without a local Postgres, run `docker run -d --name pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:<major>` first (verify finds it on 5432), or set `TEST_DATABASE_URL`. The first build compiles the whole stack (2–4 min on an M2; a second app on the same machine is no faster unless you opted into sccache, [testing](../guides/testing.md#build-speed)); after that, `cargo offrails verify` takes seconds.
3. Prove the migrations are reversible on both databases: `cargo run -- db rollback --steps <n>` then `cargo run -- db migrate`. For Postgres, set `DATABASE_URL` to a scratch database.
4. Run the app: `cargo run -- serve`, then:
   - `curl -i localhost:3000/up` returns 200;
   - `/` shows digested `/static/...?v=` URLs, and fetching one returns `cache-control: …immutable`;
   - `/dev` returns 200; with the `dev-mailbox` part, `curl -X POST -H 'Sec-Fetch-Site: same-origin' localhost:3000/dev/mailbox/test` then `curl -s localhost:3000/dev/mailbox/latest.json` shows the test email;
   - `curl -X POST -H 'Origin: https://evil.example' localhost:3000/` returns 403;
   - `curl -s -o /dev/null -w '%{http_code}' -H 'Host: evil.test' localhost:3000/dev` prints 403, and `lsof -nP -iTCP:3000 -sTCP:LISTEN` shows `127.0.0.1:3000` (development is this machine only);
   - the [fresh-database smoke](../guides/dev-tools.md#pending-migrations): an empty database shows the pending-migrations page (503), never a boot crash.

   In `inertia`/`spa`, `bin/dev` runs Vite next to the server. Open `https://localhost:3000` (not :5173) and check that the browser console is clean. Then `pnpm --dir web build` and `APP_ENV=production … serve`: the shell links `/assets/…` from the manifest, and `/assets` responses are `immutable`.
5. Optional, when Node ≥ 22.12 and Chrome are installed: `BASE=http://localhost:3000 bash .offrails/tools/browser-smoke/run.sh` ([dev-tools](../guides/dev-tools.md#browser-smoke)).
6. If Docker is available: `docker compose up --build` from a clean checkout brings `app` and `db` to `healthy`, `curl localhost:3000/up` answers `ok`, then `docker compose down -v`.
7. `git add -A && git commit -m "new: <app_name> skeleton"`. This is the one commit you make unprompted, because it's the baseline.
8. **Then build the features**, one vertical slice at a time, after the spec. Start each piece from a snippet or a generator when one fits (both tested, both yours once copied): `cargo offrails snippets` / `snippet show <name>` / `snippet add <name>` ([snippets](../template/snippets/README.md)), `cargo offrails g resource …` (or `model`, `controller`, `page`, `api`, `job`, `mailer`, `channel`; `cargo offrails g --help`; [generators](README.md)); commit after each run; hand-write the rest from the guides, in the same shape (small files per feature, one marked line per shared spot). Then the `// APP:` lines and the spec's tests.
9. Tell the human, in product terms: what exists, which parts are in (and that more can be added any time), how to run it (`bin/setup && bin/dev`), that http://localhost:3000/dev has the dev tools (and the mailbox, jobs dashboard and dev dashboard when those parts are in), and what you'll build next from the plan.

## What the template contains

Read this when you change a kernel file, port a fix from a newer template, or convert to another mode. Each item points at the guide that is its spec. The files themselves are in `.offrails/template/app/` (and in your app).

### Workspace & dependencies

- `Cargo.toml`: the app plus the `migration` crate as a workspace, `publish = false`, `[profile.dev.package."*"] opt-level = 1`, `[profile.dev] debug = "line-tables-only"` (faster rebuilds; `CARGO_PROFILE_DEV_DEBUG=true` for a debugging session; Linux's `split-debuginfo` is in `.cargo/config.toml`). `Cargo.lock` is the tested resolution; `cargo update` is an ordinary change.
- `migration/`: `sea-orm-migration` with `default-features = false` and `["runtime-tokio-rustls", "sqlx-sqlite", "sqlx-postgres"]` (no CLI runner: the app binary has `db migrate|rollback|status`), no `rust-version` line (the MSRV-aware resolver would downgrade dependencies), and the migrations list as a **multi-line** `vec![` … `]`, the only shape `sea-orm-cli migrate generate` registers into. One migration: `create_cache_entries` ([caching](../guides/caching.md)). Parts bring their own tables.
- Crates: the htmx set from [stack](../stack.md#blessed-crates), added with `cargo add`. More arrive with the feature or part that needs them. For other modes and APIs:

  ```sh
  # TS modes (inertia / spa / ISLANDS: yes):
  cargo add ts-rs --features chrono-impl,serde-json-impl
  # API: rest | both (guides/api.md), and CORS for any API:
  cargo add utoipa --features axum_extras,chrono
  cargo add tower-http --features cors
  # API: graphql | both (guides/graphql.md):
  cargo add async-graphql --features chrono,dataloader
  cargo add async-graphql-axum
  ```

  `cargo add` accepts several crates in one call only when no `--features` flag is given. In zsh, don't loop over a quoted list (it doesn't word-split); one `cargo add` per line.

### Rust skeleton

Module order (each compiles against the ones before it): config → kernel → error/models → state → views → controllers → routes → tasks → main.

1. `src/config.rs`: `Config`, `AppEnv`, `LogFormat`, `bind_address` (`APP_BIND`: 127.0.0.1 in development), `parse_allowed_hosts`, `boot_warnings`, `Config::test()`, `resolve_app_url`, and the lean runtime settings `APP_WORKER_THREADS` (2) and `APP_BLOCKING_THREADS` (16) ([config](../guides/config.md)).
2. `src/kernel/db.rs`: `connect` (SQLite: WAL, foreign keys, busy timeout, a 1 MiB page cache, a pool of 4 by default; Postgres: a pool of 10), `begin`, `ping`, and the Postgres test harness's DDL (`create_database_from`, advisory locks, `install_test_reset`/`reset_test_database`, `drop_idle_test_databases`; [testing](../guides/testing.md#postgres-test-databases)) ([database](../guides/database.md#kerneldbrs-spec)).
3. `src/error.rs`: `AppError` (with `Unavailable` → 503 + `Retry-After`), `ResponseFlavor`, `shape_errors`, `retry_after` ([errors-and-logging](../guides/errors-and-logging.md#response-mapping)).
4. `src/models/mod.rs`: `Errors`, `Page` ([models](../guides/models.md)).
5. The kernel: `security.rs` (the header matrix and CSP, [security](../guides/security.md#the-header-matrix)), `hosts.rs` (development-only Host authorization, [security](../guides/security.md#development-is-this-machine-only)), `flash.rs`, `cookie.rs`, `rate_limit.rs`, `assets.rs` + `css.rs` ([views](../guides/views.md#dark-mode)), `tls.rs` (`HttpToHttps`, [deployment](../guides/deployment.md#direct-tls-optional)), `introspect.rs` ([dev-tools](../guides/dev-tools.md#introspection)), `cache.rs`, `redact.rs` ([errors-and-logging](../guides/errors-and-logging.md#redacting-stored-error-text)), `theme.rs` (the System/Light/Dark switch), `ui.rs` (kernel chrome: `Chrome`, `Chrome::stylesheet`/`script`, `KernelFlash`, `no_store()`). CSRF is tower-http's `CsrfLayer`, configured in `routes.rs` ([security](../guides/security.md#csrf)). In TS modes add `inertia.rs` and its pieces ([frontend-inertia](../guides/frontend-inertia.md#the-adapters-rust-surface)).
   - **Parts, not base:** the mailer and dev mailbox (`mailer`, `dev-mailbox`), the job queue and `/admin/jobs` (`jobs`, `jobs-dashboard`), sessions and accounts (`auth-*`), channels and pubsub (`realtime`), `/dev/dashboard` (`dev-dashboard`). Their guides ([mailers](../guides/mailers.md), [jobs](../guides/jobs.md), [jobs-dashboard](../guides/jobs-dashboard.md), [auth](../guides/auth.md), [channels](../guides/channels.md), [dev-tools](../guides/dev-tools.md)) remain their spec. `storage` arrives with the first attachment field ([storage](../guides/storage.md)).
6. `src/state.rs`: `AppState { config, db, cache, assets, rate_limit }` plus what parts add, built by `AppState::new(config, db)`; tests build it the same way.
7. `src/views/mod.rs`: `render`, `Ctx` with the page meta (`title`, `description`, `path`, `app_url`; [views](../guides/views.md#meta-favicon-and-social-cards-every-app-with-pages)), `views/helpers.rs`, `views/home.rs`, `views/errors.rs`.
8. `src/controllers/`: `application.rs` (the `Ctx` extractor, `HxRequest`, `AdminAccess`), `home.rs`, `health.rs` (`/up`, 503 while migrations are pending), `meta.rs` (`/robots.txt`, `/sitemap.xml`), `theme.rs` (`POST /theme`), `errors.rs` (the fallback), `dev.rs` (`/dev`, routes, system, introspection, reload) and `migrations.rs` (the pending-migrations guard, [dev-tools](../guides/dev-tools.md#pending-migrations)). Actions carry `#[tracing::instrument(skip_all)]` ([errors-and-logging](../guides/errors-and-logging.md#spans)).
9. `src/routes.rs` ([routing](../guides/routing.md#middleware)): the routes, `/static` with cache headers, the fallback, the middleware stack (`kernel::hosts::middleware` right inside the security middleware; `TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, 30s)`), `paths`, and `ROUTES` with an `Access` level for every route; `dev::router()` merged only in development, then the `migrations::guard` layer. Each kernel router ends with `.layer(kernel::ui::no_store())`.
10. `src/tasks/`: `seed.rs` (idempotent per record, in one transaction: [cli](../cli.md#seeds)) and `test_db_cleanup.rs`.
11. `src/main.rs`: the CLI from [`cli.md`](../cli.md); a runtime built from `Config` (worker and blocking thread caps); `serve` on `config.bind` with `into_make_service_with_connect_info::<SocketAddr>()`, graceful shutdown on Ctrl-C or SIGTERM (Ctrl-C only on Windows), plain HTTP or direct TLS (h2 + http/1.1 via ALPN, [deployment](../guides/deployment.md#direct-tls-optional)); `app health` for the container healthcheck.

### Anchors

Shared files carry one-line comments where a part inserts its wiring: `// samples: <anchor>` in Rust, `{# samples: <anchor> #}` in templates, `# samples: <anchor>` in `.env.example` and `compose.yaml`. `cargo offrails add-part` (and the samples repo's `tools/install.sh`) inserts above them. Each is **on a line of its own**; keep them, they cost nothing ([why](../guides/samples.md#anchors)).

| File | Anchors (where) |
|------|-----------------|
| `src/lib.rs` | `lib mods` (after the `pub mod` lines) |
| `src/kernel/mod.rs` | `kernel mods` |
| `src/kernel/ui.rs` | `kernel tabs` (in the kernel header's tab list), `hidden tabs` (a list of tab keys a part replaces) |
| `src/kernel/introspect.rs` | `introspect sections` (in `snapshot`, before it returns: `out["x"] = …;`) |
| `src/kernel/redact.rs` | `token routes` (a route pattern whose `{token}` segment is a secret) |
| `src/kernel/security.rs` | `dev csp` (extra CSP directives in development only) |
| `src/config.rs` | `config fields` (end of `Config`), `config debug` (end of the `Debug` chain), `config parse` (in `from_env`, before the missing/invalid check), `config init` (end of the `Config { … }` literal in `from_env`), `config test` (end of `Config::test()`'s literal), `config sections` (end of the file, outside any `impl`) |
| `src/state.rs` | `state fields`, `state build` (in `AppState::new`, before the struct literal), `state init` (end of that literal), `state fns` (end of `impl AppState`) |
| `src/routes.rs` | `route mods` (after the `use` lines: `pub mod <feature>;`, one per `src/routes/<feature>.rs`), `routes` (after the last app `.route`), `dev routers` (inside the development-only merge), `outer layers` (just inside `SetRequestIdLayer`), `paths` (end of `mod paths`), `route tables` (end of `TABLES`: `<feature>::ROUTES,`), `routes table` (end of `APP_ROUTES`, the kernel's and the parts' rows) |
| `src/controllers/mod.rs`, `controllers/application.rs` | `controller mods`; `ctx init` (in the `Ctx` extractor's literal), `extractors` (end of file) |
| `src/controllers/dev.rs` | `dev routes` (in `router()`), `dev tiles` (end of the `/dev` tile list), `dev try-it` (end of the Try-it buttons), `system facts` (end of `/dev/system`'s facts) |
| `src/controllers/meta.rs` | `sitemap paths` (end of the public paths `/sitemap.xml` lists) |
| `src/views/mod.rs`, `src/models/mod.rs`, `src/tasks/mod.rs` | `view mods`, `ctx fields` (end of `Ctx`); `model mods`, `model aliases`; `task mods`, `tasks` (in the task `match`) |
| `src/main.rs` | `cli commands` (end of the `Command` enum), `cli dispatch` (the first line in the `match`: rustfmt pulls a comment after a last arm ending in `}` onto that brace), `serve start` (in `serve`, after state is built), `serve stop` (in the shutdown future), `tracing layers` (the layer list in `init_tracing`) |
| `migration/src/lib.rs` | `migration mods`, `migrations` (end of the `vec![…]`, after the last `Box::new`) |
| `static/app.js` | `app js` (end of file: one delegated listener per behavior) |
| `templates/layouts/application.html`, `templates/shared/_nav.html` | `app head` (in `<head>`, after `app.js`: a part's scripts and stylesheets, loaded on every page because boosted links run nothing in a page's head block), `dev footer links`; `nav links` |
| `templates/kernel/layout.html` | `kernel head` (last line inside `<head>`), `kernel main` (first line inside `<main>`, before the content block) |
| `tests/app/main.rs`, `support.rs`, `factories.rs` | `test mods`; `test support` (end of `impl TestApp`); `factories` |
| `tests/app/security_test.rs`, `kernel_web_test.rs`, `theme_switch_test.rs` | `router sources`; `no-store paths`; `kernel pages` |
| `.env.example`, `compose.yaml` | `env` (end of file); `compose env` (end of the app's `environment`) |

Generators add their own anchors to the files they write (`samples: columns`, `relations`, `related` in a generated model's `mod.rs`; `model test mods` in `tests/app/models/mod.rs`; `tenant fields`, `tenant records`, `tenant init`, `tenant fill`, `tenant unchanged`, `tenant expected`, `not tenant ids` in a generated `tests/app/tenancy_test.rs`; `page mods`, `page view mods`, `page test mods` in the shared `pages` modules; `api paths`, `api schemas` in `src/controllers/api.rs`), and use the template's for the rest ([generators](README.md#what-the-generators-decide-so-you-dont)). Every line a generator or a snippet inserts carries its owner's marker (`// posts`, `{# posts #}`), so it's found, and removed, by name.

After `cargo fmt --all`, none may trail code: `grep -rnE '[^[:space:]].*(//|\{#|#) samples: ' src templates migration tests | grep -vE ':[[:space:]]*(//|\{#|#) samples: '` prints nothing.

### Frontend

- Templates: every class in both color schemes ([views](../guides/views.md#dark-mode); `tests/app/theme_test.rs` enforces it); `templates/layouts/application.html` (the `htmx-config` meta from [frontend-htmx](../guides/frontend-htmx.md#required-config-swap-422-responses), `data-theme` from the theme cookie, `dev-reload.js` in development), `templates/shared/{_flash,_errors,_nav}.html`, `templates/home/index.html`, the standalone error pages `templates/errors/{404,403,429,500,503}.html` and the `_invalid.html` fragment, and `templates/kernel/` (layout, `/dev` pages).
- Assets: `assets/app.css` (Tailwind 4, the `dark` variant that follows the theme switch, the component layer), `static/app.js`, `static/dev-reload.js` ([dev-tools](../guides/dev-tools.md#hot-reload)), `static/kernel/admin.{css,js}`, htmx and its sse extension in `static/vendor/` ([frontend-htmx](../guides/frontend-htmx.md#setup-done-by-generatorsnewmd)), the brand (`brand.toml` → `assets/brand.css`, `src/brand.rs`, `favicon.svg`, `favicon.ico`, `apple-touch-icon.png`, `icon-512.png`, `og.png` from `assets/brand/og.svg`), the five layouts (`templates/layouts/`), the component kit (`templates/components/ui.html`, `static/components.js`) and its gallery at `/dev/components`.
- Links into kernel pages carry `hx-boost="false"` ([dev-tools](../guides/dev-tools.md#rules)).

### Scripts & ops files

| File | Content |
|------|---------|
| `.gitignore`, `.dockerignore`, `storage/.keep` | Build output, `.env`, `storage/*`, `static/app.css`, `bin/tailwindcss`, `web/node_modules`, `web/dist` |
| `.gitattributes` | `* text=auto eol=lf`: LF everywhere, so a Windows checkout keeps rustfmt, anchors and tests green |
| `.cargo/config.toml` | The `cargo offrails` alias (the vendored generators) |
| `.env.example` | Every `Config` var with dev defaults ([config](../guides/config.md#envexample)) |
| `bin/setup-tailwind`, `bin/setup` | Pinned Tailwind, verified download; first run: tools, `.env`, CSS, `db setup` |
| `bin/dev` | HTTPS + HTTP/2 only with a locally trusted certificate (`bin/dev-cert`), else plain HTTP; `cargo watch` server + Tailwind watcher; one PID file per port; stops only its own tree ([dev-tools](../guides/dev-tools.md#the-bindev-script)) |
| `bin/dev-cert`, `bin/docker-entrypoint` | mkcert or nothing ([why](../guides/dev-tools.md#https-in-development)); migrate unless `SKIP_MIGRATIONS=true`, then `exec "$@"` |
| `compose.yaml`, `Dockerfile` | The whole app, production-shaped, on Postgres; profiles `worker` and `mail` ([deployment](../guides/deployment.md#docker-compose-the-whole-app-locally), [Dockerfile](../guides/deployment.md#dockerfile-shape)) |
| `.github/workflows/ci.yml` | SQLite (with `offrails-check` first), Postgres, security (cargo-deny + gitleaks) and Docker (boots the production image, ZAP baseline) jobs ([testing](../guides/testing.md#ci)) |
| `deny.toml`, `.gitleaks.toml`, `.zap/rules.tsv`, `.github/dependabot.yml` | [security](../guides/security.md#automated-enforcement) |

Deploy config only for the chosen `DEPLOY_TARGET` ([deployment](../guides/deployment.md)); none for "decide later".

### Tests

One binary, `tests/app/` ([testing](../guides/testing.md)): `main.rs` (`#[allow(dead_code)] mod support;`), `support.rs` (`TestApp`, `TestResponse`, the query counter, the Postgres template and reset slots), `factories.rs` (empty; each model adds one), and the kernel suites:

- `smoke_test.rs`: home page and layout, `/up`, the 404 page and `/api` JSON 404, security headers, `foreign_keys = 1`, a flash rendered once, a stable query count, meta and brand assets, `/robots.txt` and `/sitemap.xml`;
- `headers_test.rs`: the header matrix per response class, development and production ([security](../guides/security.md#the-header-matrix));
- `security_test.rs`: every `ROUTES` entry is routable, non-public routes are never 2xx anonymously, `Dev` routes 404 in production, cross-site and foreign-`Origin` POSTs get 403, development refuses a rebound `Host` ([test-standards §3](../guides/test-standards.md#3-security-tests-every-app));
- `errors_test.rs`, `logs_test.rs` (tokens never reach logs), `kernel_test.rs` (cache, rate limiter, flash cookie, `DbErr` mapping, `Retry-After`), `kernel_web_test.rs` (`/dev`, introspection, `no-store`, the pending-migrations page and `/up` 503), `theme_test.rs` and `theme_switch_test.rs` ([test-standards §11](../guides/test-standards.md#11-theme-and-css-guards)); unit tests in `kernel/tls.rs`, `kernel/hosts.rs`, `config.rs`.

Each part brings its own suites (`tests/app/<part>_test.rs`). `tests/app/guard_test.rs` and `tenancy_test.rs` arrive with the first owner-scoped `cargo offrails g resource` (or by hand, [test-standards §4](../guides/test-standards.md#4-tenant-isolation-suite-every-app-with-accounts)) when `TENANCY` isn't `none`.

### Other modes

- **`api`:** no app templates, no `assets/`, no Tailwind (`bin/setup-tailwind` and the Dockerfile's Tailwind line go), no `web/`. Keep the kernel's `templates/kernel/`, the standalone `templates/errors/` pages, `static/kernel/` and the vendored htmx they use. `views/` holds JSON bodies (`#[derive(Serialize, ToSchema)]`, a `PageInfo` envelope); `controllers/root.rs` answers `GET /` with a JSON index and `/robots.txt` with `Disallow: /`; errors are JSON everywhere except `/dev` and `/admin`; `controllers/api/mod.rs` (`ApiJson`, `ApiDoc`, `ErrorBody`, `/dev/api-docs`) for `API: rest | both` ([api](../guides/api.md)). `smoke_test.rs` checks the JSON index, robots and JSON 404s instead of pages; no `theme_test.rs`.
- **`inertia`:** `pnpm create vite web --template react-ts`, then the "Vite wiring" section of [frontend-inertia](../guides/frontend-inertia.md#vite-wiring-verified-with-vite-8-vitejsplugin-react-6-tailwind-4) exactly (including `.cargo/config.toml`'s `TS_RS_EXPORT_DIR` and `TS_RS_LARGE_INT`, `test-setup.ts`, and `app.css` with the progress-bar CSS); `SharedProps` + `type Inertia`, `home::index` rendering `"home/Index"`; the HTML shell layout; `cargo test --lib export_bindings` before `pnpm --dir web typecheck`. Svelte: [frontend-svelte#inertia-mode](../guides/frontend-svelte.md#inertia-mode).
- **`spa`:** the same Vite scaffold per [frontend-react](../guides/frontend-react.md); Svelte: SvelteKit SPA per [frontend-svelte#spa-mode-sveltekit](../guides/frontend-svelte.md#spa-mode-sveltekit).
- **`ISLANDS: yes`:** `web/` in islands configuration ([frontend-htmx#islands](../guides/frontend-htmx.md#islands)).
