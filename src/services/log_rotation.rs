use sqlx::SqlitePool;
use tokio::time::{Duration, interval};
use tracing::{info, warn};

use crate::db::billing::{delete_old_request_logs, get_setting};

const DEFAULT_RETENTION_DAYS: i64 = 7;

pub async fn start_log_rotation(db: SqlitePool) {
    // Run once on startup
    run_rotation(&db).await;

    // Then daily
    let mut ticker = interval(Duration::from_secs(24 * 60 * 60));
    ticker.tick().await; // skip immediate tick (already ran above)
    loop {
        ticker.tick().await;
        run_rotation(&db).await;
    }
}

async fn run_rotation(db: &SqlitePool) {
    let retention_days = get_setting(db, "log_retention_days")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);

    match delete_old_request_logs(db, retention_days).await {
        Ok(count) => {
            if count > 0 {
                info!("Log rotation: deleted {count} request logs older than {retention_days} days");
            }
        }
        Err(e) => warn!("Log rotation failed: {e}"),
    }
}
