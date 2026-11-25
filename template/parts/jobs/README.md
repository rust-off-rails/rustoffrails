# jobs

A database-backed job queue for Rust off Rails apps, in the spirit of Elixir's **Oban**: jobs are rows in your own database (SQLite or Postgres), with an explicit state machine, typed arguments, retries, uniqueness, cron, rate limits and batches, and nothing else to run: no Redis, no broker. It replaces Rails' Active Job + Solid Queue (or Sidekiq) and Phoenix's Oban.

- **Typed jobs.** A worker is a `Serialize + Deserialize` struct with `impl Worker`; args are stored as JSON and decoded back into the struct. Args that don't decode are discarded with the reason, never run with guessed values.
- **Correct claiming on both backends.** Postgres claims with `SELECT … FOR UPDATE SKIP LOCKED` and an `UPDATE … RETURNING` in one transaction; SQLite claims inside `BEGIN IMMEDIATE`, under its single write lock. Two workers never run the same job (tested with four drains, six racing claimers, two whole runtimes, and a pooled SQLite file).
- **Retries** with exponential backoff and jitter (`15s × 2^n`, capped at 24 h, +0-25%), per-worker `MAX_ATTEMPTS` and `backoff()`, and every attempt's error kept on the row: the `{e:#}` chain, the full `{e:?}` report (backtrace when captured), the node and the duration.
- **Outcomes** for everything that isn't a plain retry: `Snooze` (not yet, gives the attempt back), `Cancel` (the work no longer matters), `Discard` (retrying can't help).
- **Scheduling.** `schedule_in`/`schedule_at`, priorities 0-9, unique jobs (by all args, some args, or the worker; optionally per time bucket), and **cron** in UTC with a five-field syntax, run once per due time across every node.
- **Queues.** Named queues with a per-node concurrency limit, an optional global **rate limit** (`webhooks:3:30/1m`: at most 30 starts a minute across all nodes), pause and resume on every node, and a pause-all switch for deploys.
- **Batches.** `Batch::new("import 42").on_complete(callback).insert(&txn, jobs)`: the callback runs exactly once when every job is final, with the counts in its meta. `insert_all` fans out thousands of jobs in a few statements.
- **Progress.** `job.progress(40).await` from a worker; the dashboard sample shows it live.
- **Safety.** Per-job timeouts, panics caught as errors, heartbeats every second, orphaned jobs of a dead node rescued (Lifeline), graceful shutdown that waits up to a grace period and then hands unfinished jobs straight back to the queue, and a Pruner for finished jobs.
- **Tooling.** An inline worker inside `app serve` for development, `app worker` for production, and `app jobs stats|list|show|errors|retry|cancel|discard|delete|run|pause|resume|cron|prune`, with `--json` everywhere and secret-looking args redacted.
- **Tests.** 60+ tests on SQLite and Postgres: every outcome, every plugin, concurrency, crash recovery, rate limits, batches, injection, redaction, the CLI and the runtime.

Extracted from Rust off Rails apps and rebuilt. Fixed on the way: a claim that could take more rows than asked on Postgres (the `UPDATE … WHERE id IN (SELECT … SKIP LOCKED LIMIT n)` pattern re-runs its subquery; caught by a test here), wake-ups lost while a producer was busy (`Notify::notify_waiters` → a `watch` channel), an abort handle registered after its task could finish, shutdown that left jobs `executing` for a minute, cron history found by string-matching JSON (now a `job_id` column), and node names that could reveal hosts (now synthetic). Added: rate limits, batches, progress, error details per attempt, discard/pause-all actions, error grouping by root cause and the CLI's `errors`/`cron` commands.

Guide: [`.offrails/guides/jobs.md`](https://rustoffrails.org/docs/guides/jobs) · [`.offrails/guides/jobs-dashboard.md`](https://rustoffrails.org/docs/guides/jobs-dashboard) (the CLI). Depends on: nothing. The [jobs-dashboard](../jobs-dashboard/) sample adds `/admin/jobs`.

## Screens

The queue has no UI of its own; see [jobs-dashboard](../jobs-dashboard/README.md#screens). In a terminal:

```
$ app jobs stats
STATE      JOBS
executing  3
available  9
…
QUEUE     STATUS    LIMIT  RATE   NODES  EXECUTING  AVAILABLE  SCHEDULED  RETRYABLE  DISCARDED
default   running   10     -      1      3          0          4          4          8
webhooks  running   3      30/1m  1      0          0          0          1          16
```

## Install

By hand, in the app root:

1. `cargo add croner`.
2. Copy `files/` into the app, keeping the paths. In `tests/app/` files and the `main.rs` wiring, replace `offrails_host` with your crate name.
3. Wiring (each at its `samples: <anchor>` comment; the exact text is in `sample.toml`):
   - `src/kernel/mod.rs`: `pub mod jobs;` · `src/lib.rs`: `pub mod jobs;`.
   - `migration/src/lib.rs`: `mod m20261016_000200_create_jobs;` and `Box::new(m20261016_000200_create_jobs::Migration),` in `migrations()`.
   - `src/config.rs`: a `jobs: JobsConfig` field on `Config` (field, `Debug` line, `let jobs = JobsConfig::from_env(env)?;`, `jobs,` in the constructor, `jobs: JobsConfig::test()` in `Config::test()`), and the `JobsConfig` struct appended at `config sections`.
   - `src/main.rs`: the `Worker { queues }` and `Jobs { command }` subcommands and their dispatch arms; in `serve`, `let inline_worker = …::jobs::start_inline_worker(&state);` before binding and `if let Some(worker) = inline_worker { worker.stop().await; }` in the shutdown future.
   - `.env.example`: the `APP_QUEUES`, `APP_INLINE_WORKER`, `APP_JOBS_*` lines.
   - `tests/app/main.rs`: `mod jobs_test;` · `tests/app/support.rs`: the `drain_jobs`, `drain_queue`, `drain_all_jobs`, `enqueued`, `assert_enqueued`, `refute_enqueued`, `perform_job` methods on `TestApp`.
4. `cargo fmt --all && cargo run -- db migrate && cargo run -- routes`, then the Definition of Done.

`tools/install.sh jobs <app>` does steps 1 to 3.

## After installing

- Set `APP_QUEUES` (and `APP_JOBS_SHUTDOWN_GRACE`, below the platform's kill timeout) in the deploy config, and run `app worker` as a process next to `app serve`, or `APP_INLINE_WORKER=true` on a single server.
- Add a line to `docs/decisions.md` if you change the queues or the defaults.
- Write your first worker (below), add it to `dispatch`, `worker_config` and `WORKERS` in `src/jobs/mod.rs`, then delete `src/jobs/examples.rs` and its entries when you no longer need the reference.
- Agents: when "X never happened", run `cargo run -q -- jobs list --state discarded --json` and `cargo run -q -- jobs show <id>` before reading code.

## Adapting it

- **Add a worker.** A file in `src/jobs/` with a struct (ids, never records or secrets) and `impl Worker` (`NAME` is permanent; set `QUEUE`, `MAX_ATTEMPTS`, `TIMEOUT` to the work), then one arm in `dispatch`, `worker_config` and `WORKERS` (`// APP:` in `src/jobs/mod.rs`). A sample that ships a worker (webhooks does) registers it at the `worker mods`, `worker dispatch`, `worker config` and `worker names` anchors in the same file, and a recurring entry at `recurring` in `src/jobs/recurring.rs`. Enqueue inside the transaction that creates its data: `jobs::enqueue(&txn, SendReceiptJob { order_id }).await?`. Test it with `app.perform_job(…)` and the flow with `app.drain_jobs()`.
- **Add a queue or change limits.** `APP_QUEUES=default:10,mailers:5` per process (`JobsConfig::DEFAULT_QUEUES` is the fallback, `// APP:`); `app worker --queues reports:2` for a dedicated process.
- **Rate-limit a queue.** Add `:<count>/<period>`: `APP_QUEUES=webhooks:3:30/1m` starts at most 30 jobs a minute across every node, 3 at a time per node.
- **Add a cron entry.** A `Recurring { key, cron, job }` in `SCHEDULE` (`src/jobs/recurring.rs`, `// APP:`). `key` is permanent; `cron` is five fields in UTC; `recurring::tests` fails on a bad expression or an unregistered worker.
- **Run a batch.** `Batch::new("import 42").on_complete(ImportDoneJob { import_id }.into_job()).insert(&txn, rows.map(|r| ImportRowJob { row_id: r.id }.into_job()).collect())`; the callback reads `job.meta["batch"]`.
- **Redact more arg names.** `SENSITIVE_KEYS` in `src/kernel/jobs/mod.rs` (`// APP:`). Better still, keep secrets out of args: pass an id and load or mint the secret in `perform`.

## Do not change

- The claim's select-then-update in one transaction (`concurrent_claims_run_each_job_exactly_once`, `racing_claimers_get_disjoint_jobs`, `rate_limited_queues_start_at_most_n_per_period`).
- Every result update filtered by `state = 'executing' AND attempted_by = node` (`a_result_never_resurrects_a_cancelled_job`).
- Unique inserts through `on_conflict_do_nothing_on`, never a caught error (`unique_jobs_insert_once_even_concurrently`).
- The `(key, run_at)` unique index and the transaction around a cron run (`cron_fires_once_across_concurrent_ticks`).
- The guarded `finished_at IS NULL` update before a batch callback (`a_batch_enqueues_its_callback_once_when_every_job_is_final`).
- Shutdown handing unfinished jobs back (`shutdown_returns_unfinished_jobs_to_the_queue`), Lifeline rescuing only dead nodes (`the_lifeline_rescues_only_dead_nodes_jobs`), the Pruner touching only old final jobs (`the_pruner_keeps_recent_and_unfinished_jobs`).
- `Utc::now()` from Rust, never the database clock; `kernel::db::begin` (`BEGIN IMMEDIATE`) for every transaction.
- Synthetic node names (`node_ids_are_synthetic_and_unique`) and redaction in every printed view (`sensitive_args_are_redacted_in_the_cli_and_admin_views`).

## How it works

A job is a row in `jobs` with a state: `scheduled` or `available` when inserted, `executing` while a node runs it, then `completed`, `retryable` (back to `available` when its backoff passes), `discarded`, `cancelled`, or `scheduled` again after a snooze. Final rows stay until the Pruner deletes them (`APP_JOBS_PRUNE_DAYS`, default 7), so the dashboard and the CLI have history.

`kernel::jobs::run` starts one **producer per queue** (a semaphore of the queue's limit) and the **plugins**, each a plain `async fn(db, now)` that tests call directly: Heartbeat (1 s: registers the node, refreshes paused queues, aborts jobs cancelled elsewhere), Stager (1 s: due `scheduled`/`retryable` → `available`), Cron (1 s), Lifeline (30 s: rescues jobs of nodes silent for 60 s) and Pruner (60 s). Every node runs every plugin; they're idempotent, so there's no leader election. Producers wake on a local insert, a finished job, or a jittered 1 s poll.

The kernel (`src/kernel/jobs/`) never names an app type: the app hands it a `DispatchFn` (a `match` on worker names that decodes the args into the struct) and a `WorkerConfigFn` (timeouts and backoff), both in `src/jobs/mod.rs`.

| Command | Does |
|---------|------|
| `app worker [--queues default:10,mailers:5]` | Runs the worker and cron until SIGTERM, then shuts down gracefully |
| `app serve` with `APP_INLINE_WORKER=true` | The same inside the web process (development default) |
| `app jobs stats \| list \| show \| errors \| cron` | Read the queue (`--json` for scripts and agents) |
| `app jobs retry \| cancel \| discard \| delete \| run <ids> \| --all <filters> [--yes]` | Act on jobs; `--all` needs a filter and asks first |
| `app jobs pause \| resume <queue> \| --all` | Pause or resume queues on every node |
| `app jobs cron --run <key>` · `app jobs prune [--days N]` | Run a cron entry now; prune once |

| Env var | Default | Meaning |
|---------|---------|---------|
| `APP_QUEUES` | `default:10,mailers:5,webhooks:3:30/1m` | `name:limit[:rate]` per node |
| `APP_INLINE_WORKER` | `true` in development | Run the worker inside `app serve` |
| `APP_JOBS_PRUNE_DAYS` | `7` | Days finished jobs are kept |
| `APP_JOBS_SHUTDOWN_GRACE` | `25` (`3` in development) | Seconds in-flight jobs get on SIGTERM |
| `APP_JOBS_NODE` | `node-<random hex>` | This process's name on the dashboard; never the hostname |

## Files

- `migration/src/m20261016_000200_create_jobs.rs`: `jobs`, `job_queues`, `job_nodes`, `recurring_executions`, `job_batches`, with a `CHECK` on states and 64-bit ids.
- `src/kernel/jobs/mod.rs`: `Outcome`, `JobInfo` (+ `progress`), `WorkerConfig`, backoff, `QueueSpec`/`Rate`, node ids, wake-ups, redaction.
- `src/kernel/jobs/entities.rs`: the SeaORM entities and `JobState`.
- `src/kernel/jobs/builder.rs`: `JobBuilder`, `Unique`, `insert`, `insert_all`, `Batch`.
- `src/kernel/jobs/executor.rs`: `claim`, `claim_rate_limited`, `execute`, `record`, `drain`, `run` and the producers.
- `src/kernel/jobs/plugins.rs`: Heartbeat, Stager, Lifeline, shutdown hand-back, Pruner, batch completion, Cron.
- `src/kernel/jobs/admin.rs`: stats, queues, nodes, throughput and latency, worker stats, error groups, filters and search, every action.
- `src/jobs/mod.rs`: the `Worker` trait, `enqueue`, `dispatch`, `worker_config`, `worker_options`, `run_worker`, the inline worker.
- `src/jobs/recurring.rs`: `SCHEDULE` and the cron syntax.
- `src/jobs/cleanup.rs`: hourly housekeeping.
- `src/jobs/examples.rs`: example workers (progress, every outcome, a rate-limited queue, a batch callback); delete when done.
- `src/jobs/cli.rs`: `app jobs …`.
- `tests/app/jobs_test.rs` (+ `jobs_test/runtime.rs`, `jobs_test/cli.rs`): the tests.
