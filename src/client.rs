//! ActivityWatch 客户端封装。
//!
//! 提供连接管理、错误处理和重试逻辑。

#![allow(dead_code)]

use anyhow::{Context, Result};
use aw_client_rust::AwClient;
use aw_models::Bucket;
use tracing::{debug, warn};
/// 默认 aw-server 端口
pub const DEFAULT_PORT: u16 = 5600;

/// ActivityWatch 客户端封装，带连接状态管理
pub struct WatcherClient {
    inner: AwClient,
    hostname: String,
    port: u16,
}

impl WatcherClient {
    /// 创建新的客户端，连接到指定地址
    pub async fn new(host: &str, port: u16, client_name: &str) -> Result<Self> {
        debug!(
            "Creating AwClient for {}:{}, name={}",
            host, port, client_name
        );
        let inner = AwClient::new(host, port, client_name)
            .map_err(|e| anyhow::anyhow!("Failed to create AwClient: {}", e))?;

        let hostname = inner.hostname.clone();
        debug!("AwClient hostname: {}", hostname);

        Ok(Self {
            inner,
            hostname,
            port,
        })
    }

    /// 从环境变量或默认值创建客户端
    /// - AW_HOST: aw-server 地址 (默认 localhost)
    /// - AW_PORT: aw-server 端口 (默认 5600)
    pub async fn from_env() -> Result<Self> {
        let host = std::env::var("AW_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port: u16 = std::env::var("AW_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);

        Self::new(&host, port, "aw-watcher-agent").await
    }

    /// 获取主机名（用于构造 bucket ID）
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// 获取当前端口
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 测试与 aw-server 的连接
    pub async fn check_connection(&self) -> Result<()> {
        debug!("Checking connection to aw-server");
        match self.inner.get_bucket("__connection_test__").await {
            Ok(_) => {
                debug!("Connection check: test bucket found");
                Ok(())
            }
            Err(e) => {
                if e.status().is_some_and(|status| status.as_u16() == 404) {
                    debug!("Connection check OK (404 on test bucket)");
                    Ok(())
                } else {
                    warn!("Connection check failed: {}", e);
                    Err(anyhow::anyhow!("Connection failed: {}", e))
                }
            }
        }
    }

    /// 创建 bucket（幂等：已存在则忽略）
    pub async fn create_bucket(&self, bucket: &Bucket) -> Result<()> {
        debug!("Creating bucket: {}", bucket.id);
        self.inner
            .create_bucket(bucket)
            .await
            .context("Failed to create bucket")?;
        Ok(())
    }

    /// 删除 bucket
    pub async fn delete_bucket(&self, bucket_id: &str) -> Result<()> {
        debug!("Deleting bucket: {}", bucket_id);
        self.inner
            .delete_bucket(bucket_id)
            .await
            .context("Failed to delete bucket")?;
        Ok(())
    }

    /// 插入单个事件
    pub async fn insert_event(&self, bucket_id: &str, event: &aw_models::Event) -> Result<()> {
        debug!("Inserting event into bucket: {}", bucket_id);
        self.inner
            .insert_event(bucket_id, event)
            .await
            .context("Failed to insert event")?;
        Ok(())
    }

    /// 发送心跳事件（用于持续时间追踪）
    pub async fn heartbeat(
        &self,
        bucket_id: &str,
        event: &aw_models::Event,
        pulsetime: f64,
    ) -> Result<()> {
        self.inner
            .heartbeat(bucket_id, event, pulsetime)
            .await
            .context("Failed to send heartbeat")?;
        Ok(())
    }

    /// 获取对内部 AwClient 的引用（用于高级操作）
    pub fn inner(&self) -> &AwClient {
        &self.inner
    }
}
