//! HTTP export layer: `/metrics`, `/healthz`, `/readyz`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::snapshot::SnapshotStore;

pub fn router(store: Arc<SnapshotStore>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(store)
}

async fn metrics(State(store): State<Arc<SnapshotStore>>) -> Response {
    let body = store.latest().as_ref().clone();
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(store): State<Arc<SnapshotStore>>) -> Response {
    if store.is_ready() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no snapshot yet").into_response()
    }
}

pub async fn serve(listen: &str, store: Arc<SnapshotStore>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "http server listening");
    axum::serve(listener, router(store)).await
}
