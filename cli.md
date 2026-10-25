# CLI: the `app` binary is `bin/rails`

Every app compiles to one binary with `clap` subcommands. The binary is named after the crate. These docs call it `app`.

## Subcommands to generate in `src/main.rs`

```
app serve                       # rails server       (default when no subcommand)
app worker [--queues default:10,mailers:5]  # bin/jobs  (queue producers + Stager/Lifeline/Pruner/Cron)
app db migrate                  # rails db:migrate   (embedded Migrator::up)
app db rollback [--steps N]     # rails db:rollback  (Migrator::down)
app db status                   # rails db:migrate:status (Migrator::get_migration_with_status; print `{:?}` of `m.status()`: "Applied/Pending <name>". MigrationStatus isn't in the prelude, and Debug needs no import)
app db seed                     # rails db:seed      (src/tasks/seed.rs; one transaction, find-or-create per record: #seeds)
app db setup                    # rails db:setup     (migrate + seed)
app db reset                    # Migrator::fresh (drop all tables, migrate) + seed; refuses unless APP_ENV=development
app db console                  # rails dbconsole    (runs sqlite3 <path> or psql <url>, based on DATABASE_URL; no pool opened)
app routes                      # rails routes       (prints ROUTES with access levels, rewrites docs/routes.md; no DB needed)
app task <name> [args…]         # rails runner / rake tasks (src/tasks/<name>.rs)
app jobs stats|list|show|retry|cancel|delete|run|pause|resume|prune   # Mission Control / Oban Web, in a terminal
app health                      # container healthcheck: GET /up on 127.0.0.1:$PORT, exit 0 on 200 (no DB, no curl in the image)
app introspect                  # development only: the app's state as JSON (routes, redacted config, migrations, jobs, assets, CSS staleness); live streams/subscribers: GET /dev/introspect.json (guides/dev-tools.md#introspection)
app task tokens create <email> [name] | list <email> | revoke <id>   # API tokens (apps with an API; guides/api.md)
```

App jobs, workers and `app jobs …` exist once the `jobs` part is in; API tokens once accounts and an API are.

**The framework's generators**, from the app root on any OS (the alias in `.cargo/config.toml`; [template](template/README.md#using-it)):

```
cargo offrails generate <kind> <name> [field:type…] [--dry-run] [--force]   # alias g; `g --help` lists the kinds
cargo offrails snippets                                   # small tested code to copy (feed, slugs, Markdown, pager…)
cargo offrails snippet show <name>                        # its README, files and wiring
cargo offrails snippet add <name> [--as <model>]          # copied and wired, recorded; `snippet remove <name>` takes it back
cargo offrails destroy <kind> <name> [--dry-run]                           # alias d; removes only unchanged files
cargo offrails parts                          # parts: installed or available, what each needs and adds (alias list-parts)
cargo offrails add <name>… [--dry-run]        # add parts (dependencies first); re-running is a no-op (alias add-part)
cargo offrails update-part <name>… --framework <newer> [--dry-run]   # 3-way merge of a newer part, conflicts marked
cargo offrails verify [--fast|--full]         # the Definition of Done in parallel
cargo offrails brand [--check]                # brand.toml → theme CSS, src/brand.rs, favicon set, social card (WCAG AA checked)
BASE=… cargo offrails prove [--strict]        # screenshots + checks of every page → docs/proof/PROOF.md (the app must be running)
```

Verified (reference app): `Migrator::up(&db, None)`, `Migrator::down(&db, Some(steps))`, `Migrator::fresh(&db)` and `Migrator::get_migration_with_status(&db)` from `sea-orm-migration` 2.0.4. `main` parses the CLI, loads `Config::from_env()`, initializes tracing, then connects. `routes` and `db console` run before connecting.

`serve` binds `$APP_BIND:$PORT`: **`127.0.0.1` in development** (its `/dev` tools have no login), `0.0.0.0` in production (the container's interface, behind the platform's proxy); a dev container sets `APP_BIND=0.0.0.0` ([security](guides/security.md#development-is-this-machine-only)). It serves `into_make_service_with_connect_info::<SocketAddr>()` (rate limiting needs the client IP), and shuts down gracefully on Ctrl-C/SIGTERM. It also runs the worker in-process when `APP_INLINE_WORKER=true`, like `SOLID_QUEUE_IN_PUMA`. That's the default in development only. In production set `APP_INLINE_WORKER=true` yourself on a single machine (always with SQLite), so one process does everything; otherwise run `app worker` beside it ([deployment](guides/deployment.md)).

Every subcommand reads `DATABASE_URL`, so **any command works against any database**:

```sh
cargo run -- db migrate                                   # local SQLite
DATABASE_URL=postgres://… cargo run -- db status          # a remote Postgres, from your laptop
```

## Migrations on deploy

The binary embeds the `migration` crate. The Docker entrypoint (`bin/docker-entrypoint`, generated) runs `app db migrate` and then `exec`s the given command, like Rails 8's `db:prepare` on boot. Postgres migrations take an advisory lock, so concurrent boots are safe. On SQLite there is one server by definition. Platform-specific "release command" alternatives are in `guides/deployment.md`.

## Command cheat sheet

| Rails                         | Off Rails |
|-------------------------------|-----------|
| `rails new`                   | `interview.md`, then `generators/new.md` |
| `bin/setup`                   | `bin/setup` (installs tools, creates `.env` from `.env.example`, `db setup`) |
| `bin/dev`                     | `bin/dev`: `cargo watch` rebuild + restart, Tailwind `--watch` (`htmx`) or Vite (`inertia`/`spa`), and open tabs reload themselves (`static/dev-reload.js` polls `/dev/reload`) ([hot reload](guides/dev-tools.md#hot-reload)). HTTPS only with a trusted certificate (mkcert), else plain http; it prints the URL to open ([HTTPS in development](guides/dev-tools.md#https-in-development)) |
| `rails s`                     | `cargo run -- serve` |
| `rails c`                     | No REPL. Write `src/tasks/<name>.rs` and run `cargo run -- task <name>` |
| `rails dbconsole`             | `cargo run -- db console` |
| `rails db:migrate`            | `cargo run -- db migrate` |
| `rails db:rollback`           | `cargo run -- db rollback` |
| `rails db:migrate:status`     | `cargo run -- db status` |
| `rails g migration X`         | `cargo offrails g migration <verb>_<noun> [field:type…]` (never `sea-orm-cli migrate generate`: it drops the anchors) |
| `rails g …` / `rails d …`     | `cargo offrails g <kind> …` / `cargo offrails d <kind> <name>` ([generators](generators/README.md)) |
| `rails routes`                | `cargo run -- routes` (or `/dev/routes` in the browser) |
| Mission Control – Jobs        | `/admin/jobs`, or `cargo run -- jobs stats` / `jobs list --state discarded` ([jobs-dashboard](guides/jobs-dashboard.md)) |
| letter_opener_web             | `<URL bin/dev printed>/dev/mailbox` ([mailers](guides/mailers.md)) |
| (none: ask the running app)   | `curl -s $URL/dev/introspect.json` or `cargo run -q -- introspect` ([dev-tools](guides/dev-tools.md#introspection)) |
| `rails test`                  | `cargo nextest run` (SQLite) / `TEST_DATABASE_URL=postgres://… cargo nextest run` |
| `rubocop`                     | `cargo fmt --check && cargo clippy --all-targets -- -D warnings` |
| (none: specs ↔ code gates)    | `sh .offrails/tools/offrails-check` or `offrails check`: header consistency, every AC cited, Security-table tests exist, tenancy suites, no unscoped lookups, `.env.example` complete, 64-bit ids, tests that can run on Postgres, development bound to this machine with a Host check. Exit 1 on any `FAIL`; runs in CI ([workflow](workflow.md#definition-of-done)) |
| `brakeman`                    | `guides/security.md` checklist (+ optional `cargo audit`) |
| `rails assets:precompile`     | `htmx`: `bin/tailwindcss -i assets/app.css -o static/app.css --minify`. `inertia`/`spa`: `pnpm --dir web build` |
| `yarn test`                   | `pnpm --dir web test --run` (Vitest) |
| `kamal deploy` / `fly deploy` | see `guides/deployment.md` |

## Running the app from an agent

- **Start it as a tracked background task** using your harness's mechanism (Claude Code: Bash with `run_in_background`), running `bin/dev` or `cargo run -- serve`, **with a PID file** (below). Don't use `nohup … &` from a tool call when you are the session the human talks to: the harness can reap it within minutes, and the human then gets "connection refused".
- **A sub-agent that hands a running server back to its caller** is the exception: its tracked background tasks end when it does, so the server dies with it. Start it detached, with its output in a file and its PID file, and hand back the three facts the caller needs:

  ```sh
  mkdir -p storage/pids storage/logs
  PORT=3005 nohup bin/dev > storage/logs/dev-3005.log 2>&1 < /dev/null &   # bin/dev writes storage/pids/dev-3005.pid
  # wait for ✓ ready in the log, then prove it: curl -fsS <the URL it printed>/up
  ```

  Report: the URL `bin/dev` printed, `storage/pids/dev-3005.pid`, `storage/logs/dev-3005.log`. The caller (or the human) stops it with `kill "$(cat storage/pids/dev-3005.pid)"`. Re-check `/up` from the caller before telling the human it's up: if it's gone, start it again as the caller's own background task.
- **Pick a free port** and set `PORT`. Check it first with `lsof -nP -iTCP:$PORT -sTCP:LISTEN`: on macOS, development's `127.0.0.1:$PORT` bind succeeds even when another process holds `*:$PORT` (a container's published port, a production-mode run, another framework's dev server), because tokio sets `SO_REUSEADDR`. Then `127.0.0.1` reaches your app while `localhost` over IPv6 (`[::1]`) may silently reach the *other* one (verified 2026-10-16: with `*:P` or `[::]:P` held, a `127.0.0.1:P` bind succeeds; with `127.0.0.1:P` held, it fails). A taken `127.0.0.1:$PORT` still fails the bind with a clear error. After starting, confirm `/up` answers from **your** process (the PID from `lsof`). In development, `APP_URL` defaults to `https://localhost:{PORT}` when `serve` has TLS (a trusted certificate from `bin/dev-cert`) and `http://localhost:{PORT}` otherwise ([config](guides/config.md)), so email links and the CSRF origin check follow the port and scheme. **Use the exact URL `bin/dev` printed** (`→ …` and `✓ ready: …`); a bare `localhost:PORT` is http to a browser (the TLS port answers it with a 307 to https).
- **Prove it's up before telling the human:** `curl -fsS $URL/up` with the printed URL (`-k` only with `DEV_CERT_SELF_SIGNED=1`, and then tell the human to open the printed http URL, never the self-signed one). Re-check after any restart. Then `curl -s $URL/dev/introspect.json` answers what state it's in (pending migrations, stale CSS, protocol).
- **Never tell a human to click through a certificate warning.** Chrome drops cookies on an untrusted certificate, so login bounces ([dev-tools](guides/dev-tools.md#https-in-development)).
- **One PID file per server, in `storage/pids/`.** This is how any agent (Claude Code, opencode, Codex, a script) stops exactly the server it started, and nothing else. The plugin's hook that blocks `pkill` only exists in Claude Code; this convention works everywhere. A peer's agent ran `pkill -f` on a server name and killed the human's own demo.

  | Started with | PID file | Stop it |
  |---|---|---|
  | `PORT=3005 bin/dev` | `bin/dev` writes `storage/pids/dev-3005.pid` itself, refuses to start if that PID is alive or the port is taken, and removes the file on exit | `kill "$(cat storage/pids/dev-3005.pid)"`: bin/dev stops its whole tree (cargo watch, the server, Tailwind, Vite) and nothing else |
  | `cargo run -- serve` | `mkdir -p storage/pids && PORT=3005 sh -c 'echo $$ > storage/pids/serve-3005.pid; exec cargo run -q -- serve'` (`cargo run` execs the app, so the PID is the server's; verified) | `kill "$(cat storage/pids/serve-3005.pid)" && rm storage/pids/serve-3005.pid` |

  Then confirm: `lsof -nP -iTCP:3005 -sTCP:LISTEN` prints nothing. **Never** `pkill`/`killall`, and never kill by name or pattern (`pkill -f`, `ps | grep | xargs kill`). **For `bin/dev`, never kill only the listening PID** (`lsof -ti … | xargs kill`): the server dies, but cargo watch and Tailwind keep running and start it again on the next edit. Killing the listener is right only for a plain `serve` you started without a PID file, after checking with `lsof -nP -iTCP:$PORT -sTCP:LISTEN` that the PID is yours.
- **Non-interactive runs (`claude -p`, `opencode run`, `codex exec`, CI): stop every server you started before your final message.** A background `bin/dev` (or `cargo watch`, Vite, Tailwind watch) keeps the session's process tree alive, so the run never exits. Last step: `kill "$(cat storage/pids/<file>)"` for each PID file you created, then confirm with `lsof -nP -iTCP:$PORT -sTCP:LISTEN` printing nothing for each port you used (Vite's too). In an interactive session, leave it running only if the human will use it, and say on which URL.
- **Read emails through the mailbox JSON** (`/dev/mailbox/latest.json?to=…`), never by grepping raw `.eml` files ([mailers](guides/mailers.md)).

## Seeds

`src/tasks/seed.rs` runs in **one transaction** (`kernel::db::begin`, committed at the end) and is idempotent **per record**: each record is found by a natural key (email, slug, team name) and created only if missing. Never key the whole seed on one check (`if teams::count(db) > 0 { return }`): a seed that failed after the first team then never retries, and the dev database stays half-seeded for good (a peer's `seed_if_empty` did exactly that). Running `app db seed` twice changes nothing; a failed run leaves nothing behind. Development only for accounts with known passwords: refuse in production.

## Tasks (`src/tasks/`)

Each task is `pub async fn run(state: &AppState, args: Vec<String>) -> anyhow::Result<()>`, registered in a `match` in `src/tasks/mod.rs`. Tasks are where data fixes, backfills and admin operations go. They're also how you "use the console": write what you'd have typed in `rails c` as a task, run it, and delete it if it was throwaway. Data-fix tasks get committed so there's a record.
