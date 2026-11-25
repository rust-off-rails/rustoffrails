//! `POST /webhooks/{id}`: inbound webhooks. The signature is the only authentication here: no
//! cookie, no session, no user. The order of the steps is the security:
//!
//! 1. an unknown provider, or one without a configured secret, is a 404 (the route doesn't exist);
//! 2. a client that failed too often is refused (429) before any work;
//! 3. the raw body is read, at most `MAX_BODY_BYTES` (413);
//! 4. the signature is verified over those raw bytes (401), and only then is anything parsed;
//! 5. the event is stored once (a replay is a no-op that still answers 200) and a job enqueued, in
//!    one transaction; the work happens in the job.
//!
//! GUIDE: .offrails/guides/security.md#csrf

use std::time::Duration;

use axum::{
    Json,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde_json::json;

use crate::{
    controllers::application::ClientIp,
    error::AppError,
    jobs::{self, process_webhook::ProcessWebhookJob},
    kernel::{db, webhooks::Provider},
    models::webhook_event,
    state::AppState,
};

/// APP: the largest body accepted. Stripe events are a few KB; GitHub caps payloads at 25 MB, so
/// raise it only if you subscribe to events that big. A larger body is a 413 before any HMAC.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// APP: failed deliveries allowed per client and provider per window, then 429. Only failures
/// count: a provider sending thousands of valid events is never limited.
const FAILED_LIMIT: u32 = 20;
const FAILED_WINDOW: Duration = Duration::from_secs(180);

/// The answer to every refusal, whatever check failed: the sender learns nothing it can use.
fn refuse(status: StatusCode) -> Response {
    let error = match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::TOO_MANY_REQUESTS => "too_many_requests",
        _ => "unauthorized",
    };
    let mut response = (status, Json(json!({ "error": error }))).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from(FAILED_WINDOW.as_secs()),
        );
    }
    response
}

pub async fn receive(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    ip: ClientIp,
    request: Request,
) -> Result<Response, AppError> {
    // SECURITY: a provider without a configured secret has no route: 404, the same answer as a
    // provider that doesn't exist, so nobody can probe which integrations an app has
    // (test: a_provider_without_a_secret_is_404).
    let Some((provider, secret)) = Provider::from_slug(&slug).and_then(|provider| {
        state
            .config
            .webhooks
            .secret(provider)
            .map(|secret| (provider, secret))
    }) else {
        return Ok(refuse(StatusCode::NOT_FOUND));
    };

    let client = ip.key(&format!("webhook:{}", provider.slug()));
    // SECURITY: once a client has used up its failures, even a valid delivery waits, so a flood
    // of forged ones can't keep the HMAC and the database busy (test: failed_deliveries_are_rate_limited_per_client).
    if state.rate_limit.exhausted(&client, FAILED_LIMIT).await {
        return Ok(refuse(StatusCode::TOO_MANY_REQUESTS));
    }

    let (parts, body) = request.into_parts();
    // SECURITY: the body is read raw and capped before anything else touches it; a declared size
    // over the cap is refused without reading a byte (test: an_oversized_body_is_413, a_declared_oversized_body_is_413).
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let bytes = match declared {
        Some(n) if n > MAX_BODY_BYTES as u64 => None,
        _ => to_bytes(body, MAX_BODY_BYTES).await.ok(),
    };
    let Some(bytes) = bytes else {
        reject(&state, &client, provider, "body_too_large").await;
        return Ok(refuse(StatusCode::PAYLOAD_TOO_LARGE));
    };

    // SECURITY: verified over the raw bytes, before any parsing (the parsing functions are on
    // `Verified`, which only a successful `verify` returns)
    // (test: a_tampered_body_is_401, a_wrong_secret_is_401, a_missing_or_malformed_signature_is_401).
    let verified = match provider.verify(secret, &parts.headers, &bytes, Utc::now().timestamp()) {
        Ok(verified) => verified,
        Err(why) => {
            reject(&state, &client, provider, why.as_str()).await;
            return Ok(refuse(StatusCode::UNAUTHORIZED));
        }
    };

    let info = verified.event();
    // INVARIANT: the event and its job commit together, or neither exists (test: replays_store_one_event_and_enqueue_one_job)
    // WHY: `kernel::db::begin` (BEGIN IMMEDIATE on SQLite), not a plain `begin`: a deferred
    // transaction that writes while the job worker writes fails with "database is locked".
    let txn = db::begin(&state.db).await?;
    let stored = webhook_event::record(
        &txn,
        provider.slug(),
        &info.id,
        info.delivery.as_deref(),
        &info.kind,
        &verified.payload(),
        Utc::now(),
    )
    .await?;
    if let Some(id) = stored {
        jobs::enqueue(&txn, ProcessWebhookJob { event_id: id }).await?;
    }
    txn.commit().await?;

    let status = if stored.is_some() {
        "received"
    } else {
        "duplicate"
    };
    tracing::info!(
        provider = provider.slug(),
        event_id = %info.id,
        delivery_id = info.delivery.as_deref(),
        event_type = %info.kind,
        status,
        "webhook accepted"
    );
    // APP: 200 for a replay too: the provider stops retrying. Never answer a verified delivery
    // with a 4xx for an event you merely don't handle.
    Ok((StatusCode::OK, Json(json!({ "status": status }))).into_response())
}

/// Counts a failed delivery against the client and logs why, at warn. The log carries the
/// provider, the reason and the client key: never the secret, the signature or the body.
// SECURITY: refusals are logged without the secret or the signature (test: failures_are_logged_at_warn_without_the_secret_or_the_signature)
async fn reject(state: &AppState, client: &str, provider: Provider, reason: &str) {
    // `check` counts a hit; its `Err` only says the client is now over the limit, which the next
    // request's `exhausted` peek acts on.
    let _ = state
        .rate_limit
        .check(client, FAILED_LIMIT, FAILED_WINDOW)
        .await;
    tracing::warn!(
        provider = provider.slug(),
        reason,
        client,
        "webhook refused"
    );
}
