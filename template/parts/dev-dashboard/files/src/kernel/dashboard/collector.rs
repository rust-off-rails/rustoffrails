//! The collector: once a second, process memory and CPU (sysinfo), open file descriptors, the
//! Tokio runtime, the database pool, and the second's requests and SQL, into the rings.
//!
//! Tokio's stable metrics are used (workers, alive tasks, global queue depth, busy time). APP: build
//! with `RUSTFLAGS="--cfg tokio_unstable"` to also see spawned tasks and blocking threads (the
//! `unstable` fields below).
// WHY: tokio_unstable is a cfg the compiler doesn't know about; without this every build warns.
#![allow(unexpected_cfgs)]

use std::time::{Duration, Instant};

use chrono::Utc;
use sea_orm::{DatabaseConnection, DbBackend};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use super::series::{Annotation, LATENCY_BUCKETS, Sample};
use super::{HUB, Tick, lock};
use crate::state::AppState;

/// How often a sample is taken.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// Starts the once-a-second loop (`serve` calls it through `serve_started`).
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut c = Collector::new();
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let s = c.sample(&state.db);
            c.commit(s);
        }
    });
}

/// Pool numbers: (size, idle, max).
pub fn pool(db: &DatabaseConnection) -> (u32, u32, u32) {
    match db.get_database_backend() {
        DbBackend::Sqlite => {
            let p = db.get_sqlite_connection_pool();
            (
                p.size(),
                p.num_idle() as u32,
                p.options().get_max_connections(),
            )
        }
        DbBackend::Postgres => {
            let p = db.get_postgres_connection_pool();
            (
                p.size(),
                p.num_idle() as u32,
                p.options().get_max_connections(),
            )
        }
        _ => (0, 0, 0),
    }
}

/// Tokio's unstable metrics, when the app is built with `--cfg tokio_unstable`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unstable {
    pub spawned_tasks: u64,
    pub blocking_threads: usize,
    pub blocking_queue: usize,
}

#[cfg(tokio_unstable)]
pub fn unstable() -> Option<Unstable> {
    let m = tokio::runtime::Handle::try_current().ok()?.metrics();
    Some(Unstable {
        spawned_tasks: m.spawned_tasks_count(),
        blocking_threads: m.num_blocking_threads(),
        blocking_queue: m.blocking_queue_depth(),
    })
}

#[cfg(not(tokio_unstable))]
pub fn unstable() -> Option<Unstable> {
    None
}

/// Server errors per second that mark an error burst on the charts.
pub const ERROR_BURST: f32 = 3.0; // APP: your app's "something is on fire" rate

pub struct Collector {
    burst: bool,
    // None when sysinfo can't read this platform: the process figures stay at zero.
    sys: Option<System>,
    pid: Option<Pid>,
    busy: Duration,
    at: Instant,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector {
    pub fn new() -> Self {
        let mut c = Self {
            burst: false,
            sys: System::new().ok(),
            pid: sysinfo::get_current_pid().ok(),
            busy: Duration::ZERO,
            at: Instant::now(),
        };
        // The first CPU reading needs a previous one.
        c.refresh();
        c.busy = runtime_busy();
        c
    }

    fn refresh(&mut self) {
        if let (Some(pid), Some(sys)) = (self.pid, self.sys.as_mut()) {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory().with_cpu(),
            );
        }
    }

    /// One sample: the process and runtime now, plus the requests and SQL since the last one.
    pub fn sample(&mut self, db: &DatabaseConnection) -> (Sample, Tick) {
        self.refresh();
        let process = self.pid.zip(self.sys.as_ref()).and_then(|(pid, sys)| sys.process(pid));
        let elapsed = self.at.elapsed().max(Duration::from_millis(1));
        self.at = Instant::now();
        let (workers, tasks, queue) = tokio::runtime::Handle::try_current()
            .map(|h| {
                let m = h.metrics();
                (m.num_workers(), m.num_alive_tasks(), m.global_queue_depth())
            })
            .unwrap_or((0, 0, 0));
        let busy_now = runtime_busy();
        let busy = (busy_now.saturating_sub(self.busy)).as_secs_f64()
            / (elapsed.as_secs_f64() * workers.max(1) as f64)
            * 100.0;
        self.busy = busy_now;
        let (pool_size, pool_idle, pool_max) = pool(db);
        let tick = std::mem::take(&mut *lock(&HUB.tick));
        let secs = elapsed.as_secs_f64() as f32;
        let sample = Sample {
            t: Utc::now().timestamp(),
            rss: process.map_or(0, sysinfo::Process::memory),
            virt: process.map_or(0, sysinfo::Process::virtual_memory),
            cpu: process.map_or(0.0, sysinfo::Process::cpu_usage),
            fds: process.and_then(|p| p.open_files()).map(|n| n as u32),
            workers: workers as u32,
            tasks: tasks as u32,
            queue: queue as u32,
            busy: busy.clamp(0.0, 100.0) as f32,
            pool_size,
            pool_idle,
            pool_max,
            req: tick.requests as f32 / secs,
            e4: tick.e4 as f32 / secs,
            e5: tick.e5 as f32 / secs,
            p50: tick.latency.quantile(0.50) as u32,
            p95: tick.latency.quantile(0.95) as u32,
            p99: tick.latency.quantile(0.99) as u32,
            sql: tick.statements as f32 / secs,
            sql_p95: tick.sql_latency.quantile(0.95) as u32,
            lat: tick.latency.bucket_counts(&LATENCY_BUCKETS),
        };
        (sample, tick)
    }

    /// Adds a sample to the rings.
    pub fn commit(&mut self, (sample, tick): (Sample, Tick)) {
        if HUB.boot_rss.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            HUB.boot_rss
                .store(sample.rss, std::sync::atomic::Ordering::Relaxed);
        }
        let mut series = lock(&HUB.series);
        let burst = sample.e5 >= ERROR_BURST;
        if burst && !self.burst {
            let a = Annotation {
                t: sample.t,
                kind: "errors",
                label: format!("error burst ({:.0} server errors/s)", sample.e5),
                demo: false,
            };
            super::history::record_mark(&a);
            series.annotate(a);
        }
        self.burst = burst;
        if let Some(minute) = series.push(sample, &tick.latency, &tick.sql_latency) {
            // Off the lock's critical path: a channel send, the history thread writes it.
            super::history::record_minute(minute, false);
        }
    }
}

/// Total time Tokio's workers have been busy.
fn runtime_busy() -> Duration {
    tokio::runtime::Handle::try_current()
        .map(|h| {
            let m = h.metrics();
            (0..m.num_workers())
                .map(|w| m.worker_total_busy_duration(w))
                .sum()
        })
        .unwrap_or_default()
}
