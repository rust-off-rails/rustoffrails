//! Does the work for a stored webhook delivery. The route only verifies, stores and enqueues
//! (so the provider gets its 2xx fast); this job reads the row back and calls `handle`.
//! GUIDE: .offrails/guides/jobs.md#rules
//!
//! Args are the event's row id, never the payload: the payload is stored once, on the event.

use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    jobs::{JobInfo, Outcome, Worker},
    kernel::webhooks::Provider,
    models::webhook_event::{self, Status},
    state::AppState,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessWebhookJob {
    /// `webhook_events.id`.
    pub event_id: i64,
}

impl Worker for ProcessWebhookJob {
    const NAME: &'static str = "process_webhook";
    // APP: the queue must be in `APP_QUEUES`. Not `webhooks`: that rate-limited queue serves the
    // jobs sample's outbound examples.
    const QUEUE: &'static str = "default";
    // APP: attempts before the event is marked `failed` (about 2 days of backoff at 8).
    const MAX_ATTEMPTS: i32 = 8;
    const TIMEOUT: Duration = Duration::from_secs(60);

    async fn perform(self, job: &JobInfo, state: &AppState) -> anyhow::Result<Outcome> {
        process(state, job, self.event_id, handle).await
    }
}

/// What a handler decided about a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    /// Acted on it.
    Done,
    /// Nothing to do for this event type: acknowledged and kept, not an error.
    Ignored,
    /// Refused for good (retrying can't help): the event is marked `failed` with this reason.
    Failed(String),
}

/// A verified, stored delivery, parsed.
#[derive(Debug)]
pub struct Delivery<'a> {
    pub provider: Provider,
    /// The provider's event type: `ping`, `push`, `invoice.paid`. For GitHub it comes from the
    /// unsigned `X-GitHub-Event` header: check the payload's shape too before acting on it.
    pub kind: &'a str,
    /// The dedupe key (`webhook_events.event_id`): Stripe's event id, or `sha256-<hex>` of the
    /// body for GitHub. Key what the handler writes on it, or on the payload's own ids.
    pub event_id: &'a str,
    /// GitHub's `X-GitHub-Delivery` (unsigned: for logs and support, never for decisions).
    pub delivery_id: Option<&'a str>,
    pub payload: Value,
}

/// APP: the one place the app reacts to webhooks: one arm per (provider, event type) it acts on.
///
/// - `Ok(Handled::Done)` / `Ok(Handled::Ignored)`: finished.
/// - `Ok(Handled::Failed(why))`: a refusal retrying can't fix (a payload that makes no sense).
/// - `Err(_)`: a transient failure (a timeout, a database error): the job retries with backoff.
///
/// WHY: it runs again after a transient `Err` and may run twice if the node dies mid-way, so make
/// it idempotent: key what it writes on `delivery.event_id`.
pub async fn handle(_state: &AppState, delivery: &Delivery<'_>) -> anyhow::Result<Handled> {
    match (delivery.provider, delivery.kind) {
        // APP: example arms (harmless: they only log). Replace them with your own.
        (Provider::GitHub, "ping") => {
            let zen = delivery.payload.get("zen").and_then(Value::as_str);
            tracing::info!(event_id = delivery.event_id, zen, "github ping");
            Ok(Handled::Done)
        }
        (Provider::Stripe, "invoice.paid") => {
            let invoice = delivery.payload.pointer("/data/object/id");
            tracing::info!(
                event_id = delivery.event_id,
                ?invoice,
                "stripe invoice paid"
            );
            Ok(Handled::Done)
        }
        // Every other event type is acknowledged and kept.
        _ => Ok(Handled::Ignored),
    }
}

/// The job's bookkeeping around a handler: the event's status, errors, and the job's outcome.
/// `perform` passes `handle`; the tests pass handlers that fail.
pub async fn process(
    state: &AppState,
    job: &JobInfo,
    event_id: i64,
    handler: impl AsyncFn(&AppState, &Delivery<'_>) -> anyhow::Result<Handled>,
) -> anyhow::Result<Outcome> {
    let Some(event) = webhook_event::find(&state.db, event_id).await? else {
        return Ok(Outcome::Discard(format!(
            "webhook event {event_id} does not exist"
        )));
    };
    // A second run of a job that already finished its event (a rescued orphan): nothing to do.
    if event.status != Status::Received {
        return Ok(Outcome::Done);
    }
    let fail = async |reason: &str| -> anyhow::Result<Outcome> {
        webhook_event::finish(
            &state.db,
            event.id,
            Status::Failed,
            Some(reason),
            Utc::now(),
        )
        .await?;
        Ok(Outcome::Discard(reason.to_owned()))
    };
    let Some(provider) = Provider::from_slug(&event.provider) else {
        return fail("the provider is not configured in this version of the app").await;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&event.payload) else {
        return fail("the payload is not JSON").await;
    };
    let delivery = Delivery {
        provider,
        kind: &event.event_type,
        event_id: &event.event_id,
        delivery_id: event.delivery_id.as_deref(),
        payload,
    };
    match handler(state, &delivery).await {
        Ok(Handled::Done) => {
            webhook_event::finish(&state.db, event.id, Status::Processed, None, Utc::now()).await?;
            Ok(Outcome::Done)
        }
        Ok(Handled::Ignored) => {
            webhook_event::finish(&state.db, event.id, Status::Ignored, None, Utc::now()).await?;
            Ok(Outcome::Done)
        }
        Ok(Handled::Failed(reason)) => fail(&reason).await,
        Err(e) => {
            let message = format!("{e:#}");
            if job.attempt >= job.max_attempts {
                // The queue discards the job after this attempt; the event says why.
                webhook_event::finish(
                    &state.db,
                    event.id,
                    Status::Failed,
                    Some(&message),
                    Utc::now(),
                )
                .await?;
            } else {
                webhook_event::note_error(&state.db, event.id, &message).await?;
            }
            Err(e)
        }
    }
}
