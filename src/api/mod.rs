use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use crate::core::downloader::DownloadEngine;
use crate::core::types::DownloadTaskConfig;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

pub struct AppState {
    pub engine: Arc<DownloadEngine>,
}

pub fn create_router(engine: Arc<DownloadEngine>) -> Router {
    let state = Arc::new(AppState { engine });
    Router::new()
        .route("/download/create", post(create_task))
        .route("/download/:id/config", patch(update_config))
        .route("/download/:id/status", get(get_status))
        .with_state(state)
}

use tracing::info;

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(config): Json<DownloadTaskConfig>,
) -> impl IntoResponse {
    info!("收到创建任务请求: {:?}", config);
    match state.engine.create_task(config).await {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateConfigPayload {
    pub max_threads: Option<u32>,
    pub limit_bps: Option<u64>,
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
    pub headers: Option<Vec<String>>,
    pub checksum: Option<String>,
    pub max_retries: Option<u32>,
    pub auth: Option<String>,
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateConfigPayload>,
) -> impl IntoResponse {
    // We need to fetch existing config to update it?
    // Or DownloadEngine handles partial update?
    // DownloadEngine::update_task_config takes full config.
    // I should modify DownloadEngine to take partial or fetch-modify-save.
    
    // Better: Fetch task, modify config, save.
    if let Some(handle) = state.engine.get_task(id) {
        let mut config = {
            let h = handle.read().await;
            h.config.clone()
        };
        
        if let Some(mt) = payload.max_threads {
            config.max_threads = mt;
        }
        if let Some(bps) = payload.limit_bps {
            config.limit_bps = Some(bps);
        }
        if let Some(ua) = payload.user_agent {
            config.user_agent = Some(ua);
        }
        if let Some(p) = payload.proxy {
            config.proxy = Some(p);
        }
        if let Some(h) = payload.headers {
            config.headers = h;
        }
        if let Some(c) = payload.checksum {
            config.checksum = Some(c);
        }
        if let Some(r) = payload.max_retries {
            config.max_retries = r;
        }
        if let Some(a) = payload.auth {
            config.auth = Some(a);
        }
        
        match state.engine.update_task_config(id, config).await {
            Ok(_) => StatusCode::OK.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    } else {
        (StatusCode::NOT_FOUND, "未找到任务").into_response()
    }
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if let Some(handle) = state.engine.get_task(id) {
        let meta = {
            let h = handle.read().await;
            let m = h.metadata.read().await;
            m.clone()
        };
        Json(meta).into_response()
    } else {
        (StatusCode::NOT_FOUND, "未找到任务").into_response()
    }
}
