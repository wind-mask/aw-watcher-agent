//! 后台 daemon HTTP ingest 服务。
//!
//! 各类 code agent 扩展通过 HTTP POST 会话事件到本 daemon，由 daemon 汇总并
//! 通过 ActivityWatch heartbeat 机制写入 session 时间段数据。
//!
//! 遵循 AW 的 heartbeat + pulsetime 模式：
//! - session 开始时立即发一次 heartbeat
//! - session 结束时发最后一次 summary heartbeat（含 token/cost），之后停止
//! - 期间由扩展通过 agent_start 等事件显式触发 heartbeat 保活

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use aw_models::Event;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{TimeDelta, Utc};
use dashmap::DashMap;
use serde::Serialize;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    buckets::BucketManager,
    client::WatcherClient,
    events::{
        ActiveSession, SessionEndRequest, SessionHeartbeatRequest, SessionStartRequest,
        SessionUpdateRequest,
    },
    shutdown_signal::wait_for_shutdown_signal,
};

/// AW heartbeat pulsetime：连续两次心跳之间的最大可接受间隔。
const PULSETIME_SECS: f64 = 25.0;

/// AW 写入最大重试次数
const MAX_RETRIES: u32 = 3;
/// 初始退避延迟（毫秒）
const INITIAL_BACKOFF_MS: u64 = 100;

#[derive(Clone)]
struct AppState {
    client: Arc<WatcherClient>,
    bucket_id: String,
    sessions: Arc<DashMap<String, ActiveSession>>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    service: &'static str,
    bucket_id: String,
    active_sessions: usize,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    ok: bool,
    session_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    ok: bool,
    error: String,
}

type ApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<ErrorResponse>)>;

/// 启动 daemon。
pub async fn run_daemon(
    client: WatcherClient,
    buckets: BucketManager,
    listen: SocketAddr,
) -> Result<()> {
    buckets.setup(&client)?;
    let state = AppState {
        client: Arc::new(client),
        bucket_id: buckets.session_bucket_id,
        sessions: Arc::new(DashMap::new()),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/session/start", post(session_start))
        .route("/api/v1/session/update", post(session_update))
        .route("/api/v1/session/heartbeat", post(session_heartbeat))
        .route("/api/v1/session/end", post(session_end))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("Failed to bind daemon on {}", listen))?;

    info!("aw-watcher-agent daemon listening on http://{}", listen);

    // 优雅关闭：收到 SIGINT / SIGTERM 时排空所有活跃 session 并发送 final heartbeat
    let shutdown_state = state.clone();
    let shutdown_signal = async move {
        let signal_name = wait_for_shutdown_signal().await;
        info!(
            "{} received, draining {} active sessions...",
            signal_name,
            shutdown_state.sessions.len()
        );

        for entry in shutdown_state.sessions.iter() {
            let session = entry.value();
            let final_heartbeat = build_heartbeat_event(session, true);
            if let Err(err) =
                send_final_event_with_retry(&shutdown_state, &final_heartbeat, &session.session_id)
            {
                warn!(
                    "Failed final heartbeat for {} on shutdown: {}",
                    session.session_id, err
                );
            }
        }

        shutdown_state.sessions.clear();
        info!("All sessions drained.");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .context("daemon server failed")?;

    Ok(())
}

// ---- 心跳 ----

/// 由扩展显式触发一次 heartbeat。
async fn session_heartbeat(
    State(state): State<AppState>,
    Json(req): Json<SessionHeartbeatRequest>,
) -> ApiResult<SessionResponse> {
    let session = state
        .sessions
        .get(&req.session_id)
        .map(|r| r.clone())
        .ok_or_else(|| {
            warn!("Heartbeat: unknown session_id={}", req.session_id);
            api_error(
                StatusCode::NOT_FOUND,
                format!("unknown session_id: {}", req.session_id),
            )
        })?;

    if let Err(err) = send_heartbeat_with_retry(&state, &session) {
        warn!("Heartbeat failed for session {}: {}", req.session_id, err);
        return Err(api_error(
            StatusCode::BAD_GATEWAY,
            format!("failed to write heartbeat: {}", err),
        ));
    }

    info!("Heartbeat sent for session {}", req.session_id);
    Ok(Json(SessionResponse {
        ok: true,
        session_id: req.session_id,
    }))
}

/// 对单个 session 发送一次 heartbeat（不重试，内部使用）
fn send_one_heartbeat(state: &AppState, session: &ActiveSession) -> Result<()> {
    let heartbeat = build_heartbeat_event(session, false);
    state
        .client
        .heartbeat(&state.bucket_id, &heartbeat, PULSETIME_SECS)
        .with_context(|| format!("heartbeat for session {}", session.session_id))
}

/// 带重试的 heartbeat 发送
fn send_heartbeat_with_retry(state: &AppState, session: &ActiveSession) -> Result<()> {
    let mut delay = Duration::from_millis(INITIAL_BACKOFF_MS);
    for attempt in 0..MAX_RETRIES {
        match send_one_heartbeat(state, session) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < MAX_RETRIES => {
                warn!(
                    "Heartbeat retry {}/{} for session {}: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    session.session_id,
                    e
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// 带重试的 AW 最终 event 写入
fn send_final_event_with_retry(state: &AppState, event: &Event, session_id: &str) -> Result<()> {
    let mut delay = Duration::from_millis(INITIAL_BACKOFF_MS);
    for attempt in 0..MAX_RETRIES {
        match state
            .client
            .heartbeat(&state.bucket_id, event, PULSETIME_SECS)
        {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < MAX_RETRIES => {
                warn!(
                    "Final event retry {}/{} for session {}: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    session_id,
                    e
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e.context(format!("final event for {}", session_id))),
        }
    }
    unreachable!()
}

/// 构建一次 heartbeat 的 AW Event。
///
/// `include_final_data`: false 表示心跳模式（稳定数据），
/// true 表示 session 结束时的最后一条 heartbeat（含 token/cost 汇总数据）。
fn build_heartbeat_event(session: &ActiveSession, include_final_data: bool) -> Event {
    let status = if include_final_data {
        "completed"
    } else {
        "active"
    };

    Event {
        id: None,
        timestamp: Utc::now(),
        duration: TimeDelta::zero(),
        data: session.to_aw_data(status, !include_final_data),
    }
}

// ---- HTTP handlers ----

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let active_sessions = state.sessions.len();
    Json(HealthResponse {
        ok: true,
        service: "aw-watcher-agent",
        bucket_id: state.bucket_id,
        active_sessions,
    })
}

async fn session_start(
    State(state): State<AppState>,
    Json(req): Json<SessionStartRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req
        .session_id
        .clone()
        .unwrap_or_else(|| format!("{}", Uuid::new_v4()));

    let code_agent = req.code_agent.clone();
    let project_dir = req.project_dir.clone();
    info!(
        "Session start: id={} agent={} project={}",
        session_id, code_agent, project_dir
    );

    let session = ActiveSession::from_start(req, session_id.clone());
    state.sessions.insert(session_id.clone(), session.clone());

    // 立即发第一次 heartbeat，让 AW 立即可见
    if let Err(err) = send_heartbeat_with_retry(&state, &session) {
        warn!(
            "Initial heartbeat failed for session {}: {}",
            session_id, err
        );
    }

    Ok(Json(SessionResponse {
        ok: true,
        session_id,
    }))
}

async fn session_update(
    State(state): State<AppState>,
    Json(req): Json<SessionUpdateRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req.session_id.clone();
    let mut session = state.sessions.get_mut(&session_id).ok_or_else(|| {
        warn!("Session update: unknown session_id={}", session_id);
        api_error(
            StatusCode::NOT_FOUND,
            format!("unknown session_id: {}", session_id),
        )
    })?;

    info!("Session update: id={}", session_id);
    session.apply_update(req);

    Ok(Json(SessionResponse {
        ok: true,
        session_id,
    }))
}

async fn session_end(
    State(state): State<AppState>,
    Json(req): Json<SessionEndRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req.session_id.clone();

    // 从活跃列表移除，拿到最终 session 状态
    let mut session = state
        .sessions
        .remove(&session_id)
        .map(|(_, v)| v)
        .ok_or_else(|| {
            warn!("Session end: unknown session_id={}", session_id);
            api_error(
                StatusCode::NOT_FOUND,
                format!("unknown session_id: {}", session_id),
            )
        })?;

    session.apply_end(&req);

    info!(
        "Session end: id={} duration={}s",
        session_id,
        Utc::now()
            .signed_duration_since(session.started_at)
            .num_seconds()
    );

    // 发送 final heartbeat（含 token/cost 汇总），带重试
    let final_heartbeat = build_heartbeat_event(&session, true);
    send_final_event_with_retry(&state, &final_heartbeat, &session_id).map_err(|err| {
        error!("Final heartbeat failed for session {}: {}", session_id, err);
        api_error(
            StatusCode::BAD_GATEWAY,
            format!("failed to write final ActivityWatch event: {}", err),
        )
    })?;

    Ok(Json(SessionResponse {
        ok: true,
        session_id,
    }))
}

fn api_error(status: StatusCode, error: String) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { ok: false, error }))
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}
