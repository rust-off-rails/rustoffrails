# The app template

> **TL;DR**
> - `template/app/` is a complete, compiling, tested blank app (htmx). A new app is a copy of it, renamed, with this framework vendored into `.offrails/`: one command, no code retyped.
> - `template/parts/<name>/` are the optional kernel parts (mailer, dev mailbox, jobs, jobs dashboard, accounts, realtime, dev dashboard). They are the [samples](https://github.com/rust-off-rails/samples), vendored into the release. Add one at any time with `cargo offrails add-part <name>`.
> - `template/snippets/<name>/` are small, tested code to copy (a feed, slugs, Markdown, a pager, a theme toggle, backdated seeds, a public page by slug, a CRUD resource): `cargo offrails snippets`, `snippet show <name>`, `snippet add <name>` ([snippets](snippets/README.md)).
> - The generators are `tools/offrails-gen`, a standard-library-only Rust tool: the same commands on Linux, macOS and Windows. Snippets and generators are both optional starting points; whatever you start from, a feature is small files in its own folders plus one marked line per shared spot.
> - Pages are built from the component kit, `templates/components/ui.html` (every component at `/dev/components`), in one of five layouts, branded by `brand.toml` (`cargo offrails brand`); generated and snippet pages use the kit. `BASE=… cargo offrails prove` screenshots and checks every page and writes `docs/proof/PROOF.md` ([views](../guides/views.md#components), [testing](../guides/testing.md#proof-cargo-offrails-prove)).
> - After generating, the app's code is the app's: agents read it, adapt it and own it, as if they had written it. The guides stay the spec for *why*.
>
> Read [Using it](#using-it) to make an app, [Choosing parts](#choosing-parts) to pick parts, [Other modes](#other-modes) for inertia/spa/api.

## Using it

**A new app** (after the interview, [new](../generators/new.md)). `$FW` is the framework: the plugin root, `~/.offrails/current`, or a checkout.

```sh
cargo run -q --manifest-path "$FW/tools/offrails-gen/Cargo.toml" --target-dir bookshelf/target/offrails-gen -- \
  new bookshelf bookshelf "Bookshelf" --pitch "track the books you read" --parts dev-dashboard,mailer,dev-mailbox
# Linux / macOS / Git Bash shorthand: sh "$FW/tools/new-app.sh" bookshelf bookshelf "Bookshelf" --parts …
```

`new <dir> <crate_name> "<Display Name>"` copies `template/app/` into `<dir>`, replaces the tokens below, vendors `$FW` into `<dir>/.offrails/` (the [new](../generators/new.md#1-generate) excludes), writes `AGENTS.md` and `product/README.md` from the blueprints, logs the generation in `docs/decisions.md`, adds `--parts` (each with its dependencies), runs `cargo fmt`, and `git init`s. It refuses a non-empty directory. Options: `--pitch "<one line>"` (the home page tagline and meta description), `--parts a,b|all|none` (default none), `--mode htmx|inertia|spa|api` (see [other modes](#other-modes)), `--prod-db postgres|sqlite`, `--no-git`, `--offline` (passes `--offline` to `cargo add`), `--framework <path>`. Nothing networked runs unless parts are asked for: a part's crates arrive with `cargo add`, which needs the crates.io index (cached or online).

**In an app**, from its root, through the alias in `.cargo/config.toml`:

```sh
cargo offrails parts                      # every part: installed or available, what it needs, what it adds
cargo offrails add mailer                 # the part and its dependencies; prints exactly what it changed and what to run next
cargo offrails add jobs-dashboard --dry-run        # what it would do, changing nothing
cargo offrails update-part mailer --framework "$FW"   # a newer framework's part, merged 3-way into the app's copy
cargo offrails g resource posts title:string body:text   # the generators: migration, model, resource, controller, page, api, job, mailer, channel, test
cargo offrails d resource posts           # undo a generator (files nobody changed)
cargo offrails snippets                   # the snippets; `snippet show <name>` prints one, `snippet add <name>` copies and wires it
cargo offrails snippet remove atom-feed   # take a snippet back (files nobody changed, its marked lines)
```

The generators ([generators](../generators/README.md)) and `snippet add` ([snippets](snippets/README.md)) write a feature's files and its wiring at the same anchors the parts use, each wired line marked with its owner (`// posts`), format them, and record them in `docs/generated.toml` (so a re-run is a no-op and `destroy` / `snippet remove` removes exactly what they wrote). `add` is `add-part`, `parts` is `list-parts`.

`add-part` checks everything before changing anything: a file the part brings that exists and differs, or a missing anchor, refuses with the reason and changes nothing (`--force` overwrites files). It then runs each `cargo add`, copies the part's `files/` (crate name replaced), inserts each wire above its `samples: <anchor>` comment, logs `installed sample <name> <version>` in `docs/decisions.md`, updates the `AGENTS.md` header line the part changes (`EMAIL` for `mailer`, `ACCOUNTS` for an auth method), and runs `cargo fmt --all`. Re-running it is a no-op. It is the samples repo's `tools/install.sh` in Rust, with the same file format and the same record line, so either tool sees the other's work. Next steps it prints: `cargo run -q -- db migrate` when the part has tables, `cargo run -q -- routes`, then the [Definition of Done](../workflow.md#definition-of-done) on SQLite **and** Postgres, and the part's README ("After installing", "Adapting it") and CHECKS.md.

## Layout

```
template/
  app/                 the base app: kernel, layout, error pages, assets, bin/, migration crate, tests/app, CI, Docker
  parts/<name>/        sample.toml (crates, dependencies, wiring), README.md, CHECKS.md, files/ (at app paths)
  parts/SOURCE         the samples commit the parts were synced from (tools/sync-parts.sh)
  snippets/<name>/     snippet.toml (a part's format + `as`, `snippets`), README.md, files/ (at app paths)
tools/offrails-gen/    new, add-part (add), list-parts (parts), update-part, generate (g), destroy (d), snippets, snippet, vendor, verify
                       (Rust, std only; its own workspace; src/generate/ holds the generators, testdata/golden/ their golden files)
tools/new-app.sh       sh shorthand for `new`
tools/ci/test-template.sh   generate bare / all parts / parts added one by one / every generator / every snippet, then the Definition of Done on both databases
tools/ci/memory-budget.sh   boot a generated app (release), serve 1000 requests, fail over an RSS budget
```

**What `template/app/` contains.** Everything [new](../generators/new.md#what-the-template-contains) lists: `config.rs` (typed env, `APP_BIND` 127.0.0.1 in development, `boot_warnings`), `error.rs` (`AppError`, `shape_errors`, `Retry-After`), the kernel (`security.rs` header matrix and CSP, `hosts.rs` development Host allowlist, `rate_limit.rs`, `flash.rs`, `cookie.rs`, `redact.rs` log and error-text redaction, `db.rs` SQLite + Postgres pools and the Postgres test template and slots, `cache.rs`, `assets.rs` digests, `css.rs` theme guards, `introspect.rs`, `theme.rs` System/Light/Dark switch, `tls.rs` direct TLS, `ui.rs` kernel chrome), CSRF (`tower-http`'s layer in `routes.rs`), `/dev` tools with the pending-migrations guard, `/up`, `robots.txt`, `sitemap.xml`, the htmx layout with meta and social cards, Tailwind (`bin/setup-tailwind`, pinned), `bin/setup`, `bin/dev`, `bin/dev-cert`, the migration crate (one table: `cache_entries`), `tests/app/` (`TestApp`, the Postgres test template and reset slots, and the kernel suites: smoke, headers, security, errors, logs, theme, theme switch, kernel, kernel web), `deny.toml`, `.gitleaks.toml`, `.zap/rules.tsv`, `.github/workflows/ci.yml`, `Dockerfile`, `compose.yaml`, `.env.example`, `docs/decisions.md` (the template's own decisions, inherited by every app), `.gitattributes`, and `.cargo/config.toml` (the `cargo offrails` alias). The look: `brand.toml` (rendered by `cargo offrails brand` into `assets/brand.css`, `src/brand.rs` and the favicon set and social card), five layouts in `templates/layouts/` (application = top nav and the shell, sidebar, marketing, auth, docs), the component kit `templates/components/ui.html` with `static/components.js`, and `/dev/components` + `/dev/layouts/*` showing all of it ([views](../guides/views.md#components)). Every shared file carries the `samples: <anchor>` comments parts wire into ([samples](../guides/samples.md#anchors)).

**Tokens.** The template is a real crate named `offrails_app`, shown as `Offrails App`, so it builds and its tests run as is. `new` replaces, in every text file: `offrails_app` → the crate name, `offrails-app` → its kebab form (compose project, image), `Offrails App` → the display name, and, with `--pitch`, `A new Rust off Rails app.` (the description) and the home page's sentence → the pitch as a sentence, then `A new Rust off Rails app` (brand.toml's `tagline`, the one tagline: `config::APP_TAGLINE` is `brand::TAGLINE`) → the pitch capitalized, without its period. Part files use `offrails_host` (the samples' host crate); `add-part` replaces that and `offrails_app` with the app's crate name. To rename by hand (any OS), run the generator; never `sed -i` (it differs between macOS and Linux and doesn't exist on Windows).

**Memory.** The base is lean by default: two Tokio workers (`APP_WORKER_THREADS`), a blocking pool capped at 16 (`APP_BLOCKING_THREADS`), a SQLite pool of 4 with a 1 MiB page cache per connection and idle connections closed after a minute ([database](../guides/database.md#sqlite-memory)). `tools/ci/memory-budget.sh` measures it: on macOS (Apple silicon, 2026-10-17) the bare app in release is about 14 MiB after boot and 15 MiB after 1000 requests (debug: 16 / 17 MiB); the app with every part is about 18 MiB after boot and 20 MiB after 1000 requests (debug: 24 / 26 MiB); the budget is 24 MiB bare and 32 MiB with every part.

## Choosing parts

The agent decides which parts the app needs from what the human said and the specs in `product/`, proposes the list with a one-line reason each, and asks the human only to confirm or adjust ([interview](../interview.md#then-confirm-the-plan-once-before-generating)). The default is the template plus the parts that fit; the human can opt out of any. Record the answer in `docs/decisions.md` (`new` and `add-part` log each part). Use this table so every agent decides the same way:

| Signal in the request or a spec | Part(s) | Why |
|---|---|---|
| Every app (development only; a read-only subset for admins in production) | `dev-dashboard` | Live charts, request traces, SQL with N+1 detection, logs, schema: how agents and humans see the running app |
| Sends any email: sign-in links, notifications, digests, receipts, a contact form | `mailer` + `dev-mailbox` | Templated HTML + text mail with SMTP in production, and every email visible at `/dev/mailbox` in development |
| Anything slow, retried, scheduled or sent later: emails in the background, imports, digests, reminders, cleanups, webhooks out | `jobs` + `jobs-dashboard` | A database-backed queue with retries, cron and an `/admin/jobs` console; no extra infrastructure |
| People sign in (any `ACCOUNTS` but `none`) | `auth-core` + one method | Users, database sessions, the account security page; brings `mailer` and `dev-mailbox` |
| `ACCOUNTS: magic-link` (or `magic-link invite`) | `auth-magic-link` | Emailed link plus a 6-digit code |
| `ACCOUNTS: password` (or `password invite`) | `auth-password` | argon2id, verification, reset |
| Two-factor, admin accounts that guard money or personal data | `auth-totp` | Authenticator-app codes with recovery codes |
| Chat, presence ("who's online"), typing indicators, multiplayer, live updates pushed to other users | `realtime` | Channels over WebSocket with SSE and long-poll fallbacks |
| Another service posts events to the app: payments (Stripe), GitHub, any HMAC-signed webhook | `webhooks` | Signature checked over the raw body, replays stored once, the work done in a job; brings `jobs` |
| None of the above (a content site, an internal read-only tool) | `dev-dashboard` only | The base already has `/dev`, the cache, security headers and the theme switch |

When the human wants to see what a part is before deciding, point them at **<https://github.com/rust-off-rails/samples>**: its README has screenshots of every screen, light and dark. When the product later grows a need, add the part then: `cargo offrails add-part <name>`. Before hand-writing a mailer, a queue, sessions or a socket layer, run `cargo offrails list-parts`: if a part does it, add the part ([workflow](../workflow.md#when-the-product-grows-a-need-add-the-part)).

## Other modes

The template is `FRONTEND_MODE: htmx`. For the others, `new --mode <mode>` generates the htmx template (whose kernel, `/dev`, tests and CI every mode shares), keeps the header at `htmx`, and logs the requested mode; then convert:

- **`api`**: follow [new](../generators/new.md#other-modes) `api`: remove `templates/home/`, `templates/shared/`, `assets/`, the Tailwind lines of `bin/setup`, `bin/dev` and the `Dockerfile`, and `tests/app/theme_test.rs`; replace `controllers/home.rs` with `root.rs` (`GET /` as a JSON index) and make every error JSON outside `/dev` ([api](../guides/api.md)). Set `FRONTEND_MODE: api` and `API: rest` once `cargo nextest run` is green.
- **`inertia`** / **`spa`**: add `web/` per [frontend-inertia](../guides/frontend-inertia.md) or [frontend-react](../guides/frontend-react.md) / [frontend-svelte](../guides/frontend-svelte.md), replace the page templates with pages, then set the header.

Full templates for these modes are not shipped yet (0.7.1).

## Windows

`offrails-gen` (`new`, `add`, `parts`, `update-part`, `generate`, `destroy`) is plain Rust and runs the same on Windows. The app builds and its SQLite suite runs there (CI matrix: Linux, macOS, Windows). `.gitattributes` keeps every source file LF, so rustfmt, the anchors and the tests agree across systems. The `bin/` scripts are POSIX shell: on Windows use Git Bash or WSL, or the plain commands they wrap:

```powershell
copy .env.example .env
# Tailwind: download tailwindcss-windows-x64.exe for the TAILWIND_VERSION pinned in bin/setup-tailwind
#   (https://github.com/tailwindlabs/tailwindcss/releases) to bin\tailwindcss.exe, then:
bin\tailwindcss.exe -i assets/app.css -o static/app.css --minify
cargo run -- db setup
cargo run -- serve                                          # one terminal (no auto-rebuild; cargo watch -x "run -- serve" adds it)
bin\tailwindcss.exe -i assets/app.css -o static/app.css --watch   # another
```

Known gaps on Windows: `bin/setup`, `bin/dev` and `bin/dev-cert` (sh), `sh .offrails/tools/offrails-check` (sh: run it in Git Bash), `tools/ci/memory-budget.sh` (uses `ps`), and the puppeteer browser smokes (`run.sh`; `smoke-boosted.mjs` is plain Node and finds Chrome or Edge under `Program Files`). Postgres is proven on Linux.

## Keeping the template, the parts and the guides in sync

- **The guides are the spec** (what each part must do, and why). The template is that spec built and tested. Where a guide used to carry a whole file the template now ships, it points at the template file instead and keeps only the excerpts that teach something, so the two can't drift.
- **Parts come from the samples repo**, one way: `tools/sync-parts.sh <samples-checkout> <ref>` copies them verbatim and records the commit in `parts/SOURCE`. Never edit `parts/` here; fix the sample upstream, then sync.
- **The base app is the framework's.** `template/app/` started as the samples repo's `host/` (2026-10-17, samples `f5541e9`); from 0.7.1 on, `host/` is generated from it (`new … offrails_host "Offrails Host"`), so parts are proven against exactly what apps get.
- **CI proves it**: `tools/ci/test-template.sh` generates the bare app, the app with every part, and a bare app with each part added afterwards (then added again, which must change nothing), and runs the Definition of Done on both databases plus the memory budget.

## Decisions

- 2026-10-17: the framework ships its app skeleton as real, tested files that agents copy (template/), instead of Markdown code they retype (because a skeleton built from the guides cost an agent 50+ minutes and still drifted; the human decided: "add the full skeleton as templates")
- 2026-10-17: optional kernel parts are the samples, vendored, and added with one command (`cargo offrails add-part`), by the same anchor/wire mechanism as the samples' install.sh; the base is lean (no jobs, no mailer) and the agent proposes the parts the product needs (because a blog without background jobs shouldn't carry a job queue, and a part added later must be as easy as one added at generation)
- 2026-10-17: Rails-style generators (`cargo offrails generate`) write feature code and its wiring instead of agents copying recipes (the human: "can we add actual code generators similar to rails? so the agent can invoke those instead of just copying"); each records what it wrote in `docs/generated.toml` so `destroy` and `--force` touch only unchanged files
- 2026-10-17: the generators are a std-only Rust tool run through cargo, not shell (because apps are built on Linux, macOS and Windows, and `sed -i` differs or doesn't exist)
