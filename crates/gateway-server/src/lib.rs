//! WhatsApp gateway server.
//!
//! ```text
//! POST /webhook -> verify HMAC (raw bytes) -> INSERT raw_events -> 200
//!                                   |
//!               ingest worker: normalize -> dedupe -> window/status state
//!                                   |
//!               delivery worker: fetch media -> EventSink (echo | forward | log)
//! ```

pub mod config;
pub mod db;
pub mod routes;
pub mod sender;
pub mod sink;
pub mod transport;
pub mod worker;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::Db;
use crate::routes::AppState;
use crate::sender::Sender;
use crate::sink::Deliverer;
use crate::transport::Transport;

/// A running gateway: shared state plus its background workers.
pub struct Gateway {
    pub state: AppState,
    cancel: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

impl Gateway {
    /// Run migrations, seed the default tenant, and start the workers.
    pub async fn start(config: Config, db: Db, transport: Arc<dyn Transport>) -> Result<Self> {
        db.migrate().await?;
        if let Some(pnid) = &config.default_phone_number_id {
            // Do not clobber a mapping edited through the admin API.
            sqlx::query(
                "INSERT INTO workspaces (phone_number_id, workspace_id, display_name)
                 VALUES ($1, 'default', 'Default test workspace') ON CONFLICT DO NOTHING",
            )
            .bind(pnid)
            .execute(db.pool())
            .await?;
        }

        let config = Arc::new(config);
        let sender = Arc::new(Sender::new(db.clone(), transport.clone()));
        let sink = sink::build(&config, sender.clone());
        let deliverer = Arc::new(Deliverer::new(db.clone(), transport, sink));

        let ingest_wake = Arc::new(Notify::new());
        let delivery_wake = Arc::new(Notify::new());
        let cancel = CancellationToken::new();

        let handles = vec![
            tokio::spawn(worker::run_ingest(
                db.clone(),
                ingest_wake.clone(),
                delivery_wake.clone(),
                cancel.clone(),
            )),
            tokio::spawn(worker::run_delivery(
                db.clone(),
                deliverer,
                delivery_wake,
                cancel.clone(),
            )),
        ];

        Ok(Self {
            state: AppState {
                config,
                db,
                sender,
                ingest_wake,
            },
            cancel,
            handles,
        })
    }

    pub fn router(&self) -> Router {
        routes::router(self.state.clone())
    }

    /// Stop the workers and wait for them to finish their current item.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for h in self.handles {
            let _ = h.await;
        }
    }
}
