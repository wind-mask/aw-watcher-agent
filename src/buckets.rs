//! Session bucket 生命周期管理。
//!
//! 仅保留一个 session bucket。为了兼容 ActivityWatch 现有编码活动视图，
//! bucket type 使用 `app.editor.activity`，实际 data 中额外包含 code-agent 字段。

use anyhow::Result;
use tracing::{info, warn};

use crate::client::WatcherClient;

/// 兼容 AW 现有 editor/coding 活动视图的 event_type。
pub const SESSION_EVENT_TYPE: &str = "app.editor.activity";

/// Bucket 管理器
pub struct BucketManager {
    /// 会话 bucket
    pub session_bucket_id: String,
}

impl BucketManager {
    /// 从 client 信息构造 bucket ID
    pub fn new(client: &WatcherClient) -> Self {
        let hostname = client.hostname();
        Self {
            session_bucket_id: format!("aw-watcher-agent_{}", hostname),
        }
    }

    /// 创建 session bucket，并清理旧版 tool bucket。
    pub fn setup(&self, client: &WatcherClient) -> Result<()> {
        info!("Setting up session bucket: {}", self.session_bucket_id);
        self.ensure_session_bucket(client)
    }

    /// 删除当前 session bucket。
    pub fn teardown(&self, client: &WatcherClient) -> Result<()> {
        info!("Tearing down session bucket: {}", self.session_bucket_id);
        let _ = client.delete_bucket(&self.session_bucket_id);
        Ok(())
    }

    fn ensure_session_bucket(&self, client: &WatcherClient) -> Result<()> {
        match client.inner().get_bucket(&self.session_bucket_id) {
            Ok(bucket) if bucket._type == SESSION_EVENT_TYPE => Ok(()),
            Ok(bucket) => {
                warn!(
                    "Recreating bucket {}: type {} -> {}",
                    self.session_bucket_id, bucket._type, SESSION_EVENT_TYPE
                );
                let _ = client.delete_bucket(&self.session_bucket_id);
                self.create_session_bucket(client)
            }
            Err(err) if err.status().is_some_and(|status| status.as_u16() == 404) => {
                self.create_session_bucket(client)
            }
            Err(err) => Err(anyhow::anyhow!("Failed to inspect bucket: {}", err)),
        }
    }

    fn create_session_bucket(&self, client: &WatcherClient) -> Result<()> {
        // `aw-client-rust` 的 `create_bucket` 内部构造 Bucket struct，
        // 这里传 bare string 即可，兼容 crates.io 版本的同步 API。
        client.create_bucket(&self.session_bucket_id, SESSION_EVENT_TYPE)
    }
}
