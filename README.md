<p align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="brand/lockup-on-dark.svg"><img src="brand/lockup-on-light.svg" alt="Rust off Rails" height="72"></picture></p>

# Rust off Rails

> The opinionated web stack for the AI era.

**Rust off Rails** is a web framework for AI coding agents. It ships no library you depend on: it's a set of Markdown files that tell agents how to build a full-stack Rust web app (one blessed stack, one directory layout, one way to do each thing), plus the **app template** those files describe, as real, tested files the agent copies instead of retyping ([template/](template/README.md)).

The convention lives in the prose; the code is copied from the template and its parts, then written by the agent for everything app-specific. Humans still write none of it.

## Take only what you need

Nothing here is all-or-nothing. There is no framework code hidden under your app: what we give you is copied into it, so it's all in your repo and yours to change. (Apps still use ordinary crates like Axum and SeaORM.) Your agent gets good ideas and inspiration: the crates we'd recommend and why, and tested reference code for things we like, such as job processing, presence and authenticated webhooks, in [the samples repo](https://github.com/rust-off-rails/samples), with screenshots. Use the whole template, one part, or only a convention. We're still experimenting and building real apps with it, and so far it has gone better than we expected.

## Standing on giants

We take the best ideas of two great ecosystems and bring them together in one stack.

- **From Ruby on Rails:**
  - convention over configuration;
  - generators (`new`, `scaffold`), Active Record-style models and migrations;
  - Rails 8 authentication;
  - the Solid trifecta: jobs, cache and pub/sub in the database, no Redis;
  - Turbo 8-style page-refresh broadcasts;
  - mailer previews, `bin/rails` tasks, and Kamal-ready Docker with `/up`.
- **From Elixir and Phoenix:**
  - **Oban** persistent jobs and the **Oban Web** dashboard;
  - **Swoosh**'s local mailbox;
  - **phx.gen.auth** magic links and the "check your mailbox" hint;
  - **LiveDashboard**-style dev tools;
  - **Channels, Presence and PubSub**;
  - multi-node clustering and "let it crash" supervision;
  - Ecto's explicit data layer (no callbacks);
  - the pending-migrations page.
- **And also:** htmx, Inertia.js, Tailwind, shadcn-style owned components, and end-to-end types from Rust to TypeScript.

[`RAILS_MAP.md`](RAILS_MAP.md) maps every Rails concept to its Off Rails equivalent, plus a [Borrowed from Phoenix / Elixir](RAILS_MAP.md#borrowed-from-phoenix--elixir) section.

## The deal

| Rails gave you…                     | Rust off Rails gives your agent…                     |
|------------------------------------|------------------------------------------------------|
| A gem with 300k lines of Ruby      | ~50 Markdown files and an app template the agent copies and owns |
| `rails generate scaffold`          | `generators/scaffold.md`: a recipe the agent runs    |
| Convention over configuration      | Convention over *deliberation*: the agent never debates which crate to use |
| "Optimized for programmer happiness" | Optimized for agent correctness: compiler-checked everything |
| Magic                              | No magic. Explicit code is cheap when you don't type it. |

## Rules of the framework

1. **All code is written by AI. No exceptions.** Humans write intent: specs in Markdown, interview answers and feedback. Agents write every line of Rust, SQL, HTML, CSS, TypeScript, config and scripts. If a human edits code anyway, the agent treats the edit as a spec change, records it, and owns that code from then on.
2. **Strong opinions, aimed at agents.** Every decision an agent would otherwise improvise (crate choice, file location, naming, error handling, testing) is already made in these docs.
3. **Everything Rails has, mapped.** Active Record, Action Controller, Action View, Active Job, Action Mailer, Action Cable, Active Storage, Solid Cache/Queue/Cable, generators, `bin/rails`, credentials, Kamal. See [`RAILS_MAP.md`](RAILS_MAP.md).

## Using it

### Install (one line)

```sh
curl -fsSL https://rustoffrails.org/install.sh | sh
```

It checks for Rust (and offers rustup), installs the cargo tools the framework uses as prebuilt binaries, downloads and **SHA-256-verifies** the framework into `~/.offrails/`, installs the Claude Code plugin if `claude` is on your PATH, and adds an `offrails` command. No sudo, no telemetry, and it's safe to re-run. Add `--dry-run` to see the plan first. [INSTALL.md](INSTALL.md) explains every step, with a manual equivalent.

```sh
offrails new bookshelf "people track the books they've read"   # starts Claude Code (or Codex) with /offrails:new
offrails doctor                                                # prerequisites and versions
offrails update                                                # latest release
```

### Or by hand: Claude Code plugin

```
/plugin marketplace add rust-off-rails/rustoffrails      # or a local checkout: /plugin marketplace add /path/to/rustoffrails
/plugin install offrails@offrails
```

Then, in an empty directory:

```
/offrails:new bookshelf: people track the books they've read and review them
```

The agent interviews you first (accounts? who owns data? htmx or React? where will it run?), using recommended defaults so you can just say "you decide". It confirms a plan, then builds. The skills also trigger on plain requests ("make me a new Rust web app…").

| Skill | Rails equivalent | What it does |
|-------|------------------|--------------|
| `/offrails:new [name] [pitch]` | `rails new` | Interview → plan → app skeleton → planned features, green |
| `/offrails:feature [request]` | — | Spec first, then code. With no argument: build what changed in `product/` |
| `/offrails:scaffold Book title:string …` | `rails g scaffold` | Full resource in the app's frontend mode |
| `/offrails:generate job\|mailer\|channel\|authentication\|model\|migration\|task …` | `rails g …` | Any other generator recipe |
| `/offrails:check` | `bin/rails test` + CI | Definition of Done: fmt, clippy, tests on SQLite **and** Postgres, frontend checks |
| `/offrails:doctor [--fix]` | `rubocop` + review | Audits the app against every convention in these docs |
| `/offrails:deploy [fly\|kamal\|railway\|render]` | `kamal setup` | Deploy config, verified image; confirms before anything outward-facing |
| `/offrails:upgrade` | `rails app:update` | Moves the app's vendored `.offrails/` to the plugin's version and migrates conventions |

The plugin also installs hooks. They only act inside Off Rails apps (directories with `.offrails/`):

- **Edit guard:** blocks edits to `.offrails/` (read-only) and `web/src/types/` (generated by ts-rs).
- **Convention check:** after each edit, flags raw SQL outside the kernel, banned crates or packages, and `/:id` routes.
- **Stop gate:** the agent can't end a turn while `cargo fmt --check` or `cargo check` fails.
- **Session start:** orients the agent and notices when the app's framework version is behind the plugin's.

Each app still vendors the docs into `.offrails/`, pinned to a version, so the app keeps working with any agent, with or without the plugin.

### With Codex, Cursor, or any agent that reads `AGENTS.md`

```
Read /path/to/rustoffrails/AGENTS.md and make me a new app called "bookshelf".
```

Generated apps carry `AGENTS.md` and `.offrails/`, so later sessions need no setup.

### Then

Describe features in plain language, or write them as specs in `product/`:

```
Scaffold Book with title:string, author:string, published_on:date, rating:integer.
Users must be logged in to create books. Send the owner an email when a book gets a review.
```

### The template and its parts

A new app is a copy of [`template/app/`](template/README.md): a complete, compiling app (htmx) with the kernel, security headers, CSRF, sessions, Host allowlist, rate limiting, log redaction, SQLite and Postgres, `/dev` tools, Tailwind, CI, Docker and the kernel tests, renamed and green in minutes instead of an hour of generated Markdown. On top of it go the **parts** the product needs: the mailer and dev mailbox, jobs and their dashboard, accounts (password, magic link, TOTP), realtime with presence, and a LiveDashboard-style dev dashboard. They're the tested samples from [rust-off-rails/samples](https://github.com/rust-off-rails/samples) (screenshots of every screen there). Your agent proposes the ones your app needs, you confirm, and later needs are one command: `cargo offrails add-part <name>`. Once copied, the app owns the code ([guides/samples.md](guides/samples.md)).

The generators (`offrails-gen`: `new`, `add-part`, `list-parts`, `generate`, `destroy`, `snippets`) are a small standard-library Rust tool, so they work the same on Linux, macOS and Windows. Features start from a **snippet** (small tested code to copy: a feed, slugs, Markdown, a pager, a theme toggle; [template/snippets](template/snippets/README.md)) or a **generator** (`cargo offrails g resource posts title:string`), both optional, both the app's once written: small files per feature, one marked line per shared spot. Pages are built from a component kit (Askama macros, light and dark, branded from one `brand.toml`) in five layouts, and `cargo offrails prove` screenshots and checks every page and writes `docs/proof/PROOF.md`, tying each acceptance criterion to its passing tests and pages.

### Field tests: real apps, ported by agents

We test the framework by having agents port well-loved open-source apps to it. Each port puts the docs, the template and the parts up against a product with years of real-world design behind it, and every rough edge an agent hits goes back into the framework.

- **fizzyer**: a Rust port of [Fizzy](https://github.com/basecamp/fizzy), 37signals' kanban-style tracker. Repository coming soon.
- **A FreeScout port**: a Rust port of [FreeScout](https://github.com/freescout-help-desk/freescout), the open-source help desk. In progress; repository coming soon.
- **uptime-offrails**: a Rust port of [Uptime Kuma](https://github.com/louislam/uptime-kuma), the self-hosted uptime monitor. Repository coming soon.

The ports are built by different agents on purpose: GPT-6 Luna (high), Claude Sonnet 5.5 (high and medium), Meta's Muse Wonder 1, and smaller open models such as GLM 5.3 Flash through opencode. The goal is a framework that makes smaller, cheaper agents productive too, not only the biggest ones.

They're for education and testing first. We also plan to run them ourselves, mostly to save server resources. Each will be open source under its original project's license, and contributions are welcome. They're independent ports, not affiliated with or endorsed by the original projects; credit for each product's design goes to its authors.

## Repository layout

```
AGENTS.md          entry point for agents: read this first
interview.md       the questions agents ask before building
doctrine.md        the philosophy, rewritten for machines
RAILS_MAP.md       every Rails concept and its Off Rails equivalent
stack.md           the blessed crates (the "Gemfile")
structure.md       directory layout and naming conventions
cli.md             the app binary and the bin/rails equivalents
workflow.md        how an agent works: spec → code → green → done
guides/            one guide per framework component
generators/        recipes that replace `rails generate`
blueprints/        files copied into every new app (AGENTS.md, spec template…)
template/          the app template (app/: a complete, tested blank app) and its parts (parts/: the samples), copied by offrails-gen
.claude-plugin/    Claude Code plugin + marketplace manifests
skills/            /offrails:* skills (thin entry points into the docs above)
hooks/             convention guards (no-ops outside Off Rails apps)
evals/             `claude plugin eval` cases: does the framework steer agents right?
tools/             offrails-gen/ (new, add-part, list-parts: Rust, any OS), offrails (the CLI), brand/ (favicons + social card), browser-smoke/ (kernel pages), release/, ci/
install.sh         the one-line installer (served at rustoffrails.org/install.sh); INSTALL.md explains it
VERSION, CHANGELOG.md   framework version; apps record theirs in .offrails/VERSION
```

## Developing the framework

```sh
claude --plugin-dir .                       # load this checkout instead of the installed plugin
claude plugin validate . --strict           # manifests, skills, hooks
claude plugin eval . --runs 1               # run the eval cases
```

Every doc change goes under `## Unreleased` in `CHANGELOG.md` the moment it's made, with **App migration** notes written for an agent.  Each release section keeps the shape described at the top of `CHANGELOG.md` (What's new, Breaking changes, Upgrading, Full list of changes): it becomes the GitHub Release notes. Apps detect drift by content fingerprint (`hooks/fingerprint.sh`), not version alone, so even unreleased edits show up as an upgrade.

Releasing:
1. Rename `Unreleased` to the new version.
2. Bump `VERSION`, `.claude-plugin/plugin.json` and `tools/offrails-gen/Cargo.toml` (+ its `Cargo.lock`) together, and commit (`release.sh` refuses a mismatch: offrails-gen's version is in Cargo's fingerprint, so an app's cached `cargo offrails` binary is rebuilt for the new release).
3. `tools/release/release.sh --dry-run` to check, then `tools/release/release.sh`. It builds a reproducible `rustoffrails.tar.gz` (the whole framework, skills and hooks included), `install.sh` and `checksums.txt`, tags `v<version>`, and creates the GitHub release with `gh`. `install.sh` downloads `releases/latest/download/…`, so a release is all it takes.
4. Publish the release's `install.sh` at `https://rustoffrails.org/install.sh` (the website serves it).
5. `claude plugin tag --push` for the plugin marketplace.

Installer tests: `tools/ci/test-install.sh` (local shells + clean Linux containers; `--full` also runs the real rustup + cargo-binstall path).

Template tests: `TEST_DATABASE_URL=postgres://… tools/ci/test-template.sh` (generates the bare app, every part, parts added one by one, every generator and every snippet; the Definition of Done on SQLite and Postgres and the memory budget). Parts come from the samples repo: `tools/sync-parts.sh <samples checkout> <ref>`, never edited here.

Installed plugins update only when the version changes.

## The stack in one breath

Axum on hyper (the binary *is* the web server: HTTP/1.1 + HTTP/2, no app server or nginx needed) · Tokio · SeaORM (**SQLite locally, Postgres in production, same code**) · Tailwind · a frontend mode chosen at birth:

- **`htmx`** (default): Askama (compile-time checked templates) + htmx. No Node. Optional React or Svelte islands.
- **`inertia`**: React or **Svelte** pages with Rails-style server routing, via a generated Inertia adapter. The default when you want a JS UI.
- **`spa`**: a Vite SPA (React Router or SvelteKit in SPA mode) + a JSON API.
- **`api`**: no UI, for mobile backends and public APIs (in progress).

`UI_LIBRARY: react | svelte` picks the JS library. Props and API types are generated from Rust by `ts-rs`, so the type chain runs from the database to the browser.

Plus a database-backed job queue, cache and pub/sub (the "Solid trifecta") · lettre · object_store · tracing · one binary, one Docker image.

Yes, the acronym is still RoR. That's on purpose, and it's a thank-you.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

Security issues: see [SECURITY.md](SECURITY.md). Contributing: see [CONTRIBUTING.md](CONTRIBUTING.md).
