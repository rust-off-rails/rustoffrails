# webhooks

Inbound webhooks for Rust off Rails apps: `POST /webhooks/{provider}` accepts a delivery only if its signature checks out, stores each event once, and does the work in a job. It is the working version of the rule in the framework's security guide: **every `/webhooks/...` route verifies the provider's signature, or it doesn't exist.** It replaces the hand-rolled webhook controller of a Rails app (and the `stripe-ruby` or `octokit` signature helpers) and Phoenix's `Plug.Parsers` body-caching dance. Depends on [jobs](../jobs/). Extracted from Rust off Rails apps.

- **Two providers, one enum.** GitHub style (`X-Hub-Signature-256: sha256=<hex>` over the raw body) and Stripe style (`Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>]` over `"{t}.{body}"`, any `v1` may match). A third scheme is a variant, its place in `Provider::ALL`, and four `match` arms in `src/kernel/webhooks.rs`.
- **The raw body, then the signature, then everything else.** The route reads the bytes (capped at 1 MiB: 413 beyond it), verifies the HMAC the provider documents with the secret from `Config`, and only then parses. The parsing functions live on a `Verified` value that only a successful `verify` returns, so parsing first doesn't compile.
- **Constant-time comparison** with the `constant_time_eq` crate, never `==`; every candidate `v1` is compared, none short-circuits.
- **Replay protection, twice.** A timestamp older than 5 minutes (or more than a minute ahead) is refused when the scheme has one. And every event is stored under a unique `(provider, event_id)`, where `event_id` always comes from signed bytes (Stripe's JSON `id`; for GitHub, whose delivery header is unsigned, the SHA-256 of the body): a replayed delivery is an `ON CONFLICT DO NOTHING` insert that still answers 200 and enqueues nothing, even when eight copies race or the GUID is changed.
- **Kept 30 days.** A daily `prune_webhook_events` job deletes finished events (and their payloads) after `KEEP_DAYS`.
- **2xx fast, work in a job.** The event row and its `process_webhook` job are written in one transaction. The job calls `handle`, the app's one extension point (a `match` on provider and event type); transient errors retry with backoff, and the event ends `processed`, `ignored` (no handler for the type) or `failed`, with the reason.
- **No route without a secret.** A provider with no `APP_WEBHOOK_<PROVIDER>_SECRET` answers 404, exactly like a provider that doesn't exist.
- **Refusals tell the sender nothing.** Missing header, malformed header, stale timestamp, wrong secret and tampered body all answer the same bare `401 {"error":"unauthorized"}`; the log (at `warn`) says which, without the secret, the signature or the body.
- **Floods are cheap to refuse.** Failed deliveries are counted per client and provider; after 20 in 3 minutes the client gets 429 until the window passes. Valid deliveries never count.
- **No cookies, no sessions, and no change to the CSRF layer.** See [CSRF](#csrf-and-the-bypass-the-guide-mentions) below.

Guides: [`.offrails/guides/security.md`](https://rustoffrails.org/docs/guides/security#csrf) (the rules this implements), [`.offrails/guides/jobs.md`](https://rustoffrails.org/docs/guides/jobs). Depends on: [jobs](../jobs/).

## Screens

No UI (see [What this does not cover](#what-this-does-not-cover)). A delivery from a terminal, against `bin/dev`:

```
$ curl -si localhost:3000/webhooks/github -H "X-Hub-Signature-256: sha256=$SIG" -H "X-GitHub-Delivery: $ID" -H "X-GitHub-Event: ping" --data-binary "$BODY"
HTTP/1.1 200 OK
{"status":"received"}
$ (the same command again)
{"status":"duplicate"}
$ (a changed body)
HTTP/1.1 401 Unauthorized
```

## Install

By hand, in the app root, after the [jobs](../jobs/) sample:

1. `cargo add hmac` and `cargo add constant_time_eq` (never with versions; this sample was verified with hmac 0.13 and constant_time_eq 0.6).
2. Copy `files/` into the app, keeping the paths. In the test file replace `offrails_host` with your crate name.
3. Wiring, each at its `samples: <anchor>` comment (exact text in `sample.toml`):
   - `src/kernel/mod.rs`: `pub mod webhooks;`; `src/models/mod.rs`: `pub mod webhook_event;`; `src/controllers/mod.rs`: `pub mod webhooks;`.
   - `src/jobs/mod.rs` (the jobs sample carries four anchor comments for samples that ship a worker): `pub mod process_webhook;` and `pub mod prune_webhook_events;`, and for each of `ProcessWebhookJob` and `PruneWebhookEventsJob` an arm in `dispatch`, in `worker_config`, and its name in `WORKERS`.
   - `src/jobs/recurring.rs` (anchor `recurring`): the daily `prune_webhook_events` entry in `SCHEDULE`. An app that installed jobs before these anchors existed adds the lines by hand at the same spots.
   - `migration/src/lib.rs`: `mod m20261024_000100_create_webhook_events;` and `Box::new(m20261024_000100_create_webhook_events::Migration),` in `migrations()`.
   - `src/config.rs`: a `webhooks: WebhooksConfig` field on `Config` (field, `Debug` line, `let webhooks = WebhooksConfig::from_env()?;`, `webhooks,` in the constructor, `webhooks: WebhooksConfig::none()` in `Config::test()`), and the `WebhooksConfig` struct appended at `config sections`.
   - `src/routes.rs`: the route `.route("/webhooks/{id}", post(webhooks::receive))`, the `paths::webhook(provider)` helper, and the `ROUTES` row (`Access::Public`).
   - `.env.example` and `compose.yaml`: the `APP_WEBHOOK_*_SECRET` variables.
   - `tests/app/main.rs`: `mod webhooks_test;`; `tests/app/support.rs`: the `post_raw` method on `TestApp`.
4. `cargo fmt --all && cargo run -- db migrate && cargo run -q -- routes`, then the Definition of Done.

`tools/install.sh webhooks <app>` does it all (it installs jobs first).

## After installing

- Create each provider's secret and set it in the **deploy configuration**, never the repo: `APP_WEBHOOK_GITHUB_SECRET` (`openssl rand -hex 32`, then paste the same value into the GitHub webhook's "Secret"), `APP_WEBHOOK_STRIPE_SECRET` (the `whsec_...` shown for the endpoint in Stripe's dashboard). At least 16 characters, or the app refuses to boot. A provider you don't set doesn't exist.
- Point each provider at `https://<your app>/webhooks/github` or `/webhooks/stripe`. In development a tunnel's host name needs `APP_ALLOWED_HOSTS=<name>` (development refuses unknown `Host` names).
- Run a worker (`app worker`, or `APP_INLINE_WORKER=true` on one server) with the `default` queue: the job runs there.
- Replace the example arms in `handle` (`src/jobs/process_webhook.rs`) with your own, and log the install and the providers you enabled in `docs/decisions.md`.

### Send a signed test delivery

The same arithmetic the providers do, with `openssl` and `curl` (the secret is the one in your `.env` or deploy config):

```sh
URL=http://localhost:3000/webhooks

# GitHub style: HMAC-SHA256 of the raw body, hex, in X-Hub-Signature-256.
SECRET=$APP_WEBHOOK_GITHUB_SECRET
BODY='{"zen":"Keep it logically awesome.","hook_id":1}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $NF}')
curl -si "$URL/github" -H "X-Hub-Signature-256: sha256=$SIG" \
  -H "X-GitHub-Delivery: $(uuidgen)" -H "X-GitHub-Event: ping" \
  -H 'Content-Type: application/json' --data-binary "$BODY"

# Stripe style: HMAC-SHA256 of "<unix time>.<raw body>", in Stripe-Signature: t=<unix time>,v1=<hex>.
SECRET=$APP_WEBHOOK_STRIPE_SECRET
BODY='{"id":"evt_test_1","object":"event","type":"invoice.paid","data":{"object":{"id":"in_1"}}}'
T=$(date +%s)
SIG=$(printf '%s.%s' "$T" "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $NF}')
curl -si "$URL/stripe" -H "Stripe-Signature: t=$T,v1=$SIG" \
  -H 'Content-Type: application/json' --data-binary "$BODY"
```

Run either twice: the second answer is `{"status":"duplicate"}`. Change one byte of `BODY` after signing, or use an old `T`, and it is a 401. `printf '%s'` (no newline) and `--data-binary` matter: the HMAC covers the exact bytes. Then look at what the job did: `sqlite3 storage/development.sqlite3 'select id, provider, event_type, status from webhook_events'`, or `cargo run -q -- jobs list`.

## CSRF and the bypass the guide mentions

The security guide shows `CsrfLayer::with_insecure_bypass(|_, uri| uri.path().starts_with("/webhooks/"))`. The app template (0.7.0) doesn't have that line and has no anchor for it, and **this sample doesn't need it**: tower-http's CSRF layer passes a POST that carries neither `Origin` nor `Sec-Fetch-Site`, which is exactly what a provider's server sends (`a_provider_server_passes_csrf`). A browser sends one of them, so a cross-site page that tries the route gets 403 before the handler, and the host's "a cross-site POST to every unsafe route is 403" sweep keeps covering it. Either way the signature is the authentication: a browser can't sign, so with or without the bypass nothing it sends is stored.

If a sender of yours does send `Origin` or `Sec-Fetch-Site` (a gateway or an in-browser tester), or your `src/routes.rs` already has it, add or keep the guide's `.with_insecure_bypass(...)` line to the `CsrfLayer` in `src/routes.rs`, and skip `/webhooks/` in the `cross_site_posts_to_every_unsafe_route_are_403` loop of `tests/app/security_test.rs`. Record it in `docs/decisions.md`. Nothing in this sample changes, and its tests pass either way.

## Adapting it

- **React to an event.** `handle` in `src/jobs/process_webhook.rs` (`// APP:`): one arm per `(provider, event type)`. Return `Ok(Handled::Done)`, `Ok(Handled::Ignored)`, `Ok(Handled::Failed(why))` for a refusal retrying can't fix, or `Err(_)` for a transient failure (retried with backoff). It can run twice, so key what it writes on `delivery.event_id` (or on the payload's own ids). For GitHub, `delivery.kind` comes from the unsigned `X-GitHub-Event` header: check the payload has the shape that type implies before acting. Test it with `process_webhook::process` (see the tests) or `app.drain_jobs()`.
- **Add a provider.** A variant in `Provider` (`src/kernel/webhooks.rs`), in `Provider::ALL`, and its arms in `slug`, `verify`, `event` and `sign`; the secret is read from `APP_WEBHOOK_<SLUG>_SECRET` automatically, so add the line to `.env.example` and `compose.yaml`. Copy a unit test from the file's `tests` module with the provider's documented example. Shopify (base64 HMAC in a header), Slack (`v0:{t}:{body}`) and Standard Webhooks are each a `verify` of a dozen lines.
- **Limits.** `MAX_BODY_BYTES` and `FAILED_LIMIT` / `FAILED_WINDOW` in `src/controllers/webhooks.rs`, `TOLERANCE_SECS` and `FUTURE_SKEW_SECS` in `src/kernel/webhooks.rs`, `MIN_SECRET_LEN` in `WebhooksConfig` (all `// APP:`).
- **The queue and the attempts.** `QUEUE`, `MAX_ATTEMPTS` and `TIMEOUT` on `ProcessWebhookJob` (`// APP:`). It runs on `default`, not the jobs sample's rate-limited `webhooks` queue (that one serves the outbound examples).
- **Keep events longer or shorter.** `KEEP_DAYS` in `src/jobs/prune_webhook_events.rs` (`// APP:`, 30 days); the schedule is the `prune_webhook_events` entry in `src/jobs/recurring.rs` (03:17 UTC daily). Finished events older than that are deleted; one still `received` never is. Keep it longer than your providers retry: a delivery replayed after its event was pruned is stored and handled again.
- **Multi-node rate limits.** The failure counter is per process (`state.rate_limit`). Behind several nodes each keeps its own count; a shared limit needs the cache table or the proxy's own limiter.
- **Behind a proxy, set `APP_TRUSTED_PROXY=true`.** The failure counter is keyed on the client IP. If every request seems to come from the proxy, 20 forged deliveries in 3 minutes make the real provider wait too (429). Stripe retries for days; GitHub does not retry on its own (a refused delivery has to be redelivered from its webhook settings).

## Do not change

- Reading the raw bytes and verifying before parsing; `Verified` is the only door to the body's meaning (`malformed_json_with_a_valid_signature_is_stored_not_a_500`, `an_unsigned_body_is_never_parsed`).
- `constant_time_eq`, never `==` (`a_one_bit_different_signature_is_rejected`, `signatures_are_compared_with_constant_time_eq_never_equals`).
- The timestamp window (`a_stale_timestamp_is_401`, `a_timestamp_from_the_future_is_401`).
- The unique `(provider, event_id)` index and the `ON CONFLICT DO NOTHING` insert, never a caught unique violation, and an `event_id` taken only from signed bytes (`replays_store_one_event_and_enqueue_one_job`, `racing_replays_store_one_event_and_enqueue_one_job`, `a_github_body_resent_with_a_new_delivery_id_is_a_duplicate`).
- `kernel::db::begin` (BEGIN IMMEDIATE on SQLite) for the event-and-job transaction, never `.begin()`.
- The event and its job in one transaction; the job never gets the payload in its args (`a_valid_github_delivery_is_accepted_stored_and_enqueued`).
- 404 for a provider without a secret, the same as an unknown one (`a_provider_without_a_secret_is_404`).
- One identical answer for every refusal (`a_refusal_never_says_which_check_failed`).
- `Debug` for the config as an allowlist, secrets absent from logs (`webhook_secrets_never_reach_debug_output`, `failures_are_logged_at_warn_without_the_secret_or_the_signature`).
- No cookies on these routes, and every other POST route CSRF-protected (`webhook_responses_involve_no_cookies_or_sessions`, `a_provider_server_passes_csrf`, `csrf_still_protects_every_other_post`).

## What this does not cover

- **Outbound webhooks.** This receives. Sending webhooks to your customers (signing, retries, a delivery log, endpoint management) is not here; the jobs sample's `DeliverWebhookJob` is a simulated example, not a sender.
- **A UI.** There is no admin page for events. Read the `webhook_events` table, or `app jobs list` for the jobs; retrying a `failed` event means enqueueing `ProcessWebhookJob { event_id }` again after setting its status back to `received`.
- **Only these two schemes.** GitHub's and Stripe's. Others are a few lines (above), but they aren't written or tested.
- **GitHub's scheme has no timestamp, and its headers are unsigned.** A captured GitHub body verifies forever. Resending it with a new `X-GitHub-Delivery` gets nowhere, because the dedupe key is the body's SHA-256, not that header, but only while the first event is kept (`KEEP_DAYS`): after it is pruned, the same body is a new event. `X-GitHub-Event` (the stored `event_type`) is unsigned too, so a handler must not trust the type alone; check the payload's shape. Two genuinely different GitHub events with byte-identical bodies would be stored once (GitHub bodies carry ids and timestamps, so this doesn't happen in practice). Stripe's signed timestamp and signed `id` don't have these gaps.
- **One secret per provider.** No two-secret rotation window on our side (Stripe signs with both while it rolls its own secret, and any `v1` is accepted); rotate by setting the new value and redeploying, and expect a few retried deliveries.
- **No IP allowlists**, no mTLS, no payload encryption at rest: the raw payload is stored as text and may hold personal data.
- **No ordering.** Events are processed as their jobs run; a later event can finish before an earlier one.
- **A timed-out last attempt** leaves the event `received` with the last noted error (the queue marks the job discarded). Query `status = 'received'` and old `received_at` to find such events.
- **Rate limiting is per process** (see above).

## How it works

`POST /webhooks/{provider}` (`controllers::webhooks::receive`), in this order:

| Step | Check | Answer |
|------|-------|--------|
| 1 | The provider is known and has a secret | else 404 |
| 2 | The client has fewer than 20 recent failures | else 429 + `Retry-After` |
| 3 | `Content-Length` and the body are at most `MAX_BODY_BYTES` | else 413 |
| 4 | The signature (and the timestamp window) is valid over the raw bytes | else 401 |
| 5 | `INSERT ... ON CONFLICT (provider, event_id) DO NOTHING`, and the job, in one transaction | 200 `received`, or 200 `duplicate` |

The `process_webhook` job (`jobs::process_webhook`) loads the event by id, parses the stored payload, calls `handle`, and records `processed`, `ignored` or `failed` with `processed_at` and the redacted error. A job that runs again for a finished event does nothing.

| Route | Handler | Access |
|-------|---------|--------|
| `POST /webhooks/{id}` (`id` is the provider slug: `github`, `stripe`) | `webhooks::receive` | public, authenticated by signature |

| Env var | Meaning |
|---------|---------|
| `APP_WEBHOOK_GITHUB_SECRET` | the GitHub webhook secret; unset means `/webhooks/github` is a 404 |
| `APP_WEBHOOK_STRIPE_SECRET` | the Stripe endpoint's signing secret (`whsec_...`); unset means 404 |

The `{id}` in the path is the provider slug: the host's route sweeps (`concrete()` in `security_test.rs`) fill `{id}` in, and name no other parameter.

Table `webhook_events`: `id` (64-bit), `provider`, `event_id` (the dedupe key), `delivery_id` (GitHub's unsigned GUID, for display), `event_type`, `payload` (text, as received), `received_at`, `processed_at`, `status` (`received`, `processed`, `ignored`, `failed`, enforced by a `CHECK`), `error` (redacted, at most 1000 characters). Unique on `(provider, event_id)`.

## Files

| File | What |
|------|------|
| `src/kernel/webhooks.rs` | `Provider` (slug, verify, event, sign), `Verified`, the Stripe header parser, hex, unit tests with GitHub's documented example |
| `src/models/webhook_event.rs` | the entity, `Status`, `record` (insert or ignore), `finish`, `note_error`, `prune` |
| `src/controllers/webhooks.rs` | the route: the five steps above |
| `src/jobs/process_webhook.rs` | `ProcessWebhookJob`, `handle` (the extension point), `process` (status and retry bookkeeping) |
| `src/jobs/prune_webhook_events.rs` | `PruneWebhookEventsJob`, the daily retention job, and `KEEP_DAYS` |
| `migration/src/m20261024_000100_create_webhook_events.rs` | the table, its `CHECK`, the unique index |
| `tests/app/webhooks_test.rs` | the tests named in CHECKS.md |
