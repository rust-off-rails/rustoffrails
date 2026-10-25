//! Shared extractors (`ApplicationController`): layout context, client IP, `HX-Request`, paging.
//! Authentication, when a sample adds it, is an extractor on each handler, never middleware, so a
//! route's protection is visible in its signature.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Query};
use axum::http::request::Parts;

use crate::error::AppError;
use crate::kernel::css::css_warning;
use crate::kernel::flash::RequestFlash;
use crate::kernel::theme::Theme;
use crate::models::Page;
use crate::state::AppState;
use crate::views::{Ctx, DEFAULT_DESCRIPTION};

impl FromRequestParts<AppState> for Ctx {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let flash = parts
            .extensions
            .get::<RequestFlash>()
            .and_then(|f| f.0.clone());
        let development = state.config.is_development();
        Ok(Self {
            flash,
            title: String::new(),
            description: DEFAULT_DESCRIPTION.to_owned(),
            path: parts.uri.path().to_owned(),
            app_url: state.config.app_url.trim_end_matches('/').to_owned(),
            noindex: false,
            assets: state.assets.assets(),
            dev: development,
            // Missing, empty or stale static/app.css: a plain-text banner first in <main>.
            css_warning: if development { css_warning() } else { None },
            theme: Theme::from_headers(&parts.headers),
            // samples: ctx init
        })
    }
}

/// The caller's IP: the socket address, or the `X-Forwarded-For` hop `APP_TRUSTED_PROXY_HOPS`
/// from the right (default 1: the rightmost) when `APP_TRUSTED_PROXY=true` (behind Fly,
/// kamal-proxy, Render and similar). `None` in tests, which run without a socket.
// SECURITY: only the hops the operator's proxies append are trusted; anything a client put in
// the header sits further left and is never read. A hop that isn't an IP (an `ip:port` some
// proxies write, or a short header) falls back to the socket address, never to a client-chosen
// value (test: the_client_ip_is_the_configured_hop_from_the_right).
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let forwarded = state
            .config
            .trusted_proxy
            .then(|| {
                parts
                    .headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| forwarded_hop(v, state.config.trusted_proxy_hops))
            })
            .flatten();
        let ip = forwarded.or_else(|| {
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        });
        Ok(Self(ip))
    }
}

/// The `hops`-th address from the right of an `X-Forwarded-For` value (1 = rightmost), or
/// `None` when the header is shorter than that or the hop isn't a bare IP.
pub fn forwarded_hop(header: &str, hops: u8) -> Option<IpAddr> {
    header
        .rsplit(',')
        .nth(usize::from(hops.max(1)) - 1)
        .and_then(|hop| hop.trim().parse::<IpAddr>().ok())
}

impl ClientIp {
    /// `"{route}:{ip}"`, or `"{route}:unknown"` without a socket (every test request shares it,
    /// so one `TestApp` can exhaust a limit in a loop).
    pub fn key(&self, route: &str) -> String {
        match self.0 {
            Some(ip) => format!("{route}:{}", rate_limit_ip(ip)),
            None => format!("{route}:unknown"),
        }
    }
}

/// The address a rate limit counts: an IPv4 address as it is (also when a dual-stack socket
/// reports it as `::ffff:a.b.c.d`), an IPv6 address as its /64.
// SECURITY: an ISP hands one subscriber a whole /64 (2^64 addresses), so counting full IPv6
// addresses gives one person an unlimited budget: sign-in, magic-link and TOTP attempts rotate
// the last 64 bits for free (test: ipv6_clients_share_one_rate_limit_per_64).
fn rate_limit_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        // WHY: on a `[::]` listener every IPv4 client arrives IPv4-mapped; as a /64 they would all
        // share `0:0:0:0::/64`, and one client's failures would lock out every IPv4 user.
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => v6
            .to_ipv4_mapped()
            .map(|v4| v4.to_string())
            .unwrap_or_default(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// `true` when the request came from htmx (`HX-Request: true`).
pub struct HxRequest(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for HxRequest {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get("hx-request")
                .is_some_and(|v| v.as_bytes() == b"true"),
        ))
    }
}

impl FromRequestParts<AppState> for Page {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Query(page) = Query::<Page>::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::Invalid(crate::models::Errors::single("page", "is invalid")))?;
        Ok(page)
    }
}

// samples: extractors
