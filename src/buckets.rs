//! ActivityWatch bucket 生命周期管理。
//!
//! event bucket 记录合并后的实时 code-agent 活动；sum bucket 记录单个
//! session 的 completed/abandoned 汇总数据。

use anyhow::Result;
use tracing::{info, warn};

use crate::client::WatcherClient;

/// 兼容 AW 现有 editor/coding 活动视图的 event_type。
pub const EVENT_BUCKET_TYPE: &str = "app.editor.activity";
/// session 汇总事件类型。
pub const SUM_BUCKET_TYPE: &str = "app.code-agent.summary";

/// Bucket 管理器
pub struct BucketManager {
    /// 合并后的实时活动 bucket
    pub event_bucket_id: String,
    /// 单 session 汇总 bucket
    pub sum_bucket_id: String,
    /// v0.1 兼容遗留 bucket，仅在 teardown 时清理。
    legacy_session_bucket_id: String,
}

impl BucketManager {
    /// 从 client 信息构造 bucket ID
    pub fn new(client: &WatcherClient) -> Self {
        let hostname = client.hostname();
        Self {
            event_bucket_id: format!("aw-watcher-agent-event_{}", hostname),
            sum_bucket_id: format!("aw-watcher-agent-sum_{}", hostname),
            legacy_session_bucket_id: format!("aw-watcher-agent_{}", hostname),
        }
    }

    /// 创建 event/sum bucket。
    pub fn setup(&self, client: &WatcherClient) -> Result<()> {
        info!("Setting up event bucket: {}", self.event_bucket_id);
        Self::ensure_bucket(client, &self.event_bucket_id, EVENT_BUCKET_TYPE)?;
        info!("Setting up sum bucket: {}", self.sum_bucket_id);
        Self::ensure_bucket(client, &self.sum_bucket_id, SUM_BUCKET_TYPE)
    }

    /// 删除当前 watcher 创建的 bucket。
    pub fn teardown(&self, client: &WatcherClient) {
        info!("Tearing down event bucket: {}", self.event_bucket_id);
        let _ = client.delete_bucket(&self.event_bucket_id);
        info!("Tearing down sum bucket: {}", self.sum_bucket_id);
        let _ = client.delete_bucket(&self.sum_bucket_id);
        info!(
            "Tearing down legacy session bucket: {}",
            self.legacy_session_bucket_id
        );
        let _ = client.delete_bucket(&self.legacy_session_bucket_id);
    }

    fn ensure_bucket(client: &WatcherClient, bucket_id: &str, bucket_type: &str) -> Result<()> {
        // 使用 get_buckets() 而非 get_bucket()，避免 aw-server 404 响应
        // 无法反序列化为 Bucket 导致 reqwest 0.10 decode error 丢失 status code。
        let buckets = client
            .inner()
            .get_buckets()
            .map_err(|e| anyhow::anyhow!("Failed to list buckets: {}", e))?;

        match buckets.get(bucket_id) {
            Some(bucket) if bucket._type == bucket_type => Ok(()),
            Some(bucket) => {
                warn!(
                    "Recreating bucket {}: type {} -> {}",
                    bucket_id, bucket._type, bucket_type
                );
                let _ = client.delete_bucket(bucket_id);
                client.create_bucket(bucket_id, bucket_type)
            }
            None => client.create_bucket(bucket_id, bucket_type),
        }
    }
}
