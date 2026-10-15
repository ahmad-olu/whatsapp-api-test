//! Background loops: raw-event ingestion and event delivery.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::db::Db;
use crate::sink::Deliverer;

const POLL: Duration = Duration::from_secs(2);
const ERROR_PAUSE: Duration = Duration::from_secs(3);

/// Turn verified raw webhook bodies into normalized, de-duplicated events.
pub async fn run_ingest(
    db: Db,
    wake: Arc<Notify>,
    delivery_wake: Arc<Notify>,
    cancel: CancellationToken,
) {
    loop {
        loop {
            match db.process_next_raw().await {
                Ok(true) => delivery_wake.notify_one(),
                Ok(false) => break,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "ingest worker error");
                    tokio::time::sleep(ERROR_PAUSE).await;
                    break;
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(POLL) => {}
        }
    }
}

/// Hand stored events to the sink with retry/backoff.
pub async fn run_delivery(
    db: Db,
    deliverer: Arc<Deliverer>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
) {
    loop {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            match db.claim_next_delivery().await {
                Ok(Some(claim)) => {
                    let ev = claim.event.clone();
                    let result = deliverer.deliver(&ev).await;
                    let finished = match result {
                        Ok(()) => {
                            tracing::info!(event_id = ev.id, key = %ev.dedupe_key, "event delivered");
                            claim.mark_delivered().await
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            tracing::warn!(event_id = ev.id, error = %msg, "delivery failed; will retry with backoff");
                            claim.mark_failed(&msg).await
                        }
                    };
                    if let Err(e) = finished {
                        tracing::error!(error = %format!("{e:#}"), "could not record delivery outcome");
                        tokio::time::sleep(ERROR_PAUSE).await;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "delivery worker error");
                    tokio::time::sleep(ERROR_PAUSE).await;
                    break;
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(POLL) => {}
        }
    }
}
