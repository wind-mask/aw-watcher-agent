//! code agent 会话事件模型。
//!
//! 这些结构用于 daemon HTTP ingest 协议。daemon 内部按
//! `(code_agent, session_id, session_instance_id)` 区分一次运行实例；实时活动写入
//! event bucket，结束/超时汇总写入 sum bucket。
use aw_models::Event;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use ts_rs::TS;

/// token 使用量。字段全部可选/可累加，方便不同 agent 只上报自己能拿到的数据。
#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub total: Option<u64>,
}

impl TokenUsage {
    pub fn total_or_sum(&self) -> Option<u64> {
        self.total.or_else(|| {
            let sum = self
                .input
                .unwrap_or(0)
                .saturating_add(self.output.unwrap_or(0))
                .saturating_add(self.cache_read.unwrap_or(0))
                .saturating_add(self.cache_write.unwrap_or(0));
            (sum > 0).then_some(sum)
        })
    }

    fn merge_max(&mut self, incoming: Self) {
        merge_optional_max(&mut self.input, incoming.input);
        merge_optional_max(&mut self.output, incoming.output);
        merge_optional_max(&mut self.cache_read, incoming.cache_read);
        merge_optional_max(&mut self.cache_write, incoming.cache_write);
        merge_optional_max(&mut self.total, incoming.total);
    }
}

/// 费用信息。不同 provider 的费用模型差异较大，因此只固定 total/currency。
#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct CostUsage {
    pub total: Option<f64>,
    pub currency: Option<String>,
}

/// 按模型拆分的用量汇总。
#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: TokenUsage,
    pub cost: f64,
}

/// daemon 内部 session key。不同 agent 可复用相同 session_id。
///
/// `session_instance_id` 用于区分同一个 Pi session 在 reload/resume 或重复启动时
/// 产生的不同运行实例；旧客户端不提供时保留 `None` 以兼容旧协议。
#[derive(Debug, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionKey {
    pub code_agent: String,
    pub session_id: String,
    pub session_instance_id: Option<String>,
}

impl SessionKey {
    pub fn new(
        code_agent: String,
        session_id: String,
        session_instance_id: Option<String>,
    ) -> Self {
        Self {
            code_agent,
            session_id,
            session_instance_id,
        }
    }
}

/// 会话开始请求。
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SessionStartRequest {
    pub session_id: String,
    pub session_instance_id: Option<String>,
    pub code_agent: String,
    pub project_dir: String,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
}

/// 会话更新请求。可携带累计 usage 快照和活动状态。
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SessionUpdateRequest {
    pub session_id: String,
    pub session_instance_id: Option<String>,
    pub code_agent: String,
    pub model: Option<String>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub model_usage: Option<Vec<ModelUsage>>,
    /// `Some(false)` 表示在 `active_at` 时刻结束当前活动片段。
    pub active: Option<bool>,
    pub active_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
}

impl SessionStartRequest {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(
            self.code_agent.clone(),
            self.session_id.clone(),
            self.session_instance_id.clone(),
        )
    }
}

impl SessionUpdateRequest {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(
            self.code_agent.clone(),
            self.session_id.clone(),
            self.session_instance_id.clone(),
        )
    }
}

/// 会话结束请求。usage 字段应为整个 session 的最终汇总。
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SessionEndRequest {
    pub session_id: String,
    pub session_instance_id: Option<String>,
    pub code_agent: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub model_usage: Option<Vec<ModelUsage>>,
    pub metadata: Option<Value>,
}

impl SessionEndRequest {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(
            self.code_agent.clone(),
            self.session_id.clone(),
            self.session_instance_id.clone(),
        )
    }
}

/// 会话心跳请求。由扩展周期性发送，表示该 session 仍然活跃。
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SessionHeartbeatRequest {
    pub session_id: String,
    pub session_instance_id: Option<String>,
    pub code_agent: String,
    /// heartbeat 在 agent 侧的采样时间；旧客户端缺失时回退到 daemon 接收时间。
    pub heartbeat_at: Option<DateTime<Utc>>,
}

impl SessionHeartbeatRequest {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(
            self.code_agent.clone(),
            self.session_id.clone(),
            self.session_instance_id.clone(),
        )
    }
}

/// daemon 内部维护的活动会话状态。
#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub key: SessionKey,
    pub project_dir: String,
    pub project_name: String,
    pub model: Option<String>,
    pub tokens: TokenUsage,
    pub cost: CostUsage,
    pub model_usage: HashMap<String, ModelUsage>,
    pub started_at: DateTime<Utc>,
    pub first_active_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    active_started_at: Option<DateTime<Utc>>,
    active_duration: TimeDelta,
    abandoned_summary_failures: u32,
    pub metadata: Option<Value>,
}

impl ActiveSession {
    pub fn from_start(req: SessionStartRequest) -> Self {
        let project_name = project_name_from_dir(&req.project_dir);

        Self {
            key: req.key(),
            project_dir: req.project_dir,
            project_name,
            model: req.model,
            tokens: TokenUsage::default(),
            cost: CostUsage::default(),
            model_usage: HashMap::new(),
            started_at: req.started_at.unwrap_or_else(Utc::now),
            first_active_at: None,
            last_active_at: None,
            active_started_at: None,
            active_duration: TimeDelta::zero(),
            abandoned_summary_failures: 0,
            metadata: req.metadata,
        }
    }

    /// 应用重复 start 中的非身份字段；带实例 ID 的 start 具备幂等性。
    pub fn apply_start(&mut self, req: SessionStartRequest) {
        if let Some(new_model) = req.model {
            self.model = Some(new_model);
        }
        merge_metadata(&mut self.metadata, req.metadata);
    }

    pub fn apply_update(&mut self, req: SessionUpdateRequest) {
        if let Some(new_model) = req.model {
            self.model = Some(new_model);
        }
        self.apply_usage(req.tokens, req.cost, req.model_usage);
        merge_metadata(&mut self.metadata, req.metadata);
    }

    pub fn apply_end(&mut self, req: SessionEndRequest) {
        self.apply_usage(req.tokens, req.cost, req.model_usage);
        merge_metadata(&mut self.metadata, req.metadata);
    }

    fn apply_usage(
        &mut self,
        tokens: Option<TokenUsage>,
        cost: Option<CostUsage>,
        model_usage: Option<Vec<ModelUsage>>,
    ) {
        let Some(model_usage) = model_usage else {
            // 新协议的 model_usage 是累计 usage 的权威来源；没有拆分数据时，
            // 仅兼容旧客户端的顶层快照。
            if self.model_usage.is_empty() {
                if let Some(tokens) = tokens {
                    self.tokens.merge_max(tokens);
                }
                if let Some(cost) = cost {
                    merge_cost_usage(&mut self.cost, cost);
                }
            }
            return;
        };

        let mut candidate = self.model_usage.clone();
        for incoming in model_usage {
            if let Some(existing) = candidate.get_mut(&incoming.model) {
                existing.tokens.merge_max(incoming.tokens);
                existing.cost = existing.cost.max(incoming.cost);
            } else {
                candidate.insert(incoming.model.clone(), incoming);
            }
        }

        let (candidate_tokens, candidate_cost) = aggregate_model_usage(&candidate);
        let mut baseline_tokens = self.tokens.clone();
        if let Some(tokens) = tokens {
            baseline_tokens.merge_max(tokens);
        }
        let mut baseline_cost = self.cost.clone();
        if let Some(cost) = cost {
            merge_cost_usage(&mut baseline_cost, cost);
        }

        // model_usage 必须覆盖当前及本次上报的顶层累计值；否则它是部分快照，
        // 不能用来覆盖已有数据，避免 token/cost 回退或顶层与拆分不一致。
        if !usage_covers(
            &candidate_tokens,
            candidate_cost,
            &baseline_tokens,
            baseline_cost.total,
        ) {
            return;
        }

        self.model_usage = candidate;
        self.tokens = candidate_tokens;
        self.cost.total = Some(candidate_cost);
        if baseline_cost.currency.is_some() {
            self.cost.currency = baseline_cost.currency;
        }
    }

    /// 记录一次活跃信号。若两次活跃信号间隔超过 TTL，则视为新的活跃片段。
    pub fn mark_active(&mut self, at: DateTime<Utc>, active_ttl: TimeDelta) {
        let at = self.last_active_at.map_or(at, |last| at.max(last));

        let should_start_period = self.active_started_at.is_none()
            || self
                .last_active_at
                .is_some_and(|last| at.signed_duration_since(last) > active_ttl);
        if should_start_period {
            if self.active_started_at.is_some() {
                if let Some(last) = self.last_active_at {
                    self.close_active_period_at(last);
                }
            }
            self.active_started_at = Some(at);
            self.first_active_at.get_or_insert(at);
        }

        self.last_active_at = Some(at);
    }

    pub fn is_active_at(&self, now: DateTime<Utc>, active_ttl: TimeDelta) -> bool {
        self.active_started_at.is_some()
            && self
                .last_active_at
                .is_some_and(|last| now.signed_duration_since(last) <= active_ttl)
    }

    pub fn has_been_active(&self) -> bool {
        self.first_active_at.is_some()
    }

    /// 结束当前活动片段，并将活动尾端延伸到指定时刻。
    pub fn finish_active_period_at(&mut self, at: DateTime<Utc>) {
        if self.active_started_at.is_none() {
            return;
        }

        let end = self.last_active_at.map_or(at, |last| at.max(last));
        self.close_active_period_at(end);
        self.last_active_at = Some(end);
    }

    pub fn finish_active_period(&mut self) {
        if let Some(last) = self.last_active_at {
            self.finish_active_period_at(last);
        }
    }

    pub fn active_duration(&self) -> TimeDelta {
        let mut duration = self.active_duration;
        if let (Some(start), Some(last)) = (&self.active_started_at, &self.last_active_at) {
            let current = last.signed_duration_since(*start);
            if current > TimeDelta::zero() {
                duration += current;
            }
        }
        duration
    }

    pub fn wall_duration_until(&self, ended_at: DateTime<Utc>) -> TimeDelta {
        ended_at.signed_duration_since(self.started_at)
    }

    pub fn record_abandoned_summary_failure(&mut self) -> u32 {
        self.abandoned_summary_failures = self.abandoned_summary_failures.saturating_add(1);
        self.abandoned_summary_failures
    }

    pub fn summary_event(
        &mut self,
        status: &str,
        ended_at: DateTime<Utc>,
        reason: Option<&str>,
    ) -> Event {
        self.finish_active_period();
        let active_duration = self.active_duration();
        let mut data = self.to_summary_data(status, ended_at, reason);
        data.insert(
            "active_duration_seconds".into(),
            Value::from(duration_seconds(active_duration)),
        );
        data.insert(
            "wall_duration_seconds".into(),
            Value::from(duration_seconds(self.wall_duration_until(ended_at))),
        );

        Event {
            id: None,
            timestamp: self.first_active_at.unwrap_or(self.started_at),
            duration: active_duration,
            data,
        }
    }

    /// 转为 sum bucket 的 ActivityWatch data 字段。
    pub fn to_summary_data(
        &self,
        status: &str,
        ended_at: DateTime<Utc>,
        reason: Option<&str>,
    ) -> Map<String, Value> {
        let mut data = Map::new();

        // ActivityWatch 常用字段。
        data.insert("project".into(), Value::String(self.project_name.clone()));
        data.insert("file".into(), Value::String(self.project_dir.clone()));
        data.insert("language".into(), Value::String("code-agent".into()));

        // 通用 code agent 扩展字段。
        data.insert("status".into(), Value::String(status.to_string()));
        if let Some(reason) = reason {
            data.insert("reason".into(), Value::String(reason.to_string()));
        }
        data.insert(
            "summary_id".into(),
            Value::String(self.summary_id(status, ended_at, reason)),
        );
        data.insert(
            "session_id".into(),
            Value::String(self.key.session_id.clone()),
        );
        if let Some(instance_id) = &self.key.session_instance_id {
            data.insert(
                "session_instance_id".into(),
                Value::String(instance_id.clone()),
            );
        }
        data.insert(
            "code_agent".into(),
            Value::String(self.key.code_agent.clone()),
        );
        data.insert(
            "project_dir".into(),
            Value::String(self.project_dir.clone()),
        );
        data.insert(
            "started_at".into(),
            Value::String(self.started_at.to_rfc3339()),
        );
        data.insert("ended_at".into(), Value::String(ended_at.to_rfc3339()));
        if let Some(value) = &self.first_active_at {
            data.insert("first_active_at".into(), Value::String(value.to_rfc3339()));
        }
        if let Some(value) = &self.last_active_at {
            data.insert("last_active_at".into(), Value::String(value.to_rfc3339()));
        }

        insert_token_usage(&mut data, &self.tokens);
        if let Some(value) = self.cost.total {
            data.insert("cost_total".into(), Value::from(value));
        }
        if let Some(value) = &self.cost.currency {
            data.insert("cost_currency".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.metadata {
            data.insert("metadata".into(), value.clone());
        }
        if let Some(selected_model) = &self.model {
            data.insert(
                "selected_model".into(),
                Value::String(selected_model.clone()),
            );
        }
        if let Some(model) = self.summary_model() {
            data.insert("model".into(), Value::String(model.to_string()));
        }

        if !self.model_usage.is_empty() {
            let per_model: Map<String, Value> = self
                .model_usage
                .iter()
                .map(|(model, mu)| {
                    let mut m = Map::new();
                    insert_token_usage(&mut m, &mu.tokens);
                    m.insert("cost".into(), Value::from(mu.cost));
                    (model.clone(), Value::Object(m))
                })
                .collect();
            data.insert("model_usage".into(), Value::Object(per_model));
        }

        data
    }

    fn close_active_period_at(&mut self, end: DateTime<Utc>) {
        if let Some(start) = self.active_started_at.take() {
            let duration = end.signed_duration_since(start);
            if duration > TimeDelta::zero() {
                self.active_duration += duration;
            }
        }
    }

    fn summary_id(&self, status: &str, ended_at: DateTime<Utc>, reason: Option<&str>) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.key.code_agent,
            self.key.session_id,
            self.key.session_instance_id.as_deref().unwrap_or("legacy"),
            status,
            reason.unwrap_or("none"),
            self.started_at.to_rfc3339(),
            ended_at.to_rfc3339()
        )
    }

    fn summary_model(&self) -> Option<&str> {
        match self.model_usage.len() {
            0 => self.model.as_deref(),
            1 => self.model_usage.keys().next().map(String::as_str),
            _ => Some("multiple"),
        }
    }
}

fn merge_optional_max(target: &mut Option<u64>, incoming: Option<u64>) {
    if let Some(incoming) = incoming {
        *target = Some(target.map_or(incoming, |current| current.max(incoming)));
    }
}

fn add_optional(target: &mut Option<u64>, incoming: Option<u64>) {
    if let Some(incoming) = incoming {
        *target = Some(target.unwrap_or(0).saturating_add(incoming));
    }
}

fn merge_cost_usage(target: &mut CostUsage, incoming: CostUsage) {
    if let Some(incoming_total) = incoming.total {
        target.total = Some(
            target
                .total
                .map_or(incoming_total, |current| current.max(incoming_total)),
        );
    }
    if incoming.currency.is_some() {
        target.currency = incoming.currency;
    }
}

fn aggregate_model_usage(model_usage: &HashMap<String, ModelUsage>) -> (TokenUsage, f64) {
    let mut tokens = TokenUsage::default();
    let mut cost = 0.0;
    for usage in model_usage.values() {
        add_optional(&mut tokens.input, usage.tokens.input);
        add_optional(&mut tokens.output, usage.tokens.output);
        add_optional(&mut tokens.cache_read, usage.tokens.cache_read);
        add_optional(&mut tokens.cache_write, usage.tokens.cache_write);
        add_optional(&mut tokens.total, usage.tokens.total_or_sum());
        cost += usage.cost;
    }
    (tokens, cost)
}

fn usage_covers(
    candidate: &TokenUsage,
    candidate_cost: f64,
    baseline: &TokenUsage,
    baseline_cost: Option<f64>,
) -> bool {
    optional_at_least(candidate.input, baseline.input)
        && optional_at_least(candidate.output, baseline.output)
        && optional_at_least(candidate.cache_read, baseline.cache_read)
        && optional_at_least(candidate.cache_write, baseline.cache_write)
        && optional_at_least(candidate.total_or_sum(), baseline.total_or_sum())
        && candidate_cost >= baseline_cost.unwrap_or(0.0)
}

fn optional_at_least(candidate: Option<u64>, baseline: Option<u64>) -> bool {
    match (candidate, baseline) {
        (_, None) => true,
        (Some(candidate), Some(baseline)) => candidate >= baseline,
        (None, Some(_)) => false,
    }
}

fn merge_metadata(target: &mut Option<Value>, incoming: Option<Value>) {
    let Some(incoming) = incoming else {
        return;
    };

    match incoming {
        Value::Object(incoming) => match target {
            Some(Value::Object(existing)) => {
                existing.extend(incoming);
            }
            _ => {
                *target = Some(Value::Object(incoming));
            }
        },
        value => {
            *target = Some(value);
        }
    }
}

fn project_name_from_dir(project_dir: &str) -> String {
    let project_dir = project_dir.trim_end_matches(['/', '\\']);
    if project_dir.is_empty() {
        return "root".to_string();
    }

    std::path::Path::new(project_dir)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(project_dir)
        .to_string()
}

fn insert_token_usage(data: &mut Map<String, Value>, usage: &TokenUsage) {
    if let Some(value) = usage.input {
        data.insert("tokens_input".into(), Value::from(value));
    }
    if let Some(value) = usage.output {
        data.insert("tokens_output".into(), Value::from(value));
    }
    if let Some(value) = usage.cache_read {
        data.insert("tokens_cache_read".into(), Value::from(value));
    }
    if let Some(value) = usage.cache_write {
        data.insert("tokens_cache_write".into(), Value::from(value));
    }
    if let Some(value) = usage.total_or_sum() {
        data.insert("tokens_total".into(), Value::from(value));
    }
}

fn duration_seconds(duration: TimeDelta) -> f64 {
    duration
        .num_nanoseconds()
        .map(|nanos| nanos as f64 / 1_000_000_000.0)
        .unwrap_or_else(|| duration.num_milliseconds() as f64 / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn ts(offset_secs: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 21, 0, 0, 0).unwrap() + TimeDelta::seconds(offset_secs)
    }

    fn start_req() -> SessionStartRequest {
        SessionStartRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            project_dir: "/tmp/example-project".to_string(),
            model: Some("gpt-5".to_string()),
            started_at: Some(ts(0)),
            metadata: Some(json!({ "source": "test" })),
        }
    }

    #[test]
    fn token_total_uses_explicit_total_before_summing_parts() {
        let explicit = TokenUsage {
            input: Some(10),
            output: Some(5),
            total: Some(99),
            ..Default::default()
        };
        assert_eq!(explicit.total_or_sum(), Some(99));

        let summed = TokenUsage {
            input: Some(10),
            output: Some(5),
            cache_read: Some(3),
            ..Default::default()
        };
        assert_eq!(summed.total_or_sum(), Some(18));

        assert_eq!(TokenUsage::default().total_or_sum(), None);
    }

    #[test]
    fn mark_active_accumulates_contiguous_periods_and_ignores_clock_regression() {
        let mut session = ActiveSession::from_start(start_req());
        let ttl = TimeDelta::seconds(15);

        session.mark_active(ts(10), ttl);
        assert_eq!(session.first_active_at, Some(ts(10)));
        assert_eq!(session.active_duration(), TimeDelta::zero());

        session.mark_active(ts(20), ttl);
        assert_eq!(session.active_duration(), TimeDelta::seconds(10));

        session.mark_active(ts(50), ttl);
        assert_eq!(session.active_duration(), TimeDelta::seconds(10));

        session.mark_active(ts(60), ttl);
        assert_eq!(session.active_duration(), TimeDelta::seconds(20));

        session.mark_active(ts(55), ttl);
        assert_eq!(session.last_active_at, Some(ts(60)));
        assert_eq!(session.active_duration(), TimeDelta::seconds(20));

        session.finish_active_period();
        assert_eq!(session.active_duration(), TimeDelta::seconds(20));
    }

    #[test]
    fn merge_metadata_merges_objects_and_replaces_other_values() {
        let mut target = Some(json!({
            "source": "start",
            "nested": { "kept": true }
        }));

        merge_metadata(
            &mut target,
            Some(json!({
                "source": "end",
                "tokens": 42
            })),
        );
        assert_eq!(
            target,
            Some(json!({
                "source": "end",
                "nested": { "kept": true },
                "tokens": 42
            }))
        );

        merge_metadata(&mut target, Some(json!(["replace"])));
        assert_eq!(target, Some(json!(["replace"])));

        merge_metadata(&mut target, None);
        assert_eq!(target, Some(json!(["replace"])));
    }

    #[test]
    fn abandoned_summary_failures_are_counted() {
        let mut session = ActiveSession::from_start(start_req());

        assert_eq!(session.record_abandoned_summary_failure(), 1);
        assert_eq!(session.record_abandoned_summary_failure(), 2);
    }

    #[test]
    fn summary_event_contains_stable_identity_duration_and_usage_fields() {
        let mut session = ActiveSession::from_start(start_req());
        let ttl = TimeDelta::seconds(30);
        session.mark_active(ts(5), ttl);
        session.mark_active(ts(20), ttl);
        session.apply_end(SessionEndRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            ended_at: Some(ts(40)),
            tokens: Some(TokenUsage {
                input: Some(12),
                output: Some(8),
                ..Default::default()
            }),
            cost: Some(CostUsage {
                total: Some(0.25),
                currency: Some("USD".to_string()),
            }),
            model_usage: Some(vec![ModelUsage {
                model: "gpt-5".to_string(),
                tokens: TokenUsage {
                    input: Some(12),
                    output: Some(8),
                    ..Default::default()
                },
                cost: 0.25,
            }]),
            metadata: Some(json!({ "status": "done" })),
        });

        let event = session.summary_event("completed", ts(40), None);
        let data = &event.data;
        let expected_summary_id = format!(
            "codex:session-1:run-1:completed:none:{}:{}",
            ts(0).to_rfc3339(),
            ts(40).to_rfc3339()
        );

        assert_eq!(event.timestamp, ts(5));
        assert_eq!(event.duration, TimeDelta::seconds(15));
        assert_eq!(data.get("status"), Some(&Value::String("completed".into())));
        assert_eq!(
            data.get("summary_id"),
            Some(&Value::String(expected_summary_id))
        );
        assert_eq!(
            data.get("project"),
            Some(&Value::String("example-project".into()))
        );
        assert_eq!(
            data.get("session_id"),
            Some(&Value::String("session-1".into()))
        );
        assert_eq!(
            data.get("session_instance_id"),
            Some(&Value::String("run-1".into()))
        );
        assert_eq!(data.get("tokens_input"), Some(&Value::from(12)));
        assert_eq!(data.get("tokens_output"), Some(&Value::from(8)));
        assert_eq!(data.get("tokens_total"), Some(&Value::from(20)));
        assert_eq!(data.get("cost_total"), Some(&Value::from(0.25)));
        assert_eq!(
            data.get("selected_model"),
            Some(&Value::String("gpt-5".into()))
        );
        assert_eq!(
            data.get("metadata"),
            Some(&json!({
                "source": "test",
                "status": "done"
            }))
        );
        assert_eq!(
            data.get("active_duration_seconds").and_then(Value::as_f64),
            Some(15.0)
        );
        assert_eq!(
            data.get("wall_duration_seconds").and_then(Value::as_f64),
            Some(40.0)
        );

        let model_usage = data
            .get("model_usage")
            .and_then(Value::as_object)
            .and_then(|usage| usage.get("gpt-5"))
            .and_then(Value::as_object)
            .expect("gpt-5 model usage should be present");
        assert_eq!(model_usage.get("tokens_total"), Some(&Value::from(20)));
        assert_eq!(model_usage.get("cost"), Some(&Value::from(0.25)));
    }

    #[test]
    fn summary_model_is_multiple_when_model_usage_has_multiple_models() {
        let mut session = ActiveSession::from_start(start_req());
        session.apply_end(SessionEndRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            ended_at: Some(ts(40)),
            tokens: None,
            cost: None,
            model_usage: Some(vec![
                ModelUsage {
                    model: "gpt-5".to_string(),
                    tokens: TokenUsage {
                        input: Some(10),
                        ..Default::default()
                    },
                    cost: 0.1,
                },
                ModelUsage {
                    model: "gpt-5-mini".to_string(),
                    tokens: TokenUsage {
                        output: Some(5),
                        ..Default::default()
                    },
                    cost: 0.02,
                },
            ]),
            metadata: None,
        });

        let data = session.to_summary_data("completed", ts(40), None);

        assert_eq!(data.get("model"), Some(&Value::String("multiple".into())));
        assert!(data.get("models").is_none());
        assert_eq!(
            data.get("model_usage")
                .and_then(Value::as_object)
                .map(|usage| usage.len()),
            Some(2)
        );
    }

    #[test]
    fn finishing_activity_at_preserves_duration_and_allows_a_new_period() {
        let mut session = ActiveSession::from_start(start_req());
        let ttl = TimeDelta::seconds(25);

        session.mark_active(ts(10), ttl);
        session.finish_active_period_at(ts(25));
        assert_eq!(session.active_duration(), TimeDelta::seconds(15));
        assert!(!session.is_active_at(ts(25), ttl));

        session.mark_active(ts(40), ttl);
        session.mark_active(ts(50), ttl);
        assert_eq!(session.active_duration(), TimeDelta::seconds(25));
    }

    #[test]
    fn update_snapshot_is_kept_for_abandoned_summary() {
        let mut session = ActiveSession::from_start(start_req());
        session.apply_update(SessionUpdateRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            tokens: Some(TokenUsage {
                input: Some(10),
                output: Some(5),
                ..Default::default()
            }),
            cost: Some(CostUsage {
                total: Some(0.1),
                currency: Some("USD".to_string()),
            }),
            model_usage: Some(vec![ModelUsage {
                model: "gpt-5".to_string(),
                tokens: TokenUsage {
                    input: Some(10),
                    output: Some(5),
                    ..Default::default()
                },
                cost: 0.1,
            }]),
            active: None,
            active_at: None,
            metadata: None,
        });

        let data = session.to_summary_data("abandoned", ts(40), Some("timeout"));
        assert_eq!(data.get("tokens_total"), Some(&Value::from(15)));
        assert_eq!(data.get("cost_total"), Some(&Value::from(0.1)));
        assert!(data.get("model_usage").is_some());
    }

    #[test]
    fn unique_usage_model_wins_over_selected_model() {
        let mut session = ActiveSession::from_start(start_req());
        session.apply_end(SessionEndRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            ended_at: Some(ts(40)),
            tokens: None,
            cost: None,
            model_usage: Some(vec![ModelUsage {
                model: "provider/actual-model".to_string(),
                tokens: TokenUsage {
                    input: Some(7),
                    output: Some(3),
                    total: Some(10),
                    ..Default::default()
                },
                cost: 0.2,
            }]),
            metadata: None,
        });

        let data = session.to_summary_data("completed", ts(40), None);
        assert_eq!(
            data.get("model"),
            Some(&Value::String("provider/actual-model".into()))
        );
        assert_eq!(
            data.get("selected_model"),
            Some(&Value::String("gpt-5".into()))
        );
    }

    #[test]
    fn usage_snapshots_are_monotonic_and_derived_from_models() {
        let mut session = ActiveSession::from_start(start_req());
        let update = |input, output, cost| SessionUpdateRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            tokens: Some(TokenUsage {
                input: Some(input),
                output: Some(output),
                total: Some(input + output),
                ..Default::default()
            }),
            cost: Some(CostUsage {
                total: Some(cost),
                currency: Some("USD".to_string()),
            }),
            model_usage: Some(vec![ModelUsage {
                model: "gpt-5".to_string(),
                tokens: TokenUsage {
                    input: Some(input),
                    output: Some(output),
                    total: Some(input + output),
                    ..Default::default()
                },
                cost,
            }]),
            active: None,
            active_at: None,
            metadata: None,
        };

        session.apply_update(update(10, 5, 0.1));
        session.apply_update(update(4, 2, 0.04));

        assert_eq!(session.tokens.input, Some(10));
        assert_eq!(session.tokens.output, Some(5));
        assert_eq!(session.tokens.total, Some(15));
        assert_eq!(session.cost.total, Some(0.1));
        assert_eq!(session.cost.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn partial_model_usage_cannot_regress_top_level_snapshot() {
        let mut session = ActiveSession::from_start(start_req());
        session.apply_update(SessionUpdateRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            tokens: Some(TokenUsage {
                input: Some(100),
                total: Some(100),
                ..Default::default()
            }),
            cost: Some(CostUsage {
                total: Some(1.0),
                currency: Some("USD".to_string()),
            }),
            model_usage: None,
            active: None,
            active_at: None,
            metadata: None,
        });

        session.apply_update(SessionUpdateRequest {
            session_id: "session-1".to_string(),
            session_instance_id: Some("run-1".to_string()),
            code_agent: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            tokens: Some(TokenUsage {
                input: Some(10),
                total: Some(10),
                ..Default::default()
            }),
            cost: Some(CostUsage {
                total: Some(0.1),
                currency: Some("USD".to_string()),
            }),
            model_usage: Some(vec![ModelUsage {
                model: "gpt-5".to_string(),
                tokens: TokenUsage {
                    input: Some(10),
                    total: Some(10),
                    ..Default::default()
                },
                cost: 0.1,
            }]),
            active: None,
            active_at: None,
            metadata: None,
        });

        assert_eq!(session.tokens.input, Some(100));
        assert_eq!(session.tokens.total, Some(100));
        assert_eq!(session.cost.total, Some(1.0));
        assert!(session.model_usage.is_empty());
    }
}
