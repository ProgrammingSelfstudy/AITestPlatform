//! 设备 MCP 服务的可执行文件：经标准输入输出和 MCP 客户端（`claude` 命令行等）通信。
//! 注意：**stdout 是协议通道，日志只能写 stderr**。

use std::sync::Arc;

use anyhow::{bail, Context};
use device::Android;
use mcp_device::{DeviceServer, ServerConfig};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // 操作哪台手机必须显式给出，不默认选第一台：两台手机并行时选错会装包 / 点错到别的设备上
    let serial = std::env::var("HAFA_SERIAL").unwrap_or_default();
    if serial.is_empty() {
        bail!("请设置环境变量 HAFA_SERIAL（要操作的设备序列号）");
    }
    let cfg = ServerConfig::from_env().map_err(anyhow::Error::msg)?;
    tracing::info!(%serial, case = %cfg.case_id, max_steps = cfg.max_steps, "设备 MCP 服务启动");

    let server = DeviceServer::new(Arc::new(Android::new(serial)), cfg).context("初始化轨迹目录失败")?;
    let service = server.serve(stdio()).await.context("MCP 服务启动失败")?;
    service.waiting().await.context("MCP 服务异常退出")?;
    Ok(())
}
