# webhooks: checks

What "working and safe" means for inbound webhooks, and the test that proves each item. `tools/security-check.sh` fails when a test named here doesn't exist; `tools/ci.sh webhooks` runs them all on SQLite and Postgres.

`POST /webhooks/{provider}` is reachable by anyone on the internet and has no login, no cookie and no session: the provider's signature is the only authentication. Its attack surface is the request itself (headers, body, size, rate), the stored payload, and the handler the job calls.

## Threat model

| STRIDE | Threat | Control | Test |
|--------|--------|---------|------|
| Spoofing | Someone posts a forged event as GitHub or Stripe | HMAC-SHA256 over the raw body with the provider's secret; a missing, malformed or wrong signature is a 401 | `a_wrong_secret_is_401`, `a_missing_or_malformed_signature_is_401`, `a_valid_github_delivery_is_accepted_stored_and_enqueued`, `a_valid_stripe_delivery_is_accepted_stored_and_enqueued` |
| Spoofing | A signature is guessed byte by byte from response timing | The comparison is `constant_time_eq`, never `==`; every candidate `v1` is compared without short-circuiting | `a_one_bit_different_signature_is_rejected`, `signatures_are_compared_with_constant_time_eq_never_equals` |
| Spoofing | A captured GitHub body is resent with a new `X-GitHub-Delivery` (or another `X-GitHub-Event`): GitHub's signature covers the body only, and the scheme has no timestamp | The dedupe key is the SHA-256 of the signed body, never the unsigned GUID, so a resend is a 200 `duplicate` that stores and enqueues nothing while the first event is kept (`KEEP_DAYS`). Residual risk, documented: the type header stays unsigned (handlers check the payload's shape), and a body resent after its event was pruned is new again | `a_github_body_resent_with_a_new_delivery_id_is_a_duplicate`, `ids_and_types_are_plain_tokens_or_fall_back` |
| Spoofing | A provider nobody configured is probed (or used) | No secret, no route: 404, identical to an unknown provider | `a_provider_without_a_secret_is_404`, `an_unknown_provider_is_404` |
| Spoofing | A web page makes a victim's browser POST to the route (CSRF) | The route reads no cookie, so a browser carries no authority there, and it can't sign: nothing it sends is stored. The CSRF layer stays on too (a provider's server sends neither `Origin` nor `Sec-Fetch-Site` and passes; a browser gets 403), and the sample holds with the guide's `/webhooks/` bypass as well | `a_provider_server_passes_csrf`, `webhook_responses_involve_no_cookies_or_sessions` |
| Tampering | The body is changed after signing | The HMAC covers the exact bytes read | `a_tampered_body_is_401` |
| Tampering | The body is parsed before it is trusted (a parser bug, a JSON bomb) | Parsing lives on `Verified`, which only a successful `verify` returns; a bad signature never reaches a parser | `an_unsigned_body_is_never_parsed`, `malformed_json_with_a_valid_signature_is_stored_not_a_500` |
| Tampering | A captured valid delivery is replayed later | Stripe style: timestamps older than 5 minutes or more than a minute ahead are refused before any HMAC; a `t` that isn't 1 to 18 digits (a sign, a space, an overflow) is malformed, so the window arithmetic can't wrap | `a_stale_timestamp_is_401`, `a_timestamp_from_the_future_is_401`, `the_timestamp_window_is_five_minutes_back_and_a_minute_ahead`, `stripe_headers_without_a_timestamp_or_v1_are_malformed` |
| Tampering | A replayed (or retried, or raced) delivery runs the handler twice | Unique `(provider, event_id)` plus `ON CONFLICT DO NOTHING`: a replay is a 200 that stores and enqueues nothing, even concurrently, even inside a Postgres transaction | `replays_store_one_event_and_enqueue_one_job`, `racing_replays_store_one_event_and_enqueue_one_job`, `the_unique_index_makes_a_second_insert_a_no_op` |
| Tampering | A row gets a status the code doesn't know | `CHECK (status IN (...))` | `the_status_column_rejects_unknown_statuses` |
| Tampering | Bytes Postgres can't store (a NUL, invalid UTF-8) turn a verified delivery into a 500 and a retry storm | The stored payload is text with both replaced | `the_stored_payload_has_no_nul_and_no_invalid_utf8`, `malformed_json_with_a_valid_signature_is_stored_not_a_500` |
| Repudiation | Nobody can say what arrived, or what became of it | Every verified delivery is a row (provider, event id and type, raw payload, time, status, error); the job records `processed`, `ignored` or `failed` with the reason | `a_valid_github_delivery_is_accepted_stored_and_enqueued`, `the_job_runs_the_handler_and_marks_the_event`, `a_transient_error_retries_and_the_last_attempt_fails_the_event`, `a_handler_can_refuse_for_good` |
| Repudiation | Refusals leave no trace | Each is logged at `warn` with the provider, the reason and the client | `failures_are_logged_at_warn_without_the_secret_or_the_signature` |
| Information disclosure | A 401 says which check failed, helping an attacker adjust | One identical answer for every refusal; the reason is only in the log | `a_refusal_never_says_which_check_failed` |
| Information disclosure | The secret or a signature reaches logs, `Debug` output or an error | `Config`'s `Debug` is an allowlist; `Verified`'s `Debug` shows the provider and the body length only; logs carry the reason, never the secret, the signature or the body; a bad secret's boot error names the variable, not the value | `webhook_secrets_never_reach_debug_output`, `verified_debug_shows_neither_the_signature_nor_the_body`, `failures_are_logged_at_warn_without_the_secret_or_the_signature`, `webhook_secrets_come_from_env_vars_and_must_be_long_enough` |
| Information disclosure | A handler's error quotes a URL with a token and the event row keeps it | Stored errors are redacted and cut at 1000 characters | `stored_errors_are_redacted_and_capped` |
| Information disclosure | Raw payloads (up to 1 MiB each, possibly personal data) pile up forever | A daily recurring job deletes finished events older than `KEEP_DAYS` (30) | `finished_webhook_events_are_pruned_daily`, `prune_deletes_only_old_finished_events` |
| Information disclosure | A cookie or session is set or read on the route | None is: nothing is set on any outcome | `webhook_responses_involve_no_cookies_or_sessions` |
| Denial of service | A huge body exhausts memory | At most 1 MiB is read; a declared `Content-Length` over the cap is refused without reading; 413 before any HMAC | `an_oversized_body_is_413`, `a_declared_oversized_body_is_413` |
| Denial of service | A flood of forged deliveries keeps the HMAC and the database busy | Failed deliveries are counted per client and provider; past 20 in 3 minutes the client gets 429 before any work. Bad ones cost an HMAC and no database write | `failed_deliveries_are_rate_limited_per_client` |
| Denial of service | Concurrent writes (the job worker) make the delivery's transaction fail with "database is locked" on SQLite, turning a valid delivery into a 500 | The event and its job are written in `kernel::db::begin` (BEGIN IMMEDIATE), never a deferred `.begin()` | `the_delivery_transaction_begins_immediate`, `racing_replays_store_one_event_and_enqueue_one_job` |
| Denial of service | The limiter blocks the real provider | Only failures count; thousands of valid deliveries are never limited | `valid_deliveries_are_never_rate_limited` |
| Denial of service | A slow or failing handler makes the provider time out and retry | The route only verifies, stores and enqueues; the handler runs in the job, with retries, backoff and a timeout | `a_valid_github_delivery_is_accepted_stored_and_enqueued`, `a_transient_error_retries_and_the_last_attempt_fails_the_event` |
| Elevation of privilege | Letting webhooks through CSRF opens other POST routes | Nothing is bypassed for this sample; every other unsafe route is still 403 for a cross-site POST without `Origin` | `csrf_still_protects_every_other_post` |
| Elevation of privilege | A webhook acts with a user's rights | There is no user: the route reads no cookie, and the job runs the app's `handle` with no session | `webhook_responses_involve_no_cookies_or_sessions`, `the_worker_is_registered_and_dispatches` |

## Invariants

| Invariant | Test |
|-----------|------|
| GitHub's documented example verifies, and `sign` produces its signature | `githubs_documented_example_verifies` |
| Stripe signs `"{t}.{body}"`, accepts any `v1`, ignores other schemes, and rejects headers without a timestamp or `v1` | `stripe_signs_the_timestamp_a_dot_and_the_raw_body`, `stripe_accepts_any_v1_and_ignores_other_schemes`, `stripe_headers_without_a_timestamp_or_v1_are_malformed` |
| Event ids and types are plain tokens, or the body's SHA-256 and `unknown` stand in | `ids_and_types_are_plain_tokens_or_fall_back` |
| A verified delivery is always stored, whatever its body | `malformed_json_with_a_valid_signature_is_stored_not_a_500` |
| The route is `POST` only, public, and in the routes table | `only_post_is_routed`, `the_webhook_route_is_public_and_listed` |
| A job for a finished event does nothing; one for a missing event is discarded; a handler can refuse for good | `a_finished_event_is_not_handled_twice`, `a_missing_event_is_discarded`, `a_handler_can_refuse_for_good` |
| The handler's outcomes land on the event (`processed`, `ignored`, `failed`) | `the_job_runs_the_handler_and_marks_the_event` |
| Pruning deletes only old finished events | `prune_deletes_only_old_finished_events` |
| Providers' slugs and env var names are fixed | `slugs_round_trip_and_name_their_env_var` |
| Secrets come from `APP_WEBHOOK_<PROVIDER>_SECRET`, blank means unset, short means no boot | `webhook_secrets_come_from_env_vars_and_must_be_long_enough` |

## Not covered (so not tested)

Outbound webhooks, a UI, schemes other than these two, two-secret rotation, IP allowlists, and replay protection for GitHub bodies older than the retention window (README, "What this does not cover").

## Commands

```sh
TEST_DATABASE_URL=postgres://.../postgres tools/ci.sh webhooks   # alone (installs jobs first): fmt, clippy (+ unwrap/expect gate), SQLite, Postgres, offrails-check, deny, audit
TEST_DATABASE_URL=postgres://.../postgres tools/ci.sh all        # with every other sample
tools/security-check.sh                                        # every test named here and in SECURITY/INVARIANT markers exists
cargo nextest run webhook                                      # in an app: just these tests
```
