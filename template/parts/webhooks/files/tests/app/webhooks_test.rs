//! webhooks: inbound deliveries are authenticated by their signature and by nothing else; a
//! replay is a no-op; the work happens in a job. On SQLite and Postgres.
//! GUIDE: .offrails/guides/security.md#csrf
//!
//! Requests are made the way a provider's server makes them (`TestApp::post_raw`): no cookies, no
//! `Sec-Fetch-Site`, no `Origin`.

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use offrails_host::{
    config::{Config, WebhooksConfig},
    controllers::webhooks::MAX_BODY_BYTES,
    jobs::{
        JobInfo, Outcome, WORKERS, Worker, dispatch,
        process_webhook::{self, Delivery, Handled, ProcessWebhookJob},
        prune_webhook_events::{KEEP_DAYS, PruneWebhookEventsJob},
        recurring::SCHEDULE,
    },
    kernel::webhooks::{FUTURE_SKEW_SECS, Provider, TOLERANCE_SECS},
    models::webhook_event::{self, Status},
    routes::{Access, ROUTES, paths},
};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, sea_query::Expr};
use serde_json::json;

use crate::support::{TestApp, TestResponse};

const GITHUB_SECRET: &str = "github-test-secret-0123456789";
const STRIPE_SECRET: &str = "whsec_test_secret_0123456789";

const PUSH: &[u8] = br#"{"ref":"refs/heads/main","zen":"Keep it logically awesome."}"#;
const PING: &[u8] = br#"{"zen":"Keep it logically awesome.","hook_id":1}"#;
const INVOICE_PAID: &[u8] = br#"{"id":"evt_1NXWPnCo6bFb1KQto6C8OWvE","object":"event","type":"invoice.paid","data":{"object":{"id":"in_123"}}}"#;

fn both_providers() -> Config {
    Config {
        webhooks: WebhooksConfig::with(&[
            (Provider::GitHub, GITHUB_SECRET),
            (Provider::Stripe, STRIPE_SECRET),
        ]),
        ..Config::test()
    }
}

async fn app() -> TestApp {
    TestApp::with_config(both_providers()).await
}

async fn post(app: &TestApp, slug: &str, headers: &[(&str, &str)], body: &[u8]) -> TestResponse {
    app.post_raw(&paths::webhook(slug), headers, body).await
}

/// A GitHub-style delivery, signed with `secret`.
async fn github_with(
    app: &TestApp,
    secret: &str,
    delivery: &str,
    event: &str,
    body: &[u8],
) -> TestResponse {
    let (name, signature) = Provider::GitHub.sign(secret.as_bytes(), body, 0);
    post(
        app,
        "github",
        &[
            (name, &signature),
            ("x-github-delivery", delivery),
            ("x-github-event", event),
            ("content-type", "application/json"),
        ],
        body,
    )
    .await
}

async fn github(app: &TestApp, delivery: &str, event: &str, body: &[u8]) -> TestResponse {
    github_with(app, GITHUB_SECRET, delivery, event, body).await
}

/// A Stripe-style delivery signed at unix time `at`.
async fn stripe_at(app: &TestApp, secret: &str, at: i64, body: &[u8]) -> TestResponse {
    let (name, signature) = Provider::Stripe.sign(secret.as_bytes(), body, at);
    post(
        app,
        "stripe",
        &[(name, &signature), ("content-type", "application/json")],
        body,
    )
    .await
}

async fn stripe(app: &TestApp, body: &[u8]) -> TestResponse {
    stripe_at(app, STRIPE_SECRET, Utc::now().timestamp(), body).await
}

async fn events(app: &TestApp) -> Vec<webhook_event::Model> {
    webhook_event::Entity::find()
        .order_by_asc(webhook_event::Column::Id)
        .all(app.db())
        .await
        .expect("events")
}

async fn queued(app: &TestApp) -> usize {
    app.enqueued::<ProcessWebhookJob>().await.len()
}

/// Nothing stored, nothing enqueued.
async fn assert_untouched(app: &TestApp) {
    assert!(events(app).await.is_empty(), "an event was stored");
    assert_eq!(queued(app).await, 0, "a job was enqueued");
}

fn status_of(res: &TestResponse) -> serde_json::Value {
    res.json::<serde_json::Value>()
}

// ---------------------------------------------------------------------------
// Accepted deliveries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_github_delivery_is_accepted_stored_and_enqueued() {
    let app = app().await;
    let res = github(&app, "72d3162e-cc78-11e3-81ab-4c9367dc0958", "push", PUSH).await;
    res.assert_ok();
    assert_eq!(status_of(&res), json!({ "status": "received" }));

    let stored = events(&app).await;
    assert_eq!(stored.len(), 1);
    let event = &stored[0];
    assert_eq!(event.provider, "github");
    // The dedupe key is the signed body's hash; the unsigned GUID is kept beside it.
    assert_eq!(
        event.delivery_id.as_deref(),
        Some("72d3162e-cc78-11e3-81ab-4c9367dc0958")
    );
    assert!(event.event_id.starts_with("sha256-") && event.event_id.len() == 7 + 64);
    assert_eq!(event.event_type, "push");
    assert_eq!(
        event.payload.as_bytes(),
        PUSH,
        "the body is stored as received"
    );
    assert_eq!(event.status, Status::Received);
    assert!(event.processed_at.is_none() && event.error.is_none());

    let jobs = app.enqueued::<ProcessWebhookJob>().await;
    assert_eq!(jobs.len(), 1, "exactly one job");
    assert_eq!(jobs[0].1, ProcessWebhookJob { event_id: event.id });
}

#[tokio::test]
async fn a_valid_stripe_delivery_is_accepted_stored_and_enqueued() {
    let app = app().await;
    let res = stripe(&app, INVOICE_PAID).await;
    res.assert_ok();
    assert_eq!(status_of(&res), json!({ "status": "received" }));

    let stored = events(&app).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].provider, "stripe");
    assert_eq!(stored[0].event_id, "evt_1NXWPnCo6bFb1KQto6C8OWvE");
    assert_eq!(stored[0].delivery_id, None);
    assert_eq!(stored[0].event_type, "invoice.paid");
    assert_eq!(queued(&app).await, 1);

    // A rolled secret: the sender signs with two, and either may match.
    let now = Utc::now().timestamp();
    let (_, good) = Provider::Stripe.sign(STRIPE_SECRET.as_bytes(), b"{}", now);
    let good_v1 = good.split("v1=").nth(1).expect("v1");
    let junk = "ab".repeat(32);
    for value in [
        format!("t={now},v1={junk},v1={good_v1}"),
        format!("t={now},v1={good_v1},v1={junk}"),
    ] {
        post(&app, "stripe", &[("stripe-signature", &value)], b"{}")
            .await
            .assert_ok();
    }
}

#[tokio::test]
async fn the_job_runs_the_handler_and_marks_the_event() {
    let app = app().await;
    github(&app, "d-ping", "ping", PING).await.assert_ok();
    github(&app, "d-star", "star", PUSH).await.assert_ok();
    stripe(&app, INVOICE_PAID).await.assert_ok();
    assert_eq!(queued(&app).await, 3);

    app.drain_jobs().await;

    let by_id = |events: &[webhook_event::Model], id: &str| {
        events
            .iter()
            .find(|e| e.event_id == id || e.delivery_id.as_deref() == Some(id))
            .cloned()
            .expect("event")
    };
    let stored = events(&app).await;
    let ping = by_id(&stored, "d-ping");
    assert_eq!((ping.status, ping.error.clone()), (Status::Processed, None));
    assert!(ping.processed_at.is_some());
    // No handler for this type: acknowledged and kept, not an error.
    assert_eq!(by_id(&stored, "d-star").status, Status::Ignored);
    assert_eq!(
        by_id(&stored, "evt_1NXWPnCo6bFb1KQto6C8OWvE").status,
        Status::Processed
    );
    assert_eq!(queued(&app).await, 0);
}

// ---------------------------------------------------------------------------
// Refused deliveries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tampered_body_is_401() {
    let app = app().await;
    // Signed for one body, delivered with another.
    let (name, signature) = Provider::GitHub.sign(GITHUB_SECRET.as_bytes(), PUSH, 0);
    let tampered = br#"{"ref":"refs/heads/evil","zen":"Keep it logically awesome."}"#;
    post(
        &app,
        "github",
        &[(name, &signature), ("x-github-delivery", "d1")],
        tampered,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);

    let (name, signature) = Provider::Stripe.sign(
        STRIPE_SECRET.as_bytes(),
        INVOICE_PAID,
        Utc::now().timestamp(),
    );
    let tampered = String::from_utf8_lossy(INVOICE_PAID).replace("in_123", "in_999");
    post(&app, "stripe", &[(name, &signature)], tampered.as_bytes())
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
}

#[tokio::test]
async fn a_wrong_secret_is_401() {
    let app = app().await;
    github_with(&app, "another-secret-0123456789", "d1", "push", PUSH)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    stripe_at(
        &app,
        "whsec_another_secret_0123456789",
        Utc::now().timestamp(),
        INVOICE_PAID,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
    // Each provider's own secret, not the other's.
    github_with(&app, STRIPE_SECRET, "d2", "push", PUSH)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
}

#[tokio::test]
async fn a_missing_or_malformed_signature_is_401() {
    let app = app().await;
    let github_headers: [&[(&str, &str)]; 7] = [
        &[],
        &[("x-hub-signature-256", "")],
        &[("x-hub-signature-256", "sha256=")],
        &[("x-hub-signature-256", "sha256=zz")],
        &[(
            "x-hub-signature-256",
            "sha1=757107ea0eb2509fc211221cce984b8a37570b6d",
        )],
        // Right digits, wrong length.
        &[(
            "x-hub-signature-256",
            "sha256=757107ea0eb2509fc211221cce984b8a",
        )],
        // The digest without its `sha256=` prefix.
        &[(
            "x-hub-signature-256",
            "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
        )],
    ];
    for headers in github_headers {
        post(&app, "github", headers, PUSH)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let now = Utc::now().timestamp();
    let stripe_headers = [
        String::new(),
        "garbage".to_owned(),
        format!("t={now}"),
        format!("v1={}", "ab".repeat(32)),
        format!("t=soon,v1={}", "ab".repeat(32)),
        format!("t={now},t={now},v1={}", "ab".repeat(32)),
        format!("t={now},v1=not-hex"),
    ];
    for value in &stripe_headers {
        post(&app, "stripe", &[("stripe-signature", value)], INVOICE_PAID)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    // And no header at all.
    post(&app, "stripe", &[], INVOICE_PAID)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
}

#[tokio::test]
async fn a_stale_timestamp_is_401() {
    let app = app().await;
    let now = Utc::now().timestamp();
    // Validly signed, but captured long ago: a replay.
    stripe_at(&app, STRIPE_SECRET, now - TOLERANCE_SECS - 5, INVOICE_PAID)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    stripe_at(&app, STRIPE_SECRET, now - 86_400, INVOICE_PAID)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    stripe_at(&app, STRIPE_SECRET, 0, INVOICE_PAID)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
    // Inside the window it is accepted.
    stripe_at(&app, STRIPE_SECRET, now - TOLERANCE_SECS + 30, INVOICE_PAID)
        .await
        .assert_ok();
}

#[tokio::test]
async fn a_timestamp_from_the_future_is_401() {
    let app = app().await;
    let now = Utc::now().timestamp();
    stripe_at(
        &app,
        STRIPE_SECRET,
        now + FUTURE_SKEW_SECS + 30,
        INVOICE_PAID,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
    stripe_at(&app, STRIPE_SECRET, now + 86_400, INVOICE_PAID)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
    // A little clock skew is fine.
    stripe_at(
        &app,
        STRIPE_SECRET,
        now + FUTURE_SKEW_SECS - 30,
        INVOICE_PAID,
    )
    .await
    .assert_ok();
}

#[tokio::test]
async fn a_refusal_never_says_which_check_failed() {
    let app = app().await;
    let now = Utc::now().timestamp();
    let (name, good) = Provider::Stripe.sign(STRIPE_SECRET.as_bytes(), INVOICE_PAID, now);
    let refusals = vec![
        post(&app, "stripe", &[], INVOICE_PAID).await,
        post(&app, "stripe", &[(name, "nonsense")], INVOICE_PAID).await,
        stripe_at(&app, STRIPE_SECRET, now - 3_600, INVOICE_PAID).await,
        stripe_at(&app, STRIPE_SECRET, now + 3_600, INVOICE_PAID).await,
        stripe_at(&app, "whsec_another_secret_0123456789", now, INVOICE_PAID).await,
        post(&app, "stripe", &[(name, &good)], b"{}").await,
    ];
    let first = &refusals[0];
    first.assert_status(StatusCode::UNAUTHORIZED);
    for other in &refusals {
        assert_eq!(other.status, first.status);
        assert_eq!(other.body, first.body, "the body differs between refusals");
        assert_eq!(
            other.headers.get("content-type"),
            first.headers.get("content-type")
        );
        assert_eq!(
            other.headers.get("www-authenticate"),
            first.headers.get("www-authenticate")
        );
    }
    assert_eq!(status_of(first), json!({ "error": "unauthorized" }));
}

// ---------------------------------------------------------------------------
// Replays
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replays_store_one_event_and_enqueue_one_job() {
    let app = app().await;
    let first = github(&app, "d-replay", "push", PUSH).await;
    first.assert_ok();
    assert_eq!(status_of(&first), json!({ "status": "received" }));
    for _ in 0..3 {
        let again = github(&app, "d-replay", "push", PUSH).await;
        again.assert_ok();
        assert_eq!(status_of(&again), json!({ "status": "duplicate" }));
    }
    assert_eq!(events(&app).await.len(), 1);
    assert_eq!(queued(&app).await, 1);

    // Stripe: a replay carries a fresh timestamp and signature, but the same event id.
    let now = Utc::now().timestamp();
    stripe_at(&app, STRIPE_SECRET, now - 20, INVOICE_PAID)
        .await
        .assert_ok();
    let again = stripe_at(&app, STRIPE_SECRET, now, INVOICE_PAID).await;
    again.assert_ok();
    assert_eq!(status_of(&again), json!({ "status": "duplicate" }));
    assert_eq!(events(&app).await.len(), 2);
    assert_eq!(queued(&app).await, 2);

    // A replay after the job ran is still a no-op: no second run.
    app.drain_jobs().await;
    github(&app, "d-replay", "push", PUSH).await.assert_ok();
    assert_eq!(events(&app).await.len(), 2);
    assert_eq!(queued(&app).await, 0);

    // The same event id from another provider is another event.
    let other = br#"{"id":"d-replay","type":"x"}"#;
    stripe(&app, other).await.assert_ok();
    assert_eq!(events(&app).await.len(), 3);
}

#[test]
fn the_delivery_transaction_begins_immediate() {
    // On SQLite a deferred `.begin()` that writes while the job worker writes fails with
    // "database is locked": a valid delivery would become a 500. The in-memory test database
    // has one connection, so a race can't show it; the source can.
    let source = include_str!("../../src/controllers/webhooks.rs");
    assert!(source.contains("db::begin(&state.db)"));
    assert!(
        !source.contains(".begin()"),
        "a raw .begin() in the webhook route"
    );
}

#[tokio::test]
async fn racing_replays_store_one_event_and_enqueue_one_job() {
    let app = app().await;
    let results =
        futures::future::join_all((0..8).map(|_| github(&app, "d-race", "push", PUSH))).await;
    for res in &results {
        res.assert_ok();
    }
    let received = results
        .iter()
        .filter(|r| status_of(r) == json!({ "status": "received" }))
        .count();
    assert_eq!(received, 1, "exactly one request stored it");
    assert_eq!(events(&app).await.len(), 1);
    assert_eq!(queued(&app).await, 1);
}

#[tokio::test]
async fn a_github_body_resent_with_a_new_delivery_id_is_a_duplicate() {
    let app = app().await;
    // GitHub signs the body only: a captured delivery resent with a fresh `X-GitHub-Delivery`
    // (and even another `X-GitHub-Event`) still verifies, so the body's hash is the key.
    let first = github(&app, "11111111-aaaa-bbbb-cccc-000000000001", "push", PUSH).await;
    assert_eq!(status_of(&first), json!({ "status": "received" }));
    for (delivery, event) in [
        ("22222222-aaaa-bbbb-cccc-000000000002", "push"),
        ("33333333-aaaa-bbbb-cccc-000000000003", "release"),
    ] {
        let again = github(&app, delivery, event, PUSH).await;
        again.assert_ok();
        assert_eq!(status_of(&again), json!({ "status": "duplicate" }));
    }
    let stored = events(&app).await;
    assert_eq!(stored.len(), 1, "one event");
    assert_eq!(queued(&app).await, 1, "one job");
    // The first delivery's GUID and type are the ones kept.
    assert_eq!(
        stored[0].delivery_id.as_deref(),
        Some("11111111-aaaa-bbbb-cccc-000000000001")
    );
    assert_eq!(stored[0].event_type, "push");
    // A different body is a different event, whatever the GUID.
    github(&app, "11111111-aaaa-bbbb-cccc-000000000001", "ping", PING)
        .await
        .assert_ok();
    assert_eq!(events(&app).await.len(), 2);
    assert_eq!(queued(&app).await, 2);
}

// ---------------------------------------------------------------------------
// Routing: the route exists only for a configured provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_provider_is_404() {
    let app = app().await;
    let (name, signature) = Provider::GitHub.sign(GITHUB_SECRET.as_bytes(), PUSH, 0);
    let headers = [(name, signature.as_str()), ("x-github-delivery", "d1")];
    for slug in ["nope", "GitHub", "github%2F", "1", "stripe2"] {
        let res = post(&app, slug, &headers, PUSH).await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert!(
            res.headers.get("x-route-fallback").is_none(),
            "a handler's 404, not the router fallback"
        );
        assert_eq!(status_of(&res), json!({ "error": "not_found" }));
    }
    assert_untouched(&app).await;
}

#[tokio::test]
async fn a_provider_without_a_secret_is_404() {
    // Only GitHub is configured: Stripe doesn't exist, even for a perfectly signed delivery.
    let app = TestApp::with_config(Config {
        webhooks: WebhooksConfig::with(&[(Provider::GitHub, GITHUB_SECRET)]),
        ..Config::test()
    })
    .await;
    let missing = stripe(&app, INVOICE_PAID).await;
    missing.assert_status(StatusCode::NOT_FOUND);
    github(&app, "d1", "push", PUSH).await.assert_ok();

    // The answer is exactly the one for a provider that was never heard of.
    let unknown = post(&app, "nope", &[], INVOICE_PAID).await;
    assert_eq!(missing.body, unknown.body);
    assert_eq!(missing.status, unknown.status);

    // The default test config has no secrets: nothing answers.
    let bare = TestApp::new().await;
    github(&bare, "d1", "push", PUSH)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    stripe(&bare, INVOICE_PAID)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_untouched(&bare).await;
}

#[tokio::test]
async fn only_post_is_routed() {
    let app = app().await;
    // A GET never stores anything (and answers the same for every slug).
    for slug in ["github", "stripe", "nope"] {
        let res = app.get(&paths::webhook(slug)).await;
        assert!(
            matches!(
                res.status,
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
            ),
            "GET answered {}",
            res.status
        );
    }
    assert_untouched(&app).await;
}

#[tokio::test]
async fn the_webhook_route_is_public_and_listed() {
    let row = ROUTES
        .iter()
        .find(|(_, path, _, _)| *path == "/webhooks/{id}")
        .expect("the route is in ROUTES");
    assert_eq!((row.0, row.3), ("POST", Access::Public));
    assert_eq!(paths::webhook("github"), "/webhooks/github");
}

// ---------------------------------------------------------------------------
// Size limit and parsing order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_body_is_413() {
    let app = app().await;
    // Validly signed and one byte over the cap: refused before the HMAC.
    let big = vec![b'a'; MAX_BODY_BYTES + 1];
    github(&app, "d-big", "push", &big)
        .await
        .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
    assert_untouched(&app).await;
    // Exactly the cap is fine (it is not JSON, and is stored all the same).
    let edge = vec![b'a'; MAX_BODY_BYTES];
    github(&app, "d-edge", "push", &edge).await.assert_ok();
    assert_eq!(events(&app).await.len(), 1);
}

#[tokio::test]
async fn a_declared_oversized_body_is_413() {
    let app = app().await;
    // The Content-Length alone is enough: the body isn't read.
    let declared = (MAX_BODY_BYTES + 1).to_string();
    let res = post(
        &app,
        "github",
        &[
            ("content-length", &declared),
            ("x-hub-signature-256", "sha256=00"),
        ],
        b"{}",
    )
    .await;
    res.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(status_of(&res), json!({ "error": "payload_too_large" }));
    assert_untouched(&app).await;
}

#[tokio::test]
async fn malformed_json_with_a_valid_signature_is_stored_not_a_500() {
    let app = app().await;
    // The signature decides, not the content: nothing parses the body first.
    let garbage: &[u8] = b"{this is not json";
    github(&app, "d-garbage", "push", garbage).await.assert_ok();
    let res = stripe(&app, garbage).await;
    res.assert_ok();
    let stored = events(&app).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].payload, "{this is not json");
    // Stripe: no id in the JSON, so the body's hash stands in (a replay still deduplicates).
    assert!(stored[1].event_id.starts_with("sha256-"));
    assert_eq!(stored[1].event_type, "unknown");
    stripe(&app, garbage).await.assert_ok();
    assert_eq!(events(&app).await.len(), 2);

    // The job refuses it for good: the event is `failed`, with the reason.
    app.drain_jobs().await;
    for event in events(&app).await {
        assert_eq!(event.status, Status::Failed);
        assert_eq!(event.error.as_deref(), Some("the payload is not JSON"));
    }

    // Bytes that aren't UTF-8 or hold a NUL are stored too (Postgres text can't hold a NUL).
    let odd: &[u8] = b"{\"a\":\"\xff\x00\"}";
    github(&app, "d-odd", "push", odd).await.assert_ok();
    let stored = events(&app).await;
    assert!(stored.last().expect("event").payload.contains('\u{FFFD}'));

    // The same garbage with a bad signature is 401, and stores nothing.
    let before = events(&app).await.len();
    github_with(&app, "another-secret-0123456789", "d-bad", "push", garbage)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(events(&app).await.len(), before);
}

#[tokio::test]
async fn an_unsigned_body_is_never_parsed() {
    let app = app().await;
    // A Stripe body whose JSON would give an event id, without a valid signature: 401, and no
    // row, so the JSON was not what decided. (The type system enforces it: see `Verified`.)
    let res = post(&app, "stripe", &[], INVOICE_PAID).await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_untouched(&app).await;
    // And bad JSON with no signature is the same 401, not a 400 or a 500.
    let res = post(&app, "stripe", &[], b"{not json").await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    let res = post(&app, "github", &[], &[0xff, 0xfe]).await;
    res.assert_status(StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// CSRF, cookies, sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn csrf_still_protects_every_other_post() {
    let app = app().await;
    // Every unsafe route except the webhook, cross-site and with no `Origin` at all: 403.
    let mut checked = 0;
    for (method, path, handler, _) in ROUTES {
        if *method == "GET" || path.starts_with("/webhooks/") {
            continue;
        }
        checked += 1;
        let res = app
            .post_form_with_headers(
                &crate::security_test::concrete(path),
                &[],
                &[("sec-fetch-site", "cross-site")],
            )
            .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{path} ({handler}) accepted a cross-site POST"
        );
    }
    assert!(checked >= 1, "no other POST route was checked");
    // Without fetch metadata the Origin decides.
    app.without_fetch_metadata()
        .with_origin("https://evil.example");
    app.post_form("/theme", &[("theme", "dark")])
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // A same-origin form still works.
    app.with_origin("http://localhost:3000");
    app.post_form("/theme", &[("theme", "dark")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_provider_server_passes_csrf() {
    let app = app().await;
    // A provider's server sends neither `Origin` nor `Sec-Fetch-Site`, and tower-http's CSRF layer
    // passes such a request, so the route works with or without the guide's `/webhooks/` bypass.
    github(&app, "d-server", "push", PUSH).await.assert_ok();
    stripe(&app, INVOICE_PAID).await.assert_ok();
    assert_eq!(events(&app).await.len(), 2);
    assert_eq!(queued(&app).await, 2);

    // A browser on another site can't sign, so its POST stores nothing: 403 from the CSRF layer
    // today, 401 from the signature check in an app that wires the bypass.
    for extra in [
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-site", "same-site"),
        ("origin", "https://evil.example"),
    ] {
        let res = post(
            &app,
            "github",
            &[("x-github-delivery", "d-browser"), extra],
            PUSH,
        )
        .await;
        assert!(
            matches!(res.status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
            "{extra:?}: {}",
            res.status
        );
    }
    assert_eq!(events(&app).await.len(), 2, "nothing from the browser");
    assert_eq!(queued(&app).await, 2);
}

#[tokio::test]
async fn webhook_responses_involve_no_cookies_or_sessions() {
    let app = app().await;
    let (name, signature) = Provider::GitHub.sign(GITHUB_SECRET.as_bytes(), PUSH, 0);
    // Cookies a browser might hold are ignored, and nothing is set, on every outcome. (A sender
    // never has any; the flash middleware would clear a `_flash` cookie, so that one is left out.)
    let cookie = ("cookie", "_session=abc; theme=dark");
    let accepted = post(
        &app,
        "github",
        &[(name, &signature), ("x-github-delivery", "d1"), cookie],
        PUSH,
    )
    .await;
    let refused = post(&app, "github", &[cookie], PUSH).await;
    let missing = post(&app, "nope", &[cookie], PUSH).await;
    accepted.assert_ok();
    refused.assert_status(StatusCode::UNAUTHORIZED);
    missing.assert_status(StatusCode::NOT_FOUND);
    for res in [accepted, refused, missing] {
        assert!(res.set_cookies().is_empty(), "{:?}", res.set_cookies());
    }
}

// ---------------------------------------------------------------------------
// Flood and logs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_deliveries_are_rate_limited_per_client() {
    let app = app().await;
    // 20 failures from one client, then even a valid delivery waits.
    for _ in 0..20 {
        github_with(&app, "another-secret-0123456789", "d-bad", "push", PUSH)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let res = github(&app, "d-good", "push", PUSH).await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers.get("retry-after").is_some());
    assert_untouched(&app).await;
    // The limit is per provider.
    stripe(&app, INVOICE_PAID).await.assert_ok();
}

#[tokio::test]
async fn valid_deliveries_are_never_rate_limited() {
    let app = app().await;
    for n in 0..30 {
        let body = format!(r#"{{"ref":"refs/heads/main","n":{n}}}"#);
        github(&app, &format!("d-{n}"), "push", body.as_bytes())
            .await
            .assert_ok();
    }
    assert_eq!(events(&app).await.len(), 30);
}

/// Every log line at trace level, in a temporary file while it lives.
struct LogCapture {
    file: tempfile::NamedTempFile,
    _guard: tracing::subscriber::DefaultGuard,
}

impl LogCapture {
    fn start() -> Self {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let sink = file.reopen().expect("reopen");
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(offrails_host::kernel::redact::log_filter(
                "trace,hyper=info,h2=info",
            ))
            .with_ansi(false)
            .with_writer(move || sink.try_clone().expect("clone log file"))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        Self {
            file,
            _guard: guard,
        }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(self.file.path()).unwrap_or_default()
    }
}

#[tokio::test]
async fn failures_are_logged_at_warn_without_the_secret_or_the_signature() {
    let logs = LogCapture::start();
    let app = app().await;
    let now = Utc::now().timestamp();
    let wrong = "another-secret-0123456789";
    let (_, wrong_github) = Provider::GitHub.sign(wrong.as_bytes(), PUSH, 0);
    let (_, wrong_stripe) = Provider::Stripe.sign(wrong.as_bytes(), INVOICE_PAID, now);
    github_with(&app, wrong, "d1", "push", PUSH).await;
    stripe_at(&app, wrong, now, INVOICE_PAID).await;
    stripe_at(&app, STRIPE_SECRET, now - 3_600, INVOICE_PAID).await;
    post(&app, "github", &[], PUSH).await;
    // And a good one, which logs too.
    github(&app, "d2", "push", PUSH).await.assert_ok();

    let text = logs.text();
    let refusals: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("webhook refused"))
        .collect();
    assert_eq!(refusals.len(), 4, "one line per refusal:\n{text}");
    for line in &refusals {
        assert!(line.contains("WARN"), "refusals are warnings: {line}");
    }
    for reason in ["bad_signature", "stale_timestamp", "missing_header"] {
        assert!(text.contains(reason), "{reason} is logged:\n{text}");
    }
    assert!(text.contains("webhook accepted"));
    // Neither secret, nor any signature (the sent ones or the real ones), nor the body.
    let (_, real_github) = Provider::GitHub.sign(GITHUB_SECRET.as_bytes(), PUSH, 0);
    let (_, real_stripe) = Provider::Stripe.sign(STRIPE_SECRET.as_bytes(), INVOICE_PAID, now);
    for secret in [
        GITHUB_SECRET,
        STRIPE_SECRET,
        wrong,
        wrong_github.trim_start_matches("sha256="),
        real_github.trim_start_matches("sha256="),
        wrong_stripe.split("v1=").nth(1).expect("v1"),
        real_stripe.split("v1=").nth(1).expect("v1"),
        "Keep it logically awesome.",
    ] {
        assert!(!text.contains(secret), "{secret} reached the logs:\n{text}");
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn webhook_secrets_never_reach_debug_output() {
    let config = both_providers();
    let printed = format!("{config:?}");
    for secret in [GITHUB_SECRET, STRIPE_SECRET] {
        assert!(!printed.contains(secret), "{printed}");
    }
    assert!(
        printed.contains("github") && printed.contains("stripe"),
        "{printed}"
    );
    let printed = format!("{:?}", config.webhooks);
    assert!(!printed.contains(GITHUB_SECRET), "{printed}");
    assert!(format!("{:?}", Config::test()).contains("providers: []"));
}

#[test]
fn webhook_secrets_come_from_env_vars_and_must_be_long_enough() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        WebhooksConfig::from_lookup(move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        })
    };
    let none = env(&[]).expect("no secrets is fine");
    assert!(none.providers().is_empty());
    assert!(none.secret(Provider::GitHub).is_none());
    // Blank counts as unset.
    assert!(
        env(&[("APP_WEBHOOK_GITHUB_SECRET", "  ")])
            .expect("blank")
            .providers()
            .is_empty()
    );

    let one = env(&[("APP_WEBHOOK_STRIPE_SECRET", "whsec_0123456789abcdef")]).expect("ok"); // gitleaks:allow (a made-up test secret)
    assert_eq!(one.providers(), vec!["stripe"]);
    assert_eq!(
        one.secret(Provider::Stripe),
        Some(&b"whsec_0123456789abcdef"[..])
    );
    assert!(one.secret(Provider::GitHub).is_none());

    // Too short: the boot fails, naming the variable and not its value.
    let err = env(&[("APP_WEBHOOK_GITHUB_SECRET", "hunter2")])
        .expect_err("a short secret is refused")
        .to_string();
    assert!(
        err.contains("APP_WEBHOOK_GITHUB_SECRET") && !err.contains("hunter2"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// Stores an event the way the route does, without the job.
async fn stored_event(app: &TestApp, payload: &str) -> i64 {
    webhook_event::record(
        app.db(),
        "github",
        "d-stored",
        None,
        "push",
        payload,
        Utc::now(),
    )
    .await
    .expect("record")
    .expect("a new event")
}

fn attempt(n: i32, of: i32) -> JobInfo {
    let mut info = JobInfo::synthetic(ProcessWebhookJob::NAME, "default");
    info.attempt = n;
    info.max_attempts = of;
    info
}

#[tokio::test]
async fn a_transient_error_retries_and_the_last_attempt_fails_the_event() {
    let app = TestApp::new().await;
    let id = stored_event(&app, "{}").await;
    let boom = async |_: &offrails_host::state::AppState,
                      _: &Delivery<'_>|
           -> anyhow::Result<Handled> { Err(anyhow::anyhow!("connection refused")) };

    // Attempt 1 of 3: the error is noted, the event is still waiting, the job retries.
    let first = process_webhook::process(&app.state, &attempt(1, 3), id, boom).await;
    assert!(first.is_err(), "an Err makes the queue retry");
    let event = webhook_event::find(app.db(), id).await.unwrap().unwrap();
    assert_eq!(event.status, Status::Received);
    assert_eq!(event.error.as_deref(), Some("connection refused"));

    // The last attempt: the event is failed, with the reason.
    let last = process_webhook::process(&app.state, &attempt(3, 3), id, boom).await;
    assert!(last.is_err());
    let event = webhook_event::find(app.db(), id).await.unwrap().unwrap();
    assert_eq!(event.status, Status::Failed);
    assert!(event.processed_at.is_some());
    assert_eq!(event.error.as_deref(), Some("connection refused"));
}

#[tokio::test]
async fn a_handler_can_refuse_for_good() {
    let app = TestApp::new().await;
    let id = stored_event(&app, "{}").await;
    let refuse =
        async |_: &offrails_host::state::AppState, _: &Delivery<'_>| -> anyhow::Result<Handled> {
            Ok(Handled::Failed("no such customer".into()))
        };
    let outcome = process_webhook::process(&app.state, &attempt(1, 8), id, refuse)
        .await
        .expect("a refusal is not an error");
    assert!(matches!(outcome, Outcome::Discard(reason) if reason == "no such customer"));
    let event = webhook_event::find(app.db(), id).await.unwrap().unwrap();
    assert_eq!(
        (event.status, event.error.as_deref()),
        (Status::Failed, Some("no such customer"))
    );
}

#[tokio::test]
async fn a_finished_event_is_not_handled_twice() {
    let app = TestApp::new().await;
    let id = stored_event(&app, r#"{"zen":"x"}"#).await;
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let counting =
        async |_: &offrails_host::state::AppState, _: &Delivery<'_>| -> anyhow::Result<Handled> {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Handled::Done)
        };
    for _ in 0..3 {
        let outcome = process_webhook::process(&app.state, &attempt(1, 8), id, counting)
            .await
            .expect("ok");
        assert!(matches!(outcome, Outcome::Done));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_missing_event_is_discarded() {
    let app = TestApp::new().await;
    let outcome =
        process_webhook::process(&app.state, &attempt(1, 8), 424_242, process_webhook::handle)
            .await
            .expect("ok");
    assert!(matches!(outcome, Outcome::Discard(reason) if reason.contains("does not exist")));
}

#[tokio::test]
async fn stored_errors_are_redacted_and_capped() {
    let app = TestApp::new().await;
    let id = stored_event(&app, "{}").await;
    let leaky =
        async |_: &offrails_host::state::AppState, _: &Delivery<'_>| -> anyhow::Result<Handled> {
            Err(anyhow::anyhow!(
                "GET https://user:hunter2pass@billing.example/v1?token=abcdef123456 failed" // gitleaks:allow (a fake URL; the test proves it is redacted)
            ))
        };
    let _ = process_webhook::process(&app.state, &attempt(1, 3), id, leaky).await;
    let noted = webhook_event::find(app.db(), id)
        .await
        .unwrap()
        .unwrap()
        .error
        .unwrap();
    assert!(
        !noted.contains("hunter2pass") && !noted.contains("abcdef123456"),
        "{noted}"
    );

    let long = async |_: &offrails_host::state::AppState,
                      _: &Delivery<'_>|
           -> anyhow::Result<Handled> { Err(anyhow::anyhow!("é".repeat(5_000))) };
    let _ = process_webhook::process(&app.state, &attempt(3, 3), id, long).await;
    let stored = webhook_event::find(app.db(), id)
        .await
        .unwrap()
        .unwrap()
        .error
        .unwrap();
    assert!(
        stored.len() <= 1000 && stored.starts_with('é'),
        "{}",
        stored.len()
    );
}

#[tokio::test]
async fn the_worker_is_registered_and_dispatches() {
    assert!(WORKERS.contains(&"process_webhook"));
    let app = app().await;
    github(&app, "d-ping", "ping", PING).await.assert_ok();
    let (row, _) = app.enqueued::<ProcessWebhookJob>().await.remove(0);
    let outcome = dispatch(
        app.state.clone(),
        row.worker.clone(),
        row.args.clone(),
        JobInfo::synthetic("process_webhook", "default"),
    )
    .await
    .expect("dispatch");
    assert!(matches!(outcome, Outcome::Done), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_status_column_rejects_unknown_statuses() {
    let app = TestApp::new().await;
    let id = stored_event(&app, "{}").await;
    let res = webhook_event::Entity::update_many()
        .col_expr(webhook_event::Column::Status, Expr::value("bogus"))
        .filter(webhook_event::Column::Id.eq(id))
        .exec(app.db())
        .await;
    assert!(res.is_err(), "the CHECK constraint rejects it");
}

#[tokio::test]
async fn the_unique_index_makes_a_second_insert_a_no_op() {
    let app = TestApp::new().await;
    let now = Utc::now();
    let db = app.db();
    let first = webhook_event::record(db, "github", "same", None, "push", "{}", now)
        .await
        .unwrap();
    let second = webhook_event::record(db, "github", "same", None, "push", "{}", now)
        .await
        .unwrap();
    let other = webhook_event::record(db, "stripe", "same", None, "push", "{}", now)
        .await
        .unwrap();
    assert!(first.is_some() && second.is_none() && other.is_some());
    // Inside a transaction too: on Postgres a caught unique violation would abort it.
    let txn = offrails_host::kernel::db::begin(db).await.unwrap();
    assert!(
        webhook_event::record(&txn, "github", "same", None, "push", "{}", now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        webhook_event::record(&txn, "github", "new", None, "push", "{}", now)
            .await
            .unwrap()
            .is_some()
    );
    txn.commit().await.unwrap();
    assert_eq!(webhook_event::Entity::find().count(db).await.unwrap(), 3);
}

#[tokio::test]
async fn prune_deletes_only_old_finished_events() {
    let app = TestApp::new().await;
    let db = app.db();
    let old = Utc::now() - Duration::days(40);
    let record =
        |id: &'static str, at| webhook_event::record(db, "github", id, None, "push", "{}", at);
    let waiting = record("old-received", old).await.unwrap().unwrap();
    let done = record("old-done", old).await.unwrap().unwrap();
    let recent = record("recent-done", Utc::now()).await.unwrap().unwrap();
    for id in [done, recent] {
        webhook_event::finish(db, id, Status::Processed, None, Utc::now())
            .await
            .unwrap();
    }
    let removed = webhook_event::prune(db, Utc::now() - Duration::days(30))
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let left: Vec<i64> = events(&app).await.iter().map(|e| e.id).collect();
    assert_eq!(left, vec![waiting, recent]);
}

#[tokio::test]
async fn finished_webhook_events_are_pruned_daily() {
    // Registered as a worker and scheduled.
    assert!(WORKERS.contains(&PruneWebhookEventsJob::NAME));
    let entry = SCHEDULE
        .iter()
        .find(|r| r.key == "prune_webhook_events")
        .expect("the prune job is in SCHEDULE");
    assert_eq!((entry.job)().worker_name(), PruneWebhookEventsJob::NAME);

    let app = TestApp::new().await;
    let db = app.db();
    let old = Utc::now() - Duration::days(KEEP_DAYS + 1);
    let record =
        |id: &'static str, at| webhook_event::record(db, "stripe", id, None, "x", "{}", at);
    let old_done = record("old-done", old).await.unwrap().unwrap();
    let old_waiting = record("old-received", old).await.unwrap().unwrap();
    let recent_done = record("recent-done", Utc::now()).await.unwrap().unwrap();
    for id in [old_done, recent_done] {
        webhook_event::finish(db, id, Status::Processed, None, Utc::now())
            .await
            .unwrap();
    }
    let outcome = app.perform_job(PruneWebhookEventsJob).await.expect("runs");
    assert!(matches!(outcome, Outcome::Done), "{outcome:?}");
    let left: Vec<i64> = events(&app).await.iter().map(|e| e.id).collect();
    assert_eq!(left, vec![old_waiting, recent_done]);
}
