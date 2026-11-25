//! The inbound webhook log: one row per verified delivery.
//! GUIDE: .offrails/guides/security.md#csrf
//!
//! WHY: the id is `big_pk_auto` (64-bit): a 32-bit id passes on SQLite and is a 500 on every
//! Postgres write.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

const STATUSES: [&str; 4] = ["received", "processed", "ignored", "failed"];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.create_table(
            Table::create()
                .table(WebhookEvents::Table)
                .if_not_exists()
                .col(big_pk_auto(WebhookEvents::Id))
                .col(string(WebhookEvents::Provider))
                .col(string(WebhookEvents::EventId))
                .col(string_null(WebhookEvents::DeliveryId))
                .col(string(WebhookEvents::EventType))
                .col(text(WebhookEvents::Payload))
                .col(timestamp_with_time_zone(WebhookEvents::ReceivedAt))
                .col(timestamp_with_time_zone_null(WebhookEvents::ProcessedAt))
                .col(string(WebhookEvents::Status).default("received"))
                .col(text_null(WebhookEvents::Error))
                // INVARIANT: an event is always in one of four statuses (test: the_status_column_rejects_unknown_statuses)
                .check(Expr::col(WebhookEvents::Status).is_in(STATUSES))
                .to_owned(),
        )
        .await?;
        // INVARIANT: a delivery is stored once per provider, whatever the number of replays or
        // racing requests (test: replays_store_one_event_and_enqueue_one_job)
        m.create_index(
            Index::create()
                .name("webhook_events_provider_event_id_key")
                .table(WebhookEvents::Table)
                .col(WebhookEvents::Provider)
                .col(WebhookEvents::EventId)
                .unique()
                .to_owned(),
        )
        .await?;
        // The pruner and the operator's "what came in lately".
        m.create_index(
            Index::create()
                .name("webhook_events_received_at_idx")
                .table(WebhookEvents::Table)
                .col(WebhookEvents::ReceivedAt)
                .to_owned(),
        )
        .await
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.drop_table(Table::drop().table(WebhookEvents::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum WebhookEvents {
    Table,
    Id,
    Provider,
    EventId,
    DeliveryId,
    EventType,
    Payload,
    ReceivedAt,
    ProcessedAt,
    Status,
    Error,
}
