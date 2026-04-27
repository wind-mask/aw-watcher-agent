//! aw-watcher-agent: ActivityWatch code-agent session watcher。
//!
//! 现在的核心模式是后台 daemon：各类 code agent 扩展通过 HTTP 发送会话事件，
//! daemon 汇总 session 时长、模型、token、费用等数据并写入 ActivityWatch。

mod buckets;
mod client;
mod daemon;
mod events;

use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use buckets::BucketManager;
use client::{WatcherClient, DEFAULT_PORT};
use daemon::run_daemon;

const DEFAULT_DAEMON_LISTEN: &str = "127.0.0.1:5667";

/// aw-watcher-agent: 将 code agent 会话追踪到 ActivityWatch
#[derive(Parser, Debug)]
#[command(name = "aw-watcher-agent")]
#[command(version, about, long_about = None)]
struct Cli {
    /// 子命令（省略则默认运行 daemon）
    #[command(subcommand)]
    command: Option<Commands>,

    /// aw-server 主机地址
    #[arg(long, global = true, default_value = "localhost")]
    host: String,

    /// aw-server 端口 (默认 5600, --testing 时 5666)
    #[arg(long, global = true)]
    port: Option<u16>,

    /// 使用 aw-server 测试模式端口 (5666)
    #[arg(long, global = true)]
    testing: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 启动 HTTP daemon，接收各 code agent 扩展发来的 session 事件
    Daemon {
        /// daemon 监听地址
        #[arg(long, default_value = DEFAULT_DAEMON_LISTEN)]
        listen: SocketAddr,
    },

    /// 删除 session bucket 和旧版 tool bucket
    Teardown,

    /// 检查与 aw-server 的连接
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化 tracing，日志级别由 RUST_LOG 环境变量控制
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let port = cli.port.unwrap_or(DEFAULT_PORT);
    // 无子命令时默认运行 daemon
    let command = cli.command.unwrap_or(Commands::Daemon {
        listen: DEFAULT_DAEMON_LISTEN
            .parse()
            .expect("Invalid default listen address"),
    });

    // 避免 daemon 与临时 CLI 命令争用 aw-client-rust 的 single-instance lock。
    let client_name = match command {
        Commands::Daemon { .. } => "aw-watcher-agent-daemon",
        _ => "aw-watcher-agent-cli",
    };
    let client = WatcherClient::new(&cli.host, port, client_name).await?;
    info!(
        "Connected to aw-server at {}:{} as {}",
        cli.host, port, client_name
    );
    let buckets = BucketManager::new(&client);
    match command {
        Commands::Daemon { listen } => {
            info!("Starting daemon on {}", listen);
            run_daemon(client, buckets, listen).await?;
        }

        Commands::Teardown => {
            info!("Tearing down buckets");
            buckets.teardown(&client).await?;
            println!("Session and legacy tool buckets removed.");
        }

        Commands::Status => match client.check_connection().await {
            Ok(()) => {
                info!("Connection check OK");
                println!("✅ Connected to aw-server at {}:{}", cli.host, port)
            }
            Err(e) => {
                tracing::warn!("Connection check failed: {}", e);
                println!("❌ Connection failed: {}", e)
            }
        },
    }

    Ok(())
}
