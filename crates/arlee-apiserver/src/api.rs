use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use arlee_models::{
    CreateSandboxRequest, EdgeCapacity, EdgeInfo, ExecRequest, ExecResult, HealthResponse,
    HeartbeatRequest, OkResponse, RegisterEdgeRequest, SandboxInfo,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::warn;

use crate::config::ApiserverConfig;
use crate::error::AppError;
use crate::scheduler::pick_edge;
use crate::state::State as ClusterState;

pub struct AppState {
    pub state: Arc<ClusterState>,
    pub cfg: Arc<ApiserverConfig>,
    pub http: reqwest::Client,
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/edges", get(list_edges))
        .route("/edges/register", post(register_edge))
        .route("/edges/:id/heartbeat", post(heartbeat))
        .route("/capacity", get(capacity))
        .route("/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route("/sandboxes/:id", delete(kill_sandbox))
        .route("/sandboxes/:id/exec", post(exec_in_sandbox))
        .route(
            "/sandboxes/:id/file",
            get(read_file).put(write_file),
        )
        .route("/sandboxes/:id/trajectory", get(get_trajectory))
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
        == Some(state.cfg.token.as_str());
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

// ---------------------------------------------------------------------------
// Edge registration & heartbeat
// ---------------------------------------------------------------------------

async fn register_edge(
    State(s): State<Arc<AppState>>,
    Json(req): Json<RegisterEdgeRequest>,
) -> Result<Json<OkResponse>, AppError> {
    s.state
        .register_edge(req.edge_id.clone(), req.url.clone(), req.sandbox_count)
        .await;
    // Best-effort: ask the Edge for its sandboxes so we can re-route after a restart.
    let url = format!("{}/sandboxes", req.url.trim_end_matches('/'));
    match s
        .http
        .get(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            if let Ok(items) = r.json::<Vec<SandboxInfo>>().await {
                for sb in items {
                    s.state.record_sandbox(sb.id, &req.edge_id).await;
                }
            }
        }
        Ok(r) => warn!("rebuild sandboxes: edge {} returned {}", req.edge_id, r.status()),
        Err(e) => warn!("rebuild sandboxes: {e}"),
    }
    tracing::info!("edge registered: {} @ {}", req.edge_id, req.url);
    Ok(Json(OkResponse::ok()))
}

async fn heartbeat(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<OkResponse>, AppError> {
    if s.state.heartbeat(&id, req.sandbox_count).await {
        Ok(Json(OkResponse::ok()))
    } else {
        Err(AppError::NotFound("unknown edge; re-register".into()))
    }
}

// ---------------------------------------------------------------------------
// Listings
// ---------------------------------------------------------------------------

async fn list_edges(State(s): State<Arc<AppState>>) -> Json<Vec<EdgeInfo>> {
    Json(s.state.edge_infos().await)
}

async fn capacity(State(s): State<Arc<AppState>>) -> Json<Vec<EdgeCapacity>> {
    let now = Utc::now();
    let threshold = s.state.healthy_threshold();
    let out: Vec<EdgeCapacity> = s
        .state
        .edges()
        .await
        .iter()
        .map(|e| EdgeCapacity {
            edge_id: e.edge_id.clone(),
            sandbox_count: e.sandbox_count,
            healthy: now
                .signed_duration_since(e.last_seen)
                .to_std()
                .map_or(false, |d| d < threshold),
        })
        .collect();
    Json(out)
}

async fn list_sandboxes(State(s): State<Arc<AppState>>) -> Json<Vec<SandboxInfo>> {
    let mut out = Vec::new();
    for edge in s.state.edges().await {
        let url = format!("{}/sandboxes", edge.url.trim_end_matches('/'));
        match s
            .http
            .get(&url)
            .header("X-Arlee-Token", &s.cfg.token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                if let Ok(items) = r.json::<Vec<SandboxInfo>>().await {
                    out.extend(items);
                }
            }
            Ok(r) => warn!("list_sandboxes: edge {} returned {}", edge.edge_id, r.status()),
            Err(e) => warn!("list_sandboxes: edge {}: {e}", edge.edge_id),
        }
    }
    Json(out)
}

async fn health(State(s): State<Arc<AppState>>) -> Json<HealthResponse> {
    let infos = s.state.edge_infos().await;
    let healthy_edges = infos.iter().filter(|e| e.healthy).count() as u32;
    Json(HealthResponse {
        ok: true,
        edge_count: infos.len() as u32,
        healthy_edges,
    })
}

// ---------------------------------------------------------------------------
// Sandbox lifecycle (forwarded)
// ---------------------------------------------------------------------------

async fn create_sandbox(
    State(s): State<Arc<AppState>>,
    Json(req): Json<CreateSandboxRequest>,
) -> Result<Json<SandboxInfo>, AppError> {
    let edge = pick_edge(&s.state).await.map_err(|_| AppError::NoEdges)?;
    let url = format!("{}/sandboxes", edge.url.trim_end_matches('/'));
    let r = s
        .http
        .post(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .json(&req)
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    let info: SandboxInfo = r
        .json()
        .await
        .map_err(|e| AppError::BadGateway(format!("decode: {e}")))?;
    s.state.record_sandbox(info.id.clone(), &edge.edge_id).await;
    Ok(Json(info))
}

async fn kill_sandbox(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<OkResponse>, AppError> {
    let edge = s
        .state
        .edge_for_sandbox(&id)
        .await
        .ok_or(AppError::NotFound("sandbox not found".into()))?;
    let url = format!("{}/sandboxes/{}", edge.url.trim_end_matches('/'), id);
    let r = s
        .http
        .delete(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    s.state.forget_sandbox(&id).await;
    Ok(Json(OkResponse::ok()))
}

// ---------------------------------------------------------------------------
// Sandbox operations (forwarded)
// ---------------------------------------------------------------------------

async fn exec_in_sandbox(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResult>, AppError> {
    let edge = s
        .state
        .edge_for_sandbox(&id)
        .await
        .ok_or(AppError::NotFound("sandbox not found".into()))?;
    let url = format!("{}/sandboxes/{}/exec", edge.url.trim_end_matches('/'), id);
    let timeout = req
        .timeout
        .map(|t| std::time::Duration::from_secs_f64(t + 30.0))
        .unwrap_or(s.cfg.edge_request_timeout);
    let r = s
        .http
        .post(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .json(&req)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    let result: ExecResult = r
        .json()
        .await
        .map_err(|e| AppError::BadGateway(format!("decode: {e}")))?;
    Ok(Json(result))
}

async fn read_file(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Response, AppError> {
    let edge = s
        .state
        .edge_for_sandbox(&id)
        .await
        .ok_or(AppError::NotFound("sandbox not found".into()))?;
    let url = format!("{}/sandboxes/{}/file", edge.url.trim_end_matches('/'), id);
    let r = s
        .http
        .get(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .query(&[("path", q.path.as_str())])
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::NotFound(format!("file not found: {}", q.path)));
    }
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    let bytes = r
        .bytes()
        .await
        .map_err(|e| AppError::BadGateway(format!("decode: {e}")))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
    Ok((StatusCode::OK, headers, bytes).into_response())
}

async fn write_file(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
    body: Bytes,
) -> Result<Json<OkResponse>, AppError> {
    let edge = s
        .state
        .edge_for_sandbox(&id)
        .await
        .ok_or(AppError::NotFound("sandbox not found".into()))?;
    let url = format!("{}/sandboxes/{}/file", edge.url.trim_end_matches('/'), id);
    let r = s
        .http
        .put(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .header("Content-Type", "application/octet-stream")
        .query(&[("path", q.path.as_str())])
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    let resp: OkResponse = r
        .json()
        .await
        .map_err(|e| AppError::BadGateway(format!("decode: {e}")))?;
    Ok(Json(resp))
}

async fn get_trajectory(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let edge = s
        .state
        .edge_for_sandbox(&id)
        .await
        .ok_or(AppError::NotFound("sandbox not found".into()))?;
    let url = format!(
        "{}/sandboxes/{}/trajectory",
        edge.url.trim_end_matches('/'),
        id
    );
    let r = s
        .http
        .get(&url)
        .header("X-Arlee-Token", &s.cfg.token)
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("{}: {e}", edge.edge_id)))?;
    if !r.status().is_success() {
        return Err(AppError::BadGateway(format!(
            "{} returned {}",
            edge.edge_id,
            r.status()
        )));
    }
    let items: Vec<serde_json::Value> = r
        .json()
        .await
        .map_err(|e| AppError::BadGateway(format!("decode: {e}")))?;
    Ok(Json(items))
}
