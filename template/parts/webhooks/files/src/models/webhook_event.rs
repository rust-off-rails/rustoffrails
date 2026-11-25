//! One row per verified inbound webhook delivery. The route stores it and returns; the
//! `process_webhook` job reads it back and calls the app's handler.
//! GUIDE: .offrails/guides/security.md#csrf

use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveValue::{NotSet, Set},
    TryInsertResult,
    entity::prelude::*,
    sea_query::Expr,
};

use crate::{error::AppError, kernel::redact};

/// `received` until the job finishes with it. `ignored` is an event type the app has no handler
/// for (acknowledged, kept); `failed` is final: the handler refused it, or retries ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Text", rename_all = "snake_case")]
pub enum Status {
    Received,
    Processed,
    Ignored,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "webhook_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The URL slug (`github`), see `kernel::webhooks::Provider`.
    pub provider: String,
    /// What a replay repeats, so the dedupe key: Stripe's event id, or `sha256-<hex>` of the body
    /// for a scheme whose id travels outside the signature (GitHub).
    pub event_id: String,
    /// The provider's unsigned delivery id (GitHub's `X-GitHub-Delivery`), for display only.
    pub delivery_id: Option<String>,
    pub event_type: String,
    /// The raw body, as received (text). Parsed by the job, never by the route.
    pub payload: String,
    pub received_at: DateTimeUtc,
    pub processed_at: Option<DateTimeUtc>,
    pub status: Status,
    /// Why it failed (redacted, capped); the last transient error while it is still retrying.
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// APP: how much of an error message is kept.
const ERROR_CAP: usize = 1000;

/// Stores a delivery and returns its id, or `None` when this (provider, event id) is already
/// stored: a replay is a no-op, and the caller still answers 2xx.
// INVARIANT: the same delivery twice leaves one row, on both backends, even concurrently (test: replays_store_one_event_and_enqueue_one_job)
// WHY: `on_conflict_do_nothing_on`, not a caught unique violation: on Postgres a failed statement
// aborts the caller's transaction, and the job insert that follows runs in it.
pub async fn record(
    db: &impl ConnectionTrait,
    provider: &str,
    event_id: &str,
    delivery_id: Option<&str>,
    event_type: &str,
    payload: &str,
    now: DateTime<Utc>,
) -> Result<Option<i64>, AppError> {
    let row = ActiveModel {
        id: NotSet,
        provider: Set(provider.to_owned()),
        event_id: Set(event_id.to_owned()),
        delivery_id: Set(delivery_id.map(str::to_owned)),
        event_type: Set(event_type.to_owned()),
        payload: Set(payload.to_owned()),
        received_at: Set(now),
        processed_at: Set(None),
        status: Set(Status::Received),
        error: Set(None),
    };
    let attempt = Entity::insert(row)
        .on_conflict_do_nothing_on([Column::Provider, Column::EventId])
        .exec(db)
        .await?;
    Ok(match attempt {
        TryInsertResult::Inserted(done) => Some(done.last_insert_id),
        _ => None,
    })
}

pub async fn find(db: &impl ConnectionTrait, id: i64) -> Result<Option<Model>, AppError> {
    Ok(Entity::find_by_id(id).one(db).await?)
}

/// Moves a `received` event to its final status. `false` when it was no longer `received` (a
/// retry of a job that already finished it): the first outcome stands.
pub async fn finish(
    db: &impl ConnectionTrait,
    id: i64,
    status: Status,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<bool, AppError> {
    let res = Entity::update_many()
        .col_expr(Column::Status, Expr::value(status))
        .col_expr(Column::ProcessedAt, Expr::value(now))
        .col_expr(Column::Error, Expr::value(error.map(clean_error)))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.eq(Status::Received))
        .exec(db)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Remembers the last transient error of an event that is still being retried.
pub async fn note_error(db: &impl ConnectionTrait, id: i64, error: &str) -> Result<(), AppError> {
    Entity::update_many()
        .col_expr(Column::Error, Expr::value(clean_error(error)))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.eq(Status::Received))
        .exec(db)
        .await?;
    Ok(())
}

/// Deletes finished events received before `before` (never one still `received`). Returns how
/// many. Call it from a task or a recurring job; nothing calls it for you.
pub async fn prune(db: &impl ConnectionTrait, before: DateTime<Utc>) -> Result<u64, AppError> {
    let res = Entity::delete_many()
        .filter(Column::ReceivedAt.lt(before))
        .filter(Column::Status.ne(Status::Received))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

// SECURITY: an error message can quote what failed (a URL with a token); it is redacted and cut
// before it is stored (test: stored_errors_are_redacted_and_capped).
fn clean_error(raw: &str) -> String {
    let mut text = redact::text(raw);
    if text.len() > ERROR_CAP {
        let mut end = ERROR_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
