//! 关闭信号处理。

use tracing::warn;

/// 等待关闭信号。
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            }
        }
        Err(err) => {
            warn!("Failed to register SIGTERM handler: {}", err);
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}

/// 等待关闭信号。
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "ctrl_c"
}
