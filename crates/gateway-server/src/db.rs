//! All SQL lives here. Queries are runtime-checked (no compile-time DB needed).

use std::time::Duration;

use anyhow::{Context, Result};
use gateway_core::status::{self, DeliveryStatus};
use gateway_core::{Event, StatusUpdate};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};

pub const MAX_RAW_ATTEMPTS: i32 = 5;
pub const MAX_DELIVERY_ATTEMPTS: i32 = 10;

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

/// An event row as handed to the sink.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub id: i64,
    pub dedupe_key: String,
    pub workspace_id: Option<String>,
    pub event: Event,
}

/// A delivery claim: the event row is locked (`FOR UPDATE SKIP LOCKED`) inside
/// an open transaction until the claim is completed.
pub struct Claim {
    tx: Transaction<'static, Postgres>,
    pub event: StoredEvent,
}

impl Claim {
    pub async fn mark_delivered(mut self) -> Result<()> {
        sqlx::query("UPDATE events SET forwarded_at = now(), forward_error = NULL WHERE id = $1")
            .bind(self.event.id)
            .execute(&mut *self.tx)
            .await?;
        self.tx.commit().await?;
        Ok(())
    }

    /// Record a failure and schedule a retry with exponential backoff
    /// (5s, 10s, 20s ... capped at 15min).
    pub async fn mark_failed(mut self, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE events SET
                forward_attempts = forward_attempts + 1,
                forward_error    = $2,
                next_attempt_at  = now() + make_interval(secs => LEAST(5 * power(2, forward_attempts), 900))
             WHERE id = $1",
        )
        .bind(self.event.id)
        .bind(error)
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await?;
        Ok(())
    }
}

impl Db {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await
            .context("connecting to database")?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("running migrations")?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // ---- workspaces -------------------------------------------------------

    pub async fn upsert_workspace(
        &self,
        phone_number_id: &str,
        workspace_id: &str,
        display_name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspaces (phone_number_id, workspace_id, display_name)
             VALUES ($1, $2, $3)
             ON CONFLICT (phone_number_id)
             DO UPDATE SET workspace_id = EXCLUDED.workspace_id, display_name = EXCLUDED.display_name",
        )
        .bind(phone_number_id)
        .bind(workspace_id)
        .bind(display_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- ingest queue -----------------------------------------------------

    /// Persist a verified webhook body verbatim. This is the ONLY thing the
    /// HTTP handler does before answering 200.
    pub async fn insert_raw(&self, body: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("INSERT INTO raw_events (body) VALUES ($1) RETURNING id")
                .bind(body)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Process the oldest unprocessed raw event inside one transaction:
    /// normalize, dedupe, update window/status state. Returns `false` when the
    /// queue is empty.
    pub async fn process_next_raw(&self) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT id, body FROM raw_events
             WHERE processed_at IS NULL AND attempts < $1
             ORDER BY id LIMIT 1
             FOR UPDATE SKIP LOCKED",
        )
        .bind(MAX_RAW_ATTEMPTS)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let id: i64 = row.get("id");
        let body: String = row.get("body");

        let normalized = match gateway_core::normalize_bytes(body.as_bytes()) {
            Ok(n) => n,
            Err(e) => {
                // Permanent failure: bad JSON will never get better.
                tracing::warn!(raw_id = id, error = %e, "unparseable webhook body");
                sqlx::query(
                    "UPDATE raw_events SET processed_at = now(), attempts = attempts + 1, last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(format!("invalid json: {e}"))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(true);
            }
        };

        let mut new_events = 0usize;
        let mut failure: Option<anyhow::Error> = None;
        for ev in &normalized.events {
            match ingest_one(&mut tx, id, ev).await {
                Ok(true) => new_events += 1,
                Ok(false) => tracing::debug!(key = %ev.dedupe_key(), "duplicate event ignored"),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }

        match failure {
            None => {
                let note = (normalized.skipped > 0)
                    .then(|| format!("{} entries skipped", normalized.skipped));
                sqlx::query(
                    "UPDATE raw_events SET processed_at = now(), last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(note)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                tracing::info!(raw_id = id, new_events, "raw event processed");
            }
            Some(e) => {
                tx.rollback().await.ok();
                tracing::error!(raw_id = id, error = %format!("{e:#}"), "processing failed; will retry");
                sqlx::query(
                    "UPDATE raw_events SET attempts = attempts + 1, last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(format!("{e:#}"))
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(true)
    }

    // ---- delivery queue ---------------------------------------------------

    pub async fn claim_next_delivery(&self) -> Result<Option<Claim>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT id, dedupe_key, workspace_id, payload FROM events
             WHERE forwarded_at IS NULL
               AND forward_attempts < $1
               AND next_attempt_at <= now()
             ORDER BY id LIMIT 1
             FOR UPDATE SKIP LOCKED",
        )
        .bind(MAX_DELIVERY_ATTEMPTS)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let id: i64 = row.get("id");
        let payload: Value = row.get("payload");
        match serde_json::from_value::<Event>(payload) {
            Ok(event) => Ok(Some(Claim {
                tx,
                event: StoredEvent {
                    id,
                    dedupe_key: row.get("dedupe_key"),
                    workspace_id: row.get("workspace_id"),
                    event,
                },
            })),
            Err(e) => {
                // Poison row: park it so it cannot block the queue.
                tracing::error!(event_id = id, error = %e, "undecodable stored event; parking");
                sqlx::query(
                    "UPDATE events SET forward_attempts = $2, forward_error = $3 WHERE id = $1",
                )
                .bind(id)
                .bind(MAX_DELIVERY_ATTEMPTS)
                .bind(format!("undecodable payload: {e}"))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(None)
            }
        }
    }

    // ---- window + outbound ------------------------------------------------

    pub async fn last_inbound(&self, phone_number_id: &str, customer: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT last_inbound_at FROM conversations WHERE phone_number_id = $1 AND customer = $2",
        )
        .bind(phone_number_id)
        .bind(customer)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn record_outbound(
        &self,
        wamid: &str,
        phone_number_id: &str,
        recipient: &str,
        kind: &str,
        request: &Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO outbound_messages (wamid, phone_number_id, recipient, kind, request)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (wamid) DO UPDATE
               SET recipient = EXCLUDED.recipient, kind = EXCLUDED.kind, request = EXCLUDED.request",
        )
        .bind(wamid)
        .bind(phone_number_id)
        .bind(recipient)
        .bind(kind)
        .bind(request)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- media ------------------------------------------------------------

    pub async fn media_exists(&self, media_id: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM media WHERE media_id = $1)")
                .bind(media_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn insert_media(
        &self,
        media_id: &str,
        phone_number_id: &str,
        mime_type: Option<&str>,
        sha256: &str,
        data: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO media (media_id, phone_number_id, mime_type, size, sha256, data)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (media_id) DO NOTHING",
        )
        .bind(media_id)
        .bind(phone_number_id)
        .bind(mime_type)
        .bind(data.len() as i64)
        .bind(sha256)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_media(&self, media_id: &str) -> Result<Option<(Option<String>, Vec<u8>)>> {
        let row = sqlx::query("SELECT mime_type, data FROM media WHERE media_id = $1")
            .bind(media_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get("mime_type"), r.get("data"))))
    }

    // ---- admin / debugging views (returned as JSON straight from SQL) -----

    pub async fn recent_events(&self, limit: i64) -> Result<Value> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(jsonb_agg(t ORDER BY t.id DESC), '[]'::jsonb) FROM (
                SELECT id, dedupe_key, kind, workspace_id, phone_number_id, payload,
                       created_at::text AS created_at,
                       (forwarded_at IS NOT NULL) AS delivered,
                       forward_attempts, forward_error
                FROM events ORDER BY id DESC LIMIT $1) t",
        )
        .bind(limit)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn recent_raw(&self, limit: i64) -> Result<Value> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(jsonb_agg(t ORDER BY t.id DESC), '[]'::jsonb) FROM (
                SELECT id, received_at::text AS received_at,
                       (processed_at IS NOT NULL) AS processed,
                       attempts, last_error, body
                FROM raw_events ORDER BY id DESC LIMIT $1) t",
        )
        .bind(limit)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn recent_outbound(&self, limit: i64) -> Result<Value> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(jsonb_agg(t ORDER BY t.created_at DESC), '[]'::jsonb) FROM (
                SELECT wamid, phone_number_id, recipient, kind, status, status_at,
                       billable, pricing_category, error_code, error_title,
                       created_at::text AS created_at
                FROM outbound_messages ORDER BY created_at DESC LIMIT $1) t",
        )
        .bind(limit)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn usage(&self) -> Result<Value> {
        Ok(sqlx::query_scalar(
            "SELECT jsonb_build_object(
               'sent_by_kind', (SELECT coalesce(jsonb_object_agg(kind, n), '{}'::jsonb)
                                FROM (SELECT kind, count(*) AS n FROM outbound_messages
                                      WHERE kind IS NOT NULL GROUP BY kind) a),
               'sent_by_phone_number_id', (SELECT coalesce(jsonb_object_agg(phone_number_id, n), '{}'::jsonb)
                                FROM (SELECT phone_number_id, count(*) AS n FROM outbound_messages
                                      WHERE kind IS NOT NULL GROUP BY phone_number_id) b),
               'by_pricing_category', (SELECT coalesce(jsonb_object_agg(cat, n), '{}'::jsonb)
                                FROM (SELECT coalesce(pricing_category, 'unknown') AS cat, count(*) AS n
                                      FROM outbound_messages WHERE status IS NOT NULL GROUP BY 1) c),
               'billable_messages', (SELECT count(*) FROM outbound_messages WHERE billable IS TRUE),
               'failed_messages', (SELECT count(*) FROM outbound_messages WHERE status = 'failed'),
               'inbound_messages', (SELECT count(*) FROM events WHERE kind = 'message'))",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Re-queue a raw event. Derived events are deleted first so they are
    /// re-derived (otherwise dedupe would make the replay a no-op).
    /// Debug tool: conversation/status state is NOT rolled back.
    pub async fn replay_raw(&self, raw_id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM events WHERE raw_event_id = $1")
            .bind(raw_id)
            .execute(&mut *tx)
            .await?;
        let n = sqlx::query(
            "UPDATE raw_events SET processed_at = NULL, attempts = 0, last_error = NULL WHERE id = $1",
        )
        .bind(raw_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }
}

/// Insert one normalized event and apply its side effects. Returns `false`
/// when the event was a duplicate (already stored under the same key).
async fn ingest_one(conn: &mut PgConnection, raw_id: i64, ev: &Event) -> Result<bool> {
    let pnid = ev.phone_number_id();
    let workspace: Option<String> =
        sqlx::query_scalar("SELECT workspace_id FROM workspaces WHERE phone_number_id = $1")
            .bind(pnid)
            .fetch_optional(&mut *conn)
            .await?;
    if workspace.is_none() {
        tracing::warn!(
            phone_number_id = pnid,
            "event for unrouted phone number (no workspace mapping)"
        );
    }

    let kind = match ev {
        Event::Message(_) => "message",
        Event::Status(_) => "status",
    };
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO events (dedupe_key, raw_event_id, phone_number_id, workspace_id, kind, payload)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (dedupe_key) DO NOTHING
         RETURNING id",
    )
    .bind(ev.dedupe_key())
    .bind(raw_id)
    .bind(pnid)
    .bind(&workspace)
    .bind(kind)
    .bind(serde_json::to_value(ev)?)
    .fetch_optional(&mut *conn)
    .await?;

    if inserted.is_none() {
        return Ok(false);
    }

    match ev {
        Event::Message(m) => {
            sqlx::query(
                "INSERT INTO conversations (phone_number_id, customer, last_inbound_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (phone_number_id, customer)
                 DO UPDATE SET last_inbound_at = GREATEST(conversations.last_inbound_at, EXCLUDED.last_inbound_at)",
            )
            .bind(&m.phone_number_id)
            .bind(&m.from)
            .bind(m.timestamp)
            .execute(&mut *conn)
            .await?;
        }
        Event::Status(s) => apply_status(conn, s).await?,
    }
    Ok(true)
}

/// Apply a delivery status using the forward-only state machine from core.
async fn apply_status(conn: &mut PgConnection, s: &StatusUpdate) -> Result<()> {
    // Ensure a row exists (status can race ahead of the send path's insert).
    sqlx::query(
        "INSERT INTO outbound_messages (wamid, phone_number_id, recipient)
         VALUES ($1, $2, $3) ON CONFLICT (wamid) DO NOTHING",
    )
    .bind(&s.id)
    .bind(&s.phone_number_id)
    .bind(&s.recipient)
    .execute(&mut *conn)
    .await?;

    let current: Option<String> =
        sqlx::query_scalar("SELECT status FROM outbound_messages WHERE wamid = $1 FOR UPDATE")
            .bind(&s.id)
            .fetch_one(&mut *conn)
            .await?;
    let current = current.as_deref().and_then(DeliveryStatus::parse);

    if let Some(next) = status::advance(current, s.status) {
        sqlx::query(
            "UPDATE outbound_messages
             SET status = $2, status_at = $3, error_code = $4, error_title = $5
             WHERE wamid = $1",
        )
        .bind(&s.id)
        .bind(next.as_str())
        .bind(s.timestamp)
        .bind(s.error.as_ref().map(|e| e.code))
        .bind(s.error.as_ref().and_then(|e| e.title.clone()))
        .execute(&mut *conn)
        .await?;
    }

    // Pricing info can arrive on any status; keep the latest non-null values.
    if s.billable.is_some() || s.pricing_category.is_some() {
        sqlx::query(
            "UPDATE outbound_messages
             SET billable = COALESCE($2, billable),
                 pricing_category = COALESCE($3, pricing_category)
             WHERE wamid = $1",
        )
        .bind(&s.id)
        .bind(s.billable)
        .bind(&s.pricing_category)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
