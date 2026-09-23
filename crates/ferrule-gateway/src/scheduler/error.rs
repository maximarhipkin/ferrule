use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid cron expression `{0}`: {1}")]
    InvalidCron(String, String),
    #[error("invalid timezone `{0}`")]
    InvalidTimezone(String),
    #[error("cron search failed: {0}")]
    CronSearch(String),
    #[error("invalid datetime `{0}`: expected RFC 3339, e.g. 2026-10-01T09:00:00+03:00")]
    InvalidDatetime(String),
    #[error("task `{0}` not found")]
    NotFound(String),
    #[error("task kind must be `cron` or `once`, got `{0}`")]
    InvalidKind(String),
    #[error("gate script error: {0}")]
    Gate(String),
    #[error("gateway error: {0}")]
    Gateway(#[from] crate::error::GatewayError),
}
