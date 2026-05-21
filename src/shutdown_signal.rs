//! 关闭信号处理。

use tracing::warn;

/// 等待关闭信号。
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(signal) => Some(signal),
        Err(err) => {
            warn!("Failed to register SIGINT handler: {}", err);
            None
        }
    };
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => Some(signal),
        Err(err) => {
            warn!("Failed to register SIGTERM handler: {}", err);
            None
        }
    };

    match (&mut interrupt, &mut terminate) {
        (Some(interrupt), Some(terminate)) => {
            tokio::select! {
                _ = interrupt.recv() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            }
        }
        (Some(interrupt), None) => {
            let _ = interrupt.recv().await;
            "SIGINT"
        }
        (None, Some(terminate)) => {
            let _ = terminate.recv().await;
            "SIGTERM"
        }
        (None, None) => {
            warn!("No shutdown signal handlers could be registered; falling back to ctrl_c()");
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
