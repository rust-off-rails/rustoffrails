//! The access matrix and the other security guarantees (test-standards.md §3). Most of these
//! iterate over `ROUTES`, so a new route is covered automatically.

use axum::http::StatusCode;
use offrails_app::{
    config::{AppEnv, Config},
    routes::{Access, ROUTES},
};

use crate::support::TestApp;

fn production() -> Config {
    Config {
        env: AppEnv::Production,
        app_url: "https://app.example.com".into(),
        ..Config::test()
    }
}

fn development() -> Config {
    Config {
        env: AppEnv::Development,
        ..Config::test()
    }
}

/// A concrete URL for a route pattern (parameters a test never resolves to a real record).
pub fn concrete(path: &str) -> String {
    path.replace("{id}", "1")
        .replace("{n}", "0")
        .replace("{token}", "not-a-real-token")
        .replace("{mailer}", "no_such_mailer")
        .replace("{action}", "no_such_action")
        .replace("{name}", "default")
        .split('/')
        .map(|seg| {
            // Any other parameter (`{slug}`, `{post_id}`…): a value no record has.
            if seg.starts_with('{') && seg.ends_with('}') {
                if seg == "{id}" || seg.ends_with("_id}") {
                    "1"
                } else {
                    "example"
                }
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn request(app: &TestApp, method: &str, path: &str) -> StatusCode {
    match method {
        // WHY: headers only: a streaming route (SSE) never ends its body.
        "GET" => app.request_head(path, &[]).await.0,
        other => app.request_method(other, path).await.status,
    }
}

/// Every non-public route, requested anonymously in production: never a 2xx.
#[tokio::test]
async fn non_public_routes_never_answer_2xx_anonymously() {
    let app = TestApp::with_config(production()).await;
    for (method, path, handler, access) in ROUTES {
        if *access == Access::Public {
            continue;
        }
        let status = request(&app, method, &concrete(path)).await;
        assert!(
            !status.is_success(),
            "{method} {path} ({handler}, {}) answered {status} anonymously",
            access.as_str()
        );
    }
}

/// `Dev` routes don't exist in production or test: 404 from the router fallback, every one.
#[tokio::test]
async fn dev_routes_are_404_outside_development() {
    for config in [production(), Config::test()] {
        let app = TestApp::with_config(config).await;
        for (method, path, handler, access) in ROUTES {
            if *access != Access::Dev {
                continue;
            }
            let status = request(&app, method, &concrete(path)).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} ({handler}) exists outside development"
            );
        }
    }
}

/// A cross-site POST to every unsafe route is rejected.
#[tokio::test]
async fn cross_site_posts_to_every_unsafe_route_are_403() {
    let app = TestApp::with_config(development()).await;
    for (method, path, handler, _) in ROUTES {
        if *method == "GET" {
            continue;
        }
        let res = app
            .post_form_with_headers(&concrete(path), &[], &[("sec-fetch-site", "cross-site")])
            .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{path} ({handler}) accepted a cross-site POST"
        );
    }
}

#[tokio::test]
async fn a_cookie_post_with_no_origin_and_no_fetch_metadata_is_403() {
    let app = TestApp::with_config(development()).await;
    app.set_cookie("session_token", "phase0-session");
    app.without_fetch_metadata();
    let res = app
        .post_form("/theme", &[("theme", "dark"), ("return_to", "/")])
        .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "a session-cookie POST with no Origin and no Sec-Fetch-Site was accepted"
    );
}

/// Without fetch metadata (older browsers) the `Origin` decides.
#[tokio::test]
async fn the_origin_header_decides_without_fetch_metadata() {
    let app = TestApp::with_config(development()).await;
    app.without_fetch_metadata()
        .with_origin("https://evil.example");
    app.post_form("/dev/migrations", &[("return_to", "/")])
        .await
        .assert_status(StatusCode::FORBIDDEN);

    app.with_origin("http://localhost:3000");
    app.post_form("/dev/migrations", &[("return_to", "/")])
        .await
        .assert_redirect_to("/");
}

#[tokio::test]
async fn the_error_pages_do_not_leak_internals_in_production() {
    let app = TestApp::with_config(production()).await;
    let res = app.get("/nope").await;
    res.assert_status(StatusCode::NOT_FOUND)
        .assert_not_contains("panicked")
        .assert_not_contains("DATABASE_URL");
}

/// A route someone mounts but forgets to list in `ROUTES` is invisible to the access matrix,
/// the CSRF sweep and the routability test. Parses the files that mount routes.
#[test]
fn every_mounted_route_is_in_the_routes_table() {
    let listed: std::collections::BTreeSet<&str> = ROUTES.iter().map(|(_, p, _, _)| *p).collect();
    let mut missing = Vec::new();
    let mut checked = 0;
    // Each feature's routes file (src/routes/<feature>.rs) mounts its own.
    let mut files: Vec<String> = std::fs::read_dir("src/routes")
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for file in [
        "src/routes.rs",
        "src/controllers/dev.rs",
        // samples: router sources
    ]
    .into_iter()
    .chain(files.iter().map(String::as_str))
    {
        let source = std::fs::read_to_string(file).expect("read source");
        for (start, _) in source
            .match_indices(".route(\"")
            .chain(source.match_indices(".route_service(\""))
        {
            let rest = &source[start..];
            let after = &rest[rest.find('"').unwrap() + 1..];
            let path = &after[..after.find('"').unwrap()];
            checked += 1;
            if !listed.contains(path) {
                missing.push(format!("{file}: {path}"));
            }
        }
    }
    assert!(checked >= 10, "the parser found only {checked} route calls");
    assert!(
        missing.is_empty(),
        "mounted but not in ROUTES (add the row with its real Access): {missing:#?}"
    );
}

/// DNS rebinding: a page on `evil.example` re-points its name at 127.0.0.1 and reads the
/// development tools same-origin. Development refuses any `Host` that isn't this machine,
/// `APP_URL`'s or an `APP_ALLOWED_HOSTS` entry, before any handler runs; the page says how to
/// allow a host on purpose.
#[tokio::test]
async fn development_refuses_a_rebound_host_name() {
    let app = TestApp::with_config(development()).await;
    for path in ["/dev", "/", "/up"] {
        let res = app
            .get_with_headers(path, &[("host", "evil.example:3000")])
            .await;
        res.assert_status(StatusCode::FORBIDDEN)
            .assert_contains("Blocked host: evil.example")
            .assert_contains("APP_ALLOWED_HOSTS=evil.example");
        assert!(
            res.headers.contains_key("content-security-policy"),
            "the refusal carries the header matrix"
        );
    }
    // A same-origin POST from the rebound page (Fetch Metadata and Origin agree) is refused too:
    // the CSRF layer alone would pass it.
    app.post_form_with_headers(
        "/theme",
        &[("theme", "dark")],
        &[
            ("host", "evil.example:3000"),
            ("origin", "http://evil.example:3000"),
        ],
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
    // Markup in the Host header is escaped on the page.
    app.get_with_headers("/", &[("host", "a<b>.example")])
        .await
        .assert_status(StatusCode::FORBIDDEN)
        .assert_not_contains("a<b>");
}

#[tokio::test]
async fn development_answers_local_and_configured_hosts() {
    let app = TestApp::with_config(Config {
        app_url: "https://dev.example.com".into(),
        allowed_hosts: vec!["tunnel.example".into(), ".lan.example".into()],
        ..development()
    })
    .await;
    for host in [
        "localhost:3000",
        "127.0.0.1:3000",
        "[::1]:3000",
        "dev.example.com",
        "tunnel.example",
        "box.lan.example:3000",
    ] {
        app.get_with_headers("/up", &[("host", host)])
            .await
            .assert_ok();
    }
    // No Host at all (HTTP/1.0, a health probe): nothing to rebind.
    app.get("/up").await.assert_ok();
}

#[tokio::test]
async fn production_and_test_answer_any_host() {
    for config in [production(), Config::test()] {
        let app = TestApp::with_config(config).await;
        app.get_with_headers("/up", &[("host", "evil.example")])
            .await
            .assert_ok();
    }
}

#[test]
fn get_handlers_do_not_write_to_the_database() {
    const WRITE_PATTERNS: &[&str] = &[
        "Entity::insert",
        "ActiveModel",
        "update_many",
        "delete_many",
        "delete_by_id",
        "execute_unprepared",
        "execute_raw",
        ".save(",
        ".insert(db",
        ".insert(&db",
        ".insert(txn",
        ".insert(&txn",
    ];
    let mut checked = 0;
    let mut violations = Vec::new();
    // WHY: the app's own routes, the dev routes, and every feature's routes file (`src/routes/`).
    let mut sources = vec![
        "src/routes.rs".to_owned(),
        "src/controllers/dev.rs".to_owned(),
    ];
    if let Ok(dir) = std::fs::read_dir("src/routes") {
        sources.extend(
            dir.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                .map(|p| p.to_string_lossy().into_owned()),
        );
    }
    for source_path in &sources {
        let source = std::fs::read_to_string(source_path).expect("read routes");
        for (at, _) in source.match_indices(".route(\"") {
            let rest = &source[at..];
            let Some(get_at) = rest.find("get(") else {
                continue;
            };
            if get_at > 120 {
                continue;
            }
            let after = &rest[get_at + 4..];
            let handler: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            if handler.is_empty() {
                continue;
            }
            // `posts::index`, `crate::controllers::posts::index` (generated features) or a bare
            // `index` in the dev routes: find the file that defines it, in `posts.rs`, or any file
            // of a `posts/` folder (0.7.3 features: one file per action pair).
            let handler_path = handler.trim_start_matches("crate::controllers::");
            let (module, func) = match handler_path.rsplit_once("::") {
                Some((module, func)) => (module.replace("::", "/"), func),
                None if source_path.ends_with("dev.rs") => ("dev".to_owned(), handler_path),
                None => continue,
            };
            let mut candidates = vec![format!("src/controllers/{module}.rs")];
            if let Ok(dir) = std::fs::read_dir(format!("src/controllers/{module}")) {
                candidates.extend(
                    dir.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                        .map(|p| p.to_string_lossy().into_owned()),
                );
            }
            let found = candidates.iter().find_map(|file| {
                let code = std::fs::read_to_string(file).ok()?;
                let at = code.find(&format!("fn {func}("))?;
                Some((file.clone(), code, at))
            });
            let Some((file, code, fn_at)) = found else {
                violations.push(format!(
                    "{handler}: fn {func} not found under src/controllers/{module}"
                ));
                continue;
            };
            let body_start = code[fn_at..].find('{').expect("fn body") + fn_at;
            let mut depth = 0i32;
            let mut body_end = body_start;
            for (i, c) in code[body_start..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            body_end = body_start + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            checked += 1;
            for (n, line) in code[body_start..body_end].lines().enumerate() {
                if line.contains("// writes:") {
                    continue;
                }
                if WRITE_PATTERNS.iter().any(|p| line.contains(p)) {
                    violations.push(format!(
                        "{file} fn {func} (GET handler) line {}: {}",
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(checked >= 5, "the parser found only {checked} GET handlers");
    assert!(
        violations.is_empty(),
        "safe methods must not change state (guides/security.md#csrf); \
         exempt one line with '// writes: <reason>' only when the write is the point: {violations:#?}"
    );
}

/// ZAP's "Suspicious Comments" rule (10027) reads these words in served script comments as SQL or
/// debug leftovers. Public pages load the top-level `static/*.js`, so their comments avoid them
/// (security.md, Gotchas).
#[test]
fn public_scripts_have_no_comment_words_zap_flags() {
    const WORDS: &str = "todo fixme bug bugs xxx query db admin administrator user username select where from later debug";
    let mut found = Vec::new();
    for entry in std::fs::read_dir("static").expect("static/") {
        let path = entry.expect("a static entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a readable script");
        for (n, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            let comment = match line.find("//") {
                Some(at) => &line[at..],
                None if trimmed.starts_with("/*") || trimmed.starts_with('*') => trimmed,
                None => continue,
            };
            let lower = comment.to_ascii_lowercase();
            let words = lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            for word in words.filter(|w| WORDS.split(' ').any(|z| z == *w)) {
                found.push(format!("{}:{}: {word}", path.display(), n + 1));
            }
        }
    }
    assert!(found.is_empty(), "reword these comments: {found:?}");
}
