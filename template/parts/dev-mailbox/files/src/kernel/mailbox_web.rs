//! The dev mailbox UI (Swoosh's `/dev/mailbox`, letter_opener_web): every email the app sent in
//! development, rendered, live. Mounted only when `APP_ENV=development`; in production the routes
//! don't exist. Every response is `no-store`. Spec: .offrails/guides/mailers.md#the-dev-mailbox
//!
//! GUIDE: .offrails/guides/mailers.md#the-dev-mailbox-kernelmailboxrs-swooshs-local-mailbox
//!
//! It styles itself: `static/kernel/mailbox.css` and `static/kernel/mailbox.js` ship with it and
//! every mailbox page declares them on its `Chrome`. Nothing here depends on `assets/app.css`.
//!
//! The HTML body of a message is never part of a mailbox page: it is served alone from
//! `/{id}/html` into an iframe with no script and an opaque origin (the `sandbox` attribute and the
//! same `sandbox` in the response's own CSP), so mail can't run code or reach this page.

use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::Utc;
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::kernel::flash::{Flash, FlashMessage, RequestFlash};
use crate::kernel::mailbox::{self, Model};
use crate::kernel::mailer::{Attachment, Email, Mailer, Preview};
use crate::kernel::theme::ThemeSwitch;
use crate::kernel::ui::{self, Chrome};
use crate::state::AppState;

/// The inbox list shows at most this many (newest first).
const LIST_LIMIT: u64 = 200; // APP: more rows, longer pages
/// `/dev/mailbox.json` returns this many.
const JSON_LIMIT: u64 = 50;
/// How often a live inbox checks for new mail.
// WHY: a poll of the table, not an in-process channel: mail from a worker or a task (another
// process) must show too. One cheap query per open tab per second, in development only.
const POLL: Duration = Duration::from_secs(1);

// SECURITY: mail can't run script or reach this page: `sandbox` without allow-scripts and without
// allow-same-origin gives the body an opaque origin, also when opened outside the frame (test: the_html_body_is_served_alone_in_a_sandbox).
/// The iframe body's own policy: no script anywhere (`sandbox` without `allow-scripts`), an opaque
/// origin, only this page may frame it. Images, styles and fonts may load, as in a mail client.
pub const BODY_CSP: &str = "sandbox allow-popups allow-popups-to-escape-sandbox; default-src 'none'; \
     img-src * data: cid:; style-src 'unsafe-inline' *; font-src * data:; form-action 'none'; \
     frame-ancestors 'self'";

// SECURITY: an HTML or SVG attachment opened from here would run on this origin; always a download, sandboxed (test: attachments_are_downloads_never_documents_on_this_origin).
/// Attachments are downloads, never documents on this origin.
const ATTACHMENT_CSP: &str = "sandbox; default-src 'none'";

pub fn router(previews: &'static [Preview]) -> axum::Router<AppState> {
    Router::new()
        .route("/dev/mailbox", get(index))
        .route("/dev/mailbox.json", get(list_json))
        .route("/dev/mailbox/latest.json", get(latest_json))
        .route("/dev/mailbox/list", get(list_fragment))
        .route("/dev/mailbox/events", get(events))
        .route("/dev/mailbox/clear", post(clear))
        .route("/dev/mailbox/test", post(send_test))
        .route("/dev/mailbox/previews", get(previews_index))
        .route("/dev/mailbox/previews/{mailer}/{action}", get(preview_show))
        .route(
            "/dev/mailbox/previews/{mailer}/{action}/html",
            get(preview_html),
        )
        .route("/dev/mailbox/{id}", get(show))
        .route("/dev/mailbox/{id}/html", get(html_body))
        .route("/dev/mailbox/{id}/message.eml", get(download))
        .route("/dev/mailbox/{id}/attachments/{n}", get(attachment))
        .route("/dev/mailbox/{id}/delete", post(delete))
        .route("/dev/mailers", get(mailers_redirect))
        .layer(Extension(Previews(previews)))
        // SECURITY: mail content never lands in a shared or back/forward cache (test: every_mailbox_response_is_no_store).
        .layer(ui::no_store())
}

#[derive(Clone, Copy)]
struct Previews(&'static [Preview]);

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

/// One row of the inbox list.
pub struct Row {
    pub id: i64,
    pub href: String,
    pub to: String,
    pub subject: String,
    pub snippet: String,
    pub when: String,
    pub at: String,
    pub unread: bool,
    pub attachments: usize,
    pub selected: bool,
}

/// One preview in the Previews list.
pub struct PreviewRow {
    pub name: &'static str,
    pub href: String,
    pub selected: bool,
}

pub struct AttachmentView {
    pub filename: String,
    pub content_type: String,
    pub size: String,
    pub href: Option<String>,
    /// Raster images get a thumbnail (an `<img>` of the download URL; never SVG, which can carry
    /// script).
    pub image: bool,
}

// SECURITY: raster types only; SVG can carry script (test: attachments_are_downloads_never_documents_on_this_origin).
fn is_previewable_image(content_type: &str) -> bool {
    matches!(
        content_type.to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
    )
}

pub struct Link {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// What the message pane shows, for a stored message or a preview alike.
pub struct MessageView {
    /// The stored message's id (`None` for a preview).
    pub id: Option<i64>,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub date: String,
    pub headers: Vec<(String, String)>,
    pub html_src: String,
    pub text: String,
    pub raw: String,
    pub links: Vec<String>,
    pub codes: Vec<String>,
    pub attachments: Vec<AttachmentView>,
    pub tab: &'static str,
    pub tabs: Vec<Link>,
    pub views: Vec<Link>,
    pub view: &'static str,
    pub eml_href: Option<String>,
    pub delete_action: Option<String>,
    pub preview: bool,
    pub checks: Vec<Check>,
    /// Errors and warnings among `checks` (infos don't count).
    pub problems: usize,
}

impl MessageView {
    fn counted(mut self) -> Self {
        self.problems = self.checks.iter().filter(|c| c.level != "info").count();
        self
    }
}

/// A deliverability or rendering problem the viewer points out.
pub struct Check {
    /// "error", "warn" or "info".
    pub level: &'static str,
    pub message: String,
}

/// Gmail clips a message whose HTML is larger than this ("[Message clipped]").
const GMAIL_CLIP_BYTES: usize = 102 * 1024;

/// What a careful reviewer would flag before this email reaches real inboxes.
pub fn checks(text: &str, html: &str, headers: &[(String, String)], subject: &str) -> Vec<Check> {
    let mut out = Vec::new();
    let mut add = |level, message: String| out.push(Check { level, message });
    let lower = html.to_ascii_lowercase();
    let relative = lower.matches("href=\"/").count()
        + lower.matches("href='/").count()
        + lower.matches("src=\"/").count();
    if relative > 0 {
        add(
            "error",
            format!(
                "{relative} relative URL(s): email clients can't resolve /paths. Build links from APP_URL."
            ),
        );
    }
    if text.trim().is_empty() {
        add("warn", "No plain-text part: some clients show nothing, and spam filters score HTML-only mail higher.".into());
    }
    let images = lower.matches("<img").count();
    let with_alt = lower
        .split("<img")
        .skip(1)
        .filter(|tag| tag.split('>').next().unwrap_or("").contains("alt="))
        .count();
    if images > with_alt {
        add(
            "warn",
            format!(
                "{} of {images} image(s) have no alt text: blocked images and screen readers show nothing.",
                images - with_alt
            ),
        );
    }
    if html.len() > GMAIL_CLIP_BYTES {
        add(
            "warn",
            format!(
                "The HTML is {}: Gmail clips messages over 102 KB.",
                ui::bytes(html.len())
            ),
        );
    }
    let wide = overflows_320(html);
    if !wide.is_empty() {
        add(
            "warn",
            format!(
                "Overflows a 320 px screen (the narrowest phones scroll sideways): {}. Give it max-width:100% (images also height:auto), or a fluid width.",
                wide.join("; ")
            ),
        );
    }
    if lower.contains("<script") {
        add(
            "warn",
            "Contains <script>: every mail client strips it (and it never runs here).".into(),
        );
    }
    if subject.trim().is_empty() {
        add("warn", "No subject.".into());
    } else if subject.chars().count() > 78 {
        add(
            "info",
            "The subject is over 78 characters: most inboxes cut it off.".into(),
        );
    }
    let local = ["//localhost", "//127.0.0.1", "//0.0.0.0"];
    if local.iter().any(|h| lower.contains(h) || text.contains(h)) {
        add("info", "Links point at this machine. That's right in development; production links come from APP_URL.".into());
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("list-unsubscribe"))
    {
        add("info", "No List-Unsubscribe header. Fine for transactional mail; bulk senders need it (Gmail and Yahoo require one-click unsubscribe).".into());
    }
    out
}

/// The narrowest screen a mail must fit.
const NARROW_PX: u32 = 320;

/// What makes a mail wider than 320 px, judged from its HTML without a browser: an element with a
/// fixed width over 320 px (a `width` attribute, or `width:`/`min-width:` in its `style`) that no
/// `max-width` reins in (on the element, or a `<style>` rule for its tag such as
/// `img{max-width:100%}`), and `<style>` rules that set such a width without a `max-width`.
/// Percentages, `auto` and widths up to 320 px pass.
pub fn overflows_320(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    // <style> blocks: rules with a fixed width over 320 px, and the tags a rule reins in.
    let mut capped_tags: Vec<String> = Vec::new();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<style") {
        let after = &rest[start..];
        let Some(open) = after.find('>') else { break };
        let end = after.find("</style").unwrap_or(after.len());
        let css = &after[open + 1..end.max(open + 1)];
        for rule in css.split('}') {
            let Some((selectors, decls)) = rule.rsplit_once('{') else {
                continue;
            };
            let selectors = selectors.rsplit('{').next().unwrap_or(selectors).trim();
            if css_px(decls, "max-width").is_some()
                || decls.contains("max-width:100%")
                || decls.contains("max-width: 100%")
            {
                capped_tags.extend(
                    selectors
                        .split(',')
                        .map(str::trim)
                        .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric()))
                        .map(str::to_owned),
                );
                continue;
            }
            for prop in ["width", "min-width"] {
                if let Some(px) = css_px(decls, prop).filter(|px| *px > NARROW_PX)
                    && !selectors.starts_with('@')
                    && !out.iter().any(|o| o.starts_with(selectors))
                {
                    out.push(format!("{selectors} {{{prop}:{px}px}}"));
                }
            }
        }
        rest = &after[end.min(after.len())..];
        if end == after.len() {
            break;
        }
    }
    // Elements.
    let mut at = 0;
    while let Some(i) = lower[at..].find('<') {
        let start = at + i + 1;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let tag = &lower[start..start + len];
        at = start + len;
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if name.is_empty() || name == "style" {
            continue;
        }
        let style = attr(tag, "style").unwrap_or_default();
        let capped = css_px(&style, "max-width").is_some()
            || style.contains("max-width:100%")
            || style.contains("max-width: 100%")
            || capped_tags.contains(&name);
        let min = css_px(&style, "min-width").filter(|px| *px > NARROW_PX);
        let fixed = css_px(&style, "width").or_else(|| {
            attr(tag, "width").and_then(|w| w.trim_end_matches("px").trim().parse::<u32>().ok())
        });
        let what = match (min, fixed) {
            (Some(px), _) => Some(format!("<{name}> min-width {px}px")),
            (None, Some(px)) if px > NARROW_PX && !capped => Some(format!("<{name}> {px}px wide")),
            _ => None,
        };
        if let Some(w) = what
            && !out.contains(&w)
        {
            out.push(w);
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// The pixel value of `prop` in CSS declarations (`width: 496px`), not of a longer property
/// that ends with it (`max-width`).
fn css_px(decls: &str, prop: &str) -> Option<u32> {
    let mut from = 0;
    while let Some(i) = decls[from..].find(prop) {
        let at = from + i;
        from = at + prop.len();
        let before = decls[..at].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        let rest = decls[from..].trim_start();
        let Some(value) = rest.strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && value[digits.len()..].trim_start().starts_with("px") {
            return digits.parse().ok();
        }
    }
    None
}

/// An attribute's value in a lowercased tag (`width="496"`, `width=496`).
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before = tag[..at].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(value) = rest.strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        return Some(match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default().to_owned(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or_default()
                .to_owned(),
        });
    }
    None
}

#[derive(Template)]
#[template(path = "kernel/mailbox/index.html")]
struct IndexPage {
    chrome: Chrome,
    /// "inbox" or "previews".
    pane: &'static str,
    rows: Vec<Row>,
    previews: Vec<PreviewRow>,
    preview_count: usize,
    message: Option<MessageView>,
    q: String,
    total: u64,
    unread: u64,
    since: i64,
    selected: i64,
    /// The list fragment's URL, for live refreshes (keeps the search and the selection).
    list_src: String,
}

#[derive(Template)]
#[template(path = "kernel/mailbox/_list.html")]
struct ListFragment {
    rows: Vec<Row>,
    q: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ViewQuery {
    #[serde(default)]
    q: String,
    tab: Option<String>,
    view: Option<String>,
    selected: Option<i64>,
}

fn tab_of(q: &ViewQuery) -> &'static str {
    match q.tab.as_deref() {
        Some("text") => "text",
        Some("raw") => "raw",
        Some("headers") => "headers",
        _ => "html",
    }
}

fn view_of(q: &ViewQuery) -> &'static str {
    match q.view.as_deref() {
        Some("600") => "600",
        Some("320") => "320",
        Some("dark") => "dark",
        _ => "desktop",
    }
}

/// `path?q=…&tab=…&view=…`, leaving out defaults and empty values.
fn href(path: &str, q: &str, tab: &str, view: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if !q.is_empty() {
        pairs.push(("q", q));
    }
    if tab != "html" {
        pairs.push(("tab", tab));
    }
    if view != "desktop" {
        pairs.push(("view", view));
    }
    if pairs.is_empty() {
        path.to_owned()
    } else {
        format!(
            "{path}?{}",
            serde_html_form::to_string(&pairs).unwrap_or_default()
        )
    }
}

fn tabs_and_views(
    path: &str,
    q: &str,
    tab: &'static str,
    view: &'static str,
) -> (Vec<Link>, Vec<Link>) {
    let tabs = [
        ("HTML", "html"),
        ("Text", "text"),
        ("Raw", "raw"),
        ("Headers", "headers"),
    ]
    .into_iter()
    .map(|(label, t)| Link {
        label,
        href: href(path, q, t, view),
        current: t == tab,
    })
    .collect();
    let views = [
        ("Desktop", "desktop"),
        ("600 px", "600"),
        ("320 px", "320"),
        ("Dark", "dark"),
    ]
    .into_iter()
    .map(|(label, v)| Link {
        label,
        href: href(path, q, tab, v),
        current: v == view,
    })
    .collect();
    (tabs, views)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn stored_attachments(m: &Model) -> Vec<Attachment> {
    serde_json::from_value(m.attachments.clone()).unwrap_or_default()
}

fn rows(messages: &[Model], selected: i64, q: &str) -> Vec<Row> {
    let now = Utc::now();
    messages
        .iter()
        .map(|m| Row {
            id: m.id,
            href: href(&format!("/dev/mailbox/{}", m.id), q, "html", "desktop"),
            to: m.to_addr.clone(),
            subject: if m.subject.trim().is_empty() {
                "(no subject)".to_owned()
            } else {
                m.subject.clone()
            },
            snippet: mailbox::snippet(&m.text, 90),
            when: ui::ago(m.created_at, now),
            at: m.created_at.to_rfc3339(),
            unread: m.read_at.is_none(),
            attachments: m.attachments.as_array().map_or(0, Vec::len),
            selected: m.id == selected,
        })
        .collect()
}

fn message_view(m: &Model, q: &ViewQuery) -> MessageView {
    let (tab, view) = (tab_of(q), view_of(q));
    let path = format!("/dev/mailbox/{}", m.id);
    let (tabs, views) = tabs_and_views(&path, &q.q, tab, view);
    let headers: Vec<(String, String)> =
        serde_json::from_value(m.headers.clone()).unwrap_or_default();
    let date = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("date"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| ui::utc(m.created_at));
    MessageView {
        subject: m.subject.clone(),
        from: m.from_addr.clone(),
        to: m.to_addr.clone(),
        cc: strings(&m.cc),
        bcc: strings(&m.bcc),
        reply_to: m.reply_to.clone(),
        date,
        headers,
        html_src: format!("{path}/html?scheme={}", scheme_of(view)),
        text: m.text.clone(),
        raw: m.eml.clone(),
        links: mailbox::links_in(&m.text, &m.html),
        codes: mailbox::codes_in(&m.text),
        attachments: stored_attachments(m)
            .iter()
            .enumerate()
            .map(|(n, a)| AttachmentView {
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                size: ui::bytes(a.bytes().len()),
                href: Some(format!("{path}/attachments/{n}")),
                image: is_previewable_image(&a.content_type),
            })
            .collect(),
        tab,
        tabs,
        views,
        view,
        eml_href: Some(format!("{path}/message.eml")),
        id: Some(m.id),
        delete_action: Some(format!("{path}/delete")),
        preview: false,
        problems: 0,
        checks: checks(&m.text, &m.html, &headers_for_checks(m), &m.subject),
    }
    .counted()
}

fn headers_for_checks(m: &Model) -> Vec<(String, String)> {
    serde_json::from_value(m.headers.clone()).unwrap_or_default()
}

fn preview_view(name: &str, email: &Email, from: &str, q: &ViewQuery) -> MessageView {
    let (tab, view) = (tab_of(q), view_of(q));
    let path = format!("/dev/mailbox/previews/{name}");
    let (tabs, views) = tabs_and_views(&path, "", tab, view);
    // The message SMTP would send, built exactly as delivery builds it (never sent).
    let raw = from
        .parse()
        .ok()
        .and_then(|from| crate::kernel::mailer::build(&from, email).ok())
        .map(|m| String::from_utf8_lossy(&m.formatted()).into_owned())
        .unwrap_or_default();
    let headers = mailbox::parse_headers(&raw);
    let preview_checks = checks(&email.text, &email.html, &headers, &email.subject);
    MessageView {
        subject: email.subject.clone(),
        from: from.to_owned(),
        to: email.to.clone(),
        cc: email.cc.clone(),
        bcc: email.bcc.clone(),
        reply_to: email.reply_to.clone(),
        date: "rendered now, not sent".to_owned(),
        headers,
        html_src: format!("{path}/html?scheme={}", scheme_of(view)),
        text: email.text.clone(),
        raw,
        links: mailbox::links_in(&email.text, &email.html),
        codes: mailbox::codes_in(&email.text),
        attachments: email
            .attachments
            .iter()
            .map(|a| AttachmentView {
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                size: ui::bytes(a.bytes().len()),
                href: None,
                image: false,
            })
            .collect(),
        tab,
        tabs,
        views,
        view,
        problems: 0,
        checks: preview_checks,
        eml_href: None,
        id: None,
        delete_action: None,
        preview: true,
    }
    .counted()
}

fn chrome(state: &AppState, flash: Option<FlashMessage>, theme: ThemeSwitch) -> Chrome {
    Chrome::new(
        "Mailbox",
        "mailbox",
        state.assets.assets(),
        true,
        flash,
        theme,
    )
    .stylesheet(state.assets.path("kernel/mailbox.css"))
    .script(state.assets.path("kernel/mailbox.js"))
}

fn list_src(q: &str, selected: i64) -> String {
    let mut pairs = vec![("selected", selected.to_string())];
    if !q.is_empty() {
        pairs.insert(0, ("q", q.to_owned()));
    }
    format!(
        "/dev/mailbox/list?{}",
        serde_html_form::to_string(&pairs).unwrap_or_default()
    )
}

// ---------------------------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------------------------

async fn inbox_page(
    state: &AppState,
    previews: &'static [Preview],
    q: &ViewQuery,
    selected: Option<Model>,
    flash: Option<FlashMessage>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    let messages = mailbox::search(&state.db, &q.q, LIST_LIMIT).await?;
    let (total, unread) = mailbox::counts(&state.db).await?;
    let selected_id = selected.as_ref().map_or(0, |m| m.id);
    Ok(ui::page(IndexPage {
        chrome: chrome(state, flash, theme),
        pane: "inbox",
        rows: rows(&messages, selected_id, &q.q),
        previews: Vec::new(),
        preview_count: previews.len(),
        message: selected.as_ref().map(|m| message_view(m, q)),
        q: q.q.clone(),
        total,
        unread,
        since: mailbox::newest_id(&state.db).await?,
        selected: selected_id,
        list_src: list_src(&q.q, selected_id),
    }))
}

/// `GET /dev/mailbox`: the inbox, with the newest matching message open.
async fn index(
    State(state): State<AppState>,
    Extension(Previews(previews)): Extension<Previews>,
    Query(q): Query<ViewQuery>,
    flash: Option<Extension<RequestFlash>>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    let newest = mailbox::search(&state.db, &q.q, 1)
        .await?
        .into_iter()
        .next();
    if let Some(m) = &newest {
        mailbox::mark_read(&state.db, m.id).await?;
    }
    inbox_page(
        &state,
        previews,
        &q,
        newest,
        flash.and_then(|f| f.0.0),
        theme,
    )
    .await
}

/// `GET /dev/mailbox/{id}`: the inbox with message `id` open; opening marks it read. A message
/// that's gone redirects to the inbox with a flash, never a bare 404.
async fn show(
    State(state): State<AppState>,
    Extension(Previews(previews)): Extension<Previews>,
    Path(id): Path<i64>,
    Query(q): Query<ViewQuery>,
    flash: Option<Extension<RequestFlash>>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    let Some(mut m) = mailbox::find(&state.db, id).await? else {
        return Ok(gone(&q.q));
    };
    if m.read_at.is_none() {
        mailbox::mark_read(&state.db, id).await?;
        m.read_at = Some(Utc::now());
    }
    inbox_page(
        &state,
        previews,
        &q,
        Some(m),
        flash.and_then(|f| f.0.0),
        theme,
    )
    .await
}

fn gone(q: &str) -> Response {
    (
        Flash::alert("That message was deleted."),
        Redirect::to(&href("/dev/mailbox", q, "html", "desktop")),
    )
        .into_response()
}

/// `GET /dev/mailbox/list?q=&selected=`: the list alone, for the live inbox to swap in.
async fn list_fragment(
    State(state): State<AppState>,
    Query(q): Query<ViewQuery>,
) -> Result<Response, AppError> {
    let messages = mailbox::search(&state.db, &q.q, LIST_LIMIT).await?;
    let html = ListFragment {
        rows: rows(&messages, q.selected.unwrap_or(0), &q.q),
        q: q.q.clone(),
    }
    .render()?;
    let (total, unread) = mailbox::counts(&state.db).await?;
    let mut res = Html(html).into_response();
    let h = res.headers_mut();
    h.insert("x-mailbox-total", HeaderValue::from(total));
    h.insert("x-mailbox-unread", HeaderValue::from(unread));
    Ok(res)
}

#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    since: Option<i64>,
}

/// The mailbox's state, as the live inbox sees it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    newest: i64,
    total: u64,
    unread: u64,
}

async fn snapshot(state: &AppState) -> Option<Snapshot> {
    let newest = mailbox::newest_id(&state.db).await.ok()?;
    let (total, unread) = mailbox::counts(&state.db).await.ok()?;
    Some(Snapshot {
        newest,
        total,
        unread,
    })
}

/// `GET /dev/mailbox/events?since=<id>`: Server-Sent Events. A `mailbox` event whenever the newest
/// id, the total or the unread count changes (mail from any process: the table is the source), and
/// once at connect if anything arrived after `since`. The data is never empty (browsers drop
/// empty events); `id` is the newest message id, so a reconnect resumes from it.
async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let since = resume.or(q.since).unwrap_or(0);
    let start = Snapshot {
        newest: since,
        total: u64::MAX,
        unread: u64::MAX,
    };
    let stream = futures::stream::unfold((state, start, true), |(state, last, first)| async move {
        loop {
            if !first {
                tokio::time::sleep(POLL).await;
            }
            let Some(now) = snapshot(&state).await else {
                tokio::time::sleep(POLL).await;
                continue;
            };
            let changed = if first {
                now.newest != last.newest
            } else {
                now != last
            };
            if changed {
                let event = Event::default()
                    .event("mailbox")
                    .id(now.newest.to_string())
                    .data(
                        json!({
                            "newest": now.newest,
                            "since": last.newest,
                            "total": now.total,
                            "unread": now.unread,
                        })
                        .to_string(),
                    );
                return Some((Ok(event), (state, now, false)));
            }
            if first {
                return Some((Ok(Event::default().comment("live")), (state, now, false)));
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `GET /dev/mailbox/{id}/html`: the HTML body alone, for the sandboxed iframe.
#[derive(Debug, Default, Deserialize)]
struct SchemeQuery {
    scheme: Option<String>,
}

fn scheme_of(view: &str) -> &'static str {
    if view == "dark" { "dark" } else { "light" }
}

async fn html_body(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<SchemeQuery>,
) -> Result<Response, AppError> {
    let m = mailbox::find(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(sandboxed_html(&in_scheme(&m.html, q.scheme.as_deref())))
}

/// The body as a mail client in that color scheme shows it.
///
/// WHY: the frame is cross-origin (sandboxed), so the browser gives it this machine's theme, not
/// the frame element's `color-scheme`. Rewriting the media queries makes the preview independent
/// of the developer's OS: `light` drops the mail's dark-mode rules; `dark` applies them, and mail
/// without any gets the full-color inversion many mail apps apply.
pub fn in_scheme(html: &str, scheme: Option<&str>) -> String {
    let dark_rules = ["prefers-color-scheme: dark", "prefers-color-scheme:dark"];
    let light_rules = ["prefers-color-scheme: light", "prefers-color-scheme:light"];
    let mut out = html.to_owned();
    match scheme {
        Some("dark") => {
            let has_dark = dark_rules.iter().any(|r| out.contains(r));
            for r in dark_rules {
                out = out.replace(r, "min-width: 0");
            }
            for r in light_rules {
                out = out.replace(r, "max-width: 0");
            }
            if !has_dark {
                out = inject_head(
                    &out,
                    "<style>html{filter:invert(1) hue-rotate(180deg);background:#fff}img,picture,video{filter:invert(1) hue-rotate(180deg)}</style>",
                );
            }
            out
        }
        _ => {
            for r in dark_rules {
                out = out.replace(r, "max-width: 0");
            }
            out
        }
    }
}

/// The iframe body with its own headers. The global header matrix overwrites handler headers,
/// except on a sandboxed document like this one (`sandbox` plus `default-src 'none'`), whose CSP
/// and frame options it keeps.
pub fn sandboxed_html(html: &str) -> Response {
    let mut res = Html(with_blank_base(html)).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(BODY_CSP),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

/// Links in an HTML body open in a new tab: `<base target="_blank">` right after `<head …>`, or
/// after `<html …>`/the doctype when there is no head, never before the doctype (quirks mode).
pub fn with_blank_base(html: &str) -> String {
    inject_head(html, r#"<base target="_blank">"#)
}

/// `snippet` right after `<head …>`, or after `<html …>`/the doctype when there is no head, never
/// before the doctype (that would switch the document to quirks mode).
fn inject_head(html: &str, snippet: &str) -> String {
    let base = snippet;
    let lower = html.to_ascii_lowercase();
    let after_tag = |tag: &str| {
        let mut from = 0;
        while let Some(i) = lower[from..].find(tag) {
            let start = from + i;
            let next = lower.as_bytes().get(start + tag.len()).copied();
            if matches!(next, Some(b'>' | b' ' | b'\t' | b'\n' | b'\r')) {
                return lower[start..].find('>').map(|j| start + j + 1);
            }
            from = start + tag.len();
        }
        None
    };
    match after_tag("<head")
        .or_else(|| after_tag("<html"))
        .or_else(|| after_tag("<!doctype"))
    {
        Some(at) => format!("{}{base}{}", &html[..at], &html[at..]),
        None => format!("{base}{html}"),
    }
}

/// `GET /dev/mailbox/{id}/message.eml`: the exact RFC 822 message, as a download.
async fn download(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let m = mailbox::find(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, "message/rfc822".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"message-{id}.eml\""),
            ),
            (header::CONTENT_SECURITY_POLICY, ATTACHMENT_CSP.to_owned()),
        ],
        m.eml,
    )
        .into_response())
}

/// `GET /dev/mailbox/{id}/attachments/{n}`: always a download, never rendered on this origin.
async fn attachment(
    State(state): State<AppState>,
    Path((id, n)): Path<(i64, usize)>,
) -> Result<Response, AppError> {
    let m = mailbox::find(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let a = stored_attachments(&m)
        .into_iter()
        .nth(n)
        .ok_or(AppError::NotFound)?;
    let content_type = HeaderValue::from_str(&a.content_type)
        .ok()
        .filter(|v| {
            v.to_str()
                .is_ok_and(|s| s.contains('/') && !s.contains(['\r', '\n']))
        })
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let mut res = a.bytes().into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, content_type);
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            safe_filename(&a.filename)
        ))
        .unwrap_or(HeaderValue::from_static("attachment")),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(ATTACHMENT_CSP),
    );
    Ok(res)
}

// SECURITY: a quote or line break in a filename would break out of the header (test: filenames_are_safe_in_a_quoted_header).
/// A filename that is safe inside a quoted `Content-Disposition` value.
pub fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else {
        cleaned
    }
}

#[derive(Debug, Default, Deserialize)]
struct ReturnQuery {
    #[serde(default)]
    q: String,
}

async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ReturnQuery>,
) -> Result<Response, AppError> {
    let flash = if mailbox::delete(&state.db, id).await? {
        Flash::notice("Message deleted.")
    } else {
        Flash::alert("That message was already deleted.")
    };
    Ok((
        flash,
        Redirect::to(&href("/dev/mailbox", &q.q, "html", "desktop")),
    )
        .into_response())
}

async fn clear(State(state): State<AppState>) -> Result<Response, AppError> {
    let n = mailbox::clear(&state.db).await?;
    let flash = Flash::notice(format!(
        "Cleared {n} message{}.",
        if n == 1 { "" } else { "s" }
    ));
    Ok((flash, Redirect::to("/dev/mailbox")).into_response())
}

/// `POST /dev/mailbox/test`: a realistic sample (HTML + text, a sign-in link, a one-time code, an
/// attachment) stored straight into the mailbox through `Mailer::local`, so it is never sent over
/// SMTP whatever `MAILER` says. How the browser smoke and a curious human seed mail.
async fn send_test(State(state): State<AppState>) -> Result<Response, AppError> {
    let from = format!("{} <no-reply@example.com>", crate::config::APP_NAME);
    let from = from
        .parse()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("bad sample sender")))?;
    let email = sample_email(
        &state.config.app_url,
        &format!("{:06}", rand::random_range(0..1_000_000)),
    );
    // INVARIANT: the test button stores locally, never over SMTP, whatever MAILER says (test: the_test_email_button_never_sends_over_smtp).
    Mailer::local(state.db.clone(), from).deliver(email).await?;
    let id = mailbox::newest_id(&state.db).await?;
    Ok((
        Flash::notice("Sent a test email to your mailbox."),
        Redirect::to(&format!("/dev/mailbox/{id}")),
    )
        .into_response())
}

/// The test email: everything the viewer can show, in one message.
pub fn sample_email(app_url: &str, code: &str) -> Email {
    let link = format!("{app_url}/dev/mailbox");
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="color-scheme" content="light dark"><title>Test email</title>
<style>body{{margin:0;padding:24px;background:#f3f4f6;font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;color:#111827}}.card{{max-width:520px;margin:0 auto;background:#fff;border:1px solid #e5e7eb;border-radius:8px;padding:28px}}.code{{font:600 28px/1.2 ui-monospace,Menlo,monospace;letter-spacing:.2em}}a.btn{{display:inline-block;background:#4f46e5;color:#fff;text-decoration:none;font-weight:600;padding:10px 18px;border-radius:6px}}@media (prefers-color-scheme: dark){{body{{background:#0b0f19;color:#e5e7eb}}.card{{background:#111827;border-color:#374151}}}}</style></head>
<body><div class="card"><h1 style="margin:0 0 12px;font-size:20px">The dev mailbox works</h1>
<p>Every email the app sends in development lands here instead of an inbox. This one shows what the viewer can do.</p>
<p>Your one-time code is</p><p class="code">{code}</p>
<p><a class="btn" href="{link}">Open the mailbox</a></p>
<p style="font-size:13px;color:#6b7280">Links open in a new tab. Scripts in mail never run here.</p></div></body></html>"#
    );
    let text = format!(
        "The dev mailbox works\n\nEvery email the app sends in development lands here instead of an inbox.\n\nYour one-time code is {code}\n\nOpen the mailbox: {link}\n"
    );
    Email::new(
        "ada@example.com",
        "Test email: the dev mailbox works",
        html,
        text,
    )
    .attach(Attachment::new(
        "hello.txt",
        "text/plain",
        b"An attachment, as the recipient would download it.\n",
    ))
}

/// `GET /dev/mailbox.json`: the newest 50, for agents.
#[derive(Debug, Default, Deserialize)]
struct ListJsonQuery {
    /// Exact recipient (case-insensitive).
    to: Option<String>,
    /// Subject contains (case-insensitive).
    subject: Option<String>,
    /// Only messages with an id above this (what arrived since a cursor).
    since: Option<i64>,
}

/// `GET /dev/mailbox.json?to=&subject=&since=`: the newest 50 that match, for agents and
/// end-to-end tests.
async fn list_json(
    State(state): State<AppState>,
    Query(q): Query<ListJsonQuery>,
) -> Result<Response, AppError> {
    let messages = mailbox::filter(
        &state.db,
        &mailbox::Filter {
            to: q.to,
            subject: q.subject,
            since: q.since,
        },
        JSON_LIMIT,
    )
    .await?;
    Ok(Json(
        messages
            .iter()
            .map(mailbox::summary_json)
            .collect::<Vec<_>>(),
    )
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
struct LatestQuery {
    to: Option<String>,
}

/// `GET /dev/mailbox/latest.json?to=`: the newest message (for one recipient); 404 when none, so
/// `curl -f` fails.
async fn latest_json(
    State(state): State<AppState>,
    Query(q): Query<LatestQuery>,
) -> Result<Response, AppError> {
    Ok(match mailbox::latest(&state.db, q.to.as_deref()).await? {
        Some(m) => Json(mailbox::summary_json(&m)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no messages" })),
        )
            .into_response(),
    })
}

// ---------------------------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------------------------

fn preview_rows(previews: &[Preview], selected: Option<&str>) -> Vec<PreviewRow> {
    previews
        .iter()
        .map(|p| PreviewRow {
            name: p.name,
            href: format!("/dev/mailbox/previews/{}", p.name),
            selected: selected == Some(p.name),
        })
        .collect()
}

async fn previews_page(
    state: &AppState,
    previews: &'static [Preview],
    selected: Option<&'static Preview>,
    q: &ViewQuery,
    flash: Option<FlashMessage>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    let message = match selected {
        Some(p) => {
            let email = (p.build)(state).await?;
            let from = format!("{} <no-reply@example.com>", crate::config::APP_NAME);
            Some(preview_view(p.name, &email, &from, q))
        }
        None => None,
    };
    let (total, unread) = mailbox::counts(&state.db).await?;
    Ok(ui::page(IndexPage {
        chrome: chrome(state, flash, theme),
        pane: "previews",
        rows: Vec::new(),
        previews: preview_rows(previews, selected.map(|p| p.name)),
        preview_count: previews.len(),
        message,
        q: String::new(),
        total,
        unread,
        since: mailbox::newest_id(&state.db).await?,
        selected: 0,
        list_src: String::new(),
    }))
}

async fn previews_index(
    State(state): State<AppState>,
    Extension(Previews(previews)): Extension<Previews>,
    Query(q): Query<ViewQuery>,
    flash: Option<Extension<RequestFlash>>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    previews_page(
        &state,
        previews,
        previews.first(),
        &q,
        flash.and_then(|f| f.0.0),
        theme,
    )
    .await
}

fn find_preview(
    previews: &'static [Preview],
    mailer: &str,
    action: &str,
) -> Option<&'static Preview> {
    let name = format!("{mailer}/{action}");
    previews.iter().find(|p| p.name == name)
}

async fn preview_show(
    State(state): State<AppState>,
    Extension(Previews(previews)): Extension<Previews>,
    Path((mailer, action)): Path<(String, String)>,
    Query(q): Query<ViewQuery>,
    flash: Option<Extension<RequestFlash>>,
    theme: ThemeSwitch,
) -> Result<Response, AppError> {
    let Some(p) = find_preview(previews, &mailer, &action) else {
        return Ok((
            Flash::alert(format!("No preview named {mailer}/{action}.")),
            Redirect::to("/dev/mailbox/previews"),
        )
            .into_response());
    };
    previews_page(
        &state,
        previews,
        Some(p),
        &q,
        flash.and_then(|f| f.0.0),
        theme,
    )
    .await
}

async fn preview_html(
    State(state): State<AppState>,
    Extension(Previews(previews)): Extension<Previews>,
    Path((mailer, action)): Path<(String, String)>,
    Query(q): Query<SchemeQuery>,
) -> Result<Response, AppError> {
    let p = find_preview(previews, &mailer, &action).ok_or(AppError::NotFound)?;
    let html = (p.build)(&state).await?.html;
    Ok(sandboxed_html(&in_scheme(&html, q.scheme.as_deref())))
}

/// Older apps linked previews at `/dev/mailers`. 301, not axum's 308 `Redirect::permanent`.
async fn mailers_redirect() -> Response {
    (
        StatusCode::MOVED_PERMANENTLY,
        [(header::LOCATION, "/dev/mailbox/previews")],
    )
        .into_response()
}

/// The `/dev` index tile's figure.
pub async fn tile_figure(state: &AppState) -> Result<String, AppError> {
    let (total, unread) = mailbox::counts(&state.db).await?;
    Ok(format!("{unread} unread of {total}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_goes_inside_head_and_never_before_the_doctype() {
        assert_eq!(
            with_blank_base("<!doctype html><html><head lang=\"en\"><title>x</title>"),
            "<!doctype html><html><head lang=\"en\"><base target=\"_blank\"><title>x</title>"
        );
        assert_eq!(
            with_blank_base("<!DOCTYPE html><p>hi</p>"),
            "<!DOCTYPE html><base target=\"_blank\"><p>hi</p>"
        );
        assert_eq!(
            with_blank_base("<p>hi</p>"),
            "<base target=\"_blank\"><p>hi</p>"
        );
        assert_eq!(
            with_blank_base("<html><header>x</header>"),
            "<html><base target=\"_blank\"><header>x</header>"
        );
    }

    #[test]
    fn previews_render_in_the_chosen_scheme_whatever_the_os_says() {
        let mail = "<html><head><style>@media (prefers-color-scheme: dark){body{background:#000}}</style></head></html>";
        assert!(in_scheme(mail, Some("light")).contains("@media (max-width: 0)"));
        assert!(in_scheme(mail, Some("dark")).contains("@media (min-width: 0)"));
        let plain = "<html><head></head><body>hi</body></html>";
        assert!(in_scheme(plain, Some("dark")).contains("invert(1)"));
        assert_eq!(in_scheme(plain, None), plain);
    }

    #[test]
    fn mail_that_overflows_a_320_px_screen_is_flagged() {
        // Flagged: fixed widths over 320 px that nothing reins in.
        assert_eq!(
            overflows_320(r#"<img src="a.png" width="496" height="120">"#),
            ["<img> 496px wide"]
        );
        assert_eq!(
            overflows_320(r#"<table width=600><tr><td>x</td></tr></table>"#),
            ["<table> 600px wide"]
        );
        assert_eq!(
            overflows_320(r#"<div style="width: 480px">x</div>"#),
            ["<div> 480px wide"]
        );
        assert_eq!(
            overflows_320(r#"<div style="min-width:400px;max-width:100%">x</div>"#),
            ["<div> min-width 400px"]
        );
        assert_eq!(
            overflows_320("<style>.hero{width:640px}</style><div class=hero></div>"),
            [".hero {width:640px}"]
        );
        // Fine: fluid, small, or reined in.
        assert!(
            overflows_320(r#"<img width="300"><td width="50%"><table width="100%">"#).is_empty()
        );
        assert!(
            overflows_320(r#"<img width="496" style="max-width:100%;height:auto">"#).is_empty()
        );
        assert!(overflows_320(r#"<style>img{max-width:100%}</style><img width="496">"#).is_empty());
        assert!(overflows_320("<style>.card{max-width:560px;margin:0 auto}</style>").is_empty());
        assert!(overflows_320(r#"<div data-width="900" title="width=900">x</div>"#).is_empty());
    }

    #[test]
    fn filenames_are_safe_in_a_quoted_header() {
        assert_eq!(safe_filename("report \"Q3\".pdf"), "report _Q3_.pdf");
        assert_eq!(safe_filename("a\r\nX-Evil: 1"), "a__X-Evil_ 1");
        assert_eq!(safe_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_filename("…"), "_");
        assert_eq!(safe_filename(""), "attachment");
    }

    #[test]
    fn links_keep_the_search_and_drop_defaults() {
        assert_eq!(
            href("/dev/mailbox/3", "", "html", "desktop"),
            "/dev/mailbox/3"
        );
        assert_eq!(
            href("/dev/mailbox/3", "a b&c", "raw", "dark"),
            "/dev/mailbox/3?q=a+b%26c&tab=raw&view=dark"
        );
    }

    #[test]
    fn the_sample_email_has_everything_the_viewer_shows() {
        let e = sample_email("http://localhost:3000", "482913");
        assert_eq!(mailbox::codes_in(&e.text), ["482913"]);
        assert_eq!(
            mailbox::links_in(&e.text, &e.html),
            ["http://localhost:3000/dev/mailbox"]
        );
        assert_eq!(e.attachments.len(), 1);
        crate::kernel::mailer::validate(&e).unwrap();
    }
}
