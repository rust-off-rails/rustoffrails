//! The job queue's kernel: claims, outcomes, retries, uniqueness, scheduling, rate limits,
//! batches, plugins and the admin API. Runs on SQLite and on Postgres (`TEST_DATABASE_URL`).
//! GUIDE: .offrails/guides/jobs.md#testing-obantesting-equivalents
//!
//! These tests use their own dispatcher (workers `ok`, `fail`, `snooze`, …), so they don't
//! depend on the app's workers; the tests at the end cover those. `runtime` covers the
//! long-running worker, `cli` the `app jobs` commands.

mod cli;
mod runtime;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use offrails_host::{
    jobs::{self as app_jobs, Worker, examples::ExportReportJob},
    kernel::jobs::{
        Batch, DispatchFn, JobBuilder, JobDefaults, JobState, Outcome, QueueSpec, Rate, Recurring,
        Unique, UniqueStates, WorkerConfig,
        admin::{self, Action, JobFilter},
        cron_tick,
        executor::{self, AttemptError},
        insert_all, job, job_batch, job_node, plugins, prune, recurring_execution, rescue_orphans,
        stage,
    },
};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    sea_query::Expr,
};
use serde_json::{Value, json};

use crate::support::TestApp;

// ---------------------------------------------------------------------------
// Test workers
// ---------------------------------------------------------------------------

/// Every job id a `count` job ran with, in order.
type Log = Arc<Mutex<Vec<i64>>>;

fn dispatcher(log: Log) -> DispatchFn {
    Arc::new(move |worker, args, job| {
        let log = log.clone();
        Box::pin(async move {
            match worker.as_str() {
                "ok" => Ok(Outcome::Done),
                "count" => {
                    log.lock().unwrap().push(job.id);
                    tokio::task::yield_now().await;
                    Ok(Outcome::Done)
                }
                "fail" => Err(anyhow::anyhow!("connection refused").context("charging card 42")),
                "hook" => {
                    let event = args["event"].as_str().unwrap_or_default().to_owned();
                    Err(anyhow::anyhow!("connection refused (os error 61)")
                        .context("connecting to hooks.partner.invalid:443")
                        .context(format!(
                            "POST https://hooks.partner.invalid/v2/events ({event})"
                        )))
                }
                // An error that quotes the URL that failed, credentials and all.
                "leak" => {
                    let url = args["url"].as_str().unwrap_or_default().to_owned();
                    Err(anyhow::anyhow!("401 Unauthorized").context(format!("GET {url}")))
                }
                "snooze" => Ok(Outcome::Snooze(chrono::Duration::minutes(5))),
                "cancel" => Ok(Outcome::Cancel("order 7 was deleted".into())),
                "discard" => Ok(Outcome::Discard("invalid address".into())),
                "panic" => panic!("worker exploded"),
                "hang" => {
                    std::future::pending::<()>().await;
                    Ok(Outcome::Done)
                }
                other => Ok(Outcome::Discard(format!("unknown worker: {other}"))),
            }
        })
    })
}

fn config(worker: &str) -> WorkerConfig {
    WorkerConfig {
        timeout: if worker == "hang" {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(10)
        },
        backoff: |_| chrono::Duration::seconds(60),
    }
}

fn builder(worker: &'static str) -> JobBuilder {
    builder_with(worker, json!({}))
}

fn builder_with(worker: &'static str, args: Value) -> JobBuilder {
    JobBuilder::new(
        JobDefaults {
            worker,
            queue: "default",
            max_attempts: 3,
            priority: 0,
            unique: None,
        },
        Ok(args),
    )
}

async fn insert(db: &DatabaseConnection, b: JobBuilder) -> i64 {
    b.insert(db).await.expect("insert").id
}

async fn get(db: &DatabaseConnection, id: i64) -> job::Model {
    admin::get(db, id).await.unwrap().expect("job exists")
}

async fn drain(app: &TestApp) -> offrails_host::kernel::jobs::Drained {
    executor::drain(app.db(), &dispatcher(Log::default()), config, None, false)
        .await
        .unwrap()
}

/// Makes a waiting job due now (simulated time passing), then stages it.
async fn make_due(db: &DatabaseConnection, id: i64) {
    job::Entity::update_many()
        .col_expr(
            job::Column::ScheduledAt,
            Expr::value(Utc::now() - chrono::Duration::seconds(1)),
        )
        .filter(job::Column::Id.eq(id))
        .exec(db)
        .await
        .unwrap();
    stage(db, Utc::now()).await.unwrap();
}

fn error_texts(j: &job::Model) -> Vec<String> {
    j.errors
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["error"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Running and outcomes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_job_runs_and_completes() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("ok")).await;
    assert_eq!(get(app.db(), id).await.state, JobState::Available);
    let drained = drain(&app).await;
    assert_eq!(drained.completed, 1);
    let j = get(app.db(), id).await;
    assert_eq!(j.state, JobState::Completed);
    assert_eq!(j.attempt, 1);
    assert!(j.completed_at.is_some() && j.attempted_at.is_some());
    assert_eq!(j.attempted_by.as_deref(), Some("drain"));
    assert_eq!(j.progress, Some(100));
}

/// INVARIANT: a job is claimed by one worker only.
#[tokio::test]
async fn concurrent_claims_run_each_job_exactly_once() {
    let app = TestApp::new().await;
    for _ in 0..30 {
        insert(app.db(), builder("count")).await;
    }
    let log = Log::default();
    let mut tasks = Vec::new();
    for n in 0..4 {
        let (db, log) = (app.db().clone(), log.clone());
        tasks.push(tokio::spawn(async move {
            executor::drain_as(
                &db,
                &dispatcher(log),
                config,
                None,
                false,
                &format!("node-{n}"),
            )
            .await
            .unwrap()
        }));
    }
    let mut completed = 0;
    for t in tasks {
        completed += t.await.unwrap().completed;
    }
    let mut ran = log.lock().unwrap().clone();
    assert_eq!(completed, 30);
    assert_eq!(ran.len(), 30, "some job ran twice or never");
    ran.sort_unstable();
    ran.dedup();
    assert_eq!(ran.len(), 30, "a job ran twice");
}

/// The claim itself, raced from many tasks: the claimed sets never overlap.
#[tokio::test]
async fn racing_claimers_get_disjoint_jobs() {
    let app = TestApp::new().await;
    insert_all(app.db(), (0..60).map(|_| builder("ok")).collect())
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for n in 0..6 {
        let db = app.db().clone();
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            loop {
                let got = executor::claim(&db, Some("default"), 4, &format!("n{n}"), Utc::now())
                    .await
                    .unwrap();
                if got.is_empty() {
                    break;
                }
                mine.extend(got.into_iter().map(|j| j.id));
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let total = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(total, 60);
    assert_eq!(all.len(), 60, "two claimers got the same job");
}

/// SQLite's in-memory test database has one connection; a file database with a pool shows
/// that the single-writer claim holds with real concurrency too.
#[tokio::test]
async fn concurrent_claims_on_a_sqlite_file_database_never_overlap() {
    use migration::{Migrator, MigratorTrait};
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path()
            .join("claims.sqlite3")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    let db = offrails_host::kernel::db::connect(&url, 6).await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    insert_all(&db, (0..80).map(|_| builder("count")).collect())
        .await
        .unwrap();
    let log = Log::default();
    let mut tasks = Vec::new();
    for n in 0..5 {
        let (db, log) = (db.clone(), log.clone());
        tasks.push(tokio::spawn(async move {
            executor::drain_as(&db, &dispatcher(log), config, None, false, &format!("f{n}"))
                .await
                .unwrap()
                .completed
        }));
    }
    let mut completed = 0;
    for t in tasks {
        completed += t.await.unwrap();
    }
    let mut ran = log.lock().unwrap().clone();
    ran.sort_unstable();
    ran.dedup();
    assert_eq!((completed, ran.len()), (80, 80));
}

#[tokio::test]
async fn failures_retry_with_backoff_then_discard_at_max_attempts() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("fail")).await;

    let drained = drain(&app).await;
    assert_eq!(drained.retryable, 1);
    let j = get(app.db(), id).await;
    assert_eq!(j.state, JobState::Retryable);
    assert_eq!(j.attempt, 1);
    let wait = j.scheduled_at - Utc::now();
    assert!(
        wait > chrono::Duration::seconds(50),
        "backoff applied: {wait}"
    );
    let errors = j.errors.as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["attempt"], 1);
    assert_eq!(
        errors[0]["error"], "charging card 42: connection refused",
        "the whole context chain"
    );
    assert!(
        errors[0]["detail"].as_str().unwrap().contains("Caused by"),
        "the full report is kept: {}",
        errors[0]["detail"]
    );
    assert_eq!(errors[0]["node"], "drain");
    assert!(errors[0]["duration_ms"].is_u64());

    // Not due yet: a drain doesn't touch it.
    assert_eq!(drain(&app).await.total(), 0);

    make_due(app.db(), id).await;
    drain(&app).await;
    assert_eq!(get(app.db(), id).await.attempt, 2);
    make_due(app.db(), id).await;
    let drained = drain(&app).await;
    assert_eq!(drained.discarded, 1);
    let j = get(app.db(), id).await;
    assert_eq!(j.state, JobState::Discarded);
    assert_eq!(j.attempt, 3);
    assert!(j.discarded_at.is_some());
    assert_eq!(error_texts(&j).len(), 3);
}

#[tokio::test]
async fn snooze_gives_the_attempt_back() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("snooze")).await;
    assert_eq!(drain(&app).await.snoozed, 1);
    let j = get(app.db(), id).await;
    assert_eq!(j.state, JobState::Scheduled);
    assert_eq!((j.attempt, j.max_attempts), (1, 4));
    assert_eq!(j.meta["snoozed"], 1);
    assert!(j.scheduled_at > Utc::now() + chrono::Duration::minutes(4));
    assert!(j.errors.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_discard_panic_and_timeout_outcomes() {
    let app = TestApp::new().await;
    let cancel = insert(app.db(), builder("cancel")).await;
    let discard = insert(app.db(), builder("discard")).await;
    let panic = insert(app.db(), builder("panic")).await;
    let hang = insert(app.db(), builder("hang")).await;
    let drained = drain(&app).await;
    assert_eq!(
        (drained.cancelled, drained.discarded, drained.retryable),
        (1, 1, 2)
    );

    let j = get(app.db(), cancel).await;
    assert_eq!(j.state, JobState::Cancelled);
    assert!(j.cancelled_at.is_some());
    assert_eq!(error_texts(&j), ["cancelled: order 7 was deleted"]);

    let j = get(app.db(), discard).await;
    assert_eq!(j.state, JobState::Discarded);
    assert_eq!(error_texts(&j), ["invalid address"]);

    let j = get(app.db(), panic).await;
    assert_eq!(j.state, JobState::Retryable);
    assert_eq!(error_texts(&j), ["panicked: worker exploded"]);

    let j = get(app.db(), hang).await;
    assert_eq!(j.state, JobState::Retryable);
    assert_eq!(error_texts(&j), ["timed out after 50ms"]);
}

/// INVARIANT: a result never overwrites a job that was cancelled or rescued while it ran.
#[tokio::test]
async fn a_result_never_resurrects_a_cancelled_job() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("ok")).await;
    let claimed = executor::claim(app.db(), None, 1, "n1", Utc::now())
        .await
        .unwrap();
    assert_eq!(admin::cancel(app.db(), &[id]).await.unwrap(), 1);
    let recorded = executor::record(app.db(), &claimed[0], "n1", Ok(Outcome::Done), |_| {
        chrono::Duration::seconds(1)
    })
    .await
    .unwrap();
    assert_eq!(recorded, None);
    assert_eq!(get(app.db(), id).await.state, JobState::Cancelled);

    // Nor does a node that lost the job to the Lifeline.
    let id = insert(app.db(), builder("ok")).await;
    let claimed = executor::claim(app.db(), None, 1, "n1", Utc::now())
        .await
        .unwrap();
    rescue_orphans(app.db(), Utc::now()).await.unwrap();
    let recorded = executor::record(
        app.db(),
        &claimed[0],
        "n1",
        Err(AttemptError::new("late")),
        |_| chrono::Duration::seconds(1),
    )
    .await
    .unwrap();
    assert_eq!(recorded, None);
    assert_eq!(get(app.db(), id).await.state, JobState::Available);
}

#[tokio::test]
async fn the_state_column_rejects_unknown_states() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("ok")).await;
    let res = job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("bogus"))
        .filter(job::Column::Id.eq(id))
        .exec(app.db())
        .await;
    assert!(res.is_err(), "the CHECK constraint rejects it");
    let res = job::Entity::update_many()
        .col_expr(job::Column::Priority, Expr::value(12))
        .filter(job::Column::Id.eq(id))
        .exec(app.db())
        .await;
    assert!(res.is_err(), "priority is 0-9");
}

// ---------------------------------------------------------------------------
// Inserting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn args_that_fail_to_serialize_insert_nothing() {
    let app = TestApp::new().await;
    let mut bad: HashMap<(i32, i32), i32> = HashMap::new();
    bad.insert((1, 2), 3);
    let b = JobBuilder::new(
        JobDefaults {
            worker: "ok",
            queue: "default",
            max_attempts: 1,
            priority: 0,
            unique: None,
        },
        serde_json::to_value(&bad),
    );
    assert!(b.insert(app.db()).await.is_err());
    assert_eq!(job::Entity::find().count(app.db()).await.unwrap(), 0);
}

/// INVARIANT: two inserts of the same unique job leave one row.
#[tokio::test]
async fn unique_jobs_insert_once_even_concurrently() {
    let app = TestApp::new().await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = app.db().clone();
        tasks.push(tokio::spawn(async move {
            builder_with("ok", json!({"user_id": 1}))
                .unique(Unique::args())
                .insert(&db)
                .await
                .unwrap()
        }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    let first = results.iter().find(|r| !r.duplicate).unwrap().id;
    assert_eq!(results.iter().filter(|r| !r.duplicate).count(), 1);
    assert!(results.iter().all(|r| r.id == first));
    assert_eq!(job::Entity::find().count(app.db()).await.unwrap(), 1);

    // Incomplete (the default): the key is freed once the job is final.
    drain(&app).await;
    assert!(get(app.db(), first).await.unique_key.is_none());
    let again = builder_with("ok", json!({"user_id": 1}))
        .unique(Unique::args())
        .insert(app.db())
        .await
        .unwrap();
    assert!(!again.duplicate);

    // All: the key outlives the job.
    let kept = builder_with("ok", json!({"user_id": 2}))
        .unique(Unique::args().states(UniqueStates::All))
        .insert(app.db())
        .await
        .unwrap();
    drain(&app).await;
    let dup = builder_with("ok", json!({"user_id": 2}))
        .unique(Unique::args().states(UniqueStates::All))
        .insert(app.db())
        .await
        .unwrap();
    assert_eq!((dup.id, dup.duplicate), (kept.id, true));
}

#[tokio::test]
async fn uniqueness_by_args_keys_worker_and_period() {
    let app = TestApp::new().await;
    let a = builder_with("ok", json!({"user_id": 1, "n": 1}))
        .unique(Unique::args_keys(&["user_id"]))
        .insert(app.db())
        .await
        .unwrap();
    let b = builder_with("ok", json!({"user_id": 1, "n": 2}))
        .unique(Unique::args_keys(&["user_id"]))
        .insert(app.db())
        .await
        .unwrap();
    assert_eq!((b.id, b.duplicate), (a.id, true));
    let c = builder_with("ok", json!({"user_id": 2, "n": 1}))
        .unique(Unique::args_keys(&["user_id"]))
        .insert(app.db())
        .await
        .unwrap();
    assert!(!c.duplicate);

    let w1 = builder_with("snooze", json!({"x": 1}))
        .unique(Unique::worker())
        .insert(app.db())
        .await
        .unwrap();
    let w2 = builder_with("snooze", json!({"x": 2}))
        .unique(Unique::worker())
        .insert(app.db())
        .await
        .unwrap();
    assert_eq!((w2.id, w2.duplicate), (w1.id, true));
    // Another queue is another job.
    let w3 = builder_with("snooze", json!({}))
        .queue("other")
        .unique(Unique::worker())
        .insert(app.db())
        .await
        .unwrap();
    assert!(!w3.duplicate);

    // A period: the key carries the time bucket.
    let p = builder_with("ok", json!({"k": 1}))
        .unique(Unique::args().period(chrono::Duration::hours(1)))
        .insert(app.db())
        .await
        .unwrap();
    let p2 = builder_with("ok", json!({"k": 1}))
        .unique(Unique::args().period(chrono::Duration::hours(1)))
        .insert(app.db())
        .await
        .unwrap();
    assert_eq!((p2.id, p2.duplicate), (p.id, true));
    let row = get(app.db(), p.id).await;
    assert_eq!(row.meta["unique_states"], "incomplete");
}

#[tokio::test]
async fn insert_all_inserts_in_bulk_and_refuses_unique_jobs() {
    let app = TestApp::new().await;
    let n = insert_all(
        app.db(),
        (0..1_234)
            .map(|i| builder_with("ok", json!({"i": i})).tag("bulk"))
            .collect(),
    )
    .await
    .unwrap();
    assert_eq!(n, 1_234);
    assert_eq!(job::Entity::find().count(app.db()).await.unwrap(), 1_234);
    assert!(
        insert_all(app.db(), vec![builder("ok").unique(Unique::args())])
            .await
            .is_err()
    );
    assert_eq!(insert_all(app.db(), Vec::new()).await.unwrap(), 0);
}

#[tokio::test]
async fn jobs_enqueued_in_a_rolled_back_transaction_never_exist() {
    let app = TestApp::new().await;
    let txn = offrails_host::kernel::db::begin(app.db()).await.unwrap();
    builder("ok").insert(&txn).await.unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(job::Entity::find().count(app.db()).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Ordering and scheduling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn priorities_run_first_and_scheduled_jobs_wait() {
    let app = TestApp::new().await;
    let low = insert(app.db(), builder("ok").priority(9)).await;
    let high = insert(app.db(), builder("ok").priority(0)).await;
    let mid = insert(app.db(), builder("ok").priority(4)).await;
    let later = insert(
        app.db(),
        builder("ok").schedule_in(chrono::Duration::hours(1)),
    )
    .await;
    assert_eq!(get(app.db(), later).await.state, JobState::Scheduled);

    let claimed = executor::claim(app.db(), None, 10, "n", Utc::now())
        .await
        .unwrap();
    let order: Vec<i64> = claimed.iter().map(|j| j.id).collect();
    assert_eq!(order, [high, mid, low]);

    // Staging only moves what is due.
    assert_eq!(stage(app.db(), Utc::now()).await.unwrap(), 0);
    make_due(app.db(), later).await;
    assert_eq!(get(app.db(), later).await.state, JobState::Available);
}

#[tokio::test]
async fn queues_are_claimed_separately() {
    let app = TestApp::new().await;
    let a = insert(app.db(), builder("ok").queue("mailers")).await;
    insert(app.db(), builder("ok")).await;
    let claimed = executor::claim(app.db(), Some("mailers"), 10, "n", Utc::now())
        .await
        .unwrap();
    assert_eq!(claimed.iter().map(|j| j.id).collect::<Vec<_>>(), [a]);
}

/// INVARIANT: a rate-limited queue never starts more than `allowed` jobs per period.
#[tokio::test]
async fn rate_limited_queues_start_at_most_n_per_period() {
    let app = TestApp::new().await;
    insert_all(
        app.db(),
        (0..12).map(|_| builder("ok").queue("hooks")).collect(),
    )
    .await
    .unwrap();
    let spec = QueueSpec::new("hooks", 10).rate(Rate::parse("3/1m").unwrap());
    let mut tasks = Vec::new();
    for n in 0..4 {
        let (db, spec) = (app.db().clone(), spec.clone());
        tasks.push(tokio::spawn(async move {
            executor::claim_rate_limited(&db, &spec, 10, &format!("n{n}"), Utc::now())
                .await
                .unwrap()
                .len()
        }));
    }
    let mut started = 0;
    for t in tasks {
        started += t.await.unwrap();
    }
    assert_eq!(started, 3, "three per minute across all claimers");

    // A minute later there is room again.
    job::Entity::update_many()
        .col_expr(
            job::Column::AttemptedAt,
            Expr::value(Utc::now() - chrono::Duration::minutes(2)),
        )
        .filter(job::Column::Queue.eq("hooks"))
        .filter(job::Column::AttemptedAt.is_not_null())
        .exec(app.db())
        .await
        .unwrap();
    let more = executor::claim_rate_limited(app.db(), &spec, 10, "n9", Utc::now())
        .await
        .unwrap();
    assert_eq!(more.len(), 3);
}

// ---------------------------------------------------------------------------
// Batches
// ---------------------------------------------------------------------------

/// INVARIANT: a batch's callback is enqueued once, after its last job finishes.
#[tokio::test]
async fn a_batch_enqueues_its_callback_once_when_every_job_is_final() {
    let app = TestApp::new().await;
    let mut jobs: Vec<JobBuilder> = (0..4).map(|_| builder("ok")).collect();
    jobs.push(builder("discard"));
    let batch = Batch::new("import 42")
        .on_complete(builder_with("count", json!({"import_id": 42})).queue("callbacks"))
        .insert(app.db(), jobs)
        .await
        .unwrap();
    assert_eq!(batch.jobs, 5);
    let callbacks = || async {
        job::Entity::find()
            .filter(job::Column::Worker.eq("count"))
            .all(app.db())
            .await
            .unwrap()
    };

    executor::drain(
        app.db(),
        &dispatcher(Log::default()),
        config,
        Some("default"),
        false,
    )
    .await
    .unwrap();
    let found = callbacks().await;
    assert_eq!(found.len(), 1, "exactly one callback");
    let cb = &found[0];
    assert_eq!(cb.queue, "callbacks");
    assert_eq!(cb.args, json!({"import_id": 42}));
    assert_eq!(cb.meta["batch"]["completed"], 4);
    assert_eq!(cb.meta["batch"]["discarded"], 1);
    assert_eq!(cb.meta["batch"]["name"], "import 42");
    assert!(cb.tag_list().contains(&"batch".to_owned()));
    let row = job_batch::Entity::find_by_id(batch.id)
        .one(app.db())
        .await
        .unwrap()
        .unwrap();
    assert!(row.finished_at.is_some());
    assert_eq!(row.callback_job_id, Some(cb.id));

    // Racing checks after the fact never add a second one.
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let db = app.db().clone();
        tasks.push(tokio::spawn(async move {
            plugins::finish_batch_if_done(&db, batch.id).await.unwrap()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), None);
    }
    assert_eq!(callbacks().await.len(), 1);

    // An operator cancelling the last open job finishes a batch too.
    let b2 = Batch::new("two")
        .on_complete(builder("count"))
        .insert(app.db(), vec![builder("ok"), builder("snooze")])
        .await
        .unwrap();
    executor::drain(
        app.db(),
        &dispatcher(Log::default()),
        config,
        Some("default"),
        false,
    )
    .await
    .unwrap();
    assert_eq!(callbacks().await.len(), 1);
    let open: Vec<i64> = job::Entity::find()
        .filter(job::Column::BatchId.eq(b2.id))
        .filter(job::Column::State.eq(JobState::Scheduled))
        .all(app.db())
        .await
        .unwrap()
        .into_iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(open.len(), 1);
    admin::cancel(app.db(), &open).await.unwrap();
    assert_eq!(callbacks().await.len(), 2);

    // An empty batch finishes at once.
    Batch::new("empty")
        .on_complete(builder("count"))
        .insert(app.db(), Vec::new())
        .await
        .unwrap();
    assert_eq!(callbacks().await.len(), 3);
}

// ---------------------------------------------------------------------------
// Lifeline, pruner, cron
// ---------------------------------------------------------------------------

async fn node_row(db: &DatabaseConnection, node: &str, heartbeat_ago: chrono::Duration) {
    job_node::Entity::insert(job_node::ActiveModel {
        node: Set(node.to_owned()),
        queues: Set(json!({"default": {"limit": 10}})),
        started_at: Set(Utc::now() - chrono::Duration::hours(1)),
        heartbeat_at: Set(Utc::now() - heartbeat_ago),
    })
    .exec(db)
    .await
    .unwrap();
}

/// INVARIANT: only dead nodes' jobs are rescued.
#[tokio::test]
async fn the_lifeline_rescues_only_dead_nodes_jobs() {
    let app = TestApp::new().await;
    node_row(app.db(), "alive", chrono::Duration::seconds(2)).await;
    node_row(app.db(), "dead", chrono::Duration::minutes(2)).await;
    let mine = insert(app.db(), builder("ok")).await;
    executor::claim(app.db(), None, 1, "alive", Utc::now())
        .await
        .unwrap();
    let orphan = insert(app.db(), builder("ok")).await;
    executor::claim(app.db(), None, 1, "dead", Utc::now())
        .await
        .unwrap();
    let last_try = insert(app.db(), builder("ok").max_attempts(1)).await;
    executor::claim(app.db(), None, 1, "vanished", Utc::now())
        .await
        .unwrap();

    assert_eq!(rescue_orphans(app.db(), Utc::now()).await.unwrap(), 2);
    assert_eq!(get(app.db(), mine).await.state, JobState::Executing);
    let j = get(app.db(), orphan).await;
    assert_eq!(j.state, JobState::Available);
    assert!(error_texts(&j)[0].starts_with("orphaned: node dead"));
    let j = get(app.db(), last_try).await;
    assert_eq!(j.state, JobState::Discarded, "out of attempts");
    // Nodes silent for > 5 minutes are forgotten.
    rescue_orphans(app.db(), Utc::now() + chrono::Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(job_node::Entity::find().count(app.db()).await.unwrap(), 0);
}

/// Crash recovery end to end: a node claims a job and dies; another node finishes it.
#[tokio::test]
async fn a_crashed_nodes_job_runs_again_elsewhere() {
    let app = TestApp::new().await;
    let id = insert(app.db(), builder("count")).await;
    executor::claim(app.db(), None, 1, "crashed", Utc::now())
        .await
        .unwrap();
    // The process died: nothing records a result, no heartbeat row exists.
    assert_eq!(drain(&app).await.total(), 0, "still executing elsewhere");
    rescue_orphans(app.db(), Utc::now()).await.unwrap();
    let log = Log::default();
    executor::drain(app.db(), &dispatcher(log.clone()), config, None, false)
        .await
        .unwrap();
    let j = get(app.db(), id).await;
    assert_eq!(j.state, JobState::Completed);
    assert_eq!(j.attempt, 2);
    assert_eq!(*log.lock().unwrap(), [id]);
}

/// INVARIANT: the Pruner never deletes a job that isn't final, or a recent one.
#[tokio::test]
async fn the_pruner_keeps_recent_and_unfinished_jobs() {
    let app = TestApp::new().await;
    let old_done = insert(app.db(), builder("ok")).await;
    let new_done = insert(app.db(), builder("ok")).await;
    let old_discarded = insert(app.db(), builder("discard")).await;
    drain(&app).await;
    let waiting = insert(
        app.db(),
        builder("ok").schedule_in(chrono::Duration::days(30)),
    )
    .await;
    let back = Utc::now() - chrono::Duration::days(8);
    for (id, col) in [
        (old_done, job::Column::CompletedAt),
        (old_discarded, job::Column::DiscardedAt),
    ] {
        job::Entity::update_many()
            .col_expr(col, Expr::value(back))
            .filter(job::Column::Id.eq(id))
            .exec(app.db())
            .await
            .unwrap();
    }
    // An old row that is not final (inserted long ago, still waiting) stays.
    job::Entity::update_many()
        .col_expr(job::Column::InsertedAt, Expr::value(back))
        .filter(job::Column::Id.eq(waiting))
        .exec(app.db())
        .await
        .unwrap();
    let n = prune(app.db(), Utc::now(), chrono::Duration::days(7))
        .await
        .unwrap();
    assert_eq!(n, 2);
    for id in [old_done, old_discarded] {
        assert!(admin::get(app.db(), id).await.unwrap().is_none());
    }
    for id in [new_done, waiting] {
        assert!(admin::get(app.db(), id).await.unwrap().is_some());
    }
}

const SCHEDULE: &[Recurring] = &[
    Recurring {
        key: "every_minute",
        cron: "* * * * *",
        job: || builder("ok").tag("mine"),
    },
    Recurring {
        key: "broken",
        cron: "not a cron",
        job: || builder("ok"),
    },
];

/// INVARIANT: one run per (key, due time) across every node.
#[tokio::test]
async fn cron_fires_once_across_concurrent_ticks() {
    let app = TestApp::new().await;
    let now = Utc::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let db = app.db().clone();
        tasks.push(tokio::spawn(async move {
            cron_tick(&db, SCHEDULE, now).await.unwrap()
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.extend(t.await.unwrap());
    }
    assert_eq!(ids.len(), 1, "one job across all ticks");
    let j = get(app.db(), ids[0]).await;
    assert_eq!(j.meta["cron"], "every_minute");
    assert_eq!(j.tag_list(), ["mine", "cron"]);
    let runs = recurring_execution::Entity::find()
        .all(app.db())
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].job_id, Some(ids[0]));
    // The same tick again changes nothing.
    assert!(cron_tick(app.db(), SCHEDULE, now).await.unwrap().is_empty());
    let last = admin::last_runs(app.db()).await.unwrap();
    assert_eq!(last["every_minute"].1, Some(ids[0]));
}

#[tokio::test]
async fn cron_skips_stale_due_times_and_runs_on_demand() {
    let app = TestApp::new().await;
    let hourly: &[Recurring] = &[Recurring {
        key: "hourly",
        cron: "0 * * * *",
        job: || builder("ok"),
    }];
    // 40 minutes past the hour: too late to replay it.
    let t = chrono::DateTime::parse_from_rfc3339("2026-10-16T09:40:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert!(cron_tick(app.db(), hourly, t).await.unwrap().is_empty());
    // 30 seconds past: runs.
    let t = t + chrono::Duration::minutes(20) + chrono::Duration::seconds(30);
    assert_eq!(cron_tick(app.db(), hourly, t).await.unwrap().len(), 1);

    let id = plugins::run_cron_now(app.db(), &hourly[0]).await.unwrap();
    let j = get(app.db(), id).await;
    assert_eq!(j.meta["manual"], true);
    assert_eq!(j.meta["cron"], "hourly");
}

// ---------------------------------------------------------------------------
// Admin API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_actions_follow_the_state_rules() {
    let app = TestApp::new().await;
    let done = insert(app.db(), builder("ok")).await;
    let failed = insert(app.db(), builder("fail").max_attempts(1)).await;
    drain(&app).await;
    let waiting = insert(
        app.db(),
        builder("ok").schedule_in(chrono::Duration::hours(1)),
    )
    .await;
    let running = insert(app.db(), builder("ok")).await;
    executor::claim(app.db(), None, 1, "n", Utc::now())
        .await
        .unwrap();

    // Retry: final jobs come back with an attempt left; the history stays.
    assert_eq!(admin::retry(app.db(), &[failed, running]).await.unwrap(), 1);
    let j = get(app.db(), failed).await;
    assert_eq!(j.state, JobState::Available);
    assert_eq!((j.attempt, j.max_attempts), (1, 2));
    assert!(j.discarded_at.is_none());
    assert_eq!(error_texts(&j).len(), 1);

    // Run now: only scheduled / retryable.
    assert_eq!(admin::run_now(app.db(), &[waiting, done]).await.unwrap(), 1);
    assert_eq!(get(app.db(), waiting).await.state, JobState::Available);

    // Delete: never an executing job.
    assert_eq!(admin::delete(app.db(), &[running, done]).await.unwrap(), 1);
    assert!(admin::get(app.db(), done).await.unwrap().is_none());

    // Discard and cancel: only unfinished jobs; executing ones too (their node aborts them).
    assert_eq!(admin::discard(app.db(), &[running]).await.unwrap(), 1);
    let j = get(app.db(), running).await;
    assert_eq!(j.state, JobState::Discarded);
    assert_eq!(error_texts(&j), ["discarded: by an operator"]);
    assert_eq!(admin::cancel(app.db(), &[running]).await.unwrap(), 0);
    assert_eq!(admin::cancel(app.db(), &[waiting]).await.unwrap(), 1);

    assert_eq!(
        Action::parse("retry")
            .unwrap()
            .apply(app.db(), &[running])
            .await
            .unwrap(),
        1
    );
}

/// INVARIANT: a bulk action with no filter changes nothing.
#[tokio::test]
async fn bulk_actions_refuse_an_empty_filter() {
    let app = TestApp::new().await;
    for _ in 0..3 {
        insert(app.db(), builder("discard")).await;
    }
    insert(app.db(), builder("ok")).await;
    drain(&app).await;
    let none = JobFilter::default();
    assert_eq!(
        Action::Retry.apply_matching(app.db(), &none).await.unwrap(),
        0
    );
    let discarded = JobFilter {
        state: Some(JobState::Discarded),
        worker: Some("discard".into()),
        ..Default::default()
    };
    assert_eq!(
        admin::count_matching(app.db(), &discarded).await.unwrap(),
        3
    );
    assert_eq!(
        Action::Retry
            .apply_matching(app.db(), &discarded)
            .await
            .unwrap(),
        3
    );
}

/// Security: a failure message quoting a URL with credentials is stored (and logged) redacted, in
/// the one-line error and in the full report, so the dashboard, search and `app jobs show` never
/// see the secret.
#[tokio::test]
async fn job_errors_are_stored_redacted() {
    let app = TestApp::new().await;
    let url = "https://deploy:hunter2@api.example.com/v1/charges?token=tok_live_abc123&page=2";
    let id = insert(app.db(), builder_with("leak", json!({ "url": url }))).await;
    drain(&app).await;
    let stored = get(app.db(), id).await.errors.to_string();
    for secret in ["hunter2", "deploy:", "tok_live_abc123"] {
        assert!(!stored.contains(secret), "{secret} stored in {stored}");
    }
    assert!(
        stored.contains("https://[redacted]@api.example.com/v1/charges?token=[redacted]&page=2"),
        "{stored}"
    );
    assert!(
        stored.contains("401 Unauthorized"),
        "the cause stays readable"
    );
    // Search can't find the secret through the errors either.
    let by_error = JobFilter {
        error: Some("hunter2".into()),
        ..Default::default()
    };
    assert!(
        admin::list(app.db(), &by_error, None, 50)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Security: `q` matches args only as every view shows them (redacted). Matching the raw args
/// would answer "is there a job whose api_key starts with sk_live_a?", one character at a time.
#[tokio::test]
async fn search_never_matches_redacted_values() {
    let app = TestApp::new().await;
    let id = insert(
        app.db(),
        builder_with(
            "ok",
            json!({ "order_id": 7781, "api_key": "TOPSECRET99", "nested": { "password": "swordfish" } }),
        )
        .queue("billing"),
    )
    .await;
    let q = |q: &str| JobFilter {
        q: Some(q.into()),
        ..Default::default()
    };
    for secret in ["TOPSECRET99", "topsecret", "SECRET9", "swordfish", "fish"] {
        let filter = q(secret);
        assert!(
            admin::list(app.db(), &filter, None, 50)
                .await
                .unwrap()
                .is_empty(),
            "{secret:?} matched through a redacted value"
        );
        assert_eq!(admin::count_matching(app.db(), &filter).await.unwrap(), 0);
        assert!(
            admin::matching_ids(app.db(), &filter)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            admin::state_counts(app.db(), &filter)
                .await
                .unwrap()
                .is_empty()
        );
    }
    // What the views show stays searchable: keys, plain values, the worker, the queue, the id.
    for shown in ["order_id", "order", "api_key", "billing", "ok"] {
        let found = admin::list(app.db(), &q(shown), None, 50).await.unwrap();
        assert_eq!(
            found.iter().map(|j| j.id).collect::<Vec<_>>(),
            [id],
            "{shown:?}"
        );
    }
    assert_eq!(
        admin::list(app.db(), &q(&id.to_string()), None, 50)
            .await
            .unwrap()[0]
            .id,
        id
    );
}

/// Security: filters and search are bound parameters; quotes and wildcards are just text.
#[tokio::test]
async fn filters_and_search_are_parameterized_against_injection() {
    let app = TestApp::new().await;
    insert(
        app.db(),
        builder_with("ok", json!({"note": "100% done_ok"})).tag("user:42"),
    )
    .await;
    insert(
        app.db(),
        builder_with("ok", json!({"note": "other"})).tag("user:4"),
    )
    .await;
    for evil in [
        "'; DROP TABLE jobs; --",
        "\" OR 1=1 --",
        "') OR ('1'='1",
        "%' OR '%'='",
        "\\",
    ] {
        for filter in [
            JobFilter {
                q: Some(evil.into()),
                ..Default::default()
            },
            JobFilter {
                tag: Some(evil.into()),
                ..Default::default()
            },
            JobFilter {
                queue: Some(evil.into()),
                worker: Some(evil.into()),
                error: Some(evil.into()),
                ..Default::default()
            },
        ] {
            assert!(
                admin::list(app.db(), &filter, None, 50)
                    .await
                    .unwrap()
                    .is_empty(),
                "{evil:?} matched something"
            );
        }
    }
    assert_eq!(
        job::Entity::find().count(app.db()).await.unwrap(),
        2,
        "table intact"
    );
    // Wildcards are literal.
    let q = |q: &str| JobFilter {
        q: Some(q.into()),
        ..Default::default()
    };
    assert_eq!(
        admin::list(app.db(), &q("%"), None, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        admin::list(app.db(), &q("1_0"), None, 50)
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        admin::list(app.db(), &q("DONE_OK"), None, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    // Tags match whole elements: user:4 is not user:42.
    let tag = JobFilter {
        tag: Some("user:4".into()),
        ..Default::default()
    };
    assert_eq!(
        admin::list(app.db(), &tag, None, 50).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn listing_pages_by_id_and_searches_ids_and_errors() {
    let app = TestApp::new().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(insert(app.db(), builder("ok")).await);
    }
    let failed = insert(app.db(), builder("fail")).await;
    drain(&app).await;
    let all = JobFilter::default();
    let first = admin::list(app.db(), &all, None, 3).await.unwrap();
    assert_eq!(first[0].id, failed, "newest first");
    let next = admin::list(app.db(), &all, Some(first[2].id), 3)
        .await
        .unwrap();
    assert!(next.iter().all(|j| j.id < first[2].id));
    let by_id = JobFilter {
        q: Some(ids[2].to_string()),
        ..Default::default()
    };
    assert_eq!(
        admin::list(app.db(), &by_id, None, 50).await.unwrap()[0].id,
        ids[2]
    );
    let by_error = JobFilter {
        error: Some("Connection REFUSED".into()),
        ..Default::default()
    };
    assert_eq!(
        admin::list(app.db(), &by_error, None, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    let counts = admin::state_counts(app.db(), &JobFilter::default())
        .await
        .unwrap();
    assert_eq!(counts[&JobState::Completed], 5);
    assert_eq!(counts[&JobState::Retryable], 1);
    assert_eq!(admin::workers(app.db()).await.unwrap(), ["fail", "ok"]);
}

#[tokio::test]
async fn stats_queues_metrics_and_error_groups() {
    let app = TestApp::new().await;
    for i in 0..4 {
        insert(app.db(), builder_with("ok", json!({"i": i}))).await;
    }
    for order in [7, 8, 9] {
        insert(
            app.db(),
            builder_with("fail", json!({"order": order})).max_attempts(1),
        )
        .await;
    }
    insert(app.db(), builder("ok").queue("mailers")).await;
    executor::drain(
        app.db(),
        &dispatcher(Log::default()),
        config,
        Some("default"),
        false,
    )
    .await
    .unwrap();
    admin::pause(app.db(), "mailers").await.unwrap();

    let stats = admin::stats(app.db()).await.unwrap();
    assert_eq!(stats.count(JobState::Completed), 4);
    assert_eq!(stats.count(JobState::Discarded), 3);
    assert_eq!(stats.count(JobState::Available), 1);
    assert_eq!(stats.by_state.len(), 7);
    assert_eq!(stats.total(), 8);

    let queues = admin::queues(app.db(), Utc::now()).await.unwrap();
    let mailers = queues.iter().find(|q| q.name == "mailers").unwrap();
    assert!(mailers.paused && mailers.orphaned());
    assert_eq!(mailers.available, 1);
    admin::resume(app.db(), "mailers").await.unwrap();
    assert_eq!(admin::pause_all(app.db()).await.unwrap(), 2);
    assert_eq!(admin::resume_all(app.db()).await.unwrap(), 2);

    let throughput = admin::throughput(app.db(), Utc::now(), 60).await.unwrap();
    assert!(throughput.wait_p95_ms.is_some() && throughput.wait_p50_ms.is_some());
    let buckets = throughput.buckets;
    assert_eq!(buckets.len(), 60);
    assert_eq!(buckets.iter().map(|b| b.completed).sum::<u64>(), 4);
    assert_eq!(buckets.iter().map(|b| b.failed).sum::<u64>(), 3);
    assert!(buckets.iter().any(|b| b.wait_p95_ms.is_some()));

    let workers = admin::worker_stats(app.db(), Utc::now(), chrono::Duration::hours(1))
        .await
        .unwrap();
    let ok = workers.iter().find(|w| w.worker == "ok").unwrap();
    assert_eq!((ok.completed, ok.failed), (4, 0));
    assert_eq!(ok.success_pct, Some(100.0));
    let fail = workers.iter().find(|w| w.worker == "fail").unwrap();
    assert_eq!(fail.success_pct, Some(0.0));

    let groups = admin::error_groups(app.db(), 1000).await.unwrap();
    assert_eq!(groups.len(), 1, "three orders, one error shape");
    assert_eq!(groups[0].count, 3);
    assert_eq!(groups[0].message, "connection refused");
    assert_eq!(groups[0].search.as_deref(), Some("connection refused"));
}

/// INVARIANT: the same root cause with different outer contexts is one group.
#[tokio::test]
async fn one_root_cause_is_one_group_with_its_variants() {
    let app = TestApp::new().await;
    for event in ["order.paid", "order.refunded", "customer.updated"] {
        insert(
            app.db(),
            builder_with("hook", json!({"event": event})).max_attempts(1),
        )
        .await;
    }
    // Another root cause stays its own group.
    insert(app.db(), builder("fail").max_attempts(1)).await;
    drain(&app).await;
    let groups = admin::error_groups(app.db(), 1000).await.unwrap();
    assert_eq!(groups.len(), 2);
    let hooks = groups.iter().find(|g| g.worker == "hook").unwrap();
    assert_eq!(hooks.count, 3);
    assert_eq!(hooks.root, "connection refused (os error N)");
    assert_eq!(hooks.host.as_deref(), Some("hooks.partner.invalid"));
    assert_eq!(
        hooks.message,
        "connection refused (os error N) · hooks.partner.invalid"
    );
    assert_eq!(hooks.variant_count, 3);
    let mut variants = hooks.variants.clone();
    variants.sort();
    assert_eq!(
        variants,
        ["customer.updated", "order.paid", "order.refunded"]
    );
    // The group's list filter matches all three jobs.
    let filter = JobFilter {
        worker: Some("hook".into()),
        error: hooks.search.clone(),
        ..Default::default()
    };
    assert_eq!(admin::count_matching(app.db(), &filter).await.unwrap(), 3);
}

// ---------------------------------------------------------------------------
// The app's workers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_registered_worker_dispatches() {
    let app = TestApp::new().await;
    for name in app_jobs::WORKERS {
        let info = offrails_host::kernel::jobs::JobInfo::synthetic(name, "default");
        let outcome = app_jobs::dispatch(
            app.state.clone(),
            (*name).to_owned(),
            json!(["not", "the", "args"]),
            info,
        )
        .await
        .unwrap();
        match outcome {
            Outcome::Discard(reason) => assert!(
                reason.starts_with("args don't deserialize"),
                "{name}: {reason}"
            ),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn unknown_workers_and_bad_args_are_discarded() {
    let app = TestApp::new().await;
    let unknown = insert(app.db(), builder("no_such_worker")).await;
    let bad = JobBuilder::new(
        JobDefaults {
            worker: ExportReportJob::NAME,
            queue: "default",
            max_attempts: 5,
            priority: 0,
            unique: None,
        },
        Ok(json!({"report_id": "not a number"})),
    )
    .insert(app.db())
    .await
    .unwrap()
    .id;
    let drained = app.drain_jobs().await;
    assert_eq!(drained.discarded, 2);
    assert_eq!(
        error_texts(&get(app.db(), unknown).await),
        ["unknown worker: no_such_worker"]
    );
    assert!(error_texts(&get(app.db(), bad).await)[0].starts_with("args don't deserialize"));
}

#[tokio::test]
async fn the_example_workers_behave_as_documented() {
    use offrails_host::jobs::examples::{DeliverWebhookJob, ReportBatchDoneJob};
    let app = TestApp::new().await;
    assert_eq!(
        app.perform_job(ExportReportJob {
            report_id: 1,
            rows: 10
        })
        .await
        .unwrap(),
        Outcome::Done
    );
    assert!(matches!(
        app.perform_job(ExportReportJob {
            report_id: 0,
            rows: 10
        })
        .await
        .unwrap(),
        Outcome::Discard(_)
    ));
    let hook = |endpoint: &str| DeliverWebhookJob {
        endpoint: endpoint.into(),
        event: "order.paid".into(),
    };
    assert_eq!(
        app.perform_job(hook("https://hooks.example.com/x"))
            .await
            .unwrap(),
        Outcome::Done
    );
    assert!(matches!(
        app.perform_job(hook("https://busy.example.com/x"))
            .await
            .unwrap(),
        Outcome::Snooze(_)
    ));
    assert!(matches!(
        app.perform_job(hook("http://hooks.example.com/x"))
            .await
            .unwrap(),
        Outcome::Discard(_)
    ));
    let err = app
        .perform_job(hook("https://down.invalid/x"))
        .await
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "POST https://down.invalid/x (order.paid): connecting to down.invalid:443: connection refused (os error 61)"
    );
    assert_eq!(
        app.perform_job(ReportBatchDoneJob { report_id: 1 })
            .await
            .unwrap(),
        Outcome::Done
    );

    // Enqueued through the app's API, then drained through the real path.
    app_jobs::enqueue(app.db(), hook("https://hooks.example.com/y"))
        .await
        .unwrap();
    app.assert_enqueued::<DeliverWebhookJob>(|w| w.endpoint.ends_with("/y"))
        .await;
    // Unique per minute: the same event again is a duplicate.
    let dup = app_jobs::enqueue(app.db(), hook("https://hooks.example.com/y"))
        .await
        .unwrap();
    assert!(dup.duplicate);
    assert_eq!(app.drain_queue("webhooks").await.completed, 1);
    app.refute_enqueued::<DeliverWebhookJob>().await;
}
