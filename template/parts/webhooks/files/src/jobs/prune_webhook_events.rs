//! Daily retention for `webhook_events` (`recurring.rs`, key `prune_webhook_events`). Payloads are
//! raw provider bodies (up to `MAX_BODY_BYTES` each, and they may hold personal data), so finished
//! events are deleted after `KEEP_DAYS`. An event still `received` is never deleted.
//! GUIDE: .offrails/guides/jobs.md#recurring-jobs-srcjobsrecurringrs-the-oban-cron-plugin

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    jobs::{JobInfo, Outcome, Unique, Worker},
    models::webhook_event,
    state::AppState,
};

/// APP: how long a finished event (and its payload) is kept. A replay older than this is stored
/// and handled again, so keep it longer than the providers retry (Stripe: 3 days).
pub const KEEP_DAYS: i64 = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PruneWebhookEventsJob;

impl Worker for PruneWebhookEventsJob {
    const NAME: &'static str = "prune_webhook_events";
    const MAX_ATTEMPTS: i32 = 3;

    // WHY: unique while pending: a backlog never stacks up several prunes.
    fn unique() -> Option<Unique> {
        Some(Unique::worker())
    }

    async fn perform(self, _job: &JobInfo, state: &AppState) -> anyhow::Result<Outcome> {
        // INVARIANT: payloads are not kept forever (test: finished_webhook_events_are_pruned_daily)
        let before = Utc::now() - chrono::Duration::days(KEEP_DAYS);
        let removed = webhook_event::prune(&state.db, before).await?;
        tracing::info!(removed, "webhook events pruned");
        Ok(Outcome::Done)
    }
}
