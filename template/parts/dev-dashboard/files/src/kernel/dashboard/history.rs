//! The dashboard's history on disk: every closed minute of the charts, an hourly rollup of them,
//! the markers (boots, deploys, load-lab runs, error bursts) and the notable traces (5xx, N+1,
//! slow), in one SQLite file, compressed. It survives restarts, and memory keeps only the hot
//! window (`series.rs`: 15 minutes at 1 s, two hours at 1 min; `trace.rs`: the latest traces).
//!
//! | Kept | Where | How long (`// APP:` below) |
//! |------|-------|------------------|
//! | Minutes (with their latency histograms) | `chunks`, level 0: one row per hour | `MINUTE_DAYS` |
//! | Hours | `chunks`, level 1: one row per day | `HOUR_DAYS` |
//! | Markers | `marks` | `HOUR_DAYS` |
//! | Notable traces (and every job or mail run), redacted as recorded | `traces`: one row each, the trace compressed | `TRACE_DAYS`, `MAX_TRACES` |
//!
//! The whole file stays under `MAX_BYTES`: past it the oldest minutes, then the oldest traces, go.
//!
//! WHY its own SQLite file and not the app's database: the app's migrations stay the app's, it
//! works the same for a Postgres app, a `db reset` doesn't wipe it, and the file is one thing to
//! delete. SeaORM's SQLite driver (already in the app) opens it; no new database crate.
//!
//! WHY one row per series-hour with a compressed blob, not one row per sample: a minute is ~20
//! numbers that barely change from one minute to the next. Stored column by column as deltas
//! (varints), then deflated (flate2, pure Rust, already in the dependency tree through tower-http),
//! an hour of minutes, with both histograms, is a few KB, and a chart's 24 hours are 24 reads.
//!
//! WHY a thread of its own: writes are batched and off the request path (the collector and the
//! request middleware only push onto a channel), and reads answer the synchronous callers the
//! charts, insights and the AI tools already are; a read is a few milliseconds on a page that only
//! the developer opens. A crash loses at most the minute in progress and the last few seconds of
//! traces (`FLUSH_EVERY`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult,
    Statement, Value,
};
use serde::{Deserialize, Serialize};

use super::hist::Hist;
use super::series::{Annotation, LATENCY_BUCKETS, Minute, Range, Sample, rollup};
use super::trace::{EventRec, SpanRec, SqlRec, TRACE_KINDS, Trace};
use super::{HUB, lock};

/// Where the file lives, relative to the app's directory. APP: `DASHBOARD_HISTORY_PATH` moves it
/// (a mounted volume in production); `DASHBOARD_HISTORY=false` turns it off.
pub const PATH: &str = "storage/dashboard/history.sqlite3";
/// Per-minute history kept, in days (the 7 days chart reads it). APP: more days, a bigger file
/// (about 100 KB a day).
pub const MINUTE_DAYS: i64 = 7;
/// Hourly rollups kept, in days (the 30 days chart reads them). APP.
pub const HOUR_DAYS: i64 = 90;
/// Notable traces kept: this many days, at most this many. APP.
pub const TRACE_DAYS: i64 = 14;
pub const MAX_TRACES: i64 = 5_000;
/// The file's ceiling. APP: past it the oldest minutes, then the oldest traces, are pruned.
pub const MAX_BYTES: i64 = 64 * 1024 * 1024;
/// Traces are written in batches at least this often (what a crash can lose).
pub const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Pruning runs at open and then this often.
const PRUNE_EVERY: Duration = Duration::from_secs(3_600);
/// The file's layout. A file with another version (or none) is moved aside, never migrated: it is
/// a cache of history, not the app's data.
const SCHEMA_VERSION: &str = "1";
/// The blobs' own format byte (`encode_chunk`).
const BLOB_VERSION: u8 = 1;

/// Chunk levels.
const MINUTES: i64 = 0;
const HOURS: i64 = 1;

enum Msg {
    Minute(Box<Minute>, bool),
    Mark(Annotation),
    Trace(Arc<Trace>),
    Query(Query, Sender<Reply>),
    Flush(Sender<()>),
    Prune(i64, i64, Sender<()>),
}

enum Query {
    Minutes(i64, i64),
    Hours(i64, i64),
    Marks(i64, i64),
    Trace(u64),
    Traces(TraceFilter),
    Stats,
}

enum Reply {
    Samples(Vec<Sample>, bool),
    Marks(Vec<Annotation>),
    Traces(Vec<Arc<Trace>>),
    Stats(Stats),
}

/// What the Config page says about the file.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub path: String,
    pub bytes: i64,
    pub minute_chunks: i64,
    pub hour_chunks: i64,
    pub traces: i64,
    pub marks: i64,
    pub oldest: Option<i64>,
}

/// Which persisted traces a list wants (all optional; newest first, at most `limit`).
#[derive(Clone, Debug, Default)]
pub struct TraceFilter {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub route: Option<String>,
    pub kind: Option<String>,
    /// `2xx`, `3xx`, `4xx`, `5xx`.
    pub status: Option<String>,
    pub min_us: Option<u64>,
    pub n_plus_one: bool,
    pub limit: usize,
    /// Leave out these ids (the ones memory already has).
    pub exclude: Vec<u64>,
    /// A job run's id (`job-<id>`, its trace's `request_id`).
    pub request_id: Option<String>,
}

struct Handle {
    tx: Sender<Msg>,
    writable: bool,
}

static STORE: Mutex<Option<Handle>> = Mutex::new(None);
/// Production keeps timings and normalized SQL only (`strip`).
static STRIP_DETAILS: AtomicBool = AtomicBool::new(false);

fn sender() -> Option<Sender<Msg>> {
    lock(&STORE).as_ref().map(|h| h.tx.clone())
}

fn writable_sender() -> Option<Sender<Msg>> {
    lock(&STORE)
        .as_ref()
        .filter(|h| h.writable)
        .map(|h| h.tx.clone())
}

pub fn is_open() -> bool {
    lock(&STORE).is_some()
}

/// Opens (or creates) the history file and starts its thread. `writable: false` reads a file
/// another process writes (a production view of a mounted volume). `strip_details` keeps log
/// events and span fields out of the file (production: the read-only view never shows them, so
/// they are never written either). The latest notable traces on disk refill memory's ring, and
/// trace ids continue after the file's.
pub fn open(path: &Path, writable: bool, strip_details: bool) -> anyhow::Result<()> {
    close();
    STRIP_DETAILS.store(strip_details, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel::<Msg>();
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<(u64, Vec<Arc<Trace>>)>>();
    let owned = path.to_path_buf();
    std::thread::Builder::new()
        .name("dashboard-history".into())
        .spawn(move || run(owned, writable, rx, ready_tx))?;
    let (max_id, traces) = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| anyhow::anyhow!("the history file didn't open in time"))??;
    // Trace ids keep counting from the last run's, so a permalink never points at two traces.
    HUB.ids_after(max_id);
    {
        let mut ring = lock(&HUB.traces);
        for t in traces.into_iter().rev() {
            ring.push(t);
        }
    }
    *lock(&STORE) = Some(Handle { tx, writable });
    Ok(())
}

/// Writes what is pending and stops the thread (`serve stop`, tests).
pub fn close() {
    let handle = lock(&STORE).take();
    if let Some(h) = handle {
        let (tx, rx) = mpsc::channel();
        if h.tx.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }
}

/// Waits until everything sent so far is on disk.
pub fn flush() {
    if let Some(tx) = sender() {
        let (done, wait) = mpsc::channel();
        if tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(10));
        }
    }
}

/// Prunes as if it were `now` (tests move the clock; the thread prunes hourly by itself).
pub fn prune_at(now: i64) {
    prune_to(now, MAX_BYTES);
}

/// Prunes as if it were `now` with a ceiling of `max_bytes` (tests: the size cap).
pub fn prune_to(now: i64, max_bytes: i64) {
    if let Some(tx) = writable_sender() {
        let (done, wait) = mpsc::channel();
        if tx.send(Msg::Prune(now, max_bytes, done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(30));
        }
    }
}

/// A closed minute (the collector, the demo history). Never blocks.
pub fn record_minute(m: Minute, demo: bool) {
    if let Some(tx) = writable_sender() {
        let _ = tx.send(Msg::Minute(Box::new(m), demo));
    }
}

/// A chart marker, kept as long as the hours.
pub fn record_mark(a: &Annotation) {
    if let Some(tx) = writable_sender() {
        let _ = tx.send(Msg::Mark(a.clone()));
    }
}

/// A finished trace: notable requests and every job or mail run are written (batched; a job's
/// page finds its run's trace later); the rest stay in memory only.
pub fn record_trace(t: &Arc<Trace>) {
    if (t.kind == "request" && !t.notable()) || t.from_disk {
        return;
    }
    if let Some(tx) = writable_sender() {
        let _ = tx.send(Msg::Trace(t.clone()));
    }
}

fn ask(q: Query) -> Option<Reply> {
    let tx = sender()?;
    let (reply, wait) = mpsc::channel();
    tx.send(Msg::Query(q, reply)).ok()?;
    wait.recv_timeout(Duration::from_secs(10)).ok()
}

/// A persisted trace by id (a permalink to a trace older than memory's).
pub fn trace(id: u64) -> Option<Arc<Trace>> {
    match ask(Query::Trace(id))? {
        Reply::Traces(mut v) => v.pop(),
        _ => None,
    }
}

/// Persisted traces matching `f`, newest first.
pub fn traces(f: TraceFilter) -> Vec<Arc<Trace>> {
    match ask(Query::Traces(f)) {
        Some(Reply::Traces(v)) => v,
        _ => Vec::new(),
    }
}

pub fn stats() -> Option<Stats> {
    match ask(Query::Stats)? {
        Reply::Stats(s) => Some(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a window: memory first, the disk for the rest
// ---------------------------------------------------------------------------------------------

/// A time window's samples, markers and whether any of it is demo data.
pub struct Slice {
    pub samples: Vec<Sample>,
    /// Seconds per sample (1, 60 or 3,600).
    pub step: i64,
    pub marks: Vec<Annotation>,
    pub demo: bool,
}

/// The samples from `from` to `to`: at 1 s when memory's seconds cover them, at 1 min when the
/// window fits the minutes kept (memory's, then the disk's), else at 1 h. Memory wins where both
/// have a minute, so the hot/cold boundary has no seam.
pub fn between(from: i64, to: i64) -> Slice {
    between_at(from, to, Utc::now().timestamp())
}

fn between_at(from: i64, to: i64, now: i64) -> Slice {
    let (mem, step, oldest, mem_marks, demo_until) = {
        let s = lock(&HUB.series);
        let (samples, step) = s.between(from, to);
        (
            samples,
            step,
            s.oldest_minute(),
            s.annotations(from, to),
            s.demo_until,
        )
    };
    let mut demo = demo_until.is_some_and(|d| d > from);
    let disk_needed = step != 1 && oldest.is_none_or(|o| o > from + 60);
    let use_hours = to - from > 8 * 86_400 || from < now - MINUTE_DAYS * 86_400;
    let (samples, step) = if !disk_needed {
        (mem, step)
    } else if use_hours {
        let (mut disk, d) = match ask(Query::Hours(from, to)) {
            Some(Reply::Samples(v, d)) => (v, d),
            _ => (Vec::new(), false),
        };
        demo |= d;
        disk.retain(|s| s.t > from && s.t <= to);
        (disk, 3_600)
    } else {
        let (disk, d) = match ask(Query::Minutes(from, to)) {
            Some(Reply::Samples(v, d)) => (v, d),
            _ => (Vec::new(), false),
        };
        demo |= d;
        let mut by_t: BTreeMap<i64, Sample> = disk
            .into_iter()
            .filter(|s| s.t > from && s.t <= to)
            .map(|s| (s.t, s))
            .collect();
        for s in mem {
            by_t.insert(s.t, s);
        }
        (by_t.into_values().collect(), 60)
    };
    let marks = if is_open() && oldest.is_none_or(|o| o > from) {
        let disk = match ask(Query::Marks(from, to)) {
            Some(Reply::Marks(v)) => v,
            _ => Vec::new(),
        };
        merge_marks(disk, mem_marks)
    } else {
        mem_marks
    };
    Slice {
        samples,
        step,
        marks,
        demo,
    }
}

fn merge_marks(mut a: Vec<Annotation>, b: Vec<Annotation>) -> Vec<Annotation> {
    let seen: HashSet<(i64, String)> = a.iter().map(|m| (m.t, m.label.clone())).collect();
    a.extend(
        b.into_iter()
            .filter(|m| !seen.contains(&(m.t, m.label.clone()))),
    );
    a.sort_by_key(|m| m.t);
    a
}

/// The last `range`, ending `now`.
pub fn window(range: Range, now: i64) -> Slice {
    match range {
        // Memory holds these whole: no read, ever (they redraw every second).
        Range::M5 | Range::M15 => {
            let s = lock(&HUB.series);
            let from = now - range.seconds();
            Slice {
                samples: s.window(range, now),
                step: range.step(),
                marks: s.annotations(from, now),
                demo: s.demo_until.is_some_and(|d| d > from),
            }
        }
        _ => between_at(now - range.seconds(), now, now),
    }
}

// ---------------------------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------------------------

fn run(
    path: PathBuf,
    writable: bool,
    rx: Receiver<Msg>,
    ready: Sender<anyhow::Result<(u64, Vec<Arc<Trace>>)>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    let db = match rt.block_on(connect_or_reset(&path, writable)) {
        Ok(db) => db,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let store = Store { db, path };
    let now = Utc::now().timestamp();
    if writable {
        rt.block_on(store.prune(now, MAX_BYTES));
    }
    let boot = rt.block_on(async {
        let max: u64 = store.max_trace_id().await.unwrap_or(0);
        // The latest notable requests refill memory's notable ring (job runs stay on disk).
        let recent: Vec<Arc<Trace>> = store
            .traces(&TraceFilter {
                kind: Some("request".to_owned()),
                limit: super::trace::NOTABLE * 2,
                ..TraceFilter::default()
            })
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.notable())
            .take(super::trace::NOTABLE)
            .collect();
        (max, recent)
    });
    let _ = ready.send(Ok(boot));
    let mut pending: Vec<Arc<Trace>> = Vec::new();
    let mut last_flush = Instant::now();
    let mut last_prune = Instant::now();
    loop {
        let msg = rx.recv_timeout(FLUSH_EVERY);
        let disconnected = matches!(msg, Err(RecvTimeoutError::Disconnected));
        match msg {
            Ok(Msg::Minute(m, demo)) => {
                rt.block_on(store.write_minute(&m, demo));
            }
            Ok(Msg::Mark(a)) => {
                rt.block_on(store.write_mark(&a));
            }
            Ok(Msg::Trace(t)) => pending.push(t),
            Ok(Msg::Query(q, reply)) => {
                // Pending traces first, so a read sees every write sent before it.
                if !pending.is_empty() {
                    rt.block_on(store.write_traces(std::mem::take(&mut pending)));
                    last_flush = Instant::now();
                }
                let _ = reply.send(rt.block_on(store.answer(q)));
            }
            Ok(Msg::Flush(done)) => {
                rt.block_on(store.write_traces(std::mem::take(&mut pending)));
                last_flush = Instant::now();
                let _ = done.send(());
            }
            Ok(Msg::Prune(now, max, done)) => {
                rt.block_on(store.write_traces(std::mem::take(&mut pending)));
                rt.block_on(store.prune(now, max));
                let _ = done.send(());
            }
            Err(_) => {}
        }
        if !pending.is_empty() && (last_flush.elapsed() >= FLUSH_EVERY || pending.len() >= 50) {
            rt.block_on(store.write_traces(std::mem::take(&mut pending)));
            last_flush = Instant::now();
        }
        if writable && last_prune.elapsed() >= PRUNE_EVERY {
            rt.block_on(store.prune(Utc::now().timestamp(), MAX_BYTES));
            last_prune = Instant::now();
        }
        if disconnected {
            rt.block_on(store.write_traces(std::mem::take(&mut pending)));
            return;
        }
    }
}

/// Opens the file; a damaged or incompatible one is moved aside (`history.sqlite3.broken-<unix>`)
/// with a warning, and a fresh one started. It is history, not the app's data: never fatal.
async fn connect_or_reset(path: &Path, writable: bool) -> anyhow::Result<DatabaseConnection> {
    if writable {
        prepare(path)?;
    }
    match connect(path, writable).await {
        Ok(db) => Ok(db),
        Err(e) if writable && path.exists() => {
            let aside = path.with_extension(format!("sqlite3.broken-{}", Utc::now().timestamp()));
            tracing::warn!(
                error = %e,
                file = %path.display(),
                moved_to = %aside.display(),
                "the dashboard's history file is damaged or from another version; starting a fresh one"
            );
            rename_when_released(path, &aside).await?;
            for ext in ["-wal", "-shm"] {
                let side = PathBuf::from(format!("{}{ext}", path.display()));
                if side.exists() {
                    let to = PathBuf::from(format!("{}{ext}", aside.display()));
                    let _ = rename_when_released(&side, &to).await;
                }
            }
            prepare(path)?;
            connect(path, writable).await
        }
        Err(e) => Err(e),
    }
}

/// Renames a file, retrying for up to 2 s while another handle still holds it. Windows refuses to
/// rename an open file (os error 32), and a failed SQLite connection's worker thread can close its
/// handle a moment after the error comes back; Unix renames at once.
async fn rename_when_released(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if tries < 20 && e.kind() != std::io::ErrorKind::NotFound => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => return other,
        }
    }
}

/// The directory (0700) and an empty file (0600) before SQLite opens it, so the file is never
/// readable by other users, even for a moment.
// SECURITY: the file holds redacted traces and timings; still, only the app's user reads it
// (test: the_history_file_is_private_and_redacted).
fn prepare(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?;
    restrict(path);
    Ok(())
}

/// 0600 on the file and its WAL and shared-memory companions (where the OS has modes).
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if p.exists() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

async fn connect(path: &Path, writable: bool) -> anyhow::Result<DatabaseConnection> {
    // WHY forward slashes: url 2.5.9 rejects `sqlite://C:\...` (InvalidPort), and SQLite on
    // Windows takes `C:/...` just the same.
    let url = format!(
        "sqlite://{}?mode={}",
        path.display().to_string().replace('\\', "/"),
        if writable { "rwc" } else { "ro" }
    );
    let mut opts = ConnectOptions::new(url);
    // WHY no SQL logging: the dashboard would record its own writes in its log tail.
    opts.max_connections(1)
        .min_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(opts).await?;
    // WHY close on failure: dropping the pool closes its connection later, in the background;
    // until then Windows refuses to move the file aside (os error 32).
    if let Err(e) = check_and_migrate(&db, writable).await {
        let _ = db.close().await;
        return Err(e);
    }
    restrict(path);
    Ok(db)
}

/// The file is SQLite, undamaged, with this dashboard's layout (created when writable).
async fn check_and_migrate(db: &DatabaseConnection, writable: bool) -> anyhow::Result<()> {
    let stmt = |sql: &str| Statement::from_string(DbBackend::Sqlite, sql.to_owned());
    // WHY a small page cache: reads are a few chunks at a time, writes append; SQLite's default
    // (2 MB) would be most of what the history saves in memory.
    db.execute_raw(stmt("PRAGMA cache_size = -256")).await?;
    // A file that isn't SQLite, or is damaged, fails here.
    let check = db
        .query_one_raw(stmt("PRAGMA quick_check"))
        .await?
        .and_then(|r| r.try_get_by_index::<String>(0).ok())
        .unwrap_or_default();
    if check != "ok" {
        anyhow::bail!("quick_check: {check}");
    }
    if writable {
        for sql in [
            "PRAGMA auto_vacuum = INCREMENTAL",
            "PRAGMA journal_mode = WAL",
            "PRAGMA synchronous = NORMAL",
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS chunks (level INTEGER NOT NULL, start INTEGER NOT NULL, \
             demo INTEGER NOT NULL DEFAULT 0, data BLOB NOT NULL, PRIMARY KEY (level, start)) WITHOUT ROWID",
            "CREATE TABLE IF NOT EXISTS marks (t INTEGER NOT NULL, kind TEXT NOT NULL, label TEXT NOT NULL, \
             demo INTEGER NOT NULL, PRIMARY KEY (t, label)) WITHOUT ROWID",
            "CREATE TABLE IF NOT EXISTS traces (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, \
             kind TEXT NOT NULL, method TEXT NOT NULL, route TEXT NOT NULL, status INTEGER NOT NULL, \
             dur_us INTEGER NOT NULL, n_plus_one INTEGER NOT NULL, data BLOB NOT NULL)",
            "CREATE INDEX IF NOT EXISTS traces_at ON traces (at)",
        ] {
            db.execute_raw(stmt(sql)).await?;
        }
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO meta (key, value) VALUES ('schema', ?) ON CONFLICT (key) DO NOTHING",
            [SCHEMA_VERSION.into()],
        ))
        .await?;
    }
    let version = db
        .query_one_raw(stmt("SELECT value FROM meta WHERE key = 'schema'"))
        .await?
        .and_then(|r| r.try_get::<String>("", "value").ok());
    if version.as_deref() != Some(SCHEMA_VERSION) {
        anyhow::bail!(
            "schema version {} (this dashboard reads {SCHEMA_VERSION})",
            version.as_deref().unwrap_or("none")
        );
    }
    Ok(())
}

struct Store {
    db: DatabaseConnection,
    path: PathBuf,
}

/// Logs a failed write once a minute at most (a full disk shouldn't flood the log tail).
fn warn_write(e: &sea_orm::DbErr) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = lock(&LAST);
    if last.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
        *last = Some(Instant::now());
        tracing::warn!(error = %e, "the dashboard couldn't write its history file");
    }
}

fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, text, values)
}

impl Store {
    async fn chunk(&self, level: i64, start: i64) -> Option<(Vec<Row>, i64)> {
        let r = self
            .db
            .query_one_raw(sql(
                "SELECT demo, data FROM chunks WHERE level = ? AND start = ?",
                vec![level.into(), start.into()],
            ))
            .await
            .ok()??;
        let demo: i64 = r.try_get("", "demo").ok()?;
        let data: Vec<u8> = r.try_get("", "data").ok()?;
        Some((decode_chunk(&data).unwrap_or_default(), demo))
    }

    async fn put_chunk(&self, level: i64, start: i64, rows: &[Row], demo: i64) {
        let data = encode_chunk(rows, level == MINUTES);
        if let Err(e) = self
            .db
            .execute_raw(sql(
                "INSERT INTO chunks (level, start, demo, data) VALUES (?, ?, ?, ?) \
                 ON CONFLICT (level, start) DO UPDATE SET demo = excluded.demo, data = excluded.data",
                vec![level.into(), start.into(), demo.into(), data.into()],
            ))
            .await
        {
            warn_write(&e);
        }
    }

    /// Merges one minute into its hour's chunk (a real minute replaces a demo one; demo data
    /// never overwrites real data), then refreshes that hour's rollup in its day's chunk.
    async fn write_minute(&self, m: &Minute, demo: bool) {
        let t = m.sample.t;
        let hour = t.div_euclid(3_600) * 3_600;
        let (mut rows, mut mask) = self.chunk(MINUTES, hour).await.unwrap_or_default();
        let bit = 1_i64 << ((t - hour) / 60).clamp(0, 59);
        match rows.iter().position(|r| r.sample.t == t) {
            Some(_) if demo && mask & bit == 0 => return,
            Some(i) => rows[i] = Row::from_minute(m),
            None => rows.push(Row::from_minute(m)),
        }
        rows.sort_by_key(|r| r.sample.t);
        if demo {
            mask |= bit;
        } else {
            mask &= !bit;
        }
        self.put_chunk(MINUTES, hour, &rows, mask).await;
        self.roll_hour(hour, &rows, mask != 0).await;
    }

    async fn roll_hour(&self, hour: i64, minutes: &[Row], demo: bool) {
        let mut hist = Hist::default();
        let mut sql_hist = Hist::default();
        for r in minutes {
            hist.merge(&r.hist);
            sql_hist.merge(&r.sql_hist);
        }
        let samples: Vec<Sample> = minutes.iter().map(|r| r.sample).collect();
        let Some(h) = rollup(&samples, &hist, &sql_hist, hour + 3_599) else {
            return;
        };
        let day = hour.div_euclid(86_400) * 86_400;
        let (mut rows, mut mask) = self.chunk(HOURS, day).await.unwrap_or_default();
        let row = Row {
            sample: h,
            hist: Hist::default(),
            sql_hist: Hist::default(),
        };
        match rows.iter().position(|r| r.sample.t == h.t) {
            Some(i) => rows[i] = row,
            None => rows.push(row),
        }
        rows.sort_by_key(|r| r.sample.t);
        let bit = 1_i64 << ((hour - day) / 3_600).clamp(0, 23);
        if demo {
            mask |= bit;
        } else {
            mask &= !bit;
        }
        self.put_chunk(HOURS, day, &rows, mask).await;
    }

    async fn write_mark(&self, a: &Annotation) {
        if let Err(e) = self
            .db
            .execute_raw(sql(
                "INSERT INTO marks (t, kind, label, demo) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                vec![
                    a.t.into(),
                    a.kind.into(),
                    a.label.clone().into(),
                    i64::from(a.demo).into(),
                ],
            ))
            .await
        {
            warn_write(&e);
        }
    }

    async fn write_traces(&self, traces: Vec<Arc<Trace>>) {
        if traces.is_empty() {
            return;
        }
        let strip = STRIP_DETAILS.load(Ordering::Relaxed);
        for t in traces {
            let data = encode_trace(&t, strip);
            if let Err(e) = self
                .db
                .execute_raw(sql(
                    "INSERT INTO traces (id, at, kind, method, route, status, dur_us, n_plus_one, data) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
                    vec![
                        (t.id as i64).into(),
                        t.at.timestamp().into(),
                        t.kind.into(),
                        t.method.into(),
                        t.route.into(),
                        i64::from(t.status).into(),
                        (t.dur_us as i64).into(),
                        (t.n_plus_one.len() as i64).into(),
                        data.into(),
                    ],
                ))
                .await
            {
                warn_write(&e);
                return;
            }
        }
        restrict(&self.path);
    }

    async fn max_trace_id(&self) -> Option<u64> {
        let r = self
            .db
            .query_one_raw(sql("SELECT MAX(id) AS id FROM traces", vec![]))
            .await
            .ok()??;
        r.try_get::<Option<i64>>("", "id").ok()?.map(|v| v as u64)
    }

    async fn samples(&self, level: i64, from: i64, to: i64) -> (Vec<Sample>, bool) {
        let width = if level == MINUTES { 3_600 } else { 86_400 };
        let rows = self
            .db
            .query_all_raw(sql(
                "SELECT start, demo, data FROM chunks WHERE level = ? AND start > ? AND start <= ? ORDER BY start",
                vec![level.into(), (from - width).into(), to.into()],
            ))
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        let mut demo = false;
        for r in rows {
            let (Ok(start), Ok(mask), Ok(data)) = (
                r.try_get::<i64>("", "start"),
                r.try_get::<i64>("", "demo"),
                r.try_get::<Vec<u8>>("", "data"),
            ) else {
                continue;
            };
            let Some(chunk) = decode_chunk(&data) else {
                continue;
            };
            let unit = if level == MINUTES { 60 } else { 3_600 };
            for row in chunk {
                let s = row.sample;
                if s.t > from && s.t <= to {
                    if mask & (1_i64 << ((s.t - start) / unit).clamp(0, 59)) != 0 {
                        demo = true;
                    }
                    out.push(s);
                }
            }
        }
        (out, demo)
    }

    async fn marks(&self, from: i64, to: i64) -> Vec<Annotation> {
        let rows = self
            .db
            .query_all_raw(sql(
                "SELECT t, kind, label, demo FROM marks WHERE t >= ? AND t <= ? ORDER BY t LIMIT 500",
                vec![from.into(), to.into()],
            ))
            .await
            .unwrap_or_default();
        rows.iter()
            .filter_map(|r| {
                Some(Annotation {
                    t: r.try_get("", "t").ok()?,
                    kind: mark_kind(&r.try_get::<String>("", "kind").ok()?),
                    label: r.try_get("", "label").ok()?,
                    demo: r.try_get::<i64>("", "demo").ok()? != 0,
                })
            })
            .collect()
    }

    async fn traces(&self, f: &TraceFilter) -> Result<Vec<Arc<Trace>>, sea_orm::DbErr> {
        let mut text = "SELECT data FROM traces WHERE 1 = 1".to_owned();
        let mut values: Vec<Value> = Vec::new();
        if let Some(from) = f.from {
            text.push_str(" AND at >= ?");
            values.push(from.into());
        }
        if let Some(to) = f.to {
            text.push_str(" AND at <= ?");
            values.push(to.into());
        }
        if let Some(route) = &f.route {
            text.push_str(" AND route = ?");
            values.push(route.clone().into());
        }
        if let Some(kind) = &f.kind {
            text.push_str(" AND kind = ?");
            values.push(kind.clone().into());
        }
        if let Some(class) = f
            .status
            .as_deref()
            .and_then(|s| s.chars().next()?.to_digit(10))
        {
            text.push_str(" AND status >= ? AND status < ?");
            values.push(i64::from(class * 100).into());
            values.push(i64::from(class * 100 + 100).into());
        }
        if let Some(min) = f.min_us {
            text.push_str(" AND dur_us >= ?");
            values.push((min as i64).into());
        }
        if f.n_plus_one {
            text.push_str(" AND n_plus_one > 0");
        }
        // The id is inside the compressed trace: match its kind here, the id after decoding.
        if f.request_id.is_some() && f.kind.is_none() {
            text.push_str(" AND kind <> 'request'");
        }
        text.push_str(" ORDER BY id DESC LIMIT ?");
        let rows = if f.request_id.is_some() {
            1_000
        } else {
            f.limit.clamp(1, 1_000)
        };
        values.push(((rows + f.exclude.len()) as i64).into());
        let rows = self.db.query_all_raw(sql(&text, values)).await?;
        let skip: HashSet<u64> = f.exclude.iter().copied().collect();
        Ok(rows
            .iter()
            .filter_map(|r: &QueryResult| r.try_get::<Vec<u8>>("", "data").ok())
            .filter_map(|d| decode_trace(&d))
            .filter(|t| !skip.contains(&t.id))
            .filter(|t| f.request_id.as_ref().is_none_or(|r| t.request_id == *r))
            .take(f.limit.max(1))
            .map(Arc::new)
            .collect())
    }

    async fn one_trace(&self, id: u64) -> Option<Arc<Trace>> {
        let r = self
            .db
            .query_one_raw(sql(
                "SELECT data FROM traces WHERE id = ?",
                vec![(id as i64).into()],
            ))
            .await
            .ok()??;
        decode_trace(&r.try_get::<Vec<u8>>("", "data").ok()?).map(Arc::new)
    }

    async fn count(&self, text: &str) -> i64 {
        self.db
            .query_one_raw(sql(text, vec![]))
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get_by_index::<i64>(0).ok())
            .unwrap_or(0)
    }

    async fn bytes(&self) -> i64 {
        self.count("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()")
            .await
    }

    async fn answer(&self, q: Query) -> Reply {
        match q {
            Query::Minutes(from, to) => {
                let (s, d) = self.samples(MINUTES, from, to).await;
                Reply::Samples(s, d)
            }
            Query::Hours(from, to) => {
                let (s, d) = self.samples(HOURS, from, to).await;
                Reply::Samples(s, d)
            }
            Query::Marks(from, to) => Reply::Marks(self.marks(from, to).await),
            Query::Trace(id) => Reply::Traces(self.one_trace(id).await.into_iter().collect()),
            Query::Traces(f) => Reply::Traces(self.traces(&f).await.unwrap_or_default()),
            Query::Stats => Reply::Stats(Stats {
                path: self.path.display().to_string(),
                bytes: self.bytes().await,
                minute_chunks: self
                    .count("SELECT COUNT(*) FROM chunks WHERE level = 0")
                    .await,
                hour_chunks: self
                    .count("SELECT COUNT(*) FROM chunks WHERE level = 1")
                    .await,
                traces: self.count("SELECT COUNT(*) FROM traces").await,
                marks: self.count("SELECT COUNT(*) FROM marks").await,
                oldest: Some(self.count("SELECT MIN(start) FROM chunks").await).filter(|v| *v > 0),
            }),
        }
    }

    /// Retention, then the size ceiling, then gives the freed pages back to the OS.
    async fn prune(&self, now: i64, max_bytes: i64) {
        let run = |text: &'static str, v: Vec<Value>| async move {
            if let Err(e) = self.db.execute_raw(sql(text, v)).await {
                warn_write(&e);
            }
        };
        run(
            "DELETE FROM chunks WHERE level = 0 AND start < ?",
            vec![(now - MINUTE_DAYS * 86_400 - 3_600).into()],
        )
        .await;
        run(
            "DELETE FROM chunks WHERE level = 1 AND start < ?",
            vec![(now - HOUR_DAYS * 86_400 - 86_400).into()],
        )
        .await;
        run(
            "DELETE FROM marks WHERE t < ?",
            vec![(now - HOUR_DAYS * 86_400).into()],
        )
        .await;
        run(
            "DELETE FROM traces WHERE at < ?",
            vec![(now - TRACE_DAYS * 86_400).into()],
        )
        .await;
        run(
            "DELETE FROM traces WHERE id NOT IN (SELECT id FROM traces ORDER BY id DESC LIMIT ?)",
            vec![MAX_TRACES.into()],
        )
        .await;
        run("PRAGMA incremental_vacuum", vec![]).await;
        // Over the ceiling: a day of the oldest minutes, then the oldest tenth of the traces, at a
        // time, until it fits (bounded, in case the file can't shrink).
        for _ in 0..64 {
            if self.bytes().await <= max_bytes {
                break;
            }
            let minutes = self
                .count("SELECT COUNT(*) FROM chunks WHERE level = 0")
                .await;
            if minutes > 0 {
                run(
                    "DELETE FROM chunks WHERE level = 0 AND start IN \
                     (SELECT start FROM chunks WHERE level = 0 ORDER BY start LIMIT 24)",
                    vec![],
                )
                .await;
            } else {
                run(
                    "DELETE FROM traces WHERE id IN (SELECT id FROM traces ORDER BY id \
                     LIMIT MAX(1, (SELECT COUNT(*) FROM traces) / 10))",
                    vec![],
                )
                .await;
            }
            run("PRAGMA incremental_vacuum", vec![]).await;
            if minutes == 0 && self.count("SELECT COUNT(*) FROM traces").await == 0 {
                break;
            }
        }
        let _ = self
            .db
            .execute_raw(sql("PRAGMA wal_checkpoint(TRUNCATE)", vec![]))
            .await;
    }
}

/// A marker's kind read back, as one of the kinds the charts draw.
fn mark_kind(s: &str) -> &'static str {
    ["boot", "deploy", "lab", "errors", "spike"]
        .into_iter()
        .find(|k| *k == s)
        .unwrap_or("deploy")
}

// ---------------------------------------------------------------------------------------------
// The chunk format
// ---------------------------------------------------------------------------------------------

/// One sample in a chunk, with its histograms (minutes only; hours keep none).
#[derive(Clone, Default)]
pub struct Row {
    pub sample: Sample,
    pub hist: Hist,
    pub sql_hist: Hist,
}

impl Row {
    fn from_minute(m: &Minute) -> Self {
        Self {
            sample: m.sample,
            hist: m.hist.clone(),
            sql_hist: m.sql_hist.clone(),
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.at)?;
            self.at += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(v);
            }
        }
        None
    }
}

/// A sample's numbers as integers, in column order. Rates keep three decimals (1 request a
/// minute is 0.017/s), CPU and busy two; everything else is already whole.
const COLUMNS: usize = 20 + LATENCY_BUCKETS.len() + 1;

fn to_columns(s: &Sample) -> [i64; COLUMNS] {
    let r3 = |v: f32| (f64::from(v) * 1_000.0).round() as i64;
    let r2 = |v: f32| (f64::from(v) * 100.0).round() as i64;
    let mut c = [0_i64; COLUMNS];
    let head = [
        s.t,
        s.rss as i64,
        s.virt as i64,
        r2(s.cpu),
        s.fds.map_or(0, |f| i64::from(f) + 1),
        i64::from(s.workers),
        i64::from(s.tasks),
        i64::from(s.queue),
        r2(s.busy),
        i64::from(s.pool_size),
        i64::from(s.pool_idle),
        i64::from(s.pool_max),
        r3(s.req),
        r3(s.e4),
        r3(s.e5),
        i64::from(s.p50),
        i64::from(s.p95),
        i64::from(s.p99),
        r3(s.sql),
        i64::from(s.sql_p95),
    ];
    c[..20].copy_from_slice(&head);
    for (i, v) in s.lat.iter().enumerate() {
        c[20 + i] = i64::from(*v);
    }
    c
}

fn from_columns(c: &[i64; COLUMNS]) -> Sample {
    let f3 = |v: i64| (v as f64 / 1_000.0) as f32;
    let f2 = |v: i64| (v as f64 / 100.0) as f32;
    let u = |v: i64| v.clamp(0, i64::from(u32::MAX)) as u32;
    let mut lat = [0_u16; LATENCY_BUCKETS.len() + 1];
    for (i, l) in lat.iter_mut().enumerate() {
        *l = c[20 + i].clamp(0, i64::from(u16::MAX)) as u16;
    }
    Sample {
        t: c[0],
        rss: c[1].max(0) as u64,
        virt: c[2].max(0) as u64,
        cpu: f2(c[3]),
        fds: (c[4] > 0).then(|| u(c[4] - 1)),
        workers: u(c[5]),
        tasks: u(c[6]),
        queue: u(c[7]),
        busy: f2(c[8]),
        pool_size: u(c[9]),
        pool_idle: u(c[10]),
        pool_max: u(c[11]),
        req: f3(c[12]),
        e4: f3(c[13]),
        e5: f3(c[14]),
        p50: u(c[15]),
        p95: u(c[16]),
        p99: u(c[17]),
        sql: f3(c[18]),
        sql_p95: u(c[19]),
        lat,
    }
}

/// `[version][has hists]` then, deflated: the count, each column as deltas from the sample
/// before (zigzag varints: most are 0 or 1 byte), then each sample's histograms as sparse pairs.
pub fn encode_chunk(rows: &[Row], hists: bool) -> Vec<u8> {
    let mut raw = Vec::with_capacity(rows.len() * 48);
    put_varint(&mut raw, rows.len() as u64);
    let cols: Vec<[i64; COLUMNS]> = rows.iter().map(|r| to_columns(&r.sample)).collect();
    for c in 0..COLUMNS {
        let mut prev = 0_i64;
        for row in &cols {
            put_varint(&mut raw, zigzag(row[c].wrapping_sub(prev)));
            prev = row[c];
        }
    }
    if hists {
        for r in rows {
            for h in [&r.hist, &r.sql_hist] {
                let (pairs, sum, max) = h.sparse();
                put_varint(&mut raw, pairs.len() as u64);
                let mut prev = 0_u16;
                for (i, n) in pairs {
                    put_varint(&mut raw, u64::from(i - prev));
                    put_varint(&mut raw, u64::from(n));
                    prev = i;
                }
                put_varint(&mut raw, sum);
                put_varint(&mut raw, max);
            }
        }
    }
    let mut out = vec![BLOB_VERSION, u8::from(hists)];
    let mut z = flate2::write::DeflateEncoder::new(&mut out, flate2::Compression::default());
    // Writing into a Vec can't fail.
    let _ = z.write_all(&raw);
    let _ = z.finish();
    out
}

/// The inverse of `encode_chunk`; `None` for anything it can't read (a damaged row is skipped,
/// never a panic).
pub fn decode_chunk(data: &[u8]) -> Option<Vec<Row>> {
    let (&version, rest) = data.split_first()?;
    let (&hists, body) = rest.split_first()?;
    if version != BLOB_VERSION {
        return None;
    }
    let mut raw = Vec::new();
    // A chunk is at most a few hundred KB inflated: refuse a "zip bomb" in a damaged file.
    flate2::read::DeflateDecoder::new(body)
        .take(4 * 1024 * 1024)
        .read_to_end(&mut raw)
        .ok()?;
    let mut r = Reader { b: &raw, at: 0 };
    let n = usize::try_from(r.varint()?).ok()?;
    if n > 10_000 {
        return None;
    }
    let mut cols = vec![[0_i64; COLUMNS]; n];
    for c in 0..COLUMNS {
        let mut prev = 0_i64;
        for row in cols.iter_mut() {
            prev = prev.wrapping_add(unzigzag(r.varint()?));
            row[c] = prev;
        }
    }
    let mut rows: Vec<Row> = cols
        .iter()
        .map(|c| Row {
            sample: from_columns(c),
            ..Row::default()
        })
        .collect();
    if hists == 1 {
        for row in rows.iter_mut() {
            for which in 0..2 {
                let k = usize::try_from(r.varint()?).ok()?;
                let mut pairs = Vec::with_capacity(k.min(512));
                let mut prev = 0_u64;
                for _ in 0..k {
                    prev += r.varint()?;
                    pairs.push((u16::try_from(prev).ok()?, u32::try_from(r.varint()?).ok()?));
                }
                let (sum, max) = (r.varint()?, r.varint()?);
                let h = Hist::from_sparse(&pairs, sum, max);
                if which == 0 {
                    row.hist = h;
                } else {
                    row.sql_hist = h;
                }
            }
        }
    }
    Some(rows)
}

// ---------------------------------------------------------------------------------------------
// Traces on disk
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredTrace {
    id: u64,
    at_us: i64,
    kind: String,
    method: String,
    route: String,
    path: String,
    status: u16,
    dur_us: u64,
    bytes: Option<u64>,
    request_id: String,
    spans: Vec<StoredSpan>,
    sql: Vec<StoredSql>,
    events: Vec<StoredEvent>,
    n_plus_one: Vec<(u64, u32, String)>,
    statements: u32,
    sql_us: u64,
    dropped: u32,
}

#[derive(Serialize, Deserialize)]
struct StoredSpan {
    id: u64,
    parent: Option<u64>,
    name: String,
    target: String,
    file: Option<String>,
    line: Option<u32>,
    start_us: u64,
    dur_us: u64,
    fields: String,
}

#[derive(Serialize, Deserialize)]
struct StoredSql {
    fp: u64,
    /// Index into `sqls` (an N+1's 20 statements are one string on disk).
    sql: u32,
    start_us: u64,
    dur_us: u64,
    parent: Option<u64>,
    failed: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredEvent {
    at_us: u64,
    level: String,
    message: String,
    parent: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct StoredBody {
    t: StoredTrace,
    sqls: Vec<String>,
}

/// The trace as recorded (already redacted: paths, secret fields, normalized SQL), as compressed
/// JSON. `strip` (production) drops log events and span fields, which the read-only view never
/// shows either.
// SECURITY: nothing reaches the file that the dashboard's own redaction didn't already pass, and
// production writes even less (test: the_history_file_is_private_and_redacted).
pub fn encode_trace(t: &Trace, strip: bool) -> Vec<u8> {
    let mut sqls: Vec<String> = Vec::new();
    let mut index: HashMap<*const u8, u32> = HashMap::new();
    let sql = t
        .sql
        .iter()
        .map(|q| {
            let at = *index.entry(q.sql.as_ptr()).or_insert_with(|| {
                sqls.push(q.sql.to_string());
                (sqls.len() - 1) as u32
            });
            StoredSql {
                fp: q.fp,
                sql: at,
                start_us: q.start_us,
                dur_us: q.dur_us,
                parent: q.parent,
                failed: q.failed,
            }
        })
        .collect();
    let body = StoredBody {
        t: StoredTrace {
            id: t.id,
            at_us: t.at.timestamp_micros(),
            kind: t.kind.to_owned(),
            method: t.method.to_owned(),
            route: t.route.to_owned(),
            path: t.path.clone(),
            status: t.status,
            dur_us: t.dur_us,
            bytes: t.bytes,
            request_id: t.request_id.clone(),
            spans: t
                .spans
                .iter()
                .map(|s| StoredSpan {
                    id: s.id,
                    parent: s.parent,
                    name: s.name.to_owned(),
                    target: s.target.to_owned(),
                    file: s.file.map(str::to_owned),
                    line: s.line,
                    start_us: s.start_us,
                    dur_us: s.dur_us,
                    fields: if strip {
                        String::new()
                    } else {
                        s.fields.clone()
                    },
                })
                .collect(),
            sql,
            events: if strip {
                Vec::new()
            } else {
                t.events
                    .iter()
                    .map(|e| StoredEvent {
                        at_us: e.at_us,
                        level: e.level.to_owned(),
                        message: e.message.clone(),
                        parent: e.parent,
                    })
                    .collect()
            },
            n_plus_one: t
                .n_plus_one
                .iter()
                .map(|(fp, n, s)| (*fp, *n, s.to_string()))
                .collect(),
            statements: t.statements,
            sql_us: t.sql_us,
            dropped: t.dropped,
        },
        sqls,
    };
    let json = serde_json::to_vec(&body).unwrap_or_default();
    let mut out = vec![BLOB_VERSION];
    let mut z = flate2::write::DeflateEncoder::new(&mut out, flate2::Compression::default());
    let _ = z.write_all(&json);
    let _ = z.finish();
    out
}

/// The trace as JSON (copy as JSON, `…/traces/{id}/json`): the same redacted shape as on disk.
pub fn trace_json(t: &Trace, strip: bool) -> String {
    let blob = encode_trace(t, strip);
    let mut json = Vec::new();
    let _ = flate2::read::DeflateDecoder::new(&blob[1..]).read_to_end(&mut json);
    serde_json::from_slice::<serde_json::Value>(&json)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_default()
}

pub fn decode_trace(data: &[u8]) -> Option<Trace> {
    let (&version, body) = data.split_first()?;
    if version != BLOB_VERSION {
        return None;
    }
    let mut json = Vec::new();
    flate2::read::DeflateDecoder::new(body)
        .take(16 * 1024 * 1024)
        .read_to_end(&mut json)
        .ok()?;
    let StoredBody { t, sqls } = serde_json::from_slice(&json).ok()?;
    let shared: Vec<Arc<str>> = sqls.into_iter().map(Arc::from).collect();
    let empty: Arc<str> = Arc::from("");
    Some(Trace {
        id: t.id,
        at: DateTime::from_timestamp_micros(t.at_us)?,
        kind: TRACE_KINDS
            .into_iter()
            .find(|k| *k == t.kind)
            .unwrap_or("request"),
        from_disk: true,
        method: intern(&t.method),
        route: route_static(&t.route),
        path: t.path,
        status: t.status,
        dur_us: t.dur_us,
        bytes: t.bytes,
        request_id: t.request_id,
        spans: t
            .spans
            .into_iter()
            .map(|s| SpanRec {
                id: s.id,
                parent: s.parent,
                name: intern(&s.name),
                target: intern(&s.target),
                file: s.file.as_deref().map(intern),
                line: s.line,
                start_us: s.start_us,
                dur_us: s.dur_us,
                fields: s.fields,
            })
            .collect(),
        sql: t
            .sql
            .into_iter()
            .map(|q| SqlRec {
                fp: q.fp,
                sql: shared
                    .get(q.sql as usize)
                    .cloned()
                    .unwrap_or_else(|| empty.clone()),
                start_us: q.start_us,
                dur_us: q.dur_us,
                parent: q.parent,
                failed: q.failed,
            })
            .collect(),
        events: t
            .events
            .into_iter()
            .map(|e| EventRec {
                at_us: e.at_us,
                level: ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
                    .into_iter()
                    .find(|l| *l == e.level)
                    .unwrap_or("INFO"),
                message: e.message,
                parent: e.parent,
            })
            .collect(),
        n_plus_one: t
            .n_plus_one
            .into_iter()
            .map(|(fp, n, s)| (fp, n, Arc::from(s)))
            .collect(),
        statements: t.statements,
        sql_us: t.sql_us,
        dropped: t.dropped,
    })
}

/// A route read back: the `ROUTES` pattern it names (static already), else interned.
fn route_static(route: &str) -> &'static str {
    crate::routes::ROUTES
        .iter()
        .map(|(_, path, _, _)| *path)
        .find(|p| *p == route)
        .or_else(|| {
            [super::http::UNMATCHED, "/static/*"]
                .into_iter()
                .find(|p| *p == route)
        })
        .unwrap_or_else(|| intern(route))
}

/// Span names, modules and files are `&'static str` in a live trace (tracing's metadata). Read
/// back from disk they are interned: each distinct string is leaked once.
// WHY a leak, and why it is bounded: the strings come from the app's own source (span names,
// module paths, file names), a set fixed at compile time; past `MAX_INTERNED` (a damaged or
// hostile file) every new string reads "(other)", so memory can't grow without bound.
const MAX_INTERNED: usize = 4_096;

static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn intern(s: &str) -> &'static str {
    let mut set = lock(&INTERNED);
    if let Some(found) = set.get(s) {
        return found;
    }
    if set.len() >= MAX_INTERNED || s.len() > 300 {
        return "(other)";
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minute(t: i64, req: f32, lat: &[u64]) -> Minute {
        let mut hist = Hist::default();
        for v in lat {
            hist.record(*v);
        }
        Minute {
            sample: Sample {
                t,
                rss: 50_000_000 + t as u64,
                cpu: 12.34,
                fds: Some(30),
                req,
                e5: 0.017,
                p95: hist.quantile(0.95) as u32,
                ..Sample::default()
            },
            hist,
            sql_hist: Hist::default(),
        }
    }

    #[test]
    fn a_chunk_round_trips_through_compression() {
        let rows: Vec<Row> = (0..60)
            .map(|i| Row::from_minute(&minute(3_600 + i * 60 + 59, 2.5, &[1_000, 2_000, 90_000])))
            .collect();
        let blob = encode_chunk(&rows, true);
        let back = decode_chunk(&blob).expect("decodes");
        assert_eq!(back.len(), 60);
        for (a, b) in rows.iter().zip(&back) {
            assert_eq!(a.sample, b.sample);
            assert_eq!(a.hist.quantile(0.99), b.hist.quantile(0.99));
            assert_eq!(a.hist.count(), b.hist.count());
        }
        // An hour of minutes, histograms included, in well under a few KB.
        assert!(blob.len() < 3_000, "{} bytes", blob.len());
        assert!(decode_chunk(&blob[..blob.len() / 2]).is_none());
        assert!(decode_chunk(b"\x09garbage").is_none());
    }

    #[test]
    fn varints_and_zigzag_round_trip() {
        for v in [0_i64, 1, -1, 63, -64, 1 << 40, i64::MIN, i64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, zigzag(v));
            assert_eq!(Reader { b: &b, at: 0 }.varint().map(unzigzag), Some(v));
        }
    }

    #[test]
    fn interning_is_bounded() {
        let a = intern("load_orders");
        assert!(std::ptr::eq(a, intern("load_orders")));
        assert_eq!(intern(&"x".repeat(400)), "(other)");
    }
}
