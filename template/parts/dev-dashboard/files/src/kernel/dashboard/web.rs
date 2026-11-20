//! The dashboard's pages, fragments, streams and actions.
//!
//! - `/dev/dashboard/…`: everything, development only (`dev_router`, merged at `dev routers`).
//! - `/admin/dashboard/…`: the read-only subset (overview, charts, requests, traces, SQL without
//!   parameters, config), behind `DashboardAccess` (`admin_router`). No load lab, no EXPLAIN, no
//!   logs, no test runner, no GitHub.
//!
//! Every response is `no-store`. Every action is a POST (CSRF applies) that redirects with a flash.

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use chrono::Utc;
use futures::Stream;
use serde::Deserialize;

use super::charts::{self, ChartView, Heatmap, SeriesSpec, Spark, Unit, Window};
use super::series::{Range, Sample};
use super::trace::{LogLine, Trace};
use super::{
    HUB, ai, github, http, insights, lab, lock, panels, route_explorer as rx, schema, sql, testruns,
};
use crate::error::AppError;
use crate::kernel::cookie;
use crate::kernel::flash::{Flash, RequestFlash};
use crate::kernel::theme::ThemeSwitch;
use crate::kernel::ui::{self, Chrome};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------------

/// "850µs", "12.3ms", "1.24s".
pub fn micros(us: u64) -> String {
    match us {
        u if u < 1_000 => format!("{u}µs"),
        u if u < 100_000 => format!("{:.1}ms", u as f64 / 1_000.0),
        u if u < 1_000_000 => format!("{}ms", u / 1_000),
        u => format!("{:.2}s", u as f64 / 1_000_000.0),
    }
}

/// "1 error", "2 errors", "0 errors": a count and its noun, singular only for exactly one.
pub fn count(n: impl std::fmt::Display, one: &str, many: &str) -> String {
    let n = n.to_string();
    let noun = if n == "1" { one } else { many };
    format!("{n} {noun}")
}

/// "512 B", "1.5 KB", "210.4 MB", "2.1 GB".
pub fn bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    match n as f64 {
        b if b < K => format!("{n} B"),
        b if b < K * K => format!("{:.1} KB", b / K),
        b if b < K * K * K => format!("{:.1} MB", b / (K * K)),
        b => format!("{:.1} GB", b / (K * K * K)),
    }
}

fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        "0%".to_owned()
    } else {
        let p = part as f64 * 100.0 / whole as f64;
        if p > 0.0 && p < 1.0 {
            format!("{p:.1}%")
        } else {
            format!("{p:.0}%")
        }
    }
}

fn status_tone(status: u16) -> &'static str {
    match status {
        500.. => "err",
        400..=499 => "warn",
        300..=399 => "info",
        _ => "ok",
    }
}

// ---------------------------------------------------------------------------------------------
// Chrome: which dashboard, and its sub-navigation
// ---------------------------------------------------------------------------------------------

/// Which dashboard a request is in.
#[derive(Clone, Copy)]
pub struct Mode {
    /// The production view: no actions, no parameters, no logs.
    pub ro: bool,
    pub base: &'static str,
}

pub(super) const DEV: Mode = Mode {
    ro: false,
    base: "/dev/dashboard",
};
pub(super) const ADMIN: Mode = Mode {
    ro: true,
    base: "/admin/dashboard",
};

pub struct NavLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// What every dashboard page shares.
pub struct Dash {
    pub ro: bool,
    pub base: &'static str,
    pub nav: Vec<NavLink>,
    /// The live stream for this page, if it has live parts.
    pub live: String,
    /// Seconds between live updates.
    pub refresh: u64,
    /// The "Ask AI" panel for this page: its tab key ("" for none) and item.
    pub ai_tab: &'static str,
    pub ai_item: String,
    /// The assistant is available here (so "Ask about this" links show).
    pub ai_on: bool,
    /// The panel's one-line "what this tab tells you" and its suggested questions (shown before
    /// the panel loads, and without JavaScript as links).
    pub ai_guide: &'static str,
    pub ai_prompts: &'static [&'static str],
}

impl Dash {
    /// Puts the "Ask AI" panel on this page, when the assistant may answer here.
    pub fn ai(
        mut self,
        config: &crate::config::Config,
        tab: ai::Tab,
        item: Option<String>,
    ) -> Self {
        self.ai_on = ai::allowed(config, self.ro);
        if self.ai_on && (!self.ro || tab.in_read_only()) {
            self.ai_tab = tab.key();
            self.ai_guide = tab.guide_line();
            self.ai_prompts = tab.suggestions(item.is_some());
            self.ai_item = item.unwrap_or_default();
        }
        self
    }

    /// The assistant's page for `tab` (and one item of it): the "Ask about this" links.
    pub fn ask_href(&self, tab: &str, item: &str) -> String {
        format!("{}/ai?tab={tab}&item={item}", self.base)
    }
}

pub(super) fn dash(mode: Mode, page: &str, live: Option<String>, refresh: u64) -> Dash {
    let all: &[(&str, &str, bool)] = &[
        ("Overview", "", true),
        ("Insights", "/insights", false),
        ("Charts", "/charts", true),
        ("Requests", "/requests", true),
        ("Routes", "/routes", false),
        ("Traces", "/traces", true),
        ("Methods", "/methods", true),
        ("Upstreams", "/upstreams", true),
        ("Hot functions", "/profile", false),
        ("SQL", "/sql", true),
        ("Logs", "/logs", false),
        ("Security", "/security", true),
        ("Frontend", "/frontend", true),
        ("Schema", "/schema", false),
        ("Tests", "/tests", false),
        ("GitHub", "/github", false),
        ("Load lab", "/lab", false),
        ("Config", "/config", true),
    ];
    Dash {
        ro: mode.ro,
        base: mode.base,
        nav: all
            .iter()
            .filter(|(_, _, in_ro)| !mode.ro || *in_ro)
            .map(|(label, path, _)| NavLink {
                label,
                href: format!("{}{path}", mode.base),
                current: *path == page,
            })
            .collect(),
        live: live.unwrap_or_default(),
        refresh,
        ai_tab: "",
        ai_item: String::new(),
        ai_on: false,
        ai_guide: "",
        ai_prompts: &[],
    }
}

pub(super) fn chrome(
    state: &AppState,
    title: &str,
    tab: &str,
    flash: Option<Extension<RequestFlash>>,
    theme: ThemeSwitch,
) -> Chrome {
    Chrome::new(
        title,
        tab,
        state.assets.assets(),
        state.config.is_development(),
        flash.and_then(|f| f.0.0),
        theme,
    )
    .stylesheet(state.assets.path("kernel/dashboard.css"))
    .stylesheet(state.assets.path("kernel/dashboard-ai.css"))
    .stylesheet(state.assets.path("kernel/dashboard-trace.css"))
    .script(state.assets.path("kernel/dashboard.js"))
    .script(state.assets.path("kernel/dashboard-trace.js"))
    .script(state.assets.path("kernel/dashboard-ai.js"))
}

/// The app's root (where `storage/`, `tests/` and `product/` are).
fn root() -> PathBuf {
    PathBuf::from(".")
}

pub(super) fn back(to: &str, flash: Flash) -> Response {
    (flash, Redirect::to(to)).into_response()
}

// ---------------------------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------------------------

/// `/dev/dashboard`: development only.
pub fn dev_router() -> Router<AppState> {
    let read = read_routes(Router::new(), "/dev/dashboard");
    read.route("/dev/dashboard.json", get(json))
        .route("/dev/dashboard/insights", get(insights_page))
        .route("/dev/dashboard/insights/dismiss", post(dismiss))
        .route("/dev/dashboard/routes", get(routes_page))
        .route(
            "/dev/dashboard/profile",
            get(super::traces_web::profile_page),
        )
        .route(
            "/dev/dashboard/profile/toggle",
            post(super::traces_web::profile_toggle),
        )
        .route("/dev/dashboard/logs", get(logs_page))
        .route("/dev/dashboard/logs/stream", get(logs_stream))
        .route("/dev/dashboard/sql/{id}/explain", post(explain))
        .route("/dev/dashboard/schema", get(schema_page))
        .route("/dev/dashboard/schema/erd.svg", get(erd_svg))
        .route("/dev/dashboard/schema/erd.mmd", get(erd_mermaid))
        .route("/dev/dashboard/schema/tables/{name}", get(schema_table))
        .route("/dev/dashboard/schema/migrations", get(migrations_page))
        .route("/dev/dashboard/tests", get(tests_page))
        .route("/dev/dashboard/tests/run", post(tests_run))
        .route("/dev/dashboard/github", get(github_page))
        .route("/dev/dashboard/lab", get(lab_page))
        .route("/dev/dashboard/lab/seed", post(lab_seed))
        .route("/dev/dashboard/lab/clear", post(lab_clear))
        .route("/dev/dashboard/lab/traffic", post(lab_traffic))
        .route("/dev/dashboard/lab/stop", post(lab_stop))
        .route("/dev/dashboard/lab/scenario", post(lab_scenario))
        .route("/dev/dashboard/reset", post(reset))
        .route("/dev/csp-reports", post(csp_report))
        .route("/dev/dashboard/rum", post(super::rum::collect))
        .route("/dev/dashboard/lab/upstreams", post(lab_upstreams))
        .route("/dev/dashboard/lab/attack", post(lab_attack))
        .layer(Extension(DEV))
        .merge(lab::demo_router())
        // SECURITY: dashboard data (SQL, logs, traces) never lands in a shared or back/forward
        // cache (test: every_dashboard_response_is_no_store).
        .layer(ui::no_store())
}

/// `/admin/dashboard`: the read-only subset. `routes.rs` mounts it behind `DashboardAccess`.
pub fn admin_router() -> Router<AppState> {
    read_routes(Router::new(), "/admin/dashboard")
        .layer(Extension(ADMIN))
        .layer(ui::no_store())
}

fn read_routes(r: Router<AppState>, base: &'static str) -> Router<AppState> {
    // WHY: the same handlers serve both; `Mode` (an extension) says which one, so the read-only
    // view can't drift from the full one.
    let p = |s: &str| format!("{base}{s}");
    r.route(&p(""), get(overview))
        .route(&p("/charts"), get(charts_page))
        .route(&p("/requests"), get(requests_page))
        .route(&p("/traces"), get(super::traces_web::explorer))
        .route(&p("/traces/compare"), get(super::traces_web::compare))
        .route(&p("/traces/lookup"), get(super::traces_web::lookup))
        .route(&p("/traces/{id}"), get(super::traces_web::trace_page))
        .route(
            &p("/traces/{id}/waterfall"),
            get(super::traces_web::waterfall_fragment),
        )
        .route(&p("/traces/{id}/json"), get(super::traces_web::trace_json))
        .route(&p("/methods"), get(super::traces_web::methods_page))
        .route(&p("/upstreams"), get(super::upstreams::upstreams_page))
        .route(&p("/security"), get(super::threats::security_page))
        .route(&p("/frontend"), get(super::rum::frontend_page))
        .route(&p("/sql"), get(sql_page))
        .route(&p("/sql/{id}"), get(sql_show))
        .route(&p("/config"), get(config_page))
        .route(&p("/stream"), get(stream))
        .route(&p("/explain"), get(super::ai_web::explain_page))
        // Ask AI: each handler answers 404 unless `ai::allowed` (development, or production with
        // DASHBOARD_AI_IN_PRODUCTION=true behind the admin gate).
        .merge(super::ai_web::routes(base))
}

// ---------------------------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------------------------

pub struct Card {
    pub key: &'static str,
    pub label: &'static str,
    pub value: String,
    pub sub: String,
    pub tone: &'static str,
    pub spark: Spark,
    pub href: String,
}

pub struct TraceRow {
    pub id: u64,
    pub when: String,
    pub method: &'static str,
    pub route: &'static str,
    pub path: String,
    pub status: u16,
    pub tone: &'static str,
    pub dur: String,
    /// Bar width on a 0..100 scale (relative to the slowest in the list).
    pub bar: u64,
    pub sql: u32,
    pub n_plus_one: bool,
}

fn trace_rows(traces: &[Arc<Trace>]) -> Vec<TraceRow> {
    let now = Utc::now();
    let max = traces.iter().map(|t| t.dur_us).max().unwrap_or(1).max(1);
    traces
        .iter()
        .map(|t| TraceRow {
            id: t.id,
            when: ui::ago(t.at, now),
            method: t.method,
            route: t.route,
            path: t.path.clone(),
            status: t.status,
            tone: status_tone(t.status),
            dur: micros(t.dur_us),
            bar: (t.dur_us * 100 / max).max(1),
            sql: t.statements,
            n_plus_one: !t.n_plus_one.is_empty(),
        })
        .collect()
}

fn latest(range: Range) -> (Vec<Sample>, i64) {
    let now = Utc::now().timestamp();
    (lock(&HUB.series).window(range, now), now)
}

/// When a card turns amber and red. APP: your app's thresholds.
pub struct Health {
    pub cpu: (f32, f32),
    /// p95 latency, µs.
    pub p95: (u32, u32),
    /// Server errors as a share of requests.
    pub errors: (f32, f32),
    /// Pool connections in use as a share of the maximum.
    pub pool: (f32, f32),
    /// Resident memory growth since boot, bytes.
    pub memory_growth: (u64, u64),
    /// SQL p95, µs.
    pub sql_p95: (u32, u32),
}

pub const HEALTH: Health = Health {
    cpu: (70.0, 90.0),
    p95: (300_000, 1_000_000),
    errors: (0.01, 0.05),
    pool: (0.7, 0.9),
    memory_growth: (256 << 20, 1 << 30),
    sql_p95: (50_000, 250_000),
};

fn grade<T: PartialOrd>(v: T, (warn, err): (T, T)) -> &'static str {
    if v >= err {
        "err"
    } else if v >= warn {
        "warn"
    } else {
        "ok"
    }
}

/// The overview's cards: the last 10 seconds as the figure (one quiet second doesn't read as
/// zero), the last 5 minutes as the sparkline, the color from `HEALTH`.
pub fn cards(mode: Mode) -> Vec<Card> {
    let (w, _) = latest(Range::M5);
    let last = w.last().copied().unwrap_or_default();
    let recent: Vec<&Sample> = w.iter().rev().take(10).collect();
    let n = recent.len().max(1) as f32;
    let avg = |f: fn(&Sample) -> f32| recent.iter().map(|s| f(s)).sum::<f32>() / n;
    let busy: Vec<&&Sample> = recent.iter().filter(|s| s.req > 0.0).collect();
    let peak = |f: fn(&Sample) -> u32| busy.iter().map(|s| f(s)).max();
    let lat =
        |f: fn(&Sample) -> u32| peak(f).map_or_else(|| "—".to_owned(), |v| micros(u64::from(v)));
    let series = |f: fn(&Sample) -> f64| charts::sparkline(&w.iter().map(f).collect::<Vec<_>>());
    let b = mode.base;
    let uptime = (Utc::now() - *ui::BOOTED_AT).num_milliseconds();
    let idle = w.is_empty();
    let boot_rss = HUB.boot_rss.load(std::sync::atomic::Ordering::Relaxed);
    let growth = last.rss.saturating_sub(boot_rss);
    let req = avg(|s| s.req);
    let e5 = avg(|s| s.e5);
    let in_use = last.pool_in_use() as f32 / last.pool_max.max(1) as f32;
    let tone = |t: &'static str| if idle { "idle" } else { t };
    vec![
        Card {
            key: "memory",
            label: "Memory",
            value: if idle { "—".into() } else { bytes(last.rss) },
            sub: if boot_rss == 0 {
                "resident".into()
            } else if last.rss >= boot_rss {
                format!("+{} since boot", bytes(growth))
            } else {
                format!("−{} since boot", bytes(boot_rss - last.rss))
            },
            tone: tone(grade(growth, HEALTH.memory_growth)),
            spark: series(|s| s.rss as f64),
            href: format!("{b}/charts#chart-memory"),
        },
        Card {
            key: "cpu",
            label: "CPU",
            value: if idle {
                "—".into()
            } else {
                format!("{:.0}%", avg(|s| s.cpu))
            },
            sub: format!("Tokio busy {:.0}%", avg(|s| s.busy)),
            tone: tone(grade(avg(|s| s.cpu), HEALTH.cpu)),
            spark: series(|s| f64::from(s.cpu)),
            href: format!("{b}/charts#chart-cpu"),
        },
        Card {
            key: "req",
            label: "Requests",
            value: if idle {
                "—".into()
            } else {
                charts::Unit::PerSecond.format(f64::from(req))
            },
            sub: format!(
                "{} in 5 min",
                ui::thousands(w.iter().map(|s| s.req as u64).sum())
            ),
            tone: tone("ok"),
            spark: series(|s| f64::from(s.req)),
            href: format!("{b}/requests"),
        },
        Card {
            key: "latency",
            label: "Latency p95",
            value: lat(|s| s.p95),
            sub: format!("p50 {} · p99 {}", lat(|s| s.p50), lat(|s| s.p99)),
            tone: match peak(|s| s.p95) {
                Some(v) => grade(v, HEALTH.p95),
                None => "idle",
            },
            spark: series(|s| f64::from(s.p95)),
            href: format!("{b}/charts#chart-latency"),
        },
        Card {
            key: "errors",
            label: "Server errors",
            value: if idle {
                "—".into()
            } else {
                charts::Unit::PerSecond.format(f64::from(e5))
            },
            sub: format!(
                "{} 5xx · {} 4xx in 5 min",
                w.iter().map(|s| s.e5 as u64).sum::<u64>(),
                w.iter().map(|s| s.e4 as u64).sum::<u64>()
            ),
            tone: tone(if req > 0.0 {
                grade(e5 / req, HEALTH.errors)
            } else if e5 > 0.0 {
                "err"
            } else {
                "ok"
            }),
            spark: series(|s| f64::from(s.e5)),
            href: format!("{b}/requests?sort=errors"),
        },
        Card {
            key: "sql",
            label: "SQL",
            value: if idle {
                "—".into()
            } else {
                charts::Unit::PerSecond.format(f64::from(avg(|s| s.sql)))
            },
            sub: format!(
                "p95 {}",
                recent
                    .iter()
                    .map(|s| s.sql_p95)
                    .max()
                    .filter(|v| *v > 0)
                    .map_or_else(|| "—".to_owned(), |v| micros(u64::from(v)))
            ),
            tone: tone(grade(
                recent.iter().map(|s| s.sql_p95).max().unwrap_or(0),
                HEALTH.sql_p95,
            )),
            spark: series(|s| f64::from(s.sql)),
            href: format!("{b}/sql"),
        },
        Card {
            key: "pool",
            label: "DB pool",
            value: if idle {
                "—".into()
            } else {
                format!("{} / {}", last.pool_in_use(), last.pool_max)
            },
            sub: format!("{} open · {} idle", last.pool_size, last.pool_idle),
            tone: tone(grade(in_use, HEALTH.pool)),
            spark: series(|s| f64::from(s.pool_in_use())),
            href: format!("{b}/charts#chart-pool"),
        },
        Card {
            key: "tasks",
            label: "Tokio tasks",
            value: if idle {
                "—".into()
            } else {
                ui::thousands(u64::from(last.tasks))
            },
            sub: format!("{} workers · queue {}", last.workers, last.queue),
            tone: tone(if last.queue > 100 { "warn" } else { "ok" }),
            spark: series(|s| f64::from(s.tasks)),
            href: format!("{b}/charts#chart-tasks"),
        },
        Card {
            key: "uptime",
            label: "Uptime",
            value: ui::duration(uptime),
            sub: match last.fds {
                Some(n) => format!("{n} open files"),
                None => "open files: n/a on this OS".into(),
            },
            tone: "ok",
            spark: charts::sparkline(
                &w.iter()
                    .filter_map(|s| s.fds.map(f64::from))
                    .collect::<Vec<_>>(),
            ),
            href: format!("{b}/config"),
        },
    ]
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_cards.html")]
struct CardsFragment {
    cards: Vec<Card>,
}

/// The overview's incident cards, live (an investigation's status and story as they arrive).
#[derive(Template)]
#[template(path = "kernel/dashboard/_incidents_panel.html")]
struct IncidentsPanel {
    incidents: Vec<super::ai_web::IncidentView>,
    base: &'static str,
}

/// The insights page's incident cards, live.
#[derive(Template)]
#[template(path = "kernel/dashboard/_incidents_box.html")]
struct IncidentsBox {
    incidents: Vec<super::ai_web::IncidentView>,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/overview.html")]
struct OverviewPage {
    chrome: Chrome,
    d: Dash,
    cards: Vec<Card>,
    panels: Vec<panels::Panel>,
    slow_routes: Vec<RequestRow>,
    slow_sql: Vec<SqlView>,
    n_plus_one: Vec<NPlusOneRow>,
    traces: Vec<TraceRow>,
    slow_fns: Vec<SlowFn>,
    findings: Vec<FindingView>,
    finding_total: usize,
    cc: super::ai_home::CommandCenter,
    digest: super::ai_watch::Digest,
    incidents: Vec<super::ai_web::IncidentView>,
}

/// A row of the overview's "Slowest functions".
pub struct SlowFn {
    pub name: &'static str,
    pub module: &'static str,
    pub p95: String,
    pub calls: u64,
    pub href: String,
}

fn slow_fns(mode: Mode) -> Vec<SlowFn> {
    let (mut stats, _) = super::traces_web::method_stats();
    stats.retain(|m| !matches!(m.kind, "handler" | "middleware" | "job"));
    stats.sort_by_key(|m| std::cmp::Reverse(m.p95()));
    stats
        .into_iter()
        .take(5)
        .map(|m| SlowFn {
            name: m.name,
            module: m.module,
            p95: micros(m.p95()),
            calls: m.calls,
            href: format!("{}/traces?fn={}", mode.base, cookie::encode(m.name)),
        })
        .collect()
}

pub struct NPlusOneRow {
    pub route: &'static str,
    pub sql: String,
    pub count: u32,
    pub times: u64,
    pub trace: u64,
    pub fp: String,
    pub when: String,
}

fn n_plus_one_rows(limit: usize) -> Vec<NPlusOneRow> {
    let now = Utc::now();
    lock(&HUB.sql)
        .n_plus_one()
        .into_iter()
        .take(limit)
        .map(|n| NPlusOneRow {
            route: n.route,
            sql: n.sql.to_string(),
            count: n.count,
            times: n.times,
            trace: n.trace,
            fp: sql::hex(n.fp),
            when: ui::ago(n.last, now),
        })
        .collect()
}

async fn overview(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    headers: HeaderMap,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Result<Response, AppError> {
    let mut slow_routes = request_rows("p95");
    slow_routes.truncate(5);
    let slow_sql = sql_views(
        lock(&HUB.sql)
            .rows(sql::Sort::P95)
            .into_iter()
            .take(5)
            .collect(),
        mode.ro,
    );
    let traces = trace_rows(
        &lock(&HUB.traces)
            .list(false)
            .into_iter()
            .take(8)
            .collect::<Vec<_>>(),
    );
    let (findings, finding_total, brief) = if mode.ro {
        (Vec::new(), 0, Vec::new())
    } else {
        let dismissed = dismissed(&headers);
        let all: Vec<FindingView> = all_findings(&state, &dismissed)
            .await
            .into_iter()
            .filter(|f| !f.dismissed)
            .collect();
        let n = all.len();
        let brief = all.iter().map(super::ai_home::finding_of).collect();
        (all.into_iter().take(3).collect(), n, brief)
    };
    let cards = cards(mode);
    let owner = cookie::get(&headers, super::ai_agent::OWNER_COOKIE);
    let cc = super::ai_home::build(&state, mode, brief, owner.as_deref()).await;
    // "Since you were away": this browser's last visit (a cookie), then now.
    let (seen, seen_cookie) = super::ai_web::seen(&headers, state.config.is_production());
    let digest = super::ai_watch::digest(seen, Utc::now().timestamp());
    let mut res = ui::page(OverviewPage {
        chrome: chrome(&state, "Dashboard", "dashboard", flash, theme),
        d: dash(
            mode,
            "",
            Some(format!("{}/stream?view=overview", mode.base)),
            1,
        ),
        cards,
        panels: if mode.ro {
            Vec::new()
        } else {
            panels::panels(&state).await
        },
        slow_routes,
        slow_sql,
        n_plus_one: n_plus_one_rows(5),
        traces,
        slow_fns: slow_fns(mode),
        findings,
        finding_total,
        cc,
        digest,
        incidents: if mode.ro {
            Vec::new()
        } else {
            super::ai_web::incident_views(3)
        },
    });
    res.headers_mut().append(header::SET_COOKIE, seen_cookie);
    Ok(res)
}

// ---------------------------------------------------------------------------------------------
// Charts
// ---------------------------------------------------------------------------------------------

/// The charts' window: the last `range` (live), or a zoomed `from`..`to` within the history kept.
/// Memory answers the short ranges; the history file the rest (`history::between`), merged so the
/// hot/cold boundary has no seam.
pub fn window(range: Range, from: Option<i64>, to: Option<i64>) -> (Window, bool) {
    let now = Utc::now().timestamp();
    let oldest = now - super::history::HOUR_DAYS * 86_400;
    let zoom = match (from, to) {
        (Some(f), Some(t)) if t - f >= 10 && f >= oldest && t <= now + 60 => Some((f, t)),
        _ => None,
    };
    let (from, to, slice, live) = match zoom {
        Some((f, t)) => (f, t, super::history::between(f, t), false),
        None => (
            now - range.seconds(),
            now,
            super::history::window(range, now),
            true,
        ),
    };
    let w = Window {
        marks: slice.marks,
        demo: slice.demo,
        samples: charts::downsample(&slice.samples, 300),
        // Legends summarize every sample read, not the drawn (peak-preserving) points.
        full: slice.samples,
        from,
        to,
    };
    (w, live)
}

/// Every chart of a window.
pub fn all_charts(win: &Window, live: bool) -> Vec<ChartView> {
    let w = &win.samples;
    let c = |key, title, unit, series: &[SeriesSpec]| {
        charts::line_chart(key, title, unit, win, series, live)
    };
    let mut out = vec![
        c(
            "memory",
            "Memory (resident)",
            Unit::Bytes,
            &[SeriesSpec {
                name: "RSS",
                value: |s| s.rss as f64,
            }],
        ),
        c(
            "cpu",
            "CPU",
            Unit::Percent,
            &[
                SeriesSpec {
                    name: "process",
                    value: |s| f64::from(s.cpu),
                },
                SeriesSpec {
                    name: "Tokio busy",
                    value: |s| f64::from(s.busy),
                },
            ],
        ),
        c(
            "req",
            "Requests",
            Unit::PerSecond,
            &[SeriesSpec {
                name: "req/s",
                value: |s| f64::from(s.req),
            }],
        ),
        c(
            "latency",
            "Latency",
            Unit::Micros,
            &[
                SeriesSpec {
                    name: "p50",
                    value: |s| f64::from(s.p50),
                },
                SeriesSpec {
                    name: "p95",
                    value: |s| f64::from(s.p95),
                },
                SeriesSpec {
                    name: "p99",
                    value: |s| f64::from(s.p99),
                },
            ],
        ),
        c(
            "errors",
            "Errors by status",
            Unit::PerSecond,
            &[
                SeriesSpec {
                    name: "4xx",
                    value: |s| f64::from(s.e4),
                },
                SeriesSpec {
                    name: "5xx",
                    value: |s| f64::from(s.e5),
                },
            ],
        ),
        c(
            "pool",
            "Database pool",
            Unit::Count,
            &[
                SeriesSpec {
                    name: "in use",
                    value: |s| f64::from(s.pool_in_use()),
                },
                SeriesSpec {
                    name: "idle",
                    value: |s| f64::from(s.pool_idle),
                },
                SeriesSpec {
                    name: "max",
                    value: |s| f64::from(s.pool_max),
                },
            ],
        ),
        c(
            "tasks",
            "Tokio tasks",
            Unit::Count,
            &[
                SeriesSpec {
                    name: "alive",
                    value: |s| f64::from(s.tasks),
                },
                SeriesSpec {
                    name: "global queue",
                    value: |s| f64::from(s.queue),
                },
            ],
        ),
        c(
            "sql",
            "SQL",
            Unit::PerSecond,
            &[SeriesSpec {
                name: "statements/s",
                value: |s| f64::from(s.sql),
            }],
        ),
        c(
            "sqlp95",
            "SQL latency p95",
            Unit::Micros,
            &[SeriesSpec {
                name: "p95",
                value: |s| f64::from(s.sql_p95),
            }],
        ),
    ];
    if w.iter().any(|s| s.fds.is_some()) {
        out.push(c(
            "fds",
            "Open files",
            Unit::Count,
            &[SeriesSpec {
                name: "descriptors",
                value: |s| s.fds.map_or(0.0, f64::from),
            }],
        ));
    }
    out
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_charts.html")]
struct ChartsFragment {
    charts: Vec<ChartView>,
    heat: Heatmap,
}

pub struct RangeLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/charts.html")]
struct ChartsPage {
    chrome: Chrome,
    d: Dash,
    ranges: Vec<RangeLink>,
    range: String,
    zoomed: bool,
    reset: String,
    charts: Vec<ChartView>,
    heat: Heatmap,
    marks: Vec<charts::Mark>,
}

#[derive(Debug, Default, Deserialize)]
struct RangeQuery {
    range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}

async fn charts_page(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    Query(q): Query<RangeQuery>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let range = Range::parse(q.range.as_deref());
    let (win, live) = window(range, q.from, q.to);
    let charts = all_charts(&win, live);
    let marks = charts
        .first()
        .map(|c| {
            c.marks
                .iter()
                .map(|m| charts::Mark {
                    x: m.x.clone(),
                    kind: m.kind,
                    label: m.label.clone(),
                    when: m.when.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let heat = charts::heatmap(&win, live);
    let label = if live {
        format!("last {}", range.label())
    } else {
        format!(
            "{} – {} UTC",
            chrono::DateTime::from_timestamp(win.from, 0)
                .map(|d| d.format("%H:%M:%S").to_string())
                .unwrap_or_default(),
            chrono::DateTime::from_timestamp(win.to, 0)
                .map(|d| d.format("%H:%M:%S").to_string())
                .unwrap_or_default()
        )
    };
    ui::page(ChartsPage {
        chrome: chrome(&state, "Charts", "dashboard", flash, theme),
        d: dash(
            mode,
            "/charts",
            live.then(|| format!("{}/stream?view=charts&range={}", mode.base, range.key())),
            range.refresh(),
        )
        .ai(&state.config, ai::Tab::Overview, None),
        ranges: Range::ALL
            .iter()
            .map(|r| RangeLink {
                label: r.label(),
                href: format!("{}/charts?range={}", mode.base, r.key()),
                current: live && *r == range,
            })
            .collect(),
        range: label,
        zoomed: !live,
        reset: format!("{}/charts?range={}", mode.base, range.key()),
        charts,
        heat,
        marks,
    })
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

pub struct RequestRow {
    pub method: &'static str,
    pub route: &'static str,
    pub count: String,
    pub errors: String,
    pub error_tone: &'static str,
    pub s2: u64,
    pub s3: u64,
    pub s4: u64,
    pub s5: u64,
    pub p50: String,
    pub p95: String,
    pub p99: String,
    pub max: String,
    pub bytes: String,
    pub last: String,
    pub slowest: Vec<(u64, String, &'static str)>,
    pub n_plus_one: u64,
    /// Bar width for p95 on 0..100.
    pub bar: u64,
}

fn request_rows(sort: &str) -> Vec<RequestRow> {
    let mut rows = lock(&HUB.http).rows();
    match sort {
        "p95" => rows.sort_by_key(|x| std::cmp::Reverse(x.p95)),
        "errors" => rows.sort_by(|a, b| {
            (b.s5 + b.s4)
                .cmp(&(a.s5 + a.s4))
                .then(b.count.cmp(&a.count))
        }),
        "bytes" => rows.sort_by_key(|x| std::cmp::Reverse(x.bytes)),
        "recent" => rows.sort_by_key(|x| std::cmp::Reverse(x.last)),
        _ => {}
    }
    let max = rows.iter().map(|r| r.p95).max().unwrap_or(1).max(1);
    let now = Utc::now();
    rows.into_iter()
        .map(|r| RequestRow {
            method: r.method,
            route: r.route,
            count: ui::thousands(r.count),
            errors: pct(r.s4 + r.s5, r.count),
            error_tone: if r.s5 > 0 {
                "err"
            } else if r.s4 > 0 {
                "warn"
            } else {
                "muted"
            },
            s2: r.s2,
            s3: r.s3,
            s4: r.s4,
            s5: r.s5,
            p50: micros(r.p50),
            p95: micros(r.p95),
            p99: micros(r.p99),
            max: micros(r.max),
            bytes: bytes(r.bytes),
            last: ui::ago(r.last, now),
            slowest: r
                .slowest
                .iter()
                .map(|(id, us, st)| (*id, micros(*us), status_tone(*st)))
                .collect(),
            n_plus_one: r.n_plus_one,
            bar: (r.p95 * 100 / max).max(1),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_requests.html")]
struct RequestsFragment {
    rows: Vec<RequestRow>,
    base: &'static str,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/requests.html")]
struct RequestsPage {
    chrome: Chrome,
    d: Dash,
    rows: Vec<RequestRow>,
    base: &'static str,
    sorts: Vec<RangeLink>,
}

#[derive(Debug, Default, Deserialize)]
struct SortQuery {
    sort: Option<String>,
}

fn sort_links(
    base: &str,
    path: &str,
    current: &str,
    options: &[(&'static str, &str)],
) -> Vec<RangeLink> {
    options
        .iter()
        .map(|(label, key)| RangeLink {
            label,
            href: format!("{base}{path}?sort={key}"),
            current: *key == current,
        })
        .collect()
}

async fn requests_page(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    Query(q): Query<SortQuery>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let sort = q.sort.unwrap_or_else(|| "count".into());
    ui::page(RequestsPage {
        chrome: chrome(&state, "Requests", "dashboard", flash, theme),
        d: dash(
            mode,
            "/requests",
            Some(format!("{}/stream?view=requests&sort={sort}", mode.base)),
            2,
        )
        .ai(&state.config, ai::Tab::Requests, None),
        rows: request_rows(&sort),
        base: mode.base,
        sorts: sort_links(
            mode.base,
            "/requests",
            &sort,
            &[
                ("Busiest", "count"),
                ("Slowest p95", "p95"),
                ("Most errors", "errors"),
                ("Most bytes", "bytes"),
                ("Recent", "recent"),
            ],
        ),
    })
}

// ---------------------------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------------------------

pub struct SqlView {
    pub id: String,
    pub sql: String,
    pub count: String,
    pub total: String,
    pub mean: String,
    pub p95: String,
    pub max: String,
    pub last: String,
    pub routes: Vec<&'static str>,
    pub params: String,
    pub failed: u64,
    pub select: bool,
    pub bar: u64,
}

fn sql_views(rows: Vec<sql::SqlRow>, ro: bool) -> Vec<SqlView> {
    let now = Utc::now();
    let max = rows.iter().map(|r| r.total).max().unwrap_or(1).max(1);
    rows.into_iter()
        .map(|r| SqlView {
            bar: (r.total * 100 / max).max(1),
            id: r.id,
            sql: r.sql.to_string(),
            count: ui::thousands(r.count),
            total: micros(r.total),
            mean: micros(r.mean),
            p95: micros(r.p95),
            max: micros(r.max),
            last: ui::ago(r.last, now),
            routes: r.routes,
            params: if ro {
                String::new()
            } else {
                r.params.join(", ")
            },
            failed: r.failed,
            select: r.select,
        })
        .collect()
}

#[derive(Template)]
#[template(path = "kernel/dashboard/sql.html")]
struct SqlPage {
    chrome: Chrome,
    d: Dash,
    rows: Vec<SqlView>,
    n_plus_one: Vec<NPlusOneRow>,
    sorts: Vec<RangeLink>,
    base: &'static str,
    route: String,
    gets_that_write: Vec<(String, String, u64, u64)>,
}

#[derive(Debug, Default, Deserialize)]
struct SqlQuery {
    sort: Option<String>,
    route: Option<String>,
}

async fn sql_page(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    Query(q): Query<SqlQuery>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let sort = sql::Sort::parse(q.sort.as_deref());
    let route = q.route.unwrap_or_default();
    let rows: Vec<sql::SqlRow> = lock(&HUB.sql)
        .rows(sort)
        .into_iter()
        .filter(|r| route.is_empty() || r.routes.iter().any(|x| *x == route))
        .take(200)
        .collect();
    let writes = lock(&HUB.http)
        .get_writes()
        .iter()
        .map(|w| (w.route.to_owned(), w.sql.to_string(), w.count, w.trace))
        .collect();
    ui::page(SqlPage {
        chrome: chrome(&state, "SQL", "dashboard", flash, theme),
        d: dash(mode, "/sql", None, 0).ai(&state.config, ai::Tab::Sql, None),
        rows: sql_views(rows, mode.ro),
        n_plus_one: n_plus_one_rows(50),
        sorts: sort_links(
            mode.base,
            "/sql",
            sort.key(),
            &[
                ("Total time", "total"),
                ("p95", "p95"),
                ("Max", "max"),
                ("Count", "count"),
                ("Recent", "recent"),
            ],
        ),
        base: mode.base,
        route,
        gets_that_write: writes,
    })
}

#[derive(Template)]
#[template(path = "kernel/dashboard/sql_show.html")]
struct SqlShowPage {
    chrome: Chrome,
    d: Dash,
    q: SqlView,
    plan: Option<Vec<String>>,
    plan_error: Option<String>,
    list: String,
    backend: &'static str,
    base: &'static str,
}

async fn sql_detail(
    state: &AppState,
    mode: Mode,
    id: &str,
    plan: Option<Result<Vec<String>, sql::ExplainError>>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let Some(fp) = sql::parse_hex(id) else {
        return back(
            &format!("{}/sql", mode.base),
            Flash::alert("No such query."),
        );
    };
    let Some(row) = lock(&HUB.sql).get(fp) else {
        return back(
            &format!("{}/sql", mode.base),
            Flash::alert("That query is no longer kept."),
        );
    };
    let traces: Vec<Arc<Trace>> = lock(&HUB.traces)
        .list(false)
        .into_iter()
        .filter(|t| t.sql.iter().any(|s| s.fp == fp))
        .take(20)
        .collect();
    let (plan, plan_error) = match plan {
        Some(Ok(p)) => (Some(p), None),
        Some(Err(sql::ExplainError::NotExplainable)) => (
            None,
            Some(
                "Only SELECTs with parameters recorded in development can be explained.".to_owned(),
            ),
        ),
        Some(Err(sql::ExplainError::Gone)) => {
            (None, Some("That query is no longer kept.".to_owned()))
        }
        Some(Err(sql::ExplainError::Failed(e))) => (None, Some(format!("EXPLAIN failed: {e}"))),
        None => (None, None),
    };
    let backend = match state.db.get_database_backend() {
        sea_orm::DbBackend::Postgres => "Postgres EXPLAIN",
        _ => "SQLite EXPLAIN QUERY PLAN",
    };
    let mut views = sql_views(vec![row], mode.ro);
    let Some(q) = views.pop() else {
        return back(
            &format!("{}/sql", mode.base),
            Flash::alert("No such query."),
        );
    };
    ui::page(SqlShowPage {
        chrome: chrome(state, "Query", "dashboard", flash, theme),
        d: dash(mode, "/sql", None, 0).ai(&state.config, ai::Tab::Sql, Some(q.id.clone())),
        q,
        plan,
        plan_error,
        list: super::traces_web::list_html(mode, &traces),
        backend,
        base: mode.base,
    })
}

async fn sql_show(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    Path(id): Path<String>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    sql_detail(&state, mode, &id, None, theme, flash).await
}

/// `POST /dev/dashboard/sql/{id}/explain`: the plan, on the query's own page.
async fn explain(
    State(state): State<AppState>,
    Path(id): Path<String>,
    theme: ThemeSwitch,
) -> Response {
    let plan = match sql::parse_hex(&id) {
        Some(fp) => sql::explain(&state.db, fp).await,
        None => Err(sql::ExplainError::Gone),
    };
    sql_detail(&state, DEV, &id, Some(plan), theme, None).await
}

// ---------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Clone)]
pub struct LogFilter {
    #[serde(default)]
    level: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    kind: String,
}

impl LogFilter {
    fn matches(&self, l: &LogLine) -> bool {
        let min = match self.level.as_str() {
            "error" => 5,
            "warn" => 4,
            "info" => 3,
            "debug" => 2,
            _ => 0,
        };
        LogLine::level_rank(l.level) >= min
            && (self.target.is_empty() || l.target.contains(self.target.trim()))
            && (self.q.is_empty()
                || l.message.to_lowercase().contains(&self.q.to_lowercase())
                || l.fields.to_lowercase().contains(&self.q.to_lowercase()))
            && match self.kind.as_str() {
                "requests" => l.request.is_some(),
                "logs" => l.request.is_none(),
                _ => true,
            }
    }

    fn query(&self) -> String {
        serde_html_form::to_string([
            ("level", &self.level),
            ("target", &self.target),
            ("q", &self.q),
            ("kind", &self.kind),
        ])
        .unwrap_or_default()
    }
}

pub struct LogView {
    pub id: u64,
    pub time: String,
    pub level: &'static str,
    pub level_class: String,
    pub target: String,
    pub message: String,
    pub fields: String,
    pub trace: Option<u64>,
    pub status_tone: &'static str,
}

fn log_view(l: &LogLine) -> LogView {
    LogView {
        id: l.id,
        time: l.at.format("%H:%M:%S%.3f").to_string(),
        level: l.level,
        level_class: l.level.to_lowercase(),
        target: l.target.clone(),
        message: l.message.clone(),
        fields: l.fields.clone(),
        trace: l.trace,
        status_tone: l.request.map_or("", |(s, _)| status_tone(s)),
    }
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_log_row.html")]
struct LogRowFragment {
    l: LogView,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/logs.html")]
struct LogsPage {
    chrome: Chrome,
    d: Dash,
    lines: Vec<LogView>,
    f: LogFilter,
    stream: String,
    levels: Vec<(&'static str, &'static str)>,
}

async fn logs_page(
    State(state): State<AppState>,
    Query(f): Query<LogFilter>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let lines: Vec<LogView> = lock(&HUB.logs)
        .lines()
        .iter()
        .filter(|l| f.matches(l))
        .rev()
        .take(300)
        .map(|l| log_view(l))
        .collect();
    let since = lines.first().map_or(0, |l| l.id);
    ui::page(LogsPage {
        chrome: chrome(&state, "Logs", "dashboard", flash, theme),
        d: dash(DEV, "/logs", None, 0).ai(&state.config, ai::Tab::Logs, None),
        stream: format!("/dev/dashboard/logs/stream?{}&since={since}", f.query()),
        lines,
        f,
        levels: vec![
            ("", "Any level"),
            ("error", "Error"),
            ("warn", "Warn and up"),
            ("info", "Info and up"),
            ("debug", "Debug and up"),
        ],
    })
}

#[derive(Debug, Default, Deserialize)]
struct LogStreamQuery {
    #[serde(flatten)]
    f: LogFilter,
    since: Option<u64>,
}

/// `GET /dev/dashboard/logs/stream`: Server-Sent Events, one `log` event per line (a rendered
/// row). Resumes after `Last-Event-ID`. A slow tab skips lines; it never slows the app.
async fn logs_stream(
    headers: HeaderMap,
    Query(q): Query<LogStreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or(q.since)
        .unwrap_or(0);
    let filter = q.f;
    let rx = HUB.log_tx.subscribe();
    let backlog: Vec<Arc<LogLine>> = lock(&HUB.logs)
        .lines()
        .into_iter()
        .filter(|l| l.id > resume && filter.matches(l))
        .collect();
    let first = futures::stream::iter(backlog.into_iter().map(Some));
    let live = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(line) => return Some((Some(line), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    use futures::StreamExt;
    let stream = futures::stream::once(async { None })
        .chain(first)
        .chain(live)
        .filter_map(move |line| {
            let filter = filter.clone();
            async move {
                match line {
                    None => Some(Ok(Event::default().comment("live"))),
                    Some(l) if filter.matches(&l) => {
                        let html = LogRowFragment { l: log_view(&l) }.render().ok()?;
                        Some(Ok(Event::default()
                            .event("log")
                            .id(l.id.to_string())
                            .data(html)))
                    }
                    Some(_) => None,
                }
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---------------------------------------------------------------------------------------------
// The live stream for the other pages
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct StreamQuery {
    view: Option<String>,
    range: Option<String>,
    sort: Option<String>,
    /// The traces explorer's own query string (its filters).
    tq: Option<String>,
}

/// The fragments of a view, by `data-live` key.
fn fragments(mode: Mode, q: &StreamQuery) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match q.view.as_deref() {
        Some("overview") => {
            if let Ok(h) = (CardsFragment { cards: cards(mode) }).render() {
                out.push(("cards", h));
            }
            if !mode.ro
                && let Ok(h) = (IncidentsPanel {
                    incidents: super::ai_web::incident_views(3),
                    base: mode.base,
                })
                .render()
            {
                out.push(("incidents", h));
            }
        }
        Some("insights") if !mode.ro => {
            if let Ok(h) = (IncidentsBox {
                incidents: super::ai_web::incident_views(super::ai_watch::INCIDENTS),
            })
            .render()
            {
                out.push(("incidents", h));
            }
        }
        Some("charts") => {
            let (win, live) = window(Range::parse(q.range.as_deref()), None, None);
            if let Ok(h) = (ChartsFragment {
                charts: all_charts(&win, live),
                heat: charts::heatmap(&win, live),
            })
            .render()
            {
                out.push(("charts", h));
            }
        }
        Some("requests") => {
            if let Ok(h) = (RequestsFragment {
                rows: request_rows(q.sort.as_deref().unwrap_or("count")),
                base: mode.base,
            })
            .render()
            {
                out.push(("requests", h));
            }
        }
        Some("traces") => {
            if let Some(h) = super::traces_web::live_fragment(mode, q.tq.as_deref()) {
                out.push(("traces", h));
            }
        }
        Some("lab") if !mode.ro => {
            if let Ok(h) = lab_status().render() {
                out.push(("lab", h));
            }
            if let Ok(h) = (CardsFragment { cards: cards(mode) }).render() {
                out.push(("cards", h));
            }
        }
        Some("tests") if !mode.ro => {
            if let Ok(h) = test_progress().render() {
                out.push(("tests", h));
            }
        }
        Some("config") => {
            if let Ok(h) = (MemoryFragment { m: memory_view() }).render() {
                out.push(("memory", h));
            }
        }
        _ => {}
    }
    out
}

/// `GET …/stream?view=…`: Server-Sent Events with re-rendered fragments, sent when they change.
async fn stream(
    Extension(mode): Extension<Mode>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let every = Duration::from_secs(Range::parse(q.range.as_deref()).refresh());
    let state = (q, Vec::<(&'static str, u64)>::new(), true);
    let s = futures::stream::unfold(state, move |(q, mut sent, first)| async move {
        if first {
            return Some((vec![Event::default().comment("live")], (q, sent, false)));
        }
        tokio::time::sleep(every).await;
        let mut events = Vec::new();
        for (key, html) in fragments(mode, &q) {
            let hash = sql::fingerprint_id(&html);
            if sent.iter().any(|(k, h)| *k == key && *h == hash) {
                continue;
            }
            sent.retain(|(k, _)| *k != key);
            sent.push((key, hash));
            events.push(Event::default().event(key).data(html));
        }
        Some((events, (q, sent, false)))
    });
    use futures::StreamExt;
    let s = s.flat_map(|events| futures::stream::iter(events.into_iter().map(Ok)));
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `GET /dev/dashboard.json`: the numbers, for agents.
async fn json() -> Response {
    Json(super::snapshot()).into_response()
}

// ---------------------------------------------------------------------------------------------
// Insights
// ---------------------------------------------------------------------------------------------

pub struct FindingView {
    pub key: String,
    pub id: String,
    pub kind: &'static str,
    pub severity: &'static str,
    pub title: String,
    pub evidence: String,
    pub fix: String,
    pub guide: &'static str,
    pub guide_url: &'static str,
    pub href: String,
    /// The trace its evidence points at (an N+1, a slow route): its waterfall opens inline.
    pub trace: String,
    pub dismissed: bool,
}

const DISMISSED_COOKIE: &str = "dashboard_dismissed";

pub fn finding_id(key: &str) -> String {
    format!("{:08x}", sql::fingerprint_id(key) as u32)
}

pub(super) fn dismissed(headers: &HeaderMap) -> HashSet<String> {
    cookie::get(headers, DISMISSED_COOKIE)
        .map(|v| {
            v.split('.')
                .filter(|s| s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn finding_view(f: insights::Finding, dismissed: &HashSet<String>) -> FindingView {
    let id = finding_id(&f.key);
    FindingView {
        dismissed: dismissed.contains(&id),
        id,
        key: f.key,
        kind: f.kind,
        severity: f.severity.key(),
        title: f.title,
        evidence: f.evidence,
        // The hints are written with `code` marks for the source; the page shows plain text.
        fix: f.fix.replace('`', ""),
        guide: f.guide,
        guide_url: f.guide_url,
        trace: f
            .href
            .strip_prefix("/dev/dashboard/traces/")
            .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or_default()
            .to_owned(),
        href: f.href,
    }
}

/// Every finding (the app doctor's and the route detectors'), highest severity first.
pub(super) async fn all_findings(
    state: &AppState,
    dismissed: &HashSet<String>,
) -> Vec<FindingView> {
    let mut all: Vec<insights::Finding> = insights::findings(state).await;
    all.extend(route_findings(state).await);
    all.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.score.cmp(&a.score)));
    all.into_iter()
        .map(|f| finding_view(f, dismissed))
        .collect()
}

pub struct CspView {
    pub page: String,
    pub directive: String,
    pub blocked: String,
    pub source: String,
    pub count: u64,
    pub when: String,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/insights.html")]
struct InsightsPage {
    chrome: Chrome,
    d: Dash,
    findings: Vec<FindingView>,
    hidden: usize,
    counts: (usize, usize, usize),
    csp: Vec<CspView>,
    incidents: Vec<super::ai_web::IncidentView>,
    watching: bool,
}

async fn insights_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let dismissed = dismissed(&headers);
    let all = all_findings(&state, &dismissed).await;
    let hidden = all.iter().filter(|f| f.dismissed).count();
    let findings: Vec<FindingView> = all.into_iter().filter(|f| !f.dismissed).collect();
    let counts = (
        findings.iter().filter(|f| f.severity == "high").count(),
        findings.iter().filter(|f| f.severity == "medium").count(),
        findings.iter().filter(|f| f.severity == "low").count(),
    );
    let now = Utc::now();
    let csp = lock(&HUB.insights.csp)
        .iter()
        .rev()
        .map(|r| CspView {
            page: r.page.clone(),
            directive: r.directive.clone(),
            blocked: r.blocked.clone(),
            source: r.source.clone(),
            count: r.count,
            when: ui::ago(r.last, now),
        })
        .collect();
    ui::page(InsightsPage {
        chrome: chrome(&state, "Insights", "dashboard", flash, theme),
        d: dash(
            DEV,
            "/insights",
            Some("/dev/dashboard/stream?view=insights".into()),
            2,
        )
        .ai(&state.config, ai::Tab::Insights, None),
        findings,
        hidden,
        counts,
        csp,
        incidents: super::ai_web::incident_views(super::ai_watch::INCIDENTS),
        watching: super::ai_watch::enabled(&state),
    })
}

#[derive(Debug, Deserialize)]
struct DismissForm {
    id: Option<String>,
    restore: Option<String>,
}

/// `POST /dev/dashboard/insights/dismiss`: hides a finding for this browser session (a session
/// cookie of finding ids), or brings them all back.
async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<DismissForm>,
) -> Response {
    let mut set = dismissed(&headers);
    let secure = state.config.is_production();
    if f.restore.is_some() {
        let mut res = back(
            "/dev/dashboard/insights",
            Flash::notice("Every finding is back."),
        );
        res.headers_mut()
            .append(header::SET_COOKIE, cookie::clear(DISMISSED_COOKIE, secure));
        return res;
    }
    if let Some(id) =
        f.id.filter(|i| i.len() == 8 && i.chars().all(|c| c.is_ascii_hexdigit()))
    {
        set.insert(id);
    }
    let mut ids: Vec<String> = set.into_iter().collect();
    ids.sort();
    ids.truncate(64);
    let mut res = back(
        "/dev/dashboard/insights",
        Flash::notice("Dismissed for this session."),
    );
    res.headers_mut().append(
        header::SET_COOKIE,
        cookie::set(DISMISSED_COOKIE, &ids.join("."), None, secure),
    );
    res
}

/// `POST /dev/csp-reports`: violation reports from the browser (the CSP's `report-uri`, added in
/// development through the `dev csp` anchor). 204, always.
async fn csp_report(body: axum::body::Bytes) -> StatusCode {
    // The global body limit applies; reports are small, and only the first 64 KB are read.
    let n = body.len().min(64 * 1024);
    if insights::record_csp(&body[..n]) > 0 {
        super::threats::csp_report();
    }
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------------------------
// Routes explorer
// ---------------------------------------------------------------------------------------------

pub struct RouteView {
    pub method: &'static str,
    pub path: &'static str,
    pub handler: &'static str,
    pub source: String,
    pub access: &'static str,
    pub middleware: Vec<&'static str>,
    pub hits: String,
    pub errors: String,
    pub error_tone: &'static str,
    pub p50: String,
    pub p95: String,
    pub last: String,
    pub spark: Spark,
    pub problems: Vec<(&'static str, String)>,
    pub try_href: Option<String>,
    pub id: String,
}

pub struct RouteGroup {
    pub area: &'static str,
    pub resource: String,
    pub routes: Vec<RouteView>,
    pub problems: usize,
    pub open: bool,
}

pub struct NotFoundView {
    pub method: &'static str,
    pub path: String,
    pub count: u64,
    pub when: String,
    pub referrer: String,
    pub nearest: Option<&'static str>,
}

/// What the route detectors found, for the explorer and for Insights.
pub struct RouteProblems {
    pub without_csrf: Vec<String>,
    pub ungated: Vec<String>,
    pub open_in_production: Vec<String>,
    pub overlaps: Vec<(String, String)>,
    pub missing_docs: Vec<String>,
    pub untested: Vec<String>,
}

/// The router probes' last answers: (when, routes without CSRF, routes open in production).
type Probes = Option<(std::time::Instant, Vec<String>, Vec<String>)>;

static PROBES: std::sync::Mutex<Probes> = std::sync::Mutex::new(None);

/// Runs the detectors (the router probes are cached for a minute).
pub async fn route_problems(state: &AppState) -> RouteProblems {
    let routes = rx::table();
    let root = root();
    let cached = lock(&PROBES)
        .as_ref()
        .filter(|(at, _, _)| at.elapsed() < Duration::from_secs(60))
        .map(|(_, a, b)| (a.clone(), b.clone()));
    let (without_csrf, open_in_production) = match cached {
        Some(c) => c,
        None => {
            let csrf = match crate::routes::router(state.clone()) {
                Ok(r) => http::quietly(rx::without_csrf(&r, &routes)).await,
                Err(_) => Vec::new(),
            };
            let mut prod_config = (*state.config).clone();
            prod_config.env = crate::config::AppEnv::Production;
            let open = match crate::state::AppState::new(prod_config, state.db.clone())
                .and_then(crate::routes::router)
            {
                Ok(r) => http::quietly(rx::open_to_anyone(&r, &routes)).await,
                Err(_) => Vec::new(),
            };
            *lock(&PROBES) = Some((std::time::Instant::now(), csrf.clone(), open.clone()));
            (csrf, open)
        }
    };
    let doc = std::fs::read_to_string(root.join("docs/routes.md")).unwrap_or_default();
    let tests = rx::tests_text(&root);
    RouteProblems {
        without_csrf,
        ungated: rx::ungated(&routes),
        open_in_production,
        overlaps: rx::overlaps(&routes),
        missing_docs: if doc.is_empty() {
            Vec::new()
        } else {
            rx::missing_from_docs(&routes, &doc)
        },
        untested: routes
            .iter()
            .filter(|r| !rx::tested(r.path, &tests))
            .map(|r| format!("{} {}", r.method, r.path))
            .collect(),
    }
}

/// The route detectors as Insights findings.
pub async fn route_findings(state: &AppState) -> Vec<insights::Finding> {
    use insights::{Finding, Severity};
    let p = route_problems(state).await;
    let mut out = Vec::new();
    let f = |key: String,
             kind,
             severity,
             title: String,
             evidence: String,
             fix: &str,
             guide,
             url,
             href: &str| Finding {
        key,
        kind,
        severity,
        score: 1,
        title,
        evidence,
        fix: fix.to_owned(),
        guide,
        guide_url: url,
        href: href.to_owned(),
    };
    for r in &p.without_csrf {
        out.push(f(format!("csrf-{r}"), "csrf", Severity::High, format!("{r} accepts cross-site requests"), "A cross-site request in an unused method reached the router instead of a 403 from the CSRF layer.".into(), "Mount the router inside the app's CsrfLayer (routes.rs); never exempt a route.", ".offrails/guides/security.md#csrf", "https://rustoffrails.org/docs/guides/security#csrf", "/dev/dashboard/routes#problems"));
    }
    for r in p.ungated.iter().chain(&p.open_in_production) {
        out.push(f(format!("gate-{r}"), "gate", Severity::High, format!("{r} has no gate"), "An /admin or /dev path that anyone may call, or an admin/login GET that answers 2xx anonymously with the production config.".into(), "Give it Access::Admin and the admin extractor (or Access::Dev and mount it only in development).", ".offrails/guides/security.md#admin--dev-surfaces", "https://rustoffrails.org/docs/guides/security#admin--dev-surfaces", "/dev/dashboard/routes#problems"));
    }
    for w in lock(&HUB.http).get_writes() {
        out.push(f(format!("getwrite-{}-{}", w.route, w.sql), "get_write", Severity::Medium, format!("GET {} writes to the database", w.route), format!("{}× during a GET: {}", w.count, ui::truncate(&w.sql, 120)), "Safe methods never change state: move the write behind a POST (CSRF applies), or allow it in GET_WRITES_ALLOWED if it is a cache fill.", ".offrails/guides/security.md#csrf", "https://rustoffrails.org/docs/guides/security#csrf", &format!("/dev/dashboard/traces/{}", w.trace)));
    }
    let routes = rx::table();
    for n in lock(&HUB.http).not_found_list().into_iter().take(10) {
        let near = rx::nearest(&n.path, &routes);
        out.push(f(format!("404-{}", n.path), "not_found", if n.referrer.as_deref().is_some_and(|r| r.starts_with('/')) { Severity::Medium } else { Severity::Low }, format!("404: {}", n.path), format!("{}× {}{}", n.count, n.referrer.as_ref().map(|r| format!("from {r}")).unwrap_or_else(|| "with no referrer".into()), near.map(|r| format!("; did you mean {r}?")).unwrap_or_default()), "A link on this app points nowhere: fix the link (build it with a paths:: helper) or route the path.", ".offrails/guides/routing.md", "https://rustoffrails.org/docs/guides/routing", "/dev/dashboard/routes#not-found"));
    }
    for r in &p.missing_docs {
        out.push(f(
            format!("docs-{r}"),
            "docs",
            Severity::Low,
            format!("{r} is missing from docs/routes.md"),
            "docs/routes.md is older than the route table.".into(),
            "Run `app routes` (it rewrites docs/routes.md).",
            ".offrails/guides/routing.md",
            "https://rustoffrails.org/docs/guides/routing",
            "/dev/dashboard/routes#problems",
        ));
    }
    if !p.untested.is_empty() {
        out.push(f(
            "untested-routes".into(),
            "tests",
            Severity::Low,
            format!("{} routes no test mentions", p.untested.len()),
            format!("For example {}", p.untested.iter().take(4).cloned().collect::<Vec<_>>().join(", ")),
            "Every route gets an integration test that requests it (the route sweeps only check access).",
            ".offrails/guides/testing.md#writing-tests",
            "https://rustoffrails.org/docs/guides/testing#writing-tests",
            "/dev/dashboard/routes?problems=1",
        ));
    }
    let hit: Vec<(&'static str, &'static str)> = lock(&HUB.http)
        .stats()
        .map(|s| (s.method, s.route))
        .collect();
    let never: Vec<String> = routes
        .iter()
        .filter(|r| {
            r.method == "GET" && !r.path.starts_with("/dev") && !r.path.starts_with("/admin")
        })
        .filter(|r| !hit.contains(&(r.method, r.path)))
        .map(|r| r.path.to_owned())
        .collect();
    if !never.is_empty() && !hit.is_empty() {
        out.push(f(
            "never-hit".into(),
            "coverage",
            Severity::Low,
            format!("{} app pages never opened this session", never.len()),
            format!(
                "For example {}",
                never.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            ),
            "Click through them (or add them to a load-lab run) before calling a change done.",
            ".offrails/guides/dev-tools.md#for-agents",
            "https://rustoffrails.org/docs/guides/dev-tools#for-agents",
            "/dev/dashboard/routes",
        ));
    }
    let now = Utc::now().timestamp().div_euclid(60);
    for s in lock(&HUB.http).stats() {
        if s.s5 > 0 && s.route != http::UNMATCHED {
            let (recent, before, _) = s.error_trend(now, 5);
            if recent <= before * 2.0 + 0.01 {
                out.push(f(
                    format!("5xx-{} {}", s.method, s.route),
                    "errors",
                    Severity::Medium,
                    format!("{} {} answered 5xx", s.method, s.route),
                    format!("{} server errors in {} requests", s.s5, s.count),
                    "Filter the live log by level=error; every 500 is logged with its request id.",
                    ".offrails/guides/errors-and-logging.md#response-mapping",
                    "https://rustoffrails.org/docs/guides/errors-and-logging#response-mapping",
                    "/dev/dashboard/logs?level=error",
                ));
            }
        }
    }
    out
}

#[derive(Template)]
#[template(path = "kernel/dashboard/routes.html")]
struct RoutesPage {
    chrome: Chrome,
    d: Dash,
    groups: Vec<RouteGroup>,
    total: usize,
    shown: usize,
    q: String,
    area: String,
    areas: Vec<RangeLink>,
    problems_only: bool,
    p: RouteProblems,
    never_hit: usize,
    untested: usize,
    not_found: Vec<NotFoundView>,
}

#[derive(Debug, Default, Deserialize)]
struct RoutesQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    area: String,
    problems: Option<String>,
}

async fn routes_page(
    State(state): State<AppState>,
    Query(q): Query<RoutesQuery>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let routes = rx::table();
    let p = route_problems(&state).await;
    let root = root();
    let now = Utc::now();
    let minute = now.timestamp().div_euclid(60);
    let http = lock(&HUB.http);
    let writes: Vec<(&'static str, String)> = http
        .get_writes()
        .iter()
        .map(|w| (w.route, w.sql.to_string()))
        .collect();
    let mut never_hit = 0;
    let mut groups = Vec::new();
    let needle = q.q.trim().to_lowercase();
    let problems_only = q.problems.as_deref() == Some("1");
    let mut shown = 0;
    for ((area, resource), defs) in rx::grouped(&routes) {
        if !q.area.is_empty() && q.area != area {
            continue;
        }
        let mut views = Vec::new();
        for def in defs {
            if !needle.is_empty()
                && ![
                    def.method,
                    def.path,
                    def.handler,
                    rx::access_label(def.access),
                ]
                .iter()
                .any(|f| f.to_lowercase().contains(&needle))
            {
                continue;
            }
            let source = rx::source_of(&root, def.handler);
            let source_text = source
                .as_ref()
                .and_then(|s| {
                    std::fs::read_to_string(root.join(s.split(':').next().unwrap_or(s))).ok()
                })
                .unwrap_or_default();
            let stats = http.get(def.method, def.path);
            let mut problems: Vec<(&'static str, String)> = Vec::new();
            let key = format!("{} {}", def.method, def.path);
            if p.without_csrf.contains(&key) {
                problems.push(("err", "accepts cross-site requests (no CSRF)".into()));
            }
            if p.ungated.contains(&key) || p.open_in_production.contains(&key) {
                problems.push(("err", "no access gate".into()));
            }
            if let Some(w) = writes
                .iter()
                .find(|(r, _)| *r == def.path && def.method == "GET")
            {
                problems.push((
                    "warn",
                    format!("writes during a GET: {}", ui::truncate(&w.1, 60)),
                ));
            }
            let (hits, errors, error_tone, p50, p95, last, spark) = match stats {
                Some(s) => {
                    if s.s5 > 0 {
                        let (recent, before, _) = s.error_trend(minute, 5);
                        problems.push((
                            "err",
                            if recent > before * 2.0 + 0.01 {
                                "rising server errors".to_owned()
                            } else {
                                format!("{} server errors", s.s5)
                            },
                        ));
                    }
                    if s.count >= 3 && s.hist.quantile(0.95) >= insights::SLOW_ROUTE_P95_US {
                        problems.push((
                            "warn",
                            format!("slow: p95 {}", micros(s.hist.quantile(0.95))),
                        ));
                    }
                    let counts: Vec<f64> = (0..30)
                        .map(|i| {
                            let m = minute - 29 + i;
                            s.minutes
                                .iter()
                                .find(|x| x.0 == m)
                                .map_or(0.0, |x| f64::from(x.1))
                        })
                        .collect();
                    (
                        ui::thousands(s.count),
                        pct(s.s4 + s.s5, s.count),
                        if s.s5 > 0 {
                            "err"
                        } else if s.s4 > 0 {
                            "warn"
                        } else {
                            "muted"
                        },
                        micros(s.hist.quantile(0.5)),
                        micros(s.hist.quantile(0.95)),
                        ui::ago(s.last, now),
                        charts::sparkline(&counts),
                    )
                }
                None => {
                    never_hit += 1;
                    problems.push(("muted", "never hit in this session".into()));
                    (
                        "0".into(),
                        "–".into(),
                        "muted",
                        "–".into(),
                        "–".into(),
                        "never".into(),
                        charts::sparkline(&[]),
                    )
                }
            };
            if p.untested.contains(&key) {
                problems.push(("warn", "no test mentions this path".into()));
            }
            if p.missing_docs.contains(&key) {
                problems.push(("muted", "missing from docs/routes.md".into()));
            }
            if p.overlaps.iter().any(|(a, b)| *a == key || *b == key) {
                problems.push(("muted", "overlaps another pattern".into()));
            }
            if problems_only && !problems.iter().any(|(t, _)| *t != "muted") {
                continue;
            }
            let safe_get = def.method == "GET"
                && !def.path.ends_with("/events")
                && !def.path.ends_with("/stream")
                && !def.path.contains("/panic");
            views.push(RouteView {
                method: def.method,
                path: def.path,
                handler: def.handler,
                middleware: rx::middleware(&def, &source_text),
                source: source.unwrap_or_default(),
                access: rx::access_label(def.access),
                hits,
                errors,
                error_tone,
                p50,
                p95,
                last,
                spark,
                problems,
                try_href: safe_get.then(|| rx::sample_url(def.path)),
                id: format!("route-{:08x}", sql::fingerprint_id(&key) as u32),
            });
        }
        if views.is_empty() {
            continue;
        }
        shown += views.len();
        let problems = views
            .iter()
            .filter(|v| v.problems.iter().any(|(t, _)| *t != "muted"))
            .count();
        groups.push(RouteGroup {
            area,
            resource,
            open: problems > 0 || !needle.is_empty() || views.len() <= 4,
            problems,
            routes: views,
        });
    }
    let not_found = http
        .not_found_list()
        .into_iter()
        .take(30)
        .map(|n| NotFoundView {
            method: n.method,
            path: n.path.clone(),
            count: n.count,
            when: ui::ago(n.last, now),
            referrer: n.referrer.clone().unwrap_or_default(),
            nearest: rx::nearest(&n.path, &routes),
        })
        .collect();
    drop(http);
    let areas = [
        ("All", ""),
        ("App", "app"),
        ("API", "api"),
        ("Admin", "admin"),
        ("Dev", "dev"),
    ]
    .iter()
    .map(|(label, key)| RangeLink {
        label,
        href: if key.is_empty() {
            "/dev/dashboard/routes".into()
        } else {
            format!("/dev/dashboard/routes?area={key}")
        },
        current: q.area == *key,
    })
    .collect();
    ui::page(RoutesPage {
        chrome: chrome(&state, "Routes", "dashboard-routes", flash, theme),
        d: dash(DEV, "/routes", None, 0).ai(&state.config, ai::Tab::Routes, None),
        total: routes.len(),
        shown,
        untested: p.untested.len(),
        groups,
        q: q.q,
        area: q.area,
        areas,
        problems_only,
        p,
        never_hit,
        not_found,
    })
}

// ---------------------------------------------------------------------------------------------
// Schema: the diagram, the tables (its text alternative), one table, the migrations
// ---------------------------------------------------------------------------------------------

pub struct TableView {
    pub name: String,
    pub rows: String,
    pub size: String,
    pub area: &'static str,
    pub columns: Vec<schema::Column>,
    pub indexes: Vec<schema::Index>,
    pub fks: Vec<schema::ForeignKey>,
    pub unindexed: Vec<String>,
    /// Per column, the table its foreign key points at ("" for none).
    pub col_fk: Vec<String>,
}

fn table_view(t: schema::Table) -> TableView {
    let unindexed = t
        .unindexed_fks()
        .iter()
        .map(|fk| fk.columns.join(", "))
        .collect();
    let col_fk = t
        .columns
        .iter()
        .map(|c| {
            t.fks
                .iter()
                .find(|fk| fk.columns.contains(&c.name))
                .map(|fk| fk.table.clone())
                .unwrap_or_default()
        })
        .collect();
    TableView {
        col_fk,
        rows: t
            .rows
            .map_or_else(|| "?".into(), |n| ui::thousands(n as u64)),
        size: t.bytes.map(|b| bytes(b as u64)).unwrap_or_default(),
        area: super::erd::area(&t.name),
        name: t.name,
        columns: t.columns,
        indexes: t.indexes,
        fks: t.fks,
        unindexed,
    }
}

fn backend_name(state: &AppState) -> &'static str {
    match state.db.get_database_backend() {
        sea_orm::DbBackend::Postgres => "Postgres",
        _ => "SQLite",
    }
}

/// The segmented control under the Schema heading.
fn schema_tabs(current: &str) -> Vec<RangeLink> {
    [
        ("Diagram and tables", "/dev/dashboard/schema"),
        ("Migrations", "/dev/dashboard/schema/migrations"),
    ]
    .iter()
    .map(|(label, href)| RangeLink {
        label,
        href: (*href).into(),
        current: *href == current,
    })
    .collect()
}

async fn catalog(state: &AppState) -> (Vec<schema::Table>, Option<String>) {
    match http::quietly(schema::tables(&state.db)).await {
        Ok(t) => (t, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

#[derive(Template)]
#[template(path = "kernel/dashboard/schema.html")]
struct SchemaPage {
    chrome: Chrome,
    d: Dash,
    tabs: Vec<RangeLink>,
    erd: super::erd::Erd,
    mermaid: String,
    tables: Vec<TableView>,
    applied: usize,
    pending: Vec<String>,
    drift: usize,
    backend: &'static str,
    error: Option<String>,
}

async fn schema_page(
    State(state): State<AppState>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let (tables, error) = catalog(&state).await;
    let erd = super::erd::build(&tables);
    let report = super::migrations::report(&state.db, &tables, &root()).await;
    let pending = report.pending().iter().map(|m| m.name.clone()).collect();
    ui::page(SchemaPage {
        chrome: chrome(&state, "Schema", "dashboard", flash, theme)
            .script(state.assets.path("kernel/dashboard-erd.js")),
        d: dash(DEV, "/schema", None, 0).ai(&state.config, ai::Tab::Schema, None),
        tabs: schema_tabs("/dev/dashboard/schema"),
        mermaid: super::erd::mermaid(&erd),
        erd,
        tables: tables.into_iter().map(table_view).collect(),
        applied: report.migrations.iter().filter(|m| m.applied()).count(),
        pending,
        drift: report.drift.len(),
        backend: backend_name(&state),
        error,
    })
}

/// `GET /dev/dashboard/schema/erd.svg`: the diagram as a file (Export SVG).
async fn erd_svg(State(state): State<AppState>) -> Response {
    let (tables, _) = catalog(&state).await;
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"schema.svg\"",
            ),
        ],
        super::erd::standalone_svg(&super::erd::build(&tables)),
    )
        .into_response()
}

/// `GET /dev/dashboard/schema/erd.mmd`: Mermaid `erDiagram` text, for docs or an LLM.
async fn erd_mermaid(State(state): State<AppState>) -> Response {
    let (tables, _) = catalog(&state).await;
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        super::erd::mermaid(&super::erd::build(&tables)),
    )
        .into_response()
}

pub struct QueryRef {
    pub id: String,
    pub sql: String,
    pub count: String,
    pub p95: String,
    pub last: String,
}

/// Recorded statements that name `table` (as a whole identifier, quoted or not).
pub fn queries_touching(table: &str, limit: usize) -> Vec<QueryRef> {
    let now = Utc::now();
    lock(&HUB.sql)
        .rows(sql::Sort::Recent)
        .into_iter()
        .filter(|r| {
            r.sql
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|w| w.eq_ignore_ascii_case(table))
        })
        .take(limit)
        .map(|r| QueryRef {
            sql: r.sql.to_string(),
            count: ui::thousands(r.count),
            p95: micros(r.p95),
            last: ui::ago(r.last, now),
            id: r.id,
        })
        .collect()
}

pub struct EdgeRef {
    pub table: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/schema_table.html")]
struct SchemaTablePage {
    chrome: Chrome,
    d: Dash,
    t: TableView,
    parents: Vec<EdgeRef>,
    children: Vec<EdgeRef>,
    queries: Vec<QueryRef>,
    created_by: Option<(String, String, Option<String>)>,
    changed_by: Vec<(String, String)>,
}

/// `GET /dev/dashboard/schema/tables/{name}`: one table, its neighbours, the queries that touch
/// it, the migration that created it. `dashboard-erd.js` shows the same page's detail block
/// beside the diagram when a table is clicked.
async fn schema_table(
    State(state): State<AppState>,
    Path(name): Path<String>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let (mut tables, _) = catalog(&state).await;
    // SECURITY: only a name the catalog itself returned is looked up; nothing from the URL
    // reaches SQL (test: only_plain_identifiers_reach_catalog_sql).
    let Some(pos) = tables.iter().position(|t| t.name == name) else {
        return back(
            "/dev/dashboard/schema",
            Flash::alert("No such table (it may have been dropped)."),
        );
    };
    let erd = super::erd::build(&tables);
    let edge_text = |e: &super::erd::ErdEdge| e.sentence();
    let parents = erd
        .edges
        .iter()
        .filter(|e| e.child == name)
        .map(|e| EdgeRef {
            table: e.parent.clone(),
            text: edge_text(e),
        })
        .collect();
    let children = erd
        .edges
        .iter()
        .filter(|e| e.parent == name && e.child != name)
        .map(|e| EdgeRef {
            table: e.child.clone(),
            text: edge_text(e),
        })
        .collect();
    let report = super::migrations::report(&state.db, &tables, &root()).await;
    let mut created_by = None;
    let mut changed_by = Vec::new();
    for m in &report.migrations {
        for e in &m.effects {
            match e {
                super::migrations::Effect::CreateTable { table, .. } if *table == name => {
                    created_by = Some((m.name.clone(), e.text(), m.file.clone()));
                }
                super::migrations::Effect::AlterTable { table, .. } if *table == name => {
                    changed_by.push((m.name.clone(), e.text()));
                }
                _ => {}
            }
        }
    }
    let t = table_view(tables.swap_remove(pos));
    ui::page(SchemaTablePage {
        chrome: chrome(&state, &t.name, "dashboard", flash, theme),
        d: dash(DEV, "/schema", None, 0).ai(&state.config, ai::Tab::Schema, Some(t.name.clone())),
        parents,
        children,
        queries: queries_touching(&t.name, 10),
        created_by,
        changed_by,
        t,
    })
}

pub struct MigrationView {
    pub name: String,
    pub applied: bool,
    pub when: String,
    pub at: String,
    pub area: &'static str,
    pub file: Option<String>,
    pub effects: Vec<(&'static str, String)>,
    pub edited: bool,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/migrations.html")]
struct MigrationsPage {
    chrome: Chrome,
    d: Dash,
    tabs: Vec<RangeLink>,
    migrations: Vec<MigrationView>,
    pending: usize,
    applied: usize,
    unknown: Vec<(String, String)>,
    drift: Vec<super::migrations::Drift>,
    backend: &'static str,
}

/// `GET /dev/dashboard/schema/migrations`: the timeline, newest first, and the drift.
async fn migrations_page(
    State(state): State<AppState>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let (tables, _) = catalog(&state).await;
    let report = super::migrations::report(&state.db, &tables, &root()).await;
    let now = Utc::now();
    let mut migrations: Vec<MigrationView> = report
        .migrations
        .iter()
        .map(|m| MigrationView {
            name: m.name.clone(),
            applied: m.applied(),
            when: m.applied_at.map(|t| ui::ago(t, now)).unwrap_or_default(),
            at: m.applied_at.map(ui::utc).unwrap_or_default(),
            area: m.area,
            file: m.file.clone(),
            effects: m.effects.iter().map(|e| (e.tone(), e.text())).collect(),
            edited: m.edited,
        })
        .collect();
    migrations.reverse();
    ui::page(MigrationsPage {
        chrome: chrome(&state, "Migrations", "dashboard", flash, theme),
        d: dash(DEV, "/schema", None, 0).ai(&state.config, ai::Tab::Migrations, None),
        tabs: schema_tabs("/dev/dashboard/schema/migrations"),
        pending: migrations.iter().filter(|m| !m.applied).count(),
        applied: migrations.iter().filter(|m| m.applied).count(),
        migrations,
        unknown: report
            .unknown_applied
            .iter()
            .map(|(n, at)| (n.clone(), at.map(ui::utc).unwrap_or_default()))
            .collect(),
        drift: report.drift,
        backend: backend_name(&state),
    })
}

// ---------------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------------

pub struct MemoryView {
    pub total: String,
    pub parts: Vec<(&'static str, String)>,
}

pub fn memory_view() -> MemoryView {
    let (secs, mins) = lock(&HUB.series).lens();
    let (recent, notable) = lock(&HUB.traces).lens();
    MemoryView {
        total: bytes(HUB.approx_bytes() as u64),
        parts: vec![
            ("Routes tracked", lock(&HUB.http).len().to_string()),
            (
                "SQL fingerprints",
                format!("{} of {}", lock(&HUB.sql).len(), sql::MAX_FINGERPRINTS),
            ),
            ("Traces", format!("{recent} recent, {notable} notable")),
            (
                "Log lines",
                format!("{} of {}", lock(&HUB.logs).len(), super::trace::LOG_LINES),
            ),
            ("Samples", format!("{secs} at 1 s, {mins} at 1 min")),
        ],
    }
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_memory.html")]
struct MemoryFragment {
    m: MemoryView,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/config.html")]
struct ConfigPage {
    chrome: Chrome,
    d: Dash,
    settings: Vec<(String, String)>,
    env: Vec<(String, String)>,
    system: Vec<(&'static str, String)>,
    build: Vec<(&'static str, String)>,
    m: MemoryView,
}

/// Env var name prefixes listed on Config (values only for `ENV_VALUES_SHOWN`). APP: add yours.
const ENV_SHOWN: &[&str] = &[
    "APP_",
    "DATABASE_",
    "PORT",
    "RUST_LOG",
    "LOG_FORMAT",
    "MAIL",
    "SMTP_",
    "GITHUB_",
    "DASHBOARD_",
    "TEST_DATABASE_URL",
    "RUST_BACKTRACE",
];

/// The env vars whose **values** Config shows: settings that are never secret. Every other
/// listed variable shows only "set (redacted)" or "not set", whatever its name: a denylist of
/// secret-looking words misses `SMTP_PASS`, `GITHUB_PAT`, `APP_PEPPER`, `STRIPE_SK`…
/// APP: add a name here only when its value can never be a secret. URLs still lose their
/// credentials and query string.
pub const ENV_VALUES_SHOWN: &[&str] = &[
    "APP_ENV",
    "APP_URL",
    "APP_REVISION",
    "APP_BIND",
    "APP_ALLOWED_HOSTS",
    "APP_TRUSTED_PROXY",
    "APP_DEV_HTTP_PORT",
    "APP_SHUTDOWN_DRAIN_SECS",
    "APP_INLINE_WORKER",
    "APP_QUEUES",
    "APP_JOBS_NODE",
    "APP_JOBS_PRUNE_DAYS",
    "APP_JOBS_SHUTDOWN_GRACE",
    "APP_NODE",
    "APP_CLUSTER",
    "APP_MAGIC_LINK_ANY_BROWSER",
    "APP_REALTIME_ORIGINS",
    "APP_REALTIME_TRANSPORTS",
    "APP_REALTIME_REVALIDATE_SECS",
    "DATABASE_URL",
    "DATABASE_POOL",
    "TEST_DATABASE_URL",
    "PORT",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "LOG_FORMAT",
    "MAILER",
    "MAIL_FROM",
    "SMTP_URL",
    "GITHUB_REPO",
    "GITHUB_API_URL",
    "DASHBOARD_IN_PRODUCTION",
    "DASHBOARD_DEMO_HISTORY",
    "DASHBOARD_GITHUB_FIXTURE",
    "DASHBOARD_AI_PROVIDER",
    "DASHBOARD_AI_MODEL",
    "DASHBOARD_AI_BASE_URL",
    "DASHBOARD_AI_IN_PRODUCTION",
];

/// Whether Config may show this env var's value (`ENV_VALUES_SHOWN`).
pub fn value_shown(name: &str) -> bool {
    ENV_VALUES_SHOWN.contains(&name)
}

/// An env var's value as Config shows it.
// SECURITY: only allowlisted, never-secret names show a value; every other one is "set
// (redacted)" or "not set"; URLs lose their credentials and query; nothing outside ENV_SHOWN is
// listed (HOME, USER and PATH name the machine) (test: config_redacts_secrets_and_credentials).
pub fn redact_env(name: &str, value: &str) -> String {
    if !value_shown(name) {
        return if value.is_empty() {
            "not set".into()
        } else {
            "set (redacted)".into()
        };
    }
    if (name.ends_with("_URL") || value.contains("://"))
        && let Some((scheme, rest)) = value.split_once("://")
    {
        {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
            let rest = rest.split('?').next().unwrap_or(rest);
            return format!("{scheme}://{rest}");
        }
    }
    ui::truncate(value, 200)
}

async fn config_page(
    State(state): State<AppState>,
    Extension(mode): Extension<Mode>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let c = &state.config;
    let settings = vec![
        ("APP_ENV".to_owned(), format!("{:?}", c.env).to_lowercase()),
        (
            "Database".to_owned(),
            crate::controllers::dev::redact_db(&c.database_url),
        ),
        ("DATABASE_POOL".to_owned(), c.database_pool.to_string()),
        (
            "Ask AI".to_owned(),
            super::ai_web::ai_setting(&c.dashboard.ai),
        ),
        ("APP_URL".to_owned(), c.app_url.clone()),
        ("PORT".to_owned(), c.port.to_string()),
        (
            "TLS".to_owned(),
            if c.tls.is_some() {
                "direct (APP_TLS_CERT)".into()
            } else {
                "off (behind a proxy, or plain http in development)".into()
            },
        ),
        (
            "LOG_FORMAT".to_owned(),
            format!("{:?}", c.log_format).to_lowercase(),
        ),
        (
            "APP_REVISION".to_owned(),
            c.revision.clone().unwrap_or_else(|| "not set".into()),
        ),
        (
            "GITHUB_REPO".to_owned(),
            c.dashboard
                .github_repo
                .clone()
                .unwrap_or_else(|| "not set".into()),
        ),
        (
            "GITHUB_TOKEN".to_owned(),
            if c.dashboard.github_token.is_some() {
                "set (redacted)".into()
            } else {
                "not set".into()
            },
        ),
        (
            "Dashboard in production".to_owned(),
            if c.dashboard.in_production {
                "on (read-only, /admin/dashboard)".into()
            } else {
                "off".into()
            },
        ),
    ];
    let mut env: Vec<(String, String)> = if mode.ro {
        Vec::new()
    } else {
        crate::config::shown_env(ENV_SHOWN)
            .into_iter()
            .map(|(k, v)| {
                let shown = redact_env(&k, &v);
                (k, shown)
            })
            .collect()
    };
    env.sort();
    // WHY: read once; two guards in one expression would deadlock on the same mutex.
    let latest = lock(&HUB.series).latest();
    let system = vec![
        (
            "OS",
            format!(
                "{} {}",
                sysinfo::System::name().unwrap_or_else(|_| std::env::consts::OS.into()),
                sysinfo::System::os_version().unwrap_or_default()
            ),
        ),
        ("Architecture", std::env::consts::ARCH.to_owned()),
        (
            "CPUs",
            std::thread::available_parallelism()
                .map(|n| n.get().to_string())
                .unwrap_or_else(|_| "?".into()),
        ),
        ("Host name", "redacted".into()),
        (
            "Resident memory",
            latest.map_or_else(|| "—".into(), |l| bytes(l.rss)),
        ),
        (
            "Virtual memory",
            latest.map_or_else(
                || "—".into(),
                |l| {
                    format!(
                        "{} (address space reserved, not used; large on macOS)",
                        bytes(l.virt)
                    )
                },
            ),
        ),
        ("Process", format!("pid {}", std::process::id())),
        ("Booted", ui::utc(*ui::BOOTED_AT)),
    ];
    let build = vec![
        ("Version", env!("CARGO_PKG_VERSION").to_owned()),
        (
            "Profile",
            if cfg!(debug_assertions) {
                "debug".into()
            } else {
                "release".into()
            },
        ),
        (
            "Tokio metrics",
            if super::collector::unstable().is_some() {
                "stable + unstable (tokio_unstable)".into()
            } else {
                "stable (build with --cfg tokio_unstable for more)".into()
            },
        ),
        (
            "Frontend mode",
            crate::controllers::dev::FRONTEND_MODE.to_owned(),
        ),
    ];
    ui::page(ConfigPage {
        chrome: chrome(&state, "Config", "dashboard", flash, theme),
        d: dash(
            mode,
            "/config",
            Some(format!("{}/stream?view=config", mode.base)),
            2,
        )
        .ai(&state.config, ai::Tab::Dev, None),
        settings,
        env,
        system,
        build,
        m: memory_view(),
    })
}

// ---------------------------------------------------------------------------------------------
// Load lab
// ---------------------------------------------------------------------------------------------

pub struct RunView {
    pub label: String,
    pub target: String,
    pub running: bool,
    pub percent: u64,
    pub sent: u64,
    pub total: u64,
    pub ok: u64,
    pub failed: u64,
    pub skipped: u64,
    pub p50: String,
    pub p95: String,
    pub p99: String,
    pub rate: String,
    pub paths: String,
    pub when: String,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_lab_status.html")]
struct LabStatus {
    run: Option<RunView>,
    spike: Option<String>,
}

fn lab_status() -> LabStatus {
    let run = HUB.lab.current().map(|r| {
        let h = lock(&r.latency).clone();
        let elapsed = r.started.elapsed().as_secs_f64().max(0.001);
        RunView {
            label: r.label.clone(),
            target: r.target.clone(),
            running: r.running(),
            percent: r.percent(),
            sent: r.sent.load(std::sync::atomic::Ordering::Relaxed),
            total: r.total(),
            ok: r.ok.load(std::sync::atomic::Ordering::Relaxed),
            failed: r.failed.load(std::sync::atomic::Ordering::Relaxed),
            skipped: r.skipped.load(std::sync::atomic::Ordering::Relaxed),
            p50: micros(h.quantile(0.5)),
            p95: micros(h.quantile(0.95)),
            p99: micros(h.quantile(0.99)),
            rate: format!(
                "{:.0}/s",
                r.done() as f64 / elapsed.min(f64::from(r.plan.secs).max(1.0))
            ),
            paths: r.plan.paths.join(", "),
            when: ui::ago(r.at, Utc::now()),
        }
    });
    let spike = lock(&HUB.lab.spike_until).map(|until| {
        format!(
            "Holding {} MiB until {}",
            lab::SPIKE_MB,
            until.format("%H:%M:%S")
        )
    });
    LabStatus { run, spike }
}

pub struct ScenarioView {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub panel: &'static str,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/lab.html")]
struct LabPage {
    chrome: Chrome,
    d: Dash,
    run: Option<RunView>,
    spike: Option<String>,
    cards: Vec<Card>,
    scenarios: Vec<ScenarioView>,
    paths: Vec<&'static str>,
    records: u64,
    max_rps: u32,
    max_secs: u32,
    sims: Vec<(&'static str, lab::UpSim)>,
    presets: &'static [(&'static str, &'static str, &'static str)],
    attacks: &'static [(&'static str, &'static str, &'static str)],
    attacking: bool,
    traceparent: String,
}

async fn lab_page(
    State(state): State<AppState>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let records = http::quietly(lab::count(&state.db)).await.unwrap_or(0);
    let status = lab_status();
    ui::page(LabPage {
        chrome: chrome(&state, "Load lab", "dashboard", flash, theme),
        d: dash(
            DEV,
            "/lab",
            Some("/dev/dashboard/stream?view=lab".into()),
            1,
        )
        .ai(&state.config, ai::Tab::Overview, None),
        run: status.run,
        spike: status.spike,
        cards: cards(DEV),
        scenarios: lab::Scenario::ALL
            .iter()
            .map(|s| ScenarioView {
                key: s.key(),
                label: s.label(),
                description: s.description(),
                panel: s.panel(),
            })
            .collect(),
        paths: lab::suggested_paths(),
        records,
        max_rps: lab::MAX_RPS,
        max_secs: lab::MAX_SECS,
        sims: lab::UPSTREAMS.into_iter().zip(lab::sims()).collect(),
        presets: &lab::PRESETS,
        attacks: &lab::ATTACKS,
        attacking: HUB.lab.attacking.load(std::sync::atomic::Ordering::Relaxed),
        traceparent: lock(&HUB.lab.last_traceparent).clone().unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct SeedForm {
    n: u64,
}

async fn lab_seed(State(state): State<AppState>, Form(f): Form<SeedForm>) -> Response {
    match http::quietly(lab::seed(&state.db, f.n)).await {
        Ok(n) => back(
            "/dev/dashboard/lab",
            Flash::notice(format!("Added {n} fake orders.")),
        ),
        Err(e) => back(
            "/dev/dashboard/lab",
            Flash::alert(format!("Seeding failed: {e}")),
        ),
    }
}

async fn lab_clear(State(state): State<AppState>) -> Response {
    match http::quietly(lab::clear(&state.db)).await {
        Ok(n) => back(
            "/dev/dashboard/lab",
            Flash::notice(format!("Removed {n} fake orders.")),
        ),
        Err(e) => back(
            "/dev/dashboard/lab",
            Flash::alert(format!("Clearing failed: {e}")),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TrafficForm {
    paths: String,
    rps: u32,
    secs: u32,
}

/// `POST /dev/dashboard/lab/traffic`: `paths` (one per line), `rps`, `secs`.
async fn lab_traffic(State(state): State<AppState>, Form(f): Form<TrafficForm>) -> Response {
    let paths: Vec<String> = f.paths.lines().map(str::to_owned).collect();
    match lab::plan(&paths, f.rps, f.secs).and_then(|p| lab::start(&state, "Traffic", p)) {
        Ok(r) => back(
            "/dev/dashboard/lab",
            Flash::notice(format!(
                "Sending {} requests over {} s to {}.",
                r.total(),
                r.plan.secs,
                r.target
            )),
        ),
        Err(e) => back("/dev/dashboard/lab", Flash::alert(e)),
    }
}

async fn lab_stop() -> Response {
    if lab::stop() {
        back("/dev/dashboard/lab", Flash::notice("Stopping the run."))
    } else {
        back("/dev/dashboard/lab", Flash::alert("Nothing is running."))
    }
}

#[derive(Debug, Deserialize)]
struct ScenarioForm {
    scenario: String,
}

async fn lab_scenario(State(state): State<AppState>, Form(f): Form<ScenarioForm>) -> Response {
    let Some(s) = lab::Scenario::parse(&f.scenario) else {
        return back("/dev/dashboard/lab", Flash::alert("No such scenario."));
    };
    match http::quietly(lab::scenario(&state, s)).await {
        Ok(msg) => back(s.panel(), Flash::notice(msg)),
        Err(e) => back("/dev/dashboard/lab", Flash::alert(e)),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct UpstreamsForm {
    preset: String,
    name: String,
    latency_ms: u64,
    jitter_ms: u64,
    error_pct: u32,
    hang_pct: u32,
}

/// `POST …/lab/upstreams`: a preset, or one simulated API's latency, jitter, errors and hangs.
async fn lab_upstreams(Form(f): Form<UpstreamsForm>) -> Response {
    let to = "/dev/dashboard/lab#lab-upstreams";
    if !f.preset.is_empty() {
        return match lab::PRESETS.iter().find(|(k, _, _)| *k == f.preset) {
            Some((k, label, what)) if lab::preset(k) => back(
                to,
                Flash::notice(format!(
                    "{label}: {what}. Run the Checkout fan-out to see it."
                )),
            ),
            _ => back(to, Flash::alert("No such preset.")),
        };
    }
    if lab::set_sim(&f.name, f.latency_ms, f.jitter_ms, f.error_pct, f.hang_pct) {
        back(to, Flash::notice(format!("{} updated.", f.name)))
    } else {
        back(to, Flash::alert("No such simulated API."))
    }
}

#[derive(Debug, Deserialize)]
struct AttackForm {
    kind: String,
}

/// `POST …/lab/attack`: runs the attack simulator against this app's own address.
async fn lab_attack(State(state): State<AppState>, Form(f): Form<AttackForm>) -> Response {
    match lab::start_attack(&state, &f.kind) {
        Ok(n) => back(
            "/dev/dashboard/security",
            Flash::notice(format!(
                "Attack simulator: {n} requests on their way. Watch this page fill in (reload in a few seconds)."
            )),
        ),
        Err(e) => back("/dev/dashboard/lab#lab-attack", Flash::alert(e)),
    }
}

async fn reset() -> Response {
    HUB.reset();
    back(
        "/dev/dashboard",
        Flash::notice("Requests, SQL, traces and logs cleared (the charts keep their history)."),
    )
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

pub struct SuiteView {
    pub db: &'static str,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub flaky: u32,
    pub duration: String,
    pub tone: &'static str,
}

pub struct RunRow {
    pub id: String,
    pub when: String,
    pub source: String,
    pub sqlite: Option<SuiteView>,
    pub postgres: Option<SuiteView>,
    pub current: bool,
    pub href: String,
}

pub struct CaseView {
    pub name: String,
    pub module: String,
    pub time: String,
    pub db: &'static str,
    pub output: String,
}

#[derive(Template)]
#[template(path = "kernel/dashboard/_test_progress.html")]
struct TestProgress {
    p: testruns::Progress,
    percent: u32,
}

fn test_progress() -> TestProgress {
    let p = lock(&HUB.tests.progress).clone();
    let done = p.passed + p.failed + p.skipped;
    let percent = (done * 100).checked_div(p.total).unwrap_or(0).min(100);
    TestProgress { p, percent }
}

#[derive(Template)]
#[template(path = "kernel/dashboard/tests.html")]
struct TestsPage {
    chrome: Chrome,
    d: Dash,
    runs: Vec<RunRow>,
    selected: Option<RunRow>,
    slowest: Vec<CaseView>,
    failures: Vec<CaseView>,
    flaky: Vec<(String, u32)>,
    coverage: Option<String>,
    spec: testruns::SpecCoverage,
    trend: Vec<charts::Bar>,
    trend_table: Vec<(String, String, String)>,
    p: testruns::Progress,
    percent: u32,
    postgres: bool,
    dir: &'static str,
}

fn suite_view(s: Option<&testruns::SuiteSummary>, db: testruns::Db) -> Option<SuiteView> {
    s.map(|s| SuiteView {
        db: db.label(),
        passed: s.passed,
        failed: s.failed,
        skipped: s.skipped,
        flaky: s.flaky,
        duration: ui::duration(s.duration_ms as i64),
        tone: if s.failed > 0 {
            "err"
        } else if s.flaky > 0 {
            "warn"
        } else {
            "ok"
        },
    })
}

#[derive(Debug, Default, Deserialize)]
struct TestsQuery {
    run: Option<String>,
}

async fn tests_page(
    State(state): State<AppState>,
    Query(q): Query<TestsQuery>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let dir = testruns::root().join(testruns::DIR);
    let runs = testruns::load_runs(&dir);
    let progress = test_progress();
    let selected_id = q
        .run
        .filter(|r| testruns::safe_id(r))
        .or_else(|| runs.first().map(|r| r.id.clone()));
    let now = Utc::now();
    let row = |r: &testruns::RunSummary| RunRow {
        id: r.id.clone(),
        when: ui::ago(r.at, now),
        source: r.source.clone(),
        sqlite: suite_view(r.suite(testruns::Db::Sqlite), testruns::Db::Sqlite),
        postgres: suite_view(r.suite(testruns::Db::Postgres), testruns::Db::Postgres),
        current: Some(&r.id) == selected_id.as_ref(),
        href: format!("/dev/dashboard/tests?run={}", r.id),
    };
    let rows: Vec<RunRow> = runs.iter().map(row).collect();
    let selected = runs
        .iter()
        .find(|r| Some(&r.id) == selected_id.as_ref())
        .map(row);
    let (mut slowest, mut failures) = (Vec::new(), Vec::new());
    if let Some(id) = &selected_id {
        for db in [testruns::Db::Sqlite, testruns::Db::Postgres] {
            for c in testruns::load_cases(&dir, id, db) {
                let view = CaseView {
                    name: c.name.clone(),
                    module: c.classname.clone(),
                    time: ui::duration(c.time_ms as i64),
                    db: db.label(),
                    output: c.output.clone(),
                };
                if c.outcome == testruns::Outcome::Failed {
                    failures.push(view);
                } else {
                    slowest.push((c.time_ms, view));
                }
            }
        }
    }
    slowest.sort_by_key(|x| std::cmp::Reverse(x.0));
    let slowest = slowest.into_iter().take(10).map(|(_, v)| v).collect();
    failures.truncate(20);
    let history: Vec<Vec<testruns::Case>> = runs
        .iter()
        .rev()
        .take(15)
        .flat_map(|r| {
            [testruns::Db::Sqlite, testruns::Db::Postgres]
                .map(|db| testruns::load_cases(&dir, &r.id, db))
        })
        .collect();
    let trend_runs: Vec<&testruns::RunSummary> = runs.iter().take(20).rev().collect();
    let trend = charts::bars(
        &trend_runs
            .iter()
            .map(|r| {
                (
                    r.duration_ms() as f64,
                    if r.failed() > 0 { "bad" } else { "good" },
                    format!("{}: {}", r.id, ui::duration(r.duration_ms() as i64)),
                )
            })
            .collect::<Vec<_>>(),
        600.0,
        120.0,
    );
    let trend_table = trend_runs
        .iter()
        .rev()
        .map(|r| {
            (
                r.id.clone(),
                ui::duration(r.duration_ms() as i64),
                if r.failed() > 0 {
                    format!("{} failed", r.failed())
                } else {
                    "passed".into()
                },
            )
        })
        .collect();
    ui::page(TestsPage {
        chrome: chrome(&state, "Tests", "dashboard", flash, theme),
        d: dash(
            DEV,
            "/tests",
            Some("/dev/dashboard/stream?view=tests".into()),
            1,
        )
        .ai(&state.config, ai::Tab::Tests, None),
        runs: rows,
        selected,
        slowest,
        failures,
        flaky: testruns::flaky(&history).into_iter().take(10).collect(),
        coverage: testruns::coverage(&dir).map(|p| format!("{p:.1}%")),
        spec: testruns::spec_coverage(&testruns::root()),
        trend,
        trend_table,
        p: progress.p,
        percent: progress.percent,
        postgres: state.config.dashboard.test_database_url.is_some(),
        dir: testruns::DIR,
    })
}

#[derive(Debug, Deserialize)]
struct RunForm {
    db: String,
}

/// `POST /dev/dashboard/tests/run`: `db=sqlite|postgres|both`, in the background.
async fn tests_run(State(state): State<AppState>, Form(f): Form<RunForm>) -> Response {
    let dbs = match f.db.as_str() {
        "sqlite" => vec![testruns::Db::Sqlite],
        "postgres" => vec![testruns::Db::Postgres],
        "both" => vec![testruns::Db::Sqlite, testruns::Db::Postgres],
        _ => {
            return back(
                "/dev/dashboard/tests",
                Flash::alert("Choose SQLite, Postgres or both."),
            );
        }
    };
    let pg = state.config.dashboard.test_database_url.clone();
    if dbs.contains(&testruns::Db::Postgres) && pg.is_none() {
        return back(
            "/dev/dashboard/tests",
            Flash::alert("Set TEST_DATABASE_URL in .env to run on Postgres."),
        );
    }
    if HUB.tests.running() {
        return back(
            "/dev/dashboard/tests",
            Flash::alert("Tests are already running."),
        );
    }
    let target = state
        .config
        .dashboard
        .cargo_target_dir
        .clone()
        .unwrap_or_else(|| "target".into());
    let root = testruns::root();
    tokio::spawn(async move {
        let _ = testruns::run(
            &root,
            FsPath::new(&target),
            &dbs,
            pg.as_deref(),
            "dashboard",
            |_| {},
        )
        .await;
    });
    back(
        "/dev/dashboard/tests",
        Flash::notice("Running the tests; progress below."),
    )
}

// ---------------------------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "kernel/dashboard/github.html")]
struct GithubPage {
    chrome: Chrome,
    d: Dash,
    g: github::GithubView,
    trend: Vec<charts::Bar>,
    rate: String,
}

async fn github_page(
    State(state): State<AppState>,
    theme: ThemeSwitch,
    flash: Option<Extension<RequestFlash>>,
) -> Response {
    let g = github::load(&state.config.dashboard, &HUB.github).await;
    let trend = charts::bars(
        &g.days
            .iter()
            .map(|d| {
                (
                    d.percent().max(2.0),
                    if d.percent() >= 90.0 {
                        "good"
                    } else if d.percent() >= 60.0 {
                        "warn"
                    } else {
                        "bad"
                    },
                    format!("{}: {:.0}% of {} runs", d.date, d.percent(), d.runs),
                )
            })
            .collect::<Vec<_>>(),
        600.0,
        120.0,
    );
    let rate = match (g.rate.remaining, g.rate.limit) {
        (Some(r), Some(l)) => format!("{r} of {l} API calls left this hour"),
        _ if g.fixture => "recorded fixture (no network)".into(),
        _ => String::new(),
    };
    ui::page(GithubPage {
        chrome: chrome(&state, "GitHub", "dashboard", flash, theme),
        d: dash(DEV, "/github", None, 0).ai(&state.config, ai::Tab::Tests, None),
        g,
        trend,
        rate,
    })
}

/// The `/dev` index tile's figure.
pub fn tile_figure() -> String {
    let last = lock(&HUB.series).latest().unwrap_or_default();
    format!(
        "{:.0} req/s · p95 {}",
        last.req,
        micros(u64::from(last.p95))
    )
}

/// Keeps `HeaderValue` imported for handlers that set headers by hand.
#[allow(dead_code)]
fn _header_value_in_use(_: HeaderValue) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_well() {
        assert_eq!(micros(850), "850µs");
        assert_eq!(micros(12_345), "12.3ms");
        assert_eq!(micros(1_240_000), "1.24s");
        assert_eq!(bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(pct(1, 200), "0.5%");
    }

    #[test]
    fn config_redacts_secrets_and_credentials() {
        assert_eq!(redact_env("GITHUB_TOKEN", "ghp_abc"), "set (redacted)");
        // Secrets whose names a denylist of words would miss: only allowlisted names show.
        for (name, value) in [
            ("SMTP_PASS", "hunter2hunter2"),
            ("GITHUB_PAT", "github_pat_11ABCDEF"),
            ("APP_PEPPER", "pepper-value"),
            ("STRIPE_SK", "sk_live_abc"),
            ("APP_TLS_KEY", "/etc/app/tls/key.pem"),
        ] {
            assert_eq!(redact_env(name, value), "set (redacted)", "{name}");
        }
        assert_eq!(redact_env("SMTP_PASS", ""), "not set");
        assert_eq!(redact_env("APP_ENV", "development"), "development");
        assert_eq!(
            redact_env(
                "SMTP_URL",
                "smtps://mailer:s3cret@smtp.example.com:465?auth=plain"
            ),
            "smtps://smtp.example.com:465"
        );
        assert_eq!(redact_env("APP_ADMIN_PASSWORD", ""), "not set");
        assert_eq!(
            redact_env(
                "DATABASE_URL",
                "postgres://u:p@db.internal:5432/app?sslmode=require"
            ),
            "postgres://db.internal:5432/app"
        );
        assert_eq!(redact_env("PORT", "3000"), "3000");
    }
}
