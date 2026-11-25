//! Background jobs (Active Job / Oban workers). The `Worker` trait and the registry live here
//! because they know `AppState`; the queue itself is `kernel::jobs`.
//! GUIDE: .offrails/guides/jobs.md#the-worker-trait-and-dispatcher-srcjobsmodrs
//!
//! Adding a worker: a file in `src/jobs/`, a `pub mod` line below, and one arm in **both**
//! `dispatch` and `worker_config`. A worker missing from `dispatch` is discarded as
//! "unknown worker" (test: every_registered_worker_dispatches). A sample that ships a worker
//! registers it at the four `samples:` anchor comments below (`worker mods`, `worker dispatch`,
//! `worker config`, `worker names`).

use std::{future::Future, sync::Arc, time::Duration};

use sea_orm::ConnectionTrait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::watch;

use crate::{error::AppError, kernel, state::AppState};

pub mod cleanup;
pub mod cli;
pub mod examples;
pub mod recurring;
// samples: worker mods

pub use crate::kernel::jobs::{
    Batch, DispatchFn, Inserted, JobBuilder, JobDefaults, JobInfo, Outcome, Unique, UniqueStates,
    WorkerConfig,
};
use cleanup::CleanupJob;
use examples::{DeliverWebhookJob, ExportReportJob, ReportBatchDoneJob};

pub trait Worker: Serialize + DeserializeOwned + Send + 'static {
    /// APP: permanent, snake_case. Queued rows and the dashboard's history reference it, so
    /// rename the struct freely but never this string.
    const NAME: &'static str;
    /// APP: the queue it runs on; it must be in `APP_QUEUES` on some node, or it waits forever
    /// (the dashboard flags such queues "no nodes").
    const QUEUE: &'static str = "default";
    /// APP: size to the work. 20 (~9 days of backoff) suits syncs; transactional mail wants ~5.
    const MAX_ATTEMPTS: i32 = 20;
    /// 0 runs first … 9 runs last.
    const PRIORITY: i16 = 0;
    /// `std::time::Duration`: tokio consumes it. A job past it is cut off and retried.
    const TIMEOUT: Duration = Duration::from_secs(300);

    /// Default uniqueness for every insert of this worker.
    fn unique() -> Option<Unique> {
        None
    }

    /// `chrono::Duration`: it is added to `DateTimeUtc` values stored in the database.
    fn backoff(attempt: i32) -> chrono::Duration {
        kernel::jobs::default_backoff(attempt)
    }

    /// The work. Return `Err` to retry with backoff, or an `Outcome`.
    /// WHY: `self` is the typed args, decoded by `dispatch` from the row's JSON; args that
    /// don't decode are discarded, never guessed at.
    fn perform(
        self,
        job: &JobInfo,
        state: &AppState,
    ) -> impl Future<Output = anyhow::Result<Outcome>> + Send;

    /// The builder with this worker's defaults.
    fn into_job(self) -> JobBuilder
    where
        Self: Sized,
    {
        JobBuilder::new(
            JobDefaults {
                worker: Self::NAME,
                queue: Self::QUEUE,
                max_attempts: Self::MAX_ATTEMPTS,
                priority: Self::PRIORITY,
                unique: Self::unique(),
            },
            serde_json::to_value(&self),
        )
    }
}

/// `perform_later` with the worker's defaults. Pass the transaction that created the job's data.
pub async fn enqueue<W: Worker>(
    db: &impl ConnectionTrait,
    worker: W,
) -> Result<Inserted, AppError> {
    Ok(worker.into_job().insert(db).await?)
}

/// The explicit, compile-checked registry: every worker appears here exactly once, and in
/// `worker_config` below.
pub async fn dispatch(
    state: AppState,
    worker: String,
    args: Value,
    job: JobInfo,
) -> anyhow::Result<Outcome> {
    match worker.as_str() {
        // APP: one arm per worker.
        CleanupJob::NAME => run::<CleanupJob>(&state, args, &job).await,
        ExportReportJob::NAME => run::<ExportReportJob>(&state, args, &job).await,
        DeliverWebhookJob::NAME => run::<DeliverWebhookJob>(&state, args, &job).await,
        ReportBatchDoneJob::NAME => run::<ReportBatchDoneJob>(&state, args, &job).await,
        // samples: worker dispatch
        other => Ok(Outcome::Discard(format!("unknown worker: {other}"))),
    }
}

/// Same match as `dispatch`, for the kernel's timeout and backoff.
pub fn worker_config(worker: &str) -> WorkerConfig {
    match worker {
        // APP: one arm per worker, the same as in `dispatch`.
        CleanupJob::NAME => config::<CleanupJob>(),
        ExportReportJob::NAME => config::<ExportReportJob>(),
        DeliverWebhookJob::NAME => config::<DeliverWebhookJob>(),
        ReportBatchDoneJob::NAME => config::<ReportBatchDoneJob>(),
        // samples: worker config
        _ => WorkerConfig::default(),
    }
}

/// Every registered worker name (tests check each one dispatches).
pub const WORKERS: &[&str] = &[
    CleanupJob::NAME,
    ExportReportJob::NAME,
    DeliverWebhookJob::NAME,
    ReportBatchDoneJob::NAME,
    // samples: worker names
];

fn config<W: Worker>() -> WorkerConfig {
    WorkerConfig {
        timeout: W::TIMEOUT,
        backoff: W::backoff,
    }
}

async fn run<W: Worker>(state: &AppState, args: Value, job: &JobInfo) -> anyhow::Result<Outcome> {
    match serde_json::from_value::<W>(args) {
        Ok(w) => w.perform(job, state).await,
        // SECURITY: typed decoding only: a row with unexpected args (edited, or from an older
        // version) is discarded, never run with guessed values
        // (test: unknown_workers_and_bad_args_are_discarded).
        Err(e) => Ok(Outcome::Discard(format!("args don't deserialize: {e}"))),
    }
}

/// The dispatcher as the function the kernel calls.
pub fn dispatch_fn(state: AppState) -> DispatchFn {
    Arc::new(move |worker, args, job| Box::pin(dispatch(state.clone(), worker, args, job)))
}

/// Options for `kernel::jobs::run`, from config (`APP_QUEUES`, `APP_JOBS_*`).
pub fn worker_options(state: &AppState) -> kernel::jobs::WorkerOptions {
    let c = &state.config.jobs;
    kernel::jobs::WorkerOptions {
        queues: c.queues.clone(),
        node: c.node.clone().unwrap_or_else(kernel::jobs::node_id),
        prune_after: chrono::Duration::days(i64::from(c.prune_days)),
        shutdown_grace: c.shutdown_grace,
        schedule: recurring::SCHEDULE,
    }
}

/// `app worker [--queues default:10]`: runs until Ctrl-C / SIGTERM, then shuts down gracefully.
pub async fn run_worker(
    state: AppState,
    queues: Option<String>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let mut opts = worker_options(&state);
    if let Some(raw) = queues {
        opts.queues = kernel::jobs::QueueSpec::parse_list(&raw)
            .map_err(|e| anyhow::anyhow!("--queues: {e}"))?;
    }
    kernel::jobs::run(
        state.db.clone(),
        opts,
        dispatch_fn(state),
        worker_config,
        shutdown,
    )
    .await;
    Ok(())
}

/// The worker running inside `app serve` (`APP_INLINE_WORKER=true`, the development default).
pub struct InlineWorker {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// Starts the inline worker when configured. `serve` stops it on shutdown.
pub fn start_inline_worker(state: &AppState) -> Option<InlineWorker> {
    if !state.config.jobs.inline_worker {
        return None;
    }
    let (stop, mut stopped) = watch::channel(false);
    let opts = worker_options(state);
    let db = state.db.clone();
    let dispatch = dispatch_fn(state.clone());
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = stopped.wait_for(|s| *s).await;
        };
        kernel::jobs::run(db, opts, dispatch, worker_config, shutdown).await;
    });
    tracing::info!("inline job worker started (APP_INLINE_WORKER)");
    Some(InlineWorker { stop, task })
}

impl InlineWorker {
    /// Stops fetching and waits for in-flight jobs (up to `APP_JOBS_SHUTDOWN_GRACE`).
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
