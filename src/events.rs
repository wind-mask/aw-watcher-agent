//! code agent 会话事件模型。
//!
//! 这些结构用于 daemon HTTP ingest 协议，并最终写入 ActivityWatch 的
//! session bucket。设计目标是兼容 pi、opencode、Claude Code 等不同 code agent。
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// token 使用量。字段全部可选/可累加，方便不同 agent 只上报自己能拿到的数据。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub total: Option<u64>,
}

impl TokenUsage {
    /// 累加合并（非覆盖）：将 next 中各字段累加到 self。
    /// 这样扩展可发送每轮增量，daemon 侧自动汇总。
    pub fn merge(&mut self, next: TokenUsage) {
        if let Some(v) = next.input {
            *self.input.get_or_insert(0) += v;
        }
        if let Some(v) = next.output {
            *self.output.get_or_insert(0) += v;
        }
        if let Some(v) = next.cache_read {
            *self.cache_read.get_or_insert(0) += v;
        }
        if let Some(v) = next.cache_write {
            *self.cache_write.get_or_insert(0) += v;
        }
        if let Some(v) = next.total {
            *self.total.get_or_insert(0) += v;
        }
    }

    pub fn total_or_sum(&self) -> Option<u64> {
        self.total.or_else(|| {
            let sum = self.input.unwrap_or(0)
                + self.output.unwrap_or(0)
                + self.cache_read.unwrap_or(0)
                + self.cache_write.unwrap_or(0);
            (sum > 0).then_some(sum)
        })
    }
}

/// 费用信息。不同 provider 的费用模型差异较大，因此只固定 total/currency。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CostUsage {
    pub total: Option<f64>,
    pub currency: Option<String>,
}

impl CostUsage {
    /// 累加合并（非覆盖）：total 累加，currency 覆盖。
    pub fn merge(&mut self, next: CostUsage) {
        if let Some(v) = next.total {
            *self.total.get_or_insert(0.0) += v;
        }
        if next.currency.is_some() {
            self.currency = next.currency;
        }
    }
}

/// 按模型拆分的用量汇总。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: TokenUsage,
    pub cost: f64,
}

/// 会话开始请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionStartRequest {
    pub session_id: Option<String>,
    pub code_agent: String,
    pub project_dir: String,
    pub model: Option<String>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub started_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
}

/// 会话更新请求。用于更新模型、token、费用等聚合数据，
/// 以及按模型拆分的增量用量（daemon 侧按模型累加）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionUpdateRequest {
    pub session_id: String,
    pub model: Option<String>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub model_usage: Option<Vec<ModelUsage>>,
    pub metadata: Option<Value>,
}
/// 会话结束请求。按模型拆分的增量数据在结束时也会发送
/// 最后一轮，确保 daemon 侧数据完整。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionEndRequest {
    pub session_id: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub tokens: Option<TokenUsage>,
    pub cost: Option<CostUsage>,
    pub model_usage: Option<Vec<ModelUsage>>,
    pub metadata: Option<Value>,
}

/// 会话心跳请求。由扩展周期性发送，表示该 session 仍然活跃。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionHeartbeatRequest {
    pub session_id: String,
}

/// daemon 内部维护的活动会话状态。
#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub session_id: String,
    pub code_agent: String,
    pub project_dir: String,
    pub project_name: String,
    pub model: Option<String>,
    pub tokens: TokenUsage,
    pub cost: CostUsage,
    pub model_usage: HashMap<String, ModelUsage>,
    pub started_at: DateTime<Utc>,
    pub metadata: Option<Value>,
}

impl ActiveSession {
    pub fn from_start(req: SessionStartRequest, session_id: String) -> Self {
        let project_name = project_name_from_dir(&req.project_dir);

        Self {
            session_id,
            code_agent: req.code_agent,
            project_dir: req.project_dir,
            project_name,
            model: req.model,
            tokens: req.tokens.unwrap_or_default(),
            cost: req.cost.unwrap_or_default(),
            model_usage: HashMap::new(),
            started_at: req.started_at.unwrap_or_else(Utc::now),
            metadata: req.metadata,
        }
    }

    pub fn apply_update(&mut self, req: SessionUpdateRequest) {
        if let Some(new_model) = req.model {
            self.model = Some(new_model);
        }
        if let Some(tokens) = req.tokens {
            self.tokens.merge(tokens);
        }
        if let Some(cost) = req.cost {
            self.cost.merge(cost);
        }
        if let Some(model_usage) = req.model_usage {
            merge_model_usage(&mut self.model_usage, model_usage);
        }
        if req.metadata.is_some() {
            self.metadata = req.metadata;
        }
    }

    pub fn apply_end(&mut self, req: &SessionEndRequest) {
        if let Some(tokens) = req.tokens.clone() {
            self.tokens.merge(tokens);
        }
        if let Some(cost) = req.cost.clone() {
            self.cost.merge(cost);
        }
        if let Some(model_usage) = req.model_usage.clone() {
            merge_model_usage(&mut self.model_usage, model_usage);
        }
        if req.metadata.is_some() {
            self.metadata = req.metadata.clone();
        }
    }

    /// 转为 ActivityWatch data 字段。
    ///
    /// `for_heartbeat`: 心跳模式下不包含 token/cost 等持续变化的数据，
    /// 避免因数据变化导致 AW 将 session 切成多个事件段。
    pub fn to_aw_data(&self, status: &str, for_heartbeat: bool) -> Map<String, Value> {
        let mut data = Map::new();

        // ActivityWatch 现有 coding/editor 活动视图常用字段。
        data.insert("project".into(), Value::String(self.project_name.clone()));
        data.insert("file".into(), Value::String(self.project_dir.clone()));
        data.insert("language".into(), Value::String("code-agent".into()));

        // 通用 code agent 扩展字段（heartbeat 和 summary 都有）。
        data.insert("status".into(), Value::String(status.to_string()));
        data.insert("session_id".into(), Value::String(self.session_id.clone()));
        data.insert("code_agent".into(), Value::String(self.code_agent.clone()));
        data.insert(
            "project_dir".into(),
            Value::String(self.project_dir.clone()),
        );
        data.insert(
            "started_at".into(),
            Value::String(self.started_at.to_rfc3339()),
        );

        // token/cost 只在 summary（session 结束）时写入，避免心跳切出新段。
        if !for_heartbeat {
            if let Some(value) = self.tokens.input {
                data.insert("tokens_input".into(), Value::from(value));
            }
            if let Some(value) = self.tokens.output {
                data.insert("tokens_output".into(), Value::from(value));
            }
            if let Some(value) = self.tokens.cache_read {
                data.insert("tokens_cache_read".into(), Value::from(value));
            }
            if let Some(value) = self.tokens.cache_write {
                data.insert("tokens_cache_write".into(), Value::from(value));
            }
            if let Some(value) = self.tokens.total_or_sum() {
                data.insert("tokens_total".into(), Value::from(value));
            }
            if let Some(value) = self.cost.total {
                data.insert("cost_total".into(), Value::from(value));
            }
            if let Some(value) = &self.cost.currency {
                data.insert("cost_currency".into(), Value::String(value.clone()));
            }
            if let Some(value) = &self.metadata {
                data.insert("metadata".into(), value.clone());
            }
            // 只保留一个主要模型；多模型明细由 model_usage 表达。
            if let Some(model) = &self.model {
                data.insert("model".into(), Value::String(model.clone()));
            }

            // 按模型拆分的 token/cost 明细
            if !self.model_usage.is_empty() {
                let per_model: Map<String, Value> = self
                    .model_usage
                    .iter()
                    .map(|(model, mu)| {
                        let mut m = Map::new();
                        if let Some(v) = mu.tokens.input {
                            m.insert("tokens_input".into(), Value::from(v));
                        }
                        if let Some(v) = mu.tokens.output {
                            m.insert("tokens_output".into(), Value::from(v));
                        }
                        if let Some(v) = mu.tokens.cache_read {
                            m.insert("tokens_cache_read".into(), Value::from(v));
                        }
                        if let Some(v) = mu.tokens.cache_write {
                            m.insert("tokens_cache_write".into(), Value::from(v));
                        }
                        if let Some(v) = mu.tokens.total_or_sum() {
                            m.insert("tokens_total".into(), Value::from(v));
                        }
                        m.insert("cost".into(), Value::from(mu.cost));
                        (model.clone(), Value::Object(m))
                    })
                    .collect();
                data.insert("model_usage".into(), Value::Object(per_model));
            }
        }

        data
    }
}

/// 将按模型拆分的增量数据累加到 session 的 model_usage map 中。
fn merge_model_usage(target: &mut HashMap<String, ModelUsage>, incoming: Vec<ModelUsage>) {
    for mu in incoming {
        target
            .entry(mu.model.clone())
            .and_modify(|existing| {
                existing.tokens.merge(mu.tokens.clone());
                existing.cost += mu.cost;
            })
            .or_insert(mu);
    }
}

fn project_name_from_dir(project_dir: &str) -> String {
    std::path::Path::new(project_dir)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(project_dir)
        .to_string()
}
