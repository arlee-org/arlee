use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use arlee_models::{
    CreateSandboxRequest, EdgeCapacity, ExecRequest, OkResponse, SandboxInfo,
};
use serde::Deserialize;

use crate::docker_runner::DockerRunner;
use crate::error::{map_runner_err, AppError};

pub struct AppState {
    pub runner: Arc<DockerRunner>,
    pub token: String,
    pub edge_id: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route("/sandboxes/:id", delete(kill_sandbox))
        .route("/sandboxes/:id/exec", post(exec_in_sandbox))
        .route(
            "/sandboxes/:id/file",
            get(read_file).put(write_file),
        )
        .route("/sandboxes/:id/trajectory", get(get_trajectory))
        .route("/capacity", get(capacity))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));
    let open = Router::new().route("/health", get(health));
    protected.merge(open).with_state(state)
}

async fn require_token(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let ok = req
        .headers()
        .get("x-arlee-token")
        .and_then(|h| h.to_str().ok())
        == Some(state.token.as_str());
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

// ----- handlers -----

async fn create_sandbox(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSandboxRequest>,
) -> Result<Json<SandboxInfo>, AppError> {
    let info = state
        .runner
        .create(&req.image, req.substrate, req.env)
        .await
        .map_err(AppError::from)?;
    Ok(Json(info))
}

async fn kill_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<OkResponse>, AppError> {
    state.runner.kill(&id).await.map_err(map_runner_err)?;
    Ok(Json(OkResponse::ok()))
}

async fn list_sandboxes(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<SandboxInfo>> {
    Json(state.runner.list_infos().await)
}

async fn exec_in_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<arlee_models::ExecResult>, AppError> {
    let r = state
        .runner
        .exec(
            &id,
            &req.command,
            req.cwd.as_deref(),
            &req.env,
            req.user.as_deref(),
            req.timeout,
        )
        .await
        .map_err(map_runner_err)?;
    Ok(Json(r))
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Response, AppError> {
    let content = state
        .runner
        .read_file(&id, &q.path)
        .await
        .map_err(map_runner_err)?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
    Ok((StatusCode::OK, headers, content).into_response())
}

async fn write_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
    body: Bytes,
) -> Result<Json<OkResponse>, AppError> {
    let size = body.len() as u64;
    state
        .runner
        .write_file(&id, &q.path, body.to_vec())
        .await
        .map_err(map_runner_err)?;
    Ok(Json(OkResponse::with_size(size)))
}

async fn get_trajectory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let entries = state
        .runner
        .get_trajectory(&id)
        .await
        .map_err(map_runner_err)?;
    Ok(Json(entries))
}

async fn capacity(State(state): State<Arc<AppState>>) -> Json<EdgeCapacity> {
    Json(EdgeCapacity {
        edge_id: state.edge_id.clone(),
        sandbox_count: state.runner.sandbox_count().await,
        healthy: true,
    })
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HashMap<&'static str, serde_json::Value>> {
    let mut out = HashMap::new();
    out.insert("ok", serde_json::Value::Bool(true));
    out.insert("edge_id", serde_json::Value::String(state.edge_id.clone()));
    out.insert(
        "sandbox_count",
        serde_json::Value::Number(state.runner.sandbox_count().await.into()),
    );
    Json(out)
}
