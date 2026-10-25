//! Spec: .offrails/guides/testing.md (kernel: cache, rate limiter, flash, transactions).

use std::time::Duration;

use axum::response::IntoResponse;
use offrails_app::{
    controllers::application::ClientIp,
    kernel::{self, cache::Cache},
};
use sea_orm::ConnectionTrait;

use crate::support::TestApp;

#[tokio::test]
async fn cache_fetch_computes_once_then_hits() {
    let app = TestApp::new().await;
    let cache = Cache::new(app.db().clone());
    let calls = std::sync::atomic::AtomicUsize::new(0);
    for _ in 0..3 {
        let v: Result<u32, sea_orm::DbErr> = cache
            .fetch("answer", Duration::from_secs(60), || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(42)
            })
            .await;
        assert_eq!(v.unwrap(), 42);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn the_cache_is_shared_through_the_database_and_deletes_reach_it() {
    let app = TestApp::new().await;
    let a = Cache::new(app.db().clone());
    let b = Cache::new(app.db().clone()); // another process: empty L1, same table
    a.write("k", &"v".to_owned(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(b.read::<String>("k").await.unwrap().as_deref(), Some("v"));
    a.delete("k").await.unwrap();
    assert_eq!(a.read::<String>("k").await.unwrap(), None);
    // `b` still holds it in L1 for up to a minute; the table is the source of truth.
}

#[tokio::test]
async fn delete_prefix_removes_only_matching_keys() {
    let app = TestApp::new().await;
    let cache = Cache::new(app.db().clone());
    let ttl = Duration::from_secs(60);
    for key in ["books:1", "books:2", "users:1", "books_x"] {
        cache.write(key, &1u8, ttl).await.unwrap();
    }
    cache.delete_prefix("books:").await.unwrap();
    assert_eq!(cache.read::<u8>("books:1").await.unwrap(), None);
    assert_eq!(cache.read::<u8>("books:2").await.unwrap(), None);
    assert_eq!(cache.read::<u8>("users:1").await.unwrap(), Some(1));
    assert_eq!(cache.read::<u8>("books_x").await.unwrap(), Some(1));
}

#[tokio::test]
async fn an_expired_entry_is_a_miss() {
    let app = TestApp::new().await;
    let cache = Cache::new(app.db().clone());
    cache
        .write("short", &1u8, Duration::from_millis(1))
        .await
        .unwrap();
    crate::support::wait_until("the entry expires", async || {
        cache.read::<u8>("short").await.unwrap().is_none()
    })
    .await;
}

#[tokio::test]
async fn the_rate_limiter_blocks_the_eleventh_hit_and_keys_are_independent() {
    let app = TestApp::new().await;
    let rl = &app.state.rate_limit;
    let window = Duration::from_secs(60);
    for _ in 0..10 {
        rl.check("t:a", 10, window).await.unwrap();
    }
    assert!(rl.check("t:a", 10, window).await.is_err());
    assert!(rl.exhausted("t:a", 10).await);
    assert!(rl.check("t:b", 10, window).await.is_ok());
    assert!(!rl.exhausted("t:b", 10).await);
}

#[tokio::test]
async fn the_flash_cookie_is_http_only_lax_and_cleared_after_one_read() {
    let app = TestApp::with_config(offrails_app::config::Config {
        env: offrails_app::config::AppEnv::Development,
        ..offrails_app::config::Config::test()
    })
    .await;
    let res = app
        .post_form("/dev/migrations", &[("return_to", "/")])
        .await;
    let flash = res
        .set_cookies()
        .into_iter()
        .find(|c| c.starts_with("_flash="))
        .expect("a flash cookie is set");
    assert!(flash.contains("HttpOnly"), "{flash}");
    assert!(flash.contains("SameSite=Lax"), "{flash}");
    assert!(flash.contains("Path=/"), "{flash}");
    // The next page shows the message and clears the cookie.
    let page = app.get("/").await;
    page.assert_ok().assert_text("Ran 0 migrations.");
    assert!(
        page.set_cookies()
            .iter()
            .any(|c| c.starts_with("_flash=;") && c.contains("Max-Age=0")),
        "{:?}",
        page.set_cookies()
    );
    app.get("/")
        .await
        .assert_ok()
        .assert_not_contains("Ran 0 migrations.");
}

#[tokio::test]
async fn a_503_from_the_timeout_layer_carries_retry_after() {
    let res = offrails_app::error::retry_after(
        axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    )
    .await;
    assert_eq!(res.headers()["retry-after"], "1");
}

async fn client_ip(app: &TestApp, forwarded: Option<&str>) -> Option<std::net::IpAddr> {
    use axum::{extract::FromRequestParts, http::Request};
    let mut req = Request::builder().uri("/");
    if let Some(v) = forwarded {
        req = req.header("x-forwarded-for", v);
    }
    let (mut parts, _) = req.body(()).unwrap().into_parts();
    ClientIp::from_request_parts(&mut parts, &app.state)
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn x_forwarded_for_is_ignored_unless_a_proxy_is_trusted() {
    let app = TestApp::new().await;
    assert_eq!(client_ip(&app, Some("203.0.113.9")).await, None);

    let mut config = (*app.state.config).clone();
    config.trusted_proxy = true;
    let trusted = TestApp::with_config(config).await;
    // The rightmost hop is the one our own proxy appended; earlier ones are client-supplied.
    assert_eq!(
        client_ip(&trusted, Some("6.6.6.6, 203.0.113.9")).await,
        Some("203.0.113.9".parse().unwrap())
    );
    assert_eq!(client_ip(&trusted, Some("not-an-ip")).await, None);
}

/// Behind two proxies (Cloudflare → Caddy → app) the client is the second hop from the right;
/// the rightmost is Cloudflare's address, which every user would share as a rate-limit key.
#[tokio::test]
async fn the_client_ip_is_the_configured_hop_from_the_right() {
    let app = TestApp::new().await;
    let mut config = (*app.state.config).clone();
    config.trusted_proxy = true;
    config.trusted_proxy_hops = 2;
    let trusted = TestApp::with_config(config).await;
    assert_eq!(
        client_ip(&trusted, Some("6.6.6.6, 203.0.113.9, 198.51.100.7")).await,
        Some("203.0.113.9".parse().unwrap())
    );
    // A header shorter than the hop count was not written by our proxies: not trusted.
    assert_eq!(client_ip(&trusted, Some("203.0.113.9")).await, None);
    // A hop that isn't a bare IP (some proxies write ip:port) is never a client-chosen value.
    assert_eq!(
        client_ip(&trusted, Some("6.6.6.6, 203.0.113.9:4242, 198.51.100.7")).await,
        None
    );
}

/// `kernel::db::begin` is `BEGIN IMMEDIATE` on SQLite. Plain deferred transactions that read then
/// write under concurrency fail with "database is locked" (SQLITE_BUSY_SNAPSHOT), and an
/// in-memory database (one connection) can't show it, so this uses a file.
#[tokio::test]
async fn concurrent_read_then_write_transactions_do_not_fail_with_database_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("t.sqlite3").display()
    );
    let db = kernel::db::connect(&url, 8).await.unwrap();
    db.execute_unprepared("CREATE TABLE counter (n INTEGER NOT NULL)")
        .await
        .unwrap();
    db.execute_unprepared("INSERT INTO counter (n) VALUES (0)")
        .await
        .unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            let tx = kernel::db::begin(&db).await?;
            let rows = tx
                .query_one_raw(sea_orm::Statement::from_string(
                    sea_orm::DbBackend::Sqlite,
                    "SELECT n FROM counter",
                ))
                .await?
                .expect("a row");
            let n: i32 = rows.try_get("", "n")?;
            tokio::task::yield_now().await; // let the other transactions try to get in
            tx.execute_unprepared(&format!("UPDATE counter SET n = {}", n + 1))
                .await?;
            tx.commit().await
        }));
    }
    for t in tasks {
        t.await.unwrap().expect("transaction succeeds");
    }
    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT n FROM counter",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i32>("", "n").unwrap(), 8, "no lost updates");
}

/// Postgres test slots (testing.md#postgres-test-databases): the reset empties what a test
/// wrote, and refuses once a test changed the schema (the harness then clones a fresh slot).
#[tokio::test]
async fn the_postgres_test_reset_empties_tables_and_refuses_a_changed_schema() {
    let app = TestApp::new().await;
    if app.on_sqlite() {
        return;
    }
    let db = app.db();
    let cache = Cache::new(db.clone());
    cache
        .write("left-over", &1u32, Duration::from_secs(60))
        .await
        .unwrap();
    kernel::db::reset_test_database(db).await.unwrap();
    let rows = db
        .query_one_raw(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT count(*) AS n FROM cache_entries",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows.try_get::<i64>("", "n").unwrap(), 0);

    db.execute_unprepared("CREATE TABLE planted (id BIGSERIAL PRIMARY KEY)")
        .await
        .unwrap();
    let refused = kernel::db::reset_test_database(db).await.unwrap_err();
    assert!(refused.to_string().contains("schema changed"), "{refused}");
}

/// An unset `APP_ENV` is an error, never a default: run the binary in a directory with no `.env`,
/// so the fallback has nothing to load (config.rs).
#[test]
fn an_unset_app_env_refuses_to_boot() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_offrails_app"))
        .args(["db", "status"])
        .env_remove("APP_ENV")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("APP_ENV is not set"), "{err}");
}

#[test]
fn ipv6_clients_share_one_rate_limit_per_64() {
    use offrails_app::controllers::application::ClientIp;
    let key = |ip: &str| ClientIp(Some(ip.parse().unwrap())).key("login");
    // The same /64, different interface ids: one budget.
    assert_eq!(
        key("2001:db8:1:2::1"),
        key("2001:db8:1:2:ffff:ffff:ffff:ffff")
    );
    // Another /64, and an IPv4 address, are separate budgets.
    assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
    assert_eq!(key("203.0.113.7"), "login:203.0.113.7");
    assert_ne!(key("203.0.113.7"), key("203.0.113.8"));
    // A dual-stack listener reports IPv4 clients as ::ffff:a.b.c.d: still one budget each, never
    // one shared /64 for every IPv4 user.
    assert_eq!(key("::ffff:203.0.113.7"), "login:203.0.113.7");
    assert_ne!(key("::ffff:203.0.113.7"), key("::ffff:198.51.100.9"));
    assert_eq!(ClientIp(None).key("login"), "login:unknown");
}
