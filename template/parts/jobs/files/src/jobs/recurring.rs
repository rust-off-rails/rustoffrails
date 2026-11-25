//! The recurring schedule (`config/recurring.yml`, Oban's Cron plugin).
//! GUIDE: .offrails/guides/jobs.md#recurring-jobs-srcjobsrecurringrs-the-oban-cron-plugin
//!
//! Syntax: five fields, `minute hour day-of-month month day-of-week`, always **UTC**, or a
//! nickname: `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. Seconds are not accepted.
//!
//! | Expression     | Runs                         |
//! |----------------|------------------------------|
//! | `0 * * * *`    | every hour, on the hour      |
//! | `*/15 * * * *` | every 15 minutes             |
//! | `30 2 * * *`   | 02:30 UTC every day          |
//! | `0 8 * * MON`  | 08:00 UTC every Monday       |
//! | `0 0 1 * *`    | midnight UTC on the 1st      |
//!
//! Each run happens once across all nodes; a node that was down at the due time doesn't replay
//! it later (more than 60s late is skipped).

use crate::{
    jobs::{
        Worker,
        cleanup::CleanupJob,
        examples::{DeliverWebhookJob, ExportReportJob},
    },
    kernel::jobs::Recurring,
};

/// APP: one entry per recurring job. `key` is permanent (run history references it).
/// WHY: `job` is a non-capturing closure, which coerces to `fn() -> JobBuilder`, so this stays
/// a `const`.
pub const SCHEDULE: &[Recurring] = &[
    Recurring {
        key: "cleanup",
        cron: "0 * * * *",
        job: || CleanupJob.into_job(),
    },
    // APP: the two entries below drive the examples (src/jobs/examples.rs); delete them with it.
    Recurring {
        key: "nightly_report",
        cron: "30 2 * * *",
        job: || {
            ExportReportJob {
                report_id: 1,
                rows: 5_000,
            }
            .into_job()
            .priority(5)
        },
    },
    Recurring {
        key: "webhook_ping",
        cron: "*/15 * * * *",
        job: || {
            DeliverWebhookJob {
                endpoint: "https://hooks.example.com/ping".into(),
                event: "ping".into(),
            }
            .into_job()
        },
    },
    // samples: recurring
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn every_schedule_entry_parses_and_has_a_next_run() {
        for entry in SCHEDULE {
            assert!(
                crate::kernel::jobs::next_run(entry.cron, Utc::now()).is_some(),
                "{} has an invalid cron expression {:?}",
                entry.key,
                entry.cron
            );
        }
    }

    #[test]
    fn keys_are_unique_and_jobs_are_registered() {
        let mut keys: Vec<_> = SCHEDULE.iter().map(|e| e.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), SCHEDULE.len());
        for entry in SCHEDULE {
            let worker = (entry.job)().worker_name().to_owned();
            assert!(
                crate::jobs::WORKERS.contains(&worker.as_str()),
                "{} enqueues {worker}, which isn't in jobs::dispatch",
                entry.key
            );
        }
    }
}
