//! 证据 MCP 的标准输入输出入口，日志写入标准错误。

use std::path::Path;

use anyhow::{Context, bail};
use mcp_device::evidence::EvidenceServer;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let Some(run_dir) = std::env::var_os("HAFA_RUN_DIR").filter(|dir| !dir.is_empty()) else {
        bail!("请设置环境变量 HAFA_RUN_DIR（本次执行的轨迹目录）");
    };
    let server = EvidenceServer::new(Path::new(&run_dir)).context("初始化证据清单失败")?;
    tracing::info!("证据 MCP 服务启动");
    let service = server.serve(stdio()).await.context("MCP 服务启动失败")?;
    service.waiting().await.context("MCP 服务异常退出")?;
    Ok(())
}
