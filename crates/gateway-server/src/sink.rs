//! Where normalized events go. Swap the sink, not the pipeline.
//!
//! Delivery is **at-least-once and not strictly ordered across retries**.
//! Consumers must be idempotent: every forwarded request carries an
//! `Idempotency-Key` header (the event's dedupe key).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use gateway_core::{demo, signature, Event, Outbound};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::{Config, SinkMode};
use crate::db::{Db, StoredEvent};
use crate::sender::Sender;
use crate::transport::Transport;

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn handle(&self, ev: &StoredEvent) -> Result<()>;
}

/// Logs events and does nothing else.
pub struct LogSink;

#[async_trait]
impl EventSink for LogSink {
    async fn handle(&self, ev: &StoredEvent) -> Result<()> {
        tracing::info!(event_id = ev.id, key = %ev.dedupe_key, workspace = ?ev.workspace_id, "event: {}",
            serde_json::to_string(&ev.event).unwrap_or_default());
        Ok(())
    }
}

/// The demo bookkeeping bot: text in -> confirm buttons out -> reply in -> text out.
///
/// Note: if the second of two replies fails and the event is retried, the
/// first reply is sent again. Fine for a demo; a real consumer should be
/// idempotent on the event id.
pub struct EchoSink {
    sender: Arc<Sender>,
    auto_mark_read: bool,
}

#[async_trait]
impl EventSink for EchoSink {
    async fn handle(&self, ev: &StoredEvent) -> Result<()> {
        let Event::Message(m) = &ev.event else {
            return Ok(()); // statuses need no reply
        };
        if self.auto_mark_read {
            let read = Outbound::MarkRead {
                message_id: m.id.clone(),
            };
            if let Err(e) = self.sender.send(&m.phone_number_id, &read).await {
                tracing::warn!(error = %e, "mark-as-read failed (ignored)");
            }
        }
        for out in demo::respond(m) {
            self.sender
                .send(&m.phone_number_id, &out)
                .await
                .with_context(|| format!("sending {} reply", out.kind()))?;
        }
        Ok(())
    }
}

/// POSTs each event to the main app, HMAC-signed with `FORWARD_SECRET`.
///
/// Request body:
/// `{"id": <event row id>, "workspace_id": "..."|null, "event": <normalized Event>}`
/// Headers: `X-Gateway-Signature-256: sha256=<hmac of raw body>`, `Idempotency-Key`.
pub struct ForwardSink {
    client: reqwest::Client,
    url: String,
    secret: String,
}

impl ForwardSink {
    pub fn new(url: String, secret: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("whatsapp-gateway/0.1")
            .build()
            .expect("reqwest client");
        Self {
            client,
            url,
            secret,
        }
    }
}

#[async_trait]
impl EventSink for ForwardSink {
    async fn handle(&self, ev: &StoredEvent) -> Result<()> {
        let body = serde_json::to_vec(&json!({
            "id": ev.id,
            "workspace_id": ev.workspace_id,
            "event": ev.event,
        }))?;
        let resp = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header(
                "x-gateway-signature-256",
                signature::sign(&self.secret, &body),
            )
            .header("idempotency-key", &ev.dedupe_key)
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("forward request failed: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("forward target answered HTTP {status}");
        }
        Ok(())
    }
}

pub fn build(config: &Config, sender: Arc<Sender>) -> Arc<dyn EventSink> {
    match config.sink {
        SinkMode::Echo => Arc::new(EchoSink {
            sender,
            auto_mark_read: config.auto_mark_read,
        }),
        SinkMode::Log => Arc::new(LogSink),
        SinkMode::Forward => Arc::new(ForwardSink::new(
            config.forward_url.clone().unwrap_or_default(),
            config.forward_secret.clone().unwrap_or_default(),
        )),
    }
}

/// Pre-sink pipeline stage: fetch and store inbound media, then hand off.
pub struct Deliverer {
    db: Db,
    transport: Arc<dyn Transport>,
    sink: Arc<dyn EventSink>,
}

impl Deliverer {
    pub fn new(db: Db, transport: Arc<dyn Transport>, sink: Arc<dyn EventSink>) -> Self {
        Self {
            db,
            transport,
            sink,
        }
    }

    pub async fn deliver(&self, ev: &StoredEvent) -> Result<()> {
        if let Event::Message(m) = &ev.event {
            if let Some(info) = m.content.media() {
                self.ensure_media(&m.phone_number_id, &info.media_id)
                    .await?;
            }
        }
        self.sink.handle(ev).await
    }

    /// Media URLs expire within minutes, so download immediately; the media
    /// *id* stays valid for ~30 days, which lets retries re-resolve a fresh URL.
    async fn ensure_media(&self, phone_number_id: &str, media_id: &str) -> Result<()> {
        if self.db.media_exists(media_id).await? {
            return Ok(());
        }
        let meta = self
            .transport
            .media_meta(media_id)
            .await
            .context("resolving media url")?;
        let data = self
            .transport
            .download(&meta.url)
            .await
            .context("downloading media")?;
        let actual = hex::encode(Sha256::digest(&data));
        if let Some(expected) = &meta.sha256 {
            if !expected.eq_ignore_ascii_case(&actual) {
                // Meta documents this field but its encoding has varied; warn, don't fail.
                tracing::warn!(media_id, %expected, %actual, "media sha256 differs from metadata");
            }
        }
        self.db
            .insert_media(
                media_id,
                phone_number_id,
                meta.mime_type.as_deref(),
                &actual,
                &data,
            )
            .await?;
        tracing::info!(media_id, bytes = data.len(), "media stored");
        Ok(())
    }
}
