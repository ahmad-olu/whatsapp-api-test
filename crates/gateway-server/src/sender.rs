//! Outbound sending: validation, 24h-window guard, transport call, recording.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gateway_core::{window, Outbound, OutboundError};

use crate::db::Db;
use crate::transport::{Transport, TransportError};

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("invalid message: {0}")]
    Invalid(#[from] OutboundError),
    #[error("24h customer service window is closed for this recipient; send a template instead")]
    WindowClosed { last_inbound_at: Option<i64> },
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("database error: {0}")]
    Db(String),
}

#[derive(Debug, Clone)]
pub struct Sent {
    pub wamid: Option<String>,
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Digits only: "+234 801 234 5678" -> "2348012345678" (matches inbound `from`).
pub fn normalize_phone(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

pub struct Sender {
    db: Db,
    transport: Arc<dyn Transport>,
}

impl Sender {
    pub fn new(db: Db, transport: Arc<dyn Transport>) -> Self {
        Self { db, transport }
    }

    /// Send `out` from `phone_number_id`.
    ///
    /// No automatic retry here: a retry after an ambiguous failure could
    /// double-send. Callers decide.
    pub async fn send(&self, phone_number_id: &str, out: &Outbound) -> Result<Sent, SendError> {
        let body = out.to_json()?;

        if out.requires_open_window() {
            if let Some(to) = out.recipient() {
                let customer = normalize_phone(to);
                let last = self
                    .db
                    .last_inbound(phone_number_id, &customer)
                    .await
                    .map_err(|e| SendError::Db(format!("{e:#}")))?;
                if !window::is_open(last, now_secs()) {
                    return Err(SendError::WindowClosed {
                        last_inbound_at: last,
                    });
                }
            }
        }

        let resp = self.transport.send(phone_number_id, &body).await?;

        if let (Some(wamid), Some(to)) = (&resp.wamid, out.recipient()) {
            // The message is already out; a bookkeeping failure must not
            // turn a successful send into an error.
            if let Err(e) = self
                .db
                .record_outbound(
                    wamid,
                    phone_number_id,
                    &normalize_phone(to),
                    out.kind(),
                    &body,
                )
                .await
            {
                tracing::error!(%wamid, error = %format!("{e:#}"), "sent but failed to record outbound message");
            }
        }
        Ok(Sent { wamid: resp.wamid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_normalization() {
        assert_eq!(normalize_phone("+234 801 234 5678"), "2348012345678");
        assert_eq!(normalize_phone("2348012345678"), "2348012345678");
        assert_eq!(normalize_phone("(234)-801-234-5678"), "2348012345678");
    }
}
