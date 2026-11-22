use std::path::PathBuf;

use anyhow::{bail, Context};
use node::real::{RealConfig, RealExecutor};
use node::{run, AdbDevices, NoExecutor, NodeConfig, Only};
use reqwest::Url;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // 令牌必须显式给出；不写日志
    let token = std::env::var("NODE_TOKEN").unwrap_or_default();
    if token.is_empty() {
        bail!("请先设置环境变量 NODE_TOKEN（和服务端一致的节点接入令牌）");
    }
    let url = std::env::var("PLATFORM_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/node".into());
    let state_dir = PathBuf::from(std::env::var("NODE_STATE_DIR").unwrap_or_else(|_| "data/node".into()));

    tracing::info!("执行节点启动，连接 {url}");
    let mut cfg = NodeConfig::new(url, token, state_dir);
    cfg.user_token = std::env::var("NODE_USER_TOKEN").ok().filter(|v| !v.is_empty());
    if cfg.user_token.is_none() {
        tracing::warn!("没有设置 NODE_USER_TOKEN：节点没有属主，任何人都发不了任务到这个节点（先 server create-user，再把令牌设进来）");
    }
    // HAFA_DEVICE_ALLOW=序列号,序列号：只让这些设备参与（账号不能同时登在两台手机上时用）
    let allow: Vec<String> = std::env::var("HAFA_DEVICE_ALLOW").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !allow.is_empty() {
        tracing::info!("设备白名单：{allow:?}");
    }
    // 配了 HAFA_PYTHON 就具备真实执行能力；否则领到任务只会如实报受阻
    match real_config(&cfg)? {
        Some(rc) => {
            tracing::info!("真实执行器已启用");
            tokio::select! {
                r = run(cfg, Only { inner: AdbDevices::default(), serials: allow }, RealExecutor(rc)) => r?,
                _ = tokio::signal::ctrl_c() => tracing::info!("收到退出信号，节点停止"),
            }
        }
        None => {
            tracing::warn!("没有配置 HAFA_PYTHON，节点没有执行能力");
            tokio::select! {
                r = run(cfg, Only { inner: AdbDevices::default(), serials: allow }, NoExecutor) => r?,
                _ = tokio::signal::ctrl_c() => tracing::info!("收到退出信号，节点停止"),
            }
        }
    }
    Ok(())
}

/// 是否回环地址（本机直连，明文不出机器）。
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 由节点连接地址推导平台 HTTP 基址。`wss://` → `https://`。
/// 明文 `ws://` 只允许回环地址：令牌、节点密钥和账号明文都走这条链路，跨机必须加密；
/// 确有内网明文需求时显式设 `NODE_ALLOW_INSECURE=1`。
fn platform_http(ws: &str, allow_insecure: bool) -> anyhow::Result<Url> {
    let mut url = Url::parse(ws).context("PLATFORM_WS 不是合法的地址")?;
    if !matches!(url.scheme(), "ws" | "wss") || url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("PLATFORM_WS 必须是无用户信息的 ws:// 或 wss:// 地址");
    }
    if url.scheme() == "ws" && !is_loopback(&url) && !allow_insecure {
        bail!("PLATFORM_WS 不是本机地址，必须用 wss://（令牌和账号凭证会经过这条连接）；内网确需明文请设置 NODE_ALLOW_INSECURE=1");
    }
    let http = if url.scheme() == "wss" { "https" } else { "http" };
    url.set_scheme(http).map_err(|_| anyhow::anyhow!("无法推导平台 HTTP 地址"))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// 环境变量：HAFA_PYTHON、HAFA_AUTOMATION_DIR、HAFA_AUDIO_DIR、HAFA_MCP_BIN、HAFA_APP_INFO（app_info.yaml 路径）、
/// HAFA_RUNS_DIR、HAFA_BOTTOM_SHIFTS（如 `15064060=-84,其它=0`）、HAFA_EVIDENCE_BIN（配置后启用独立判定）。
fn real_config(node: &NodeConfig) -> anyhow::Result<Option<RealConfig>> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(python) = get("HAFA_PYTHON") else { return Ok(None) };
    let need = |k: &str| get(k).ok_or_else(|| anyhow::anyhow!("配置了 HAFA_PYTHON 就必须同时设置 {k}"));
    let app_info = std::fs::read_to_string(need("HAFA_APP_INFO")?)?;
    let mut shifts = std::collections::HashMap::new();
    for kv in get("HAFA_BOTTOM_SHIFTS").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("HAFA_BOTTOM_SHIFTS 格式是 序列号=偏移,..."))?;
        shifts.insert(k.trim().to_string(), v.trim().parse()?);
    }
    Ok(Some(RealConfig {
        platform_http: platform_http(&node.url, std::env::var("NODE_ALLOW_INSECURE").is_ok_and(|v| v == "1"))?,
        node_token: node.token.clone(),
        python,
        automation_dir: need("HAFA_AUTOMATION_DIR")?.into(),
        audio_dir: need("HAFA_AUDIO_DIR")?.into(),
        state_dir: std::env::temp_dir().join("hafa_node_state"),
        runs_dir: get("HAFA_RUNS_DIR").unwrap_or_else(|| "data/runs".into()).into(),
        mcp_bin: need("HAFA_MCP_BIN")?.into(),
        evidence_bin: get("HAFA_EVIDENCE_BIN").unwrap_or_default().into(),
        package: "com.hafa.pikka".into(),
        activity: "com.hafa.pikka.MainActivity".into(),
        bottom_shift: shifts,
        screen: (720, 1600),
        app_info,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从节点连接地址推导平台_http_基址() {
        let base = platform_http("ws://127.0.0.1:9000/ws/node?param=value", false).unwrap();
        assert_eq!(base.as_str(), "http://127.0.0.1:9000/");
    }

    #[test]
    fn wss_推导为_https() {
        assert_eq!(platform_http("wss://ci.example.com/ws/node", false).unwrap().as_str(), "https://ci.example.com/");
    }

    #[test]
    fn 跨机明文被拒绝_本机和显式放行除外() {
        assert!(platform_http("ws://192.168.1.5:8080/ws/node", false).is_err());
        assert!(platform_http("ws://ci.example.com/ws/node", false).is_err());
        assert!(platform_http("ws://localhost:8080/ws/node", false).is_ok());
        assert!(platform_http("ws://[::1]:8080/ws/node", false).is_ok());
        assert!(platform_http("ws://192.168.1.5:8080/ws/node", true).is_ok(), "显式放行");
        assert!(platform_http("http://127.0.0.1/ws/node", false).is_err());
    }
}
