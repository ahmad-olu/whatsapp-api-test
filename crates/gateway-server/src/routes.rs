//! HTTP surface: the webhook (Meta-facing) and an admin API (you-facing).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use gateway_core::{signature, window, Outbound};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::config::Config;
use crate::db::Db;
use crate::sender::{normalize_phone, now_secs, SendError, Sender};
use crate::transport::TransportError;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub sender: Arc<Sender>,
    pub ingest_wake: Arc<Notify>,
}

pub fn router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/send", post(admin_send))
        .route("/events", get(admin_events))
        .route("/raw", get(admin_raw))
        .route("/raw/{id}/replay", post(admin_replay))
        .route("/window", get(admin_window))
        .route("/usage", get(admin_usage))
        .route("/outbound", get(admin_outbound))
        .route("/media/{id}", get(admin_media))
        .route("/workspaces", put(admin_workspace))
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_auth));

    Router::new()
        .route("/", get(|| async { "whatsapp-gateway" }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/webhook", get(verify).post(receive))
        .nest("/admin", admin)
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes))
        .with_state(state)
}

// ---------------------------------------------------------------- errors --

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{e:#}"), "admin request failed");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<SendError> for ApiError {
    fn from(e: SendError) -> Self {
        let status = match &e {
            SendError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            SendError::WindowClosed { .. } => StatusCode::CONFLICT,
            SendError::Transport(TransportError::NotConfigured(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            SendError::Transport(_) => StatusCode::BAD_GATEWAY,
            SendError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

// --------------------------------------------------------------- webhook --

async fn readyz(State(st): State<AppState>) -> StatusCode {
    match st.db.ping().await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Deserialize)]
struct VerifyQuery {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

/// Meta's one-time subscription handshake: echo `hub.challenge` iff the
/// verify token matches.
async fn verify(State(st): State<AppState>, Query(q): Query<VerifyQuery>) -> Response {
    let ok = q.mode.as_deref() == Some("subscribe")
        && q.token
            .as_deref()
            .map(|t| signature::ct_eq(t, &st.config.verify_token))
            .unwrap_or(false);
    match (ok, q.challenge) {
        (true, Some(challenge)) => {
            tracing::info!("webhook verification handshake succeeded");
            (StatusCode::OK, challenge).into_response()
        }
        _ => {
            tracing::warn!("webhook verification handshake rejected");
            StatusCode::FORBIDDEN.into_response()
        }
    }
}

/// Receive a webhook. Verify the signature over the RAW bytes, persist, ack.
/// Nothing else happens on this path, so Meta never waits on our logic.
async fn receive(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if !st.config.insecure_skip_signature {
        let secret = st.config.app_secret.as_deref().unwrap_or_default();
        let header = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok());
        if let Err(e) = signature::verify(secret, &body, header) {
            tracing::warn!(error = %e, bytes = body.len(), "webhook rejected: bad signature");
            return StatusCode::UNAUTHORIZED;
        }
    }
    let Ok(text) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    match st.db.insert_raw(text).await {
        Ok(id) => {
            tracing::info!(raw_id = id, bytes = body.len(), "webhook accepted");
            st.ingest_wake.notify_one();
            StatusCode::OK
        }
        Err(e) => {
            // 5xx makes Meta retry, which is what we want when we could not persist.
            tracing::error!(error = %format!("{e:#}"), "failed to persist webhook");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ----------------------------------------------------------------- admin --

async fn admin_auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| signature::ct_eq(t, &st.config.admin_token))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token".into(),
        )
        .into_response()
    }
}

#[derive(Deserialize)]
struct SendRequest {
    /// Business number to send from. Defaults to WHATSAPP_PHONE_NUMBER_ID.
    phone_number_id: Option<String>,
    message: Outbound,
}

async fn admin_send(
    State(st): State<AppState>,
    Json(req): Json<SendRequest>,
) -> Result<Json<Value>, ApiError> {
    let pnid = req
        .phone_number_id
        .or_else(|| st.config.default_phone_number_id.clone())
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "phone_number_id missing and WHATSAPP_PHONE_NUMBER_ID not set".into(),
            )
        })?;
    let sent = st.sender.send(&pnid, &req.message).await?;
    Ok(Json(
        json!({ "wamid": sent.wamid, "phone_number_id": pnid }),
    ))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

fn limit(q: &LimitQuery) -> i64 {
    q.limit.unwrap_or(50).clamp(1, 500)
}

async fn admin_events(
    State(st): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.db.recent_events(limit(&q)).await?))
}

async fn admin_raw(
    State(st): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.db.recent_raw(limit(&q)).await?))
}

async fn admin_outbound(
    State(st): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.db.recent_outbound(limit(&q)).await?))
}

async fn admin_usage(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(st.db.usage().await?))
}

async fn admin_replay(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if st.db.replay_raw(id).await? {
        st.ingest_wake.notify_one();
        Ok(Json(json!({ "replayed": id })))
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no raw event {id}"),
        ))
    }
}

#[derive(Deserialize)]
struct WindowQuery {
    phone_number_id: Option<String>,
    customer: String,
}

async fn admin_window(
    State(st): State<AppState>,
    Query(q): Query<WindowQuery>,
) -> Result<Json<Value>, ApiError> {
    let pnid = q
        .phone_number_id
        .or_else(|| st.config.default_phone_number_id.clone())
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "phone_number_id required".into(),
            )
        })?;
    let customer = normalize_phone(&q.customer);
    let last = st.db.last_inbound(&pnid, &customer).await?;
    Ok(Json(json!({
        "phone_number_id": pnid,
        "customer": customer,
        "last_inbound_at": last,
        "window_open": window::is_open(last, now_secs()),
        "closes_at": window::closes_at(last),
    })))
}

async fn admin_media(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    match st.db.get_media(&id).await? {
        Some((mime, data)) => Ok((
            [(
                header::CONTENT_TYPE,
                mime.unwrap_or_else(|| "application/octet-stream".into()),
            )],
            data,
        )
            .into_response()),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such media".into())),
    }
}

#[derive(Deserialize)]
struct WorkspaceBody {
    phone_number_id: String,
    workspace_id: String,
    display_name: Option<String>,
}

/// Map a WhatsApp business number to a workspace (tenant routing).
async fn admin_workspace(
    State(st): State<AppState>,
    Json(b): Json<WorkspaceBody>,
) -> Result<Json<Value>, ApiError> {
    st.db
        .upsert_workspace(
            &b.phone_number_id,
            &b.workspace_id,
            b.display_name.as_deref(),
        )
        .await?;
    Ok(Json(json!({ "ok": true })))
}
