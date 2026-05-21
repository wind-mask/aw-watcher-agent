//! ActivityWatch 客户端封装。
//!
//! 提供连接管理和错误处理。

use anyhow::{Context, Result};
use aw_client_rust::AwClient;
use tracing::{debug, warn};

/// 默认 aw-server 端口
pub const DEFAULT_PORT: u16 = 5600;

/// ActivityWatch 客户端封装，带连接状态管理
pub struct WatcherClient {
    inner: AwClient,
    hostname: String,
}

impl WatcherClient {
    /// 创建新的客户端，连接到指定地址
    pub fn new(host: &str, port: u16, client_name: &str) -> Self {
        debug!(
            "Creating AwClient for {}:{}, name={}",
            host, port, client_name
        );
        let port_str = port.to_string();
        let inner = AwClient::new(host, &port_str, client_name);

        let hostname = inner.hostname.clone();
        debug!("AwClient hostname: {}", hostname);

        Self { inner, hostname }
    }

    /// 获取主机名（用于构造 bucket ID）
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// 测试与 aw-server 的连接
    pub fn check_connection(&self) -> Result<()> {
        debug!("Checking connection to aw-server");
        self.inner.get_buckets().map(|_| ()).map_err(|e| {
            warn!("Connection check failed: {}", e);
            anyhow::anyhow!("Connection failed: {}", e)
        })
    }

    /// 创建 bucket（幂等：已存在则忽略）
    pub fn create_bucket(&self, bucket_id: &str, bucket_type: &str) -> Result<()> {
        debug!("Creating bucket: {} (type: {})", bucket_id, bucket_type);
        self.inner
            .create_bucket(bucket_id, bucket_type)
            .context("Failed to create bucket")?;
        Ok(())
    }

    /// 删除 bucket
    pub fn delete_bucket(&self, bucket_id: &str) -> Result<()> {
        debug!("Deleting bucket: {}", bucket_id);
        self.inner
            .delete_bucket(bucket_id)
            .context("Failed to delete bucket")?;
        Ok(())
    }

    /// 发送心跳事件（用于持续时间追踪）
    pub fn heartbeat(
        &self,
        bucket_id: &str,
        event: &aw_models::Event,
        pulsetime: f64,
    ) -> Result<()> {
        self.inner
            .heartbeat(bucket_id, event, pulsetime)
            .context("Failed to send heartbeat")?;
        Ok(())
    }

    /// 写入普通 AW event（用于 summary bucket）
    pub fn insert_event(&self, bucket_id: &str, event: &aw_models::Event) -> Result<()> {
        self.inner
            .insert_event(bucket_id, event)
            .context("Failed to insert event")?;
        Ok(())
    }

    /// 获取对内部 AwClient 的引用（用于高级操作）
    pub fn inner(&self) -> &AwClient {
        &self.inner
    }
}
