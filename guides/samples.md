# Samples and parts: tested code for the parts most apps need

> **TL;DR**
> - Samples are tested, security-reviewed code for the parts most apps need: mailer, dev mailbox, jobs, jobs dashboard, auth (core, password, magic link, TOTP), realtime, dev dashboard. They're developed in **<https://github.com/rust-off-rails/samples>** (screenshots of every screen in its README) and **shipped in every release as parts**: `.offrails/template/parts/<name>/`.
> - **The default path:** a new app is the template plus the parts its product needs, proposed by the agent and confirmed by the human ([template → choosing parts](../template/README.md#choosing-parts)). Later needs get a part too: `cargo offrails add-part <name>`.
> - The guides stay the spec; a part is that spec built. Once added, the app owns the code: adapt `// APP:` lines, keep every `// SECURITY:` and `// INVARIANT:` marker and the tests CHECKS.md cites, then the Definition of Done on both databases.
>
> Read the rest when you propose, add or adopt a part (interview, a feature that needs one, upgrade).

The guides say what each part must do. A part is that part **built**: compiled, tested on SQLite and Postgres, checked by `offrails-check`, `cargo deny` and `cargo audit`, given a threat model, and reviewed adversarially. It follows the framework's conventions exactly, so added code looks like code you would have written. From the moment it's added, the app owns it. There's no crate, no update channel and no runtime link back.

## Rules

1. **Propose them by default.** At the interview, propose the template plus the parts that fit the plan, with a one-line reason each and the link <https://github.com/rust-off-rails/samples> so the human sees what they get; take the human's yes, or their opt-out, and record it ([interview](../interview.md#then-confirm-the-plan-once-before-generating)). When a feature needs a part the app lacks, add it ([workflow](../workflow.md#when-the-product-grows-a-need-add-the-part)). On upgrade, offer parts for areas an older app built by hand ([upgrade](../generators/upgrade.md#7b-offer-the-parts)).
2. **Never replace working code without consent.** An app's hand-built mailbox that passes its tests stays until the human says otherwise.
3. **The guide wins.** A part may do more than its guide (more screens, more checks). Where it contradicts a rule in `.offrails/` (a security non-negotiable, a convention), follow the guide, note the difference in `docs/decisions.md` and tell the human. That's a samples bug worth reporting.
4. **Keep what makes it safe.** `// SECURITY: … (test: <name>)` and `// INVARIANT: … (test: <name>)` markers stay, and so does every test they name and every test in the part's CHECKS.md table. Change `// APP:` lines freely; that's what they're for. `// WHY:` explains a choice: change it only once you've understood it.
5. **Prove it in this app.** Run the part's own tests plus the Definition of Done on SQLite **and** Postgres. Add the part's CHECKS.md STRIDE rows to the feature spec that uses it (`offrails-check` then verifies the tests exist).

## Using one

In an app made from the template (0.7.1 and later):

```sh
cargo offrails list-parts                # installed vs available, what each needs and adds
cargo offrails add-part dev-mailbox      # installs mailer first; refuses (changing nothing) on a conflicting file or a missing anchor
```

`add-part` runs each `cargo add` in the part's `sample.toml`, copies `files/` to the same paths (the samples' host crate name `offrails_host` becomes yours), applies each `[[wire]]` above its `samples: <anchor>` comment, logs `installed sample <name> <version>` in `docs/decisions.md`, keeps the `AGENTS.md` header current and runs `cargo fmt --all`. Re-running it changes nothing. Then: `cargo run -q -- db migrate` (parts with tables), `cargo run -q -- routes`, the part's README ("After installing", "Adapting it") and CHECKS.md in `.offrails/template/parts/<name>/`, and the Definition of Done on both databases. Log what you adapted: `YYYY-MM-DD: adapted part <name> (<what>)`.

The samples repo's own `tools/install.sh <name> <app>` does the same from a checkout (same format, same record line), for a sample newer than the release's parts: fetch the tag matching `.offrails/VERSION` (`git clone --depth 1 --branch "v$V" https://github.com/rust-off-rails/samples ../offrails-samples`), never `main` without the human's OK.

**An app built from the guides before 0.7.1** may lack pieces parts build on: the anchors, the theme switch (`kernel/theme.rs`, `kernel/cookie.rs`, `POST /theme`, the `offrails_theme` cookie), `kernel/redact.rs`, `Chrome::stylesheet`/`script` in `kernel/ui.rs`, and shapes the wires assume (the `Config` `Debug` builder `d.field(…)`, `Tab { key, … }` in `kernel/ui.rs`). `add-part` names each missing anchor and changes nothing. Compare the app with `.offrails/template/app/` (the files listed in [new → what the template contains](../generators/new.md#what-the-template-contains)), port what's missing as an ordinary change (`cargo check` names it), add the anchors at the spots [new → anchors](../generators/new.md#anchors) lists, then add the part. When the app has its own version of a part (say, a hand-built `kernel/mailbox.rs`), the part replaces it: remove yours only once the part's tests are green.

A part that ships a migration ships it with a fixed timestamped name. In an app that already has the equivalent table, **don't copy that migration**: reconcile the table with a new migration, per [upgrade step 4](../generators/upgrade.md#4-reconcile-dont-re-create), and never edit an applied one. The auth parts bring their own schema (`users` with `email_verified_at`, `sessions`, `user_tokens`, `auth_events`, credential tables). In an app that already has accounts, adopting them is a data migration, not a copy: plan it as one and show the plan first.

## Anchors

Shared files (`routes.rs`, `config.rs`, `state.rs`, `main.rs`, `kernel/mod.rs`, `migration/src/lib.rs`, `tests/app/main.rs`, the layout, `.env.example`…) carry one-line comments where a sample inserts code: `// samples: routes`, `{# samples: nav links #}`, `# samples: env`. `install.sh` inserts each wire **above** its anchor, indented like it, so the next sample finds the anchor too. Apps generated from the template have them ([new → anchors](../generators/new.md#anchors)). Older apps don't, so `install.sh` refuses and names each missing one. Either add the anchors, or apply that wiring by hand (each README's *Wiring* section says where in words).

- **An anchor is a line of its own, never a trailing comment.** `rustfmt` leaves a comment that stands alone on its line where it is: in a `vec![…]`, a `match`, a struct literal, between the links of a builder chain, after a block of `mod` lines. A comment **trailing** code travels with that code instead: `pub mod z; // samples: kernel mods` moves to wherever `rustfmt` sorts `mod z`, and `.route(…) // samples: routes` stays glued to that call when the chain is reflowed. An insertion "above" such an anchor lands in the wrong place, so `install.sh` refuses it. (Checked with rustfmt 1.99, edition 2024, 2026-10-16.)
- A standalone anchor also splits `rustfmt`'s `mod` sorting into groups: a `pub mod x;` inserted above `// samples: kernel mods` is sorted among the lines above it and never crosses the anchor.
- Inside a builder chain or a `vec![…]`, put the anchor where a whole new element or `.layer(…)` line fits; a wire's `text` is always a complete element.
- After `cargo fmt`, every anchor must still stand alone: `grep -rnE '[^[:space:]].*(//|\{#|#) samples: ' src templates migration tests | grep -vE ':[[:space:]]*(//|\{#|#) samples: '` prints nothing.

## The samples

Each row says what the part adds over what the guide alone leads you to build. Use it to describe the part honestly when you propose it, with the link <https://github.com/rust-off-rails/samples> (screenshots of every screen).

| Sample | Guide | Depends on | What it adds |
|--------|-------|------------|--------------|
| `mailer` | [mailers](mailers.md) | — | The adapters (SMTP, dev mailbox storage, log, memory), previews, a `send_test_email` task to prove SMTP works, and header-injection checks before anything is sent |
| `dev-mailbox` | [mailers](mailers.md#the-dev-mailbox-kernelmailboxrs-swooshs-local-mailbox) | mailer | A live inbox: HTML, text, raw and headers; width previews with a 320 px phone lint (fixed widths nothing reins in) and dark previews; sign-in codes and links one keystroke from copied; attachments as safe downloads; search, keyboard shortcuts, JSON and a CLI |
| `jobs` | [jobs](jobs.md) | — | The Oban-style queue, finished: `SKIP LOCKED` claims on Postgres, typed args, backoff with jitter, unique jobs, priorities, scheduled and cron jobs, rate-limited queues, batches, timeouts, orphan rescue, graceful shutdown, pruning, `app jobs`; args and failure messages redacted wherever they're shown or searched |
| `jobs-dashboard` | [jobs-dashboard](jobs-dashboard.md) | jobs | `/admin/jobs` as an operations console: live counters, throughput and queue-wait charts, a now-running lane with progress, queues with pause/resume, worker success rates, **errors grouped by root cause**, job timelines with every attempt, cron with run-now, bulk actions; keyboard and mobile friendly |
| `auth-core` | [auth](auth.md) | mailer, dev-mailbox | Users and database sessions with rotation and revocation, an account security page (devices, activity), re-auth for sensitive actions, email change, security emails with a "this wasn't me" link, an audit trail, progressive throttling, and claiming unverified accounts (pre-hijacking) |
| `auth-password` | [auth](auth.md#password-variants-signup-invite) | auth-core | argon2id with OWASP parameters and rehash on sign-in, sign-up with email verification, per-account progressive delays, reset by single-use link, change password, a bundled common-password check and a strength meter that works without JS |
| `auth-magic-link` | [auth](auth.md#magic-link-variants-magic-link-magic-link-invite) | auth-core | The emailed link **plus a 6-digit code** (for phones that open links in another browser), bound to the asking browser, race-safe, sign-up on first use, re-auth by emailed code, wrong codes throttled per address |
| `auth-totp` | [auth](auth.md) | auth-core | Two-factor with an authenticator app: QR enrolment rendered on the server, confirm-before-enable, a code step after any sign-in method, replay protection, 10 hashed recovery codes, "trust this device for 30 days", secrets encrypted at rest |
| `realtime` | [channels](channels.md), [realtime](realtime.md) | — (accounts with auth-core) | Channels over `offrails.v1` with SSE and long-poll fallbacks, typed rooms, topic authorization on every join and push, presence across tabs and nodes, resume after reconnect, signed socket tokens, revocation, per-user caps, tuned buffers, `/dev/sockets` and a live room |
| `webhooks` | [security](security.md#csrf) | jobs | `POST /webhooks/{provider}` that verifies the HMAC over the raw body (GitHub and Stripe schemes, constant-time), refuses stale timestamps, stores each event once so a replay is a no-op, answers fast and does the work in a job; no route without a secret, the same bare 401 for every refusal, 429 after repeated failures |
| `dev-dashboard` | [dev-tools](dev-tools.md), [errors-and-logging](errors-and-logging.md) | — | `/dev/dashboard`, a LiveDashboard in-process: live charts (memory, CPU, req/s, p50/p95/p99, errors, DB pool, Tokio), requests per route, trace waterfalls, SQL fingerprints with N+1 detection and EXPLAIN, a live log tail, ranked insights, a routes explorer, a schema browser, test runs, CI and a load lab; a read-only subset for admins in production |

The kernel pages of the samples follow one theme switch: the `offrails_theme` cookie, rendered server-side as `data-theme` ([views](views.md#dark-mode)).
