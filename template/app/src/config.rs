//! Typed configuration, parsed from env vars once at startup.
//! This is the only module that reads environment variables.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// The human product name: page titles, kernel UIs (/dev), the default MAIL_FROM. It is
/// brand.toml's `name` (`cargo offrails brand` writes src/brand.rs). Never derived from the crate
/// name: crates get renamed, products don't.
pub const APP_NAME: &str = crate::brand::NAME;
/// The tagline: brand.toml's `tagline` (`cargo offrails brand` writes src/brand.rs), the one
/// source for the home page title ("Offrails App: <tagline>"), the social card and any nav that
/// shows it. Change it in brand.toml, never here.
pub const APP_TAGLINE: &str = crate::brand::TAGLINE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Development,
    Test,
    Production,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

/// `APP_TLS_CERT` + `APP_TLS_KEY`: serve HTTPS (h2 + http/1.1 via ALPN) directly.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: String,
    pub key: String,
}

#[derive(Clone)]
pub struct Config {
    pub env: AppEnv,
    pub database_url: String,
    /// `DATABASE_POOL`: 4 for SQLite (one writer at a time; each connection keeps its own page
    /// cache), 10 for Postgres.
    pub database_pool: u32,
    /// `APP_WORKER_THREADS`: Tokio worker threads (default 2). Raise it for CPU-bound request work
    /// on a box with more cores; I/O-bound apps rarely gain past 2–4.
    pub worker_threads: usize,
    /// `APP_BLOCKING_THREADS`: the cap on Tokio's blocking pool (default 16; Tokio's own is 512).
    /// `spawn_blocking` work (static files, password hashing) queues past it instead of adding threads.
    pub blocking_threads: usize,
    pub app_url: String,
    pub port: u16,
    /// `APP_SHUTDOWN_DRAIN_SECS`: how long open connections get after SIGTERM.
    pub shutdown_drain: Duration,
    /// `APP_TRUSTED_PROXY`: read the client IP from `X-Forwarded-For` (rate limiting).
    pub trusted_proxy: bool,
    /// `APP_TRUSTED_PROXY_HOPS` (default 1): how many proxies append to `X-Forwarded-For` before
    /// the app. The client is that many hops from the right: 1 behind one platform proxy (Fly, Render),
    /// 2 behind Cloudflare → Caddy → app. Fewer than the real count takes a proxy's address
    /// (every user shares one rate-limit key); more lets a client forge its address.
    pub trusted_proxy_hops: u8,
    pub log_format: LogFormat,
    /// `APP_REVISION`: git sha of the build, shown on /dev/system.
    pub revision: Option<String>,
    pub tls: Option<TlsFiles>,
    /// `APP_DEV_HTTP_PORT` (development only, with TLS on): the same app on plain http.
    pub dev_http_port: Option<u16>,
    /// `APP_BIND`: the address `serve` listens on. Development: 127.0.0.1 (this machine only);
    /// production: 0.0.0.0 (the container's interface, behind the platform's proxy).
    pub bind: IpAddr,
    /// `APP_ALLOWED_HOSTS`: extra `Host` names development answers (a tunnel, a LAN name);
    /// `.example.com` allows its subdomains too. Localhost and `APP_URL`'s host always work.
    pub allowed_hosts: Vec<String>,
    /// Kept for parts that read it: always `false` now that an unset `APP_ENV` refuses to boot.
    pub env_defaulted: bool,
    // samples: config fields
}

// Credentials live in these fields, so Debug is hand-written.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Config");
        d.field("env", &self.env)
            .field("database_url", &"[redacted]")
            .field("database_pool", &self.database_pool)
            .field("worker_threads", &self.worker_threads)
            .field("blocking_threads", &self.blocking_threads)
            .field("app_url", &self.app_url)
            .field("port", &self.port)
            .field("shutdown_drain", &self.shutdown_drain)
            .field("trusted_proxy", &self.trusted_proxy)
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .field("log_format", &self.log_format)
            .field("revision", &self.revision)
            .field("tls", &self.tls.is_some())
            .field("dev_http_port", &self.dev_http_port)
            .field("bind", &self.bind)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("env_defaulted", &self.env_defaulted);
        // samples: config debug
        d.finish()
    }
}

const DEV_DATABASE_URL: &str = "sqlite://storage/development.sqlite3?mode=rwc";

/// In development `APP_URL` follows `PORT` (https when `bin/dev` serves TLS), so links and the
/// CSRF origin check still work when the app runs on another port.
pub fn dev_app_url(port: u16, https: bool) -> String {
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://localhost:{port}")
}

/// Where links (emails, canonical URLs, the CSRF origin) point. Production: `APP_URL`, required.
/// Development: the address actually served. A localhost `APP_URL` left in `.env` from another
/// port would send every emailed link to the wrong server, so on localhost the port and scheme
/// always follow `PORT` and TLS; a non-local `APP_URL` (a tunnel) is kept as given.
pub fn resolve_app_url(env: AppEnv, app_url: Option<&str>, port: u16, https: bool) -> String {
    let given = app_url.map(|u| u.trim().trim_end_matches('/').to_owned());
    match (env, given) {
        (AppEnv::Production, Some(url)) => url,
        (AppEnv::Production, None) => String::new(),
        (_, Some(url)) if !is_localhost(&url) => url,
        (_, Some(url)) => {
            let served = dev_app_url(port, https);
            if url != served {
                tracing::warn!(app_url = %url, served = %served, "APP_URL points at another local address; links follow the one being served");
            }
            served
        }
        (_, None) => dev_app_url(port, https),
    }
}

/// `APP_BIND`, or the environment's default: loopback in development, every interface otherwise.
// SECURITY: development serves /dev tools (mailbox, dashboard, SQL, logs) without a login; on
// 0.0.0.0 anyone on the café Wi-Fi could open them. A container sets APP_BIND=0.0.0.0 explicitly
// (test: development_binds_loopback_unless_told_otherwise).
pub fn bind_address(env: AppEnv, raw: Option<&str>) -> Result<IpAddr, String> {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .map_err(|_| {
                format!("APP_BIND (expected an IP address such as 127.0.0.1 or 0.0.0.0, got {v:?})")
            }),
        None if env == AppEnv::Development => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        None => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    }
}

/// `APP_ALLOWED_HOSTS`: comma-separated host names, lowercased, ports dropped.
pub fn parse_allowed_hosts(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

fn is_localhost(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0")
}

impl Config {
    /// Called once in `main`. Collects ALL missing or invalid vars and fails with one message.
    pub fn from_env() -> anyhow::Result<Self> {
        let get_env = || {
            std::env::var("APP_ENV")
                .ok()
                .filter(|v| !v.trim().is_empty())
        };
        // SECURITY: an unset APP_ENV is an error, never a default. Development mounts /dev tools
        // without a login, so "forgot to set it on a server" must not silently become
        // development. `.env` (bin/setup writes APP_ENV=development there) is read first, so
        // `cargo run` in a set-up checkout still works; a production image has no `.env`
        // (.dockerignore) and sets APP_ENV itself (test: an_unset_app_env_refuses_to_boot, kernel_test.rs).
        if get_env().is_none() {
            dotenvy::dotenv().ok();
        }
        let env = match get_env().as_deref() {
            Some("development") => AppEnv::Development,
            Some("test") => AppEnv::Test,
            Some("production") => AppEnv::Production,
            Some(other) => {
                anyhow::bail!("invalid APP_ENV: {other} (development | test | production)")
            }
            None => anyhow::bail!(
                "APP_ENV is not set. Locally: APP_ENV=development in .env (bin/setup creates it \
                 from .env.example). On every server: APP_ENV=production (the Dockerfile sets it)."
            ),
        };
        let env_defaulted = false;

        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let mut missing: Vec<String> = Vec::new();
        let mut invalid: Vec<String> = Vec::new();
        let production = env == AppEnv::Production;

        let mut parse_num = |name: &str, default: u64| -> u64 {
            match get(name) {
                None => default,
                Some(v) => v.parse().unwrap_or_else(|_| {
                    invalid.push(format!("{name} (expected a number)"));
                    default
                }),
            }
        };
        // PORT first: the development APP_URL default depends on it.
        let port = parse_num("PORT", 3000) as u16;
        // 0 = the backend's default, chosen below once DATABASE_URL is known.
        let database_pool = parse_num("DATABASE_POOL", 0) as u32;
        let worker_threads = (parse_num("APP_WORKER_THREADS", 2) as usize).max(1);
        let blocking_threads = (parse_num("APP_BLOCKING_THREADS", 16) as usize).max(1);
        let shutdown_drain = Duration::from_secs(parse_num("APP_SHUTDOWN_DRAIN_SECS", 5));
        let dev_http_port = get("APP_DEV_HTTP_PORT").map(|v| {
            v.parse::<u16>().unwrap_or_else(|_| {
                invalid.push("APP_DEV_HTTP_PORT (expected a port number)".into());
                0
            })
        });
        if dev_http_port.is_some() && env != AppEnv::Development {
            invalid.push("APP_DEV_HTTP_PORT (development only)".into());
        }

        let tls = match (get("APP_TLS_CERT"), get("APP_TLS_KEY")) {
            (Some(cert), Some(key)) => Some(TlsFiles { cert, key }),
            (None, None) => None,
            _ => {
                invalid.push("APP_TLS_CERT and APP_TLS_KEY (set both, or neither)".into());
                None
            }
        };

        let database_url = match get("DATABASE_URL") {
            Some(v) => v,
            None if production => {
                missing.push("DATABASE_URL".into());
                String::new()
            }
            None => DEV_DATABASE_URL.into(),
        };
        let database_pool = match database_pool {
            0 if database_url.starts_with("sqlite:") => 4,
            0 => 10,
            n => n,
        };
        if production && get("APP_URL").is_none() {
            missing.push("APP_URL".into());
        }
        // With APP_DEV_HTTP_PORT the browser uses the plain-http port; links stay http.
        let https = tls.is_some() && dev_http_port.is_none();
        let app_url = resolve_app_url(env, get("APP_URL").as_deref(), port, https);

        let trusted_proxy = match get("APP_TRUSTED_PROXY").as_deref() {
            None | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            Some(_) => {
                invalid.push("APP_TRUSTED_PROXY (expected true or false)".into());
                false
            }
        };

        let trusted_proxy_hops = match get("APP_TRUSTED_PROXY_HOPS") {
            None => 1,
            Some(v) => match v.parse::<u8>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    invalid.push("APP_TRUSTED_PROXY_HOPS (expected a number from 1 to 255)".into());
                    1
                }
            },
        };

        let bind = bind_address(env, get("APP_BIND").as_deref()).unwrap_or_else(|problem| {
            invalid.push(problem);
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        });
        let dev_remote = match get("APP_DEV_REMOTE").as_deref() {
            None | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            Some(_) => {
                invalid.push("APP_DEV_REMOTE (expected true or false)".into());
                false
            }
        };
        if env == AppEnv::Development && !bind.is_loopback() && !dev_remote {
            invalid.push(
                "APP_DEV_REMOTE (set APP_DEV_REMOTE=1 to serve development beyond this machine: \
                 /dev tools have no login, and a Host header is not authentication)"
                    .into(),
            );
        }
        let allowed_hosts = parse_allowed_hosts(get("APP_ALLOWED_HOSTS").as_deref());

        let log_format = match get("LOG_FORMAT").as_deref() {
            None if production => LogFormat::Json,
            None => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some("pretty") => LogFormat::Pretty,
            Some(_) => {
                invalid.push("LOG_FORMAT (expected pretty or json)".into());
                LogFormat::Pretty
            }
        };

        // samples: config parse

        if !missing.is_empty() || !invalid.is_empty() {
            let mut parts = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("missing required env vars: {}", missing.join(", ")));
            }
            if !invalid.is_empty() {
                parts.push(format!("invalid env vars: {}", invalid.join(", ")));
            }
            anyhow::bail!("{} (see .env.example)", parts.join("; "));
        }

        Ok(Self {
            env,
            database_url,
            database_pool,
            worker_threads,
            blocking_threads,
            app_url,
            port,
            shutdown_drain,
            trusted_proxy,
            trusted_proxy_hops,
            log_format,
            revision: get("APP_REVISION"),
            tls,
            dev_http_port,
            bind,
            allowed_hosts,
            env_defaulted,
            // samples: config init
        })
    }

    /// The test suite's config. Never reads the environment.
    pub fn test() -> Self {
        Self {
            env: AppEnv::Test,
            database_url: "sqlite::memory:".into(),
            database_pool: 5,
            worker_threads: 2,
            blocking_threads: 16,
            app_url: dev_app_url(3000, false),
            port: 3000,
            shutdown_drain: Duration::from_secs(5),
            trusted_proxy: false,
            trusted_proxy_hops: 1,
            log_format: LogFormat::Pretty,
            revision: None,
            tls: None,
            dev_http_port: None,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            allowed_hosts: Vec::new(),
            env_defaulted: false,
            // samples: config test
        }
    }

    pub fn is_production(&self) -> bool {
        self.env == AppEnv::Production
    }

    pub fn is_development(&self) -> bool {
        self.env == AppEnv::Development
    }

    /// Loud warnings `main` logs right after logging starts (parts add theirs here).
    pub fn boot_warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

// samples: config sections

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_config_never_reads_the_environment() {
        let c = Config::test();
        assert_eq!(c.database_url, "sqlite::memory:");
        assert!(!c.is_production() && !c.is_development());
    }

    #[test]
    fn debug_redacts_credentials() {
        let mut c = Config::test();
        c.database_url = "postgres://user:hunter2@db.example.com/app".into();
        assert!(!format!("{c:?}").contains("hunter2"));
    }

    #[test]
    fn a_stale_local_app_url_follows_the_port_being_served() {
        let dev = AppEnv::Development;
        assert_eq!(
            resolve_app_url(dev, Some("http://localhost:3000/"), 3960, false),
            "http://localhost:3960"
        );
        assert_eq!(
            resolve_app_url(dev, Some("http://127.0.0.1:3000"), 3960, true),
            "https://localhost:3960"
        );
        assert_eq!(
            resolve_app_url(dev, None, 3960, false),
            "http://localhost:3960"
        );
        // A tunnel or a LAN name is kept: that's where the links must go.
        assert_eq!(
            resolve_app_url(dev, Some("https://dev.example.com"), 3960, false),
            "https://dev.example.com"
        );
        assert_eq!(
            resolve_app_url(
                AppEnv::Production,
                Some("https://app.example.com/"),
                3960,
                false
            ),
            "https://app.example.com"
        );
    }

    #[test]
    fn development_binds_loopback_unless_told_otherwise() {
        let local = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(bind_address(AppEnv::Development, None), Ok(local));
        assert_eq!(bind_address(AppEnv::Development, Some("")), Ok(local));
        // A container opts in.
        assert_eq!(bind_address(AppEnv::Development, Some("0.0.0.0")), Ok(any));
        assert_eq!(
            bind_address(AppEnv::Development, Some("[::1]")),
            Ok("::1".parse().unwrap())
        );
        // Production keeps listening on every interface (its proxy is in front).
        assert_eq!(bind_address(AppEnv::Production, None), Ok(any));
        assert!(bind_address(AppEnv::Development, Some("localhost")).is_err());
        assert_eq!(
            parse_allowed_hosts(Some(" Dev.Example.com, .tunnel.example ,")),
            ["dev.example.com", ".tunnel.example"]
        );
    }

    #[test]
    fn the_development_url_follows_port_and_tls() {
        assert_eq!(dev_app_url(3001, false), "http://localhost:3001");
        assert_eq!(dev_app_url(3000, true), "https://localhost:3000");
    }
}
