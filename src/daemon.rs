//! 后台 daemon HTTP ingest 服务。
//!
//! 各类 code agent 扩展通过 HTTP POST 会话事件到本 daemon。daemon 内部按
//! `(code_agent, session_id)` 维护单个会话，再将同一时间活跃的会话合并成
//! 一个 ActivityWatch heartbeat 写入 event bucket；单个会话结束或超时时，
//! 另写一条 summary event 到 sum bucket。

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use aw_models::Event;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::{
    sync::{mpsc, oneshot},
    task,
    time::{interval, sleep, timeout, MissedTickBehavior},
};
use tracing::{error, info, warn};

use crate::{
    buckets::BucketManager,
    client::WatcherClient,
    events::{
        ActiveSession, SessionEndRequest, SessionHeartbeatRequest, SessionKey, SessionStartRequest,
        SessionUpdateRequest,
    },
    shutdown_signal::wait_for_shutdown_signal,
};

/// AW heartbeat pulsetime：连续两次心跳之间的最大可接受间隔。
const PULSETIME_SECS: f64 = 25.0;
/// 会话最近一次活跃信号在该窗口内时，会进入合并后的实时状态。
const ACTIVE_TTL_SECS: i64 = 50;
/// 活跃过但长时间没有 end 的 session 会被写成 abandoned summary。
const ABANDON_TIMEOUT_SECS: i64 = 300;
/// abandoned session 扫描间隔。
const SWEEP_INTERVAL_SECS: u64 = 10;
/// HTTP 请求体最大大小。扩展只上报会话级结构化数据，但给 metadata 留出余量。
const BODY_LIMIT_BYTES: usize = 256 * 1024;
/// session actor 命令队列长度。
const SESSION_COMMAND_BUFFER: usize = 1024;
/// 同进程 summary 去重窗口，避免长期运行时无界增长。
const EMITTED_SUMMARY_ID_LIMIT: usize = 10_000;
/// /health 不应被 actor 内部慢写入长时间拖住。
const HEALTH_STATS_TIMEOUT_MS: u64 = 200;
/// abandoned summary 写入失败轮数上限；每轮内部仍受 MAX_RETRIES 约束。
const ABANDONED_SUMMARY_MAX_FAILURES: u32 = 10;

/// AW 写入最大重试次数。
const MAX_RETRIES: u32 = 3;
/// 初始退避延迟（毫秒）。
const INITIAL_BACKOFF_MS: u64 = 100;

#[derive(Clone)]
struct AppState {
    session_tx: mpsc::Sender<SessionCommand>,
    event_bucket_id: String,
    sum_bucket_id: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    service: &'static str,
    event_bucket_id: String,
    sum_bucket_id: String,
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

#[derive(Debug)]
struct CommandError {
    status: StatusCode,
    error: String,
}

type ApiError = (StatusCode, Json<ErrorResponse>);
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;
type CommandResult = std::result::Result<SessionResponse, CommandError>;

enum SessionCommand {
    Start(SessionStartRequest),
    Update(SessionUpdateRequest),
    Heartbeat(SessionHeartbeatRequest),
    End(SessionEndRequest, oneshot::Sender<CommandResult>),
    Sweep,
    Drain {
        reason: &'static str,
        done: oneshot::Sender<()>,
    },
    Stats(oneshot::Sender<usize>),
}

fn active_ttl() -> TimeDelta {
    TimeDelta::seconds(ACTIVE_TTL_SECS)
}

fn abandon_timeout() -> TimeDelta {
    TimeDelta::seconds(ABANDON_TIMEOUT_SECS)
}

/// 启动 daemon。
pub async fn run_daemon(
    client: WatcherClient,
    buckets: BucketManager,
    listen: SocketAddr,
) -> Result<()> {
    buckets.setup(&client)?;

    let client = Arc::new(client);
    let (session_tx, session_rx) = mpsc::channel(SESSION_COMMAND_BUFFER);
    let state = AppState {
        session_tx,
        event_bucket_id: buckets.event_bucket_id.clone(),
        sum_bucket_id: buckets.sum_bucket_id.clone(),
    };
    let actor = SessionActor::new(client, buckets.event_bucket_id, buckets.sum_bucket_id);
    let mut actor_task = tokio::spawn(actor.run(session_rx));

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/session/start", post(session_start))
        .route("/api/v1/session/update", post(session_update))
        .route("/api/v1/session/heartbeat", post(session_heartbeat))
        .route("/api/v1/session/end", post(session_end))
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("Failed to bind daemon on {}", listen))?;

    info!("aw-watcher-agent daemon listening on http://{}", listen);

    let sweeper = tokio::spawn(run_abandoned_session_sweeper(state.session_tx.clone()));

    // 优雅关闭：收到 SIGINT / SIGTERM 时将未结束但活跃过的 session 写成 abandoned。
    let shutdown_tx = state.session_tx.clone();
    let shutdown_signal = async move {
        let signal_name = wait_for_shutdown_signal().await;
        info!("{} received, draining active sessions...", signal_name);
        let (done, wait_done) = oneshot::channel();
        if shutdown_tx
            .send(SessionCommand::Drain {
                reason: "shutdown",
                done,
            })
            .await
            .is_err()
        {
            warn!("Session actor already stopped before shutdown drain");
            return;
        }
        let _ = wait_done.await;
        info!("All sessions drained.");
    };

    let server = async {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await
            .context("daemon server failed")
    };
    tokio::pin!(server);

    let mut actor_stopped_early = false;
    let result = tokio::select! {
        server_result = &mut server => server_result,
        actor_result = &mut actor_task => {
            actor_stopped_early = true;
            Err(actor_early_stop_error(actor_result))
        },
    };

    sweeper.abort();
    let _ = sweeper.await;
    drop(state);
    if !actor_stopped_early {
        log_actor_shutdown(actor_task.await);
    }
    result?;

    Ok(())
}

struct SessionActor {
    client: Arc<WatcherClient>,
    event_bucket_id: String,
    sum_bucket_id: String,
    sessions: HashMap<SessionKey, ActiveSession>,
    emitted_summary_ids: HashSet<String>,
    emitted_summary_id_order: VecDeque<String>,
}

fn actor_early_stop_error(result: std::result::Result<(), task::JoinError>) -> anyhow::Error {
    match result {
        Ok(()) => anyhow!("session actor stopped unexpectedly"),
        Err(err) if err.is_panic() => anyhow!("session actor panicked: {}", err),
        Err(err) => anyhow!("session actor task failed: {}", err),
    }
}

fn log_actor_shutdown(result: std::result::Result<(), task::JoinError>) {
    match result {
        Ok(()) => {}
        Err(err) if err.is_panic() => error!("Session actor panicked during shutdown: {}", err),
        Err(err) => error!("Session actor failed during shutdown: {}", err),
    }
}

impl SessionActor {
    fn new(client: Arc<WatcherClient>, event_bucket_id: String, sum_bucket_id: String) -> Self {
        Self {
            client,
            event_bucket_id,
            sum_bucket_id,
            sessions: HashMap::new(),
            emitted_summary_ids: HashSet::new(),
            emitted_summary_id_order: VecDeque::new(),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<SessionCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                SessionCommand::Start(req) => self.handle_start(req).await,
                SessionCommand::Update(req) => self.handle_update(req),
                SessionCommand::Heartbeat(req) => self.handle_heartbeat(req),
                SessionCommand::End(req, done) => {
                    let _ = done.send(self.handle_end(req).await);
                }
                SessionCommand::Sweep => self.handle_sweep().await,
                SessionCommand::Drain { reason, done } => {
                    self.drain_sessions_as_abandoned(reason).await;
                    let _ = done.send(());
                }
                SessionCommand::Stats(done) => {
                    let _ = done.send(self.sessions.len());
                }
            }
        }

        self.drain_sessions_as_abandoned("actor_stopped").await;
    }

    async fn handle_start(&mut self, req: SessionStartRequest) {
        let key = SessionKey::new(req.code_agent.clone(), req.session_id.clone());
        info!(
            "Session start queued: id={} agent={} project={}",
            key.session_id, key.code_agent, req.project_dir
        );

        if let Some(mut old_session) = self.sessions.remove(&key) {
            if old_session.has_been_active() {
                let ended_at = old_session.last_active_at.unwrap_or_else(Utc::now);
                if let Err(err) = self
                    .write_abandoned_summary(&mut old_session, ended_at, "duplicate_start")
                    .await
                {
                    warn!(
                        "Failed abandoned summary for duplicate start {}/{}: {}",
                        old_session.key.code_agent, old_session.key.session_id, err
                    );
                }
            }
        }

        let session = ActiveSession::from_start(req);
        self.sessions.insert(key, session);
    }

    fn handle_update(&mut self, req: SessionUpdateRequest) {
        let key = req.key();
        let now = Utc::now();
        let active_ttl = active_ttl();
        let mut should_emit_heartbeat = false;

        if let Some(session) = self.sessions.get_mut(&key) {
            let was_active = session.is_active_at(now, active_ttl);
            if was_active {
                session.mark_active(now, active_ttl);
                should_emit_heartbeat = true;
            }
            session.apply_update(req);
            info!(
                "Session update merged: {}/{}",
                key.code_agent, key.session_id
            );
        } else {
            warn!(
                "Session update ignored for unknown session={}/{}",
                key.code_agent, key.session_id
            );
        }

        if should_emit_heartbeat {
            self.spawn_aggregate_heartbeat(None);
        }
    }

    fn handle_heartbeat(&mut self, req: SessionHeartbeatRequest) {
        let key = req.key();
        if let Some(session) = self.sessions.get_mut(&key) {
            session.mark_active(Utc::now(), active_ttl());
            info!(
                "Heartbeat merged for session {}/{}",
                key.code_agent, key.session_id
            );
            self.spawn_aggregate_heartbeat(None);
        } else {
            warn!(
                "Heartbeat ignored for unknown session={}/{}",
                key.code_agent, key.session_id
            );
        }
    }

    async fn handle_end(&mut self, req: SessionEndRequest) -> CommandResult {
        let key = req.key();
        let session_id = key.session_id.clone();
        let ended_at = req.ended_at.unwrap_or_else(Utc::now);
        let active_ttl = active_ttl();

        let Some(current) = self.sessions.get(&key) else {
            warn!(
                "Session end: unknown session={}/{}",
                key.code_agent, key.session_id
            );
            return Err(CommandError {
                status: StatusCode::NOT_FOUND,
                error: format!("unknown session: {}/{}", key.code_agent, key.session_id),
            });
        };

        let mut session = current.clone();
        let was_active = session.is_active_at(ended_at, active_ttl);
        if was_active {
            session.mark_active(ended_at, active_ttl);
        }
        session.apply_end(req);

        if was_active {
            self.spawn_aggregate_heartbeat(Some(&session));
        }

        info!(
            "Session end: {}/{} active_duration={}s wall_duration={}s",
            session.key.code_agent,
            session.key.session_id,
            session.active_duration().num_seconds(),
            session.wall_duration_until(ended_at).num_seconds()
        );

        let key_for_log = session.key.clone();
        let summary = session.summary_event("completed", ended_at, None);
        self.write_summary_event_once(&summary, &key_for_log)
            .await
            .map_err(|err| {
                error!(
                    "Summary event failed for {}/{}: {}",
                    key_for_log.code_agent, key_for_log.session_id, err
                );
                CommandError {
                    status: StatusCode::BAD_GATEWAY,
                    error: format!("failed to write summary ActivityWatch event: {}", err),
                }
            })?;

        self.sessions.remove(&key);
        Ok(SessionResponse::ok(session_id))
    }

    async fn handle_sweep(&mut self) {
        let now = Utc::now();
        let timeout = abandon_timeout();
        let keys: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter_map(|(key, session)| match session.last_active_at.as_ref() {
                Some(last_active_at) if now.signed_duration_since(*last_active_at) > timeout => {
                    Some(key.clone())
                }
                None if now.signed_duration_since(session.started_at) > timeout => {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect();

        for key in keys {
            if let Some(mut session) = self.sessions.remove(&key) {
                if !session.has_been_active() {
                    warn!(
                        "Dropping inactive timed-out session {}/{}",
                        session.key.code_agent, session.key.session_id
                    );
                    continue;
                }

                let ended_at = session.last_active_at.unwrap_or(now);
                if let Err(err) = self
                    .write_abandoned_summary(&mut session, ended_at, "timeout")
                    .await
                {
                    let failures = session.record_abandoned_summary_failure();
                    if failures >= ABANDONED_SUMMARY_MAX_FAILURES {
                        warn!(
                            "Dropping abandoned session {}/{} after {} failed summary write attempts: {}",
                            session.key.code_agent, session.key.session_id, failures, err
                        );
                    } else {
                        warn!(
                            "Failed abandoned summary for {}/{} ({}/{}): {}",
                            session.key.code_agent,
                            session.key.session_id,
                            failures,
                            ABANDONED_SUMMARY_MAX_FAILURES,
                            err
                        );
                        self.sessions.insert(key, session);
                    }
                }
            }
        }
    }

    async fn drain_sessions_as_abandoned(&mut self, reason: &'static str) {
        let now = Utc::now();
        let keys: Vec<SessionKey> = self.sessions.keys().cloned().collect();

        for key in keys {
            if let Some(mut session) = self.sessions.remove(&key) {
                if !session.has_been_active() {
                    continue;
                }
                let ended_at = session.last_active_at.unwrap_or(now);
                if let Err(err) = self
                    .write_abandoned_summary(&mut session, ended_at, reason)
                    .await
                {
                    warn!(
                        "Failed abandoned summary for {}/{}: {}",
                        session.key.code_agent, session.key.session_id, err
                    );
                }
            }
        }
    }

    async fn write_abandoned_summary(
        &mut self,
        session: &mut ActiveSession,
        ended_at: DateTime<Utc>,
        reason: &'static str,
    ) -> Result<()> {
        let key = session.key.clone();
        let summary = session.summary_event("abandoned", ended_at, Some(reason));
        self.write_summary_event_once(&summary, &key).await
    }

    async fn write_summary_event_once(&mut self, event: &Event, key: &SessionKey) -> Result<()> {
        let summary_id = summary_id_from_event(event);
        if summary_id
            .as_ref()
            .is_some_and(|id| self.emitted_summary_ids.contains(id))
        {
            info!(
                "Skipping duplicate summary event for {}/{}",
                key.code_agent, key.session_id
            );
            return Ok(());
        }

        send_summary_event_with_retry(
            Arc::clone(&self.client),
            self.sum_bucket_id.clone(),
            event.clone(),
            key,
        )
        .await?;

        if let Some(summary_id) = summary_id {
            self.remember_summary_id(summary_id);
        }
        Ok(())
    }

    fn remember_summary_id(&mut self, summary_id: String) {
        if !self.emitted_summary_ids.insert(summary_id.clone()) {
            return;
        }

        self.emitted_summary_id_order.push_back(summary_id);
        while self.emitted_summary_id_order.len() > EMITTED_SUMMARY_ID_LIMIT {
            if let Some(expired) = self.emitted_summary_id_order.pop_front() {
                self.emitted_summary_ids.remove(&expired);
            }
        }
    }

    fn spawn_aggregate_heartbeat(&self, override_session: Option<&ActiveSession>) {
        let Some(event) = self.build_aggregate_heartbeat_event(override_session) else {
            return;
        };
        let client = Arc::clone(&self.client);
        let bucket_id = self.event_bucket_id.clone();

        tokio::spawn(async move {
            if let Err(err) = send_aggregate_heartbeat_with_retry(client, bucket_id, event).await {
                warn!("Aggregate heartbeat failed: {}", err);
            }
        });
    }

    fn build_aggregate_heartbeat_event(
        &self,
        override_session: Option<&ActiveSession>,
    ) -> Option<Event> {
        let now = Utc::now();
        let active_ttl = active_ttl();
        let mut sessions: Vec<&ActiveSession> = self
            .sessions
            .values()
            .filter_map(|session| {
                let session = match override_session {
                    Some(override_session) if override_session.key == session.key => {
                        override_session
                    }
                    _ => session,
                };
                session.is_active_at(now, active_ttl).then_some(session)
            })
            .collect();

        if sessions.is_empty() {
            return None;
        }

        sessions.sort_by(|a, b| {
            a.key
                .code_agent
                .cmp(&b.key.code_agent)
                .then(a.key.session_id.cmp(&b.key.session_id))
        });

        Some(Event {
            id: None,
            timestamp: now,
            duration: TimeDelta::zero(),
            data: aggregate_to_aw_data(&sessions),
        })
    }
}

impl SessionResponse {
    fn ok(session_id: String) -> Self {
        Self {
            ok: true,
            session_id,
        }
    }
}

// ---- 心跳与 summary 写入 ----

async fn run_abandoned_session_sweeper(session_tx: mpsc::Sender<SessionCommand>) {
    let mut ticker = interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if session_tx.send(SessionCommand::Sweep).await.is_err() {
            break;
        }
    }
}

async fn send_aggregate_heartbeat_with_retry(
    client: Arc<WatcherClient>,
    bucket_id: String,
    event: Event,
) -> Result<()> {
    let mut delay = Duration::from_millis(INITIAL_BACKOFF_MS);
    for attempt in 1..=MAX_RETRIES {
        let result = {
            let client = Arc::clone(&client);
            let bucket_id = bucket_id.clone();
            let event = event.clone();
            task::spawn_blocking(move || client.heartbeat(&bucket_id, &event, PULSETIME_SECS))
                .await
                .context("aggregate heartbeat task join")?
        };

        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_RETRIES => {
                warn!(
                    "Aggregate heartbeat retry {}/{}: {}",
                    attempt, MAX_RETRIES, e
                );
                sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e.context("aggregate heartbeat")),
        }
    }

    Err(anyhow!("aggregate heartbeat retry loop exhausted"))
}

async fn send_summary_event_with_retry(
    client: Arc<WatcherClient>,
    bucket_id: String,
    event: Event,
    key: &SessionKey,
) -> Result<()> {
    let mut delay = Duration::from_millis(INITIAL_BACKOFF_MS);
    for attempt in 1..=MAX_RETRIES {
        let result = {
            let client = Arc::clone(&client);
            let bucket_id = bucket_id.clone();
            let event = event.clone();
            task::spawn_blocking(move || client.insert_event(&bucket_id, &event))
                .await
                .context("summary event task join")?
        };

        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_RETRIES => {
                warn!(
                    "Summary event retry {}/{} for {}/{}: {}",
                    attempt, MAX_RETRIES, key.code_agent, key.session_id, e
                );
                sleep(delay).await;
                delay *= 2;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "summary event for {}/{}",
                    key.code_agent, key.session_id
                )))
            }
        }
    }

    Err(anyhow!(
        "summary event retry loop exhausted for {}/{}",
        key.code_agent,
        key.session_id
    ))
}

fn aggregate_to_aw_data(sessions: &[&ActiveSession]) -> Map<String, Value> {
    let mut code_agents = BTreeSet::new();
    let mut projects = BTreeSet::new();
    let mut project_dirs = BTreeSet::new();
    let mut models = BTreeSet::new();

    for session in sessions.iter().copied() {
        code_agents.insert(session.key.code_agent.clone());
        projects.insert(session.project_name.clone());
        project_dirs.insert(session.project_dir.clone());
        if let Some(model) = &session.model {
            models.insert(model.clone());
        }
    }

    let mut data = Map::new();
    data.insert("status".into(), Value::String("active".into()));
    data.insert("language".into(), Value::String("code-agent".into()));
    data.insert(
        "project".into(),
        Value::String(single_or_multiple(&projects)),
    );
    data.insert(
        "file".into(),
        Value::String(single_or_multiple(&project_dirs)),
    );
    data.insert(
        "active_session_count".into(),
        Value::from(sessions.len() as u64),
    );
    data.insert(
        "code_agents".into(),
        Value::Array(code_agents.into_iter().map(Value::String).collect()),
    );
    data.insert(
        "projects".into(),
        Value::Array(projects.into_iter().map(Value::String).collect()),
    );
    data.insert(
        "project_dirs".into(),
        Value::Array(project_dirs.into_iter().map(Value::String).collect()),
    );
    if !models.is_empty() {
        data.insert(
            "models".into(),
            Value::Array(models.into_iter().map(Value::String).collect()),
        );
    }

    let session_values = sessions
        .iter()
        .copied()
        .map(|session| {
            let mut item = Map::new();
            item.insert(
                "code_agent".into(),
                Value::String(session.key.code_agent.clone()),
            );
            item.insert(
                "session_id".into(),
                Value::String(session.key.session_id.clone()),
            );
            item.insert(
                "project".into(),
                Value::String(session.project_name.clone()),
            );
            item.insert(
                "project_dir".into(),
                Value::String(session.project_dir.clone()),
            );
            if let Some(model) = &session.model {
                item.insert("model".into(), Value::String(model.clone()));
            }
            Value::Object(item)
        })
        .collect();
    data.insert("sessions".into(), Value::Array(session_values));

    data
}

fn single_or_multiple(values: &BTreeSet<String>) -> String {
    if values.len() == 1 {
        values.iter().next().cloned().unwrap_or_default()
    } else {
        "multiple".to_string()
    }
}

fn summary_id_from_event(event: &Event) -> Option<String> {
    event
        .data
        .get("summary_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

// ---- HTTP handlers ----

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let active_sessions = match timeout(
        Duration::from_millis(HEALTH_STATS_TIMEOUT_MS),
        actor_session_count(&state),
    )
    .await
    {
        Ok(Some(count)) => count,
        Ok(None) => {
            warn!("Session actor unavailable while serving /health");
            0
        }
        Err(_) => {
            warn!("Session actor stats timed out while serving /health");
            0
        }
    };

    Json(HealthResponse {
        ok: true,
        service: "aw-watcher-agent",
        event_bucket_id: state.event_bucket_id,
        sum_bucket_id: state.sum_bucket_id,
        active_sessions,
    })
}

async fn session_start(
    State(state): State<AppState>,
    Json(req): Json<SessionStartRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req.session_id.clone();
    enqueue_session_command(&state, SessionCommand::Start(req)).await?;

    Ok(Json(SessionResponse::ok(session_id)))
}

async fn session_update(
    State(state): State<AppState>,
    Json(req): Json<SessionUpdateRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req.session_id.clone();
    enqueue_session_command(&state, SessionCommand::Update(req)).await?;

    Ok(Json(SessionResponse::ok(session_id)))
}

/// 由扩展显式触发一次 session 活跃信号。
async fn session_heartbeat(
    State(state): State<AppState>,
    Json(req): Json<SessionHeartbeatRequest>,
) -> ApiResult<SessionResponse> {
    let session_id = req.session_id.clone();
    enqueue_session_command(&state, SessionCommand::Heartbeat(req)).await?;

    Ok(Json(SessionResponse::ok(session_id)))
}

async fn session_end(
    State(state): State<AppState>,
    Json(req): Json<SessionEndRequest>,
) -> ApiResult<SessionResponse> {
    let (done, wait_done) = oneshot::channel();
    enqueue_session_command(&state, SessionCommand::End(req, done)).await?;

    match wait_done.await {
        Ok(Ok(response)) => Ok(Json(response)),
        Ok(Err(err)) => Err(api_error(err.status, err.error)),
        Err(_) => Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session actor stopped before completing end request".to_string(),
        )),
    }
}

async fn enqueue_session_command(
    state: &AppState,
    command: SessionCommand,
) -> std::result::Result<(), ApiError> {
    state.session_tx.send(command).await.map_err(|_| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session actor unavailable".to_string(),
        )
    })
}

async fn actor_session_count(state: &AppState) -> Option<usize> {
    let (done, wait_done) = oneshot::channel();
    state
        .session_tx
        .send(SessionCommand::Stats(done))
        .await
        .ok()?;
    wait_done.await.ok()
}

fn api_error(status: StatusCode, error: String) -> ApiError {
    (status, Json(ErrorResponse { ok: false, error }))
}
