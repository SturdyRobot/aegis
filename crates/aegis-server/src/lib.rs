//! # aegis-server
//!
//! A lightweight embedded HTTP control plane (`aegis serve`) so external
//! dashboards, web UIs, and the WASM terminal can inspect runs and — crucially —
//! **resolve pending human-in-the-loop approvals** remotely, instead of a terminal
//! `y/N`. It's the callback target for [`aegis_hitl::WebhookApprover`].
//!
//! Endpoints:
//! - `GET  /health`                — liveness
//! - `GET  /runs`                  — every recorded run (from the ledger)
//! - `GET  /runs/{id}`             — a run's trajectory + events
//! - `GET  /approvals`             — approvals awaiting a human
//! - `POST /approvals/{id}`        — `{"approved": true|false}` resolves one

use std::net::SocketAddr;
use std::sync::Arc;

use aegis_hitl::{PendingApproval, PendingApprovals};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sturdy_core::TaskId;
use sturdy_ledger::Ledger;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    ledger: Ledger,
    approvals: Arc<PendingApprovals>,
}

/// A minimal API error that renders as an HTTP status + message.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl From<sturdy_ledger::LedgerError> for ApiError {
    fn from(e: sturdy_ledger::LedgerError) -> Self {
        ApiError(StatusCode::NOT_FOUND, e.to_string())
    }
}

/// Build the router over a ledger + a shared approvals registry. Exposed for tests.
pub fn router(ledger: Ledger, approvals: Arc<PendingApprovals>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runs", get(list_runs))
        .route("/runs/:id", get(get_run))
        .route("/approvals", get(list_approvals))
        .route("/approvals/:id", post(resolve_approval))
        .with_state(AppState { ledger, approvals })
}

/// Serve the control API on `addr` until the process exits.
pub async fn serve(
    ledger: Ledger,
    approvals: Arc<PendingApprovals>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "aegis control API listening");
    axum::serve(listener, router(ledger, approvals)).await
}

async fn health() -> &'static str {
    "ok"
}

async fn list_runs(State(s): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = s.ledger.list_runs()?;
    Ok(Json(json!(runs)))
}

async fn get_run(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = TaskId(
        Uuid::parse_str(&id)
            .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid run id".into()))?,
    );
    let trajectory = s.ledger.replay(tid)?; // NotFound → 404
    let events = s.ledger.events(tid).unwrap_or_default();
    Ok(Json(json!({ "trajectory": trajectory, "events": events })))
}

async fn list_approvals(State(s): State<AppState>) -> Json<Vec<PendingApproval>> {
    Json(s.approvals.list())
}

#[derive(Deserialize)]
struct ApproveBody {
    approved: bool,
}

async fn resolve_approval(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> StatusCode {
    if s.approvals.resolve(&id, body.approved) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND // unknown or already-resolved/expired id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn() -> (String, Ledger, Arc<PendingApprovals>) {
        let ledger = Ledger::in_memory().unwrap();
        let approvals = PendingApprovals::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(ledger.clone(), approvals.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), ledger, approvals)
    }

    #[tokio::test]
    async fn health_and_runs() {
        let (base, _ledger, _approvals) = spawn().await;
        let c = reqwest::Client::new();
        assert_eq!(
            c.get(format!("{base}/health"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        let runs: serde_json::Value = c
            .get(format!("{base}/runs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(runs.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn approvals_can_be_resolved_over_http() {
        let (base, _ledger, approvals) = spawn().await;
        let c = reqwest::Client::new();

        // The agent side registers a pending approval and awaits the decision.
        let (id, rx) = approvals.register("delete_file", "high");

        // It shows up on the API…
        let listed: Vec<serde_json::Value> = c
            .get(format!("{base}/approvals"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);

        // …a human resolves it over HTTP, and the awaiter unblocks.
        let resp = c
            .post(format!("{base}/approvals/{id}"))
            .json(&json!({ "approved": true }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        assert!(rx.await.unwrap());

        // Unknown id → 404.
        let resp = c
            .post(format!("{base}/approvals/does-not-exist"))
            .json(&json!({ "approved": false }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }
}
