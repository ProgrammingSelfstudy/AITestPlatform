//! 页面发起的回归：节点轮询平台、领取请求、在自己的手机上跑、回报。
//!
//! - 只领**本节点**的请求（节点身份由令牌和 node_id 确认），所以不会跑到别人的手机上。
//! - 一次只跑一个：循环里领到就跑完再领下一个。设备和正常派发的互斥由服务端保证
//!   （领取要求设备空闲，运行期间服务端不会把任务派到这台设备）。
//! - 失败是回归的结论，不是请求的失败：`finished = true` 表示得到了结论，
//!   环境问题、脚本已换版本之类没能得到结论的才报 `finished = false` 并写原因。

use std::time::Duration;

use anyhow::{bail, Context};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::regress_run::regress_script;
use crate::selfcheck_run::{authed, CheckConfig};

#[derive(Deserialize)]
struct Claimed {
    id: Uuid,
    script_id: Uuid,
    project: String,
    case_code: String,
}

fn url(cfg: &CheckConfig, segments: &[&str]) -> anyhow::Result<reqwest::Url> {
    let mut url = cfg.platform_http.clone();
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("平台地址不能带路径"))?.pop_if_empty().extend(segments);
    Ok(url)
}

/// 上报说明的字数上限要和服务端一致（1800）：只留末尾，失败原因通常在最后。
fn clip(text: &str) -> String {
    const LIMIT: usize = 1800;
    let count = text.chars().count();
    if count <= LIMIT { text.to_string() } else { format!("…{}", text.chars().skip(count - LIMIT + 1).collect::<String>()) }
}

/// 节点刚启动：这个进程肯定没有在跑回归，之前领走而没回报的请求作废，免得一直占着设备。
pub async fn abandon_stale(cfg: &CheckConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let response = authed(cfg, client.put(url(cfg, &["api", "node", "regression-requests", "abandon"])?)).json(&json!({"serial": cfg.serial})).send().await.context("连不上平台服务端")?;
    if response.status() != StatusCode::NO_CONTENT {
        bail!("作废旧的回归请求失败：平台返回 {}", response.status());
    }
    Ok(())
}

/// 领一个请求并跑完，返回是否处理了一个。没有请求或设备正忙返回 `false`。
pub async fn poll_once(cfg: &CheckConfig) -> anyhow::Result<bool> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let response = authed(cfg, client.put(url(cfg, &["api", "node", "regression-requests", "claim"])?)).json(&json!({"serial": cfg.serial})).send().await.context("连不上平台服务端")?;
    let claimed: Claimed = match response.status() {
        StatusCode::NO_CONTENT => return Ok(false),
        StatusCode::OK => response.json().await.context("平台返回的回归请求格式不对")?,
        other => bail!("领取回归请求失败：平台返回 {other}"),
    };
    tracing::info!("领到回归请求 {}：{} 的 {}", claimed.id, claimed.project, claimed.case_code);
    let (finished, detail) = match regress_script(cfg, &claimed.project, &claimed.case_code, Some(claimed.script_id)).await {
        Ok(outcome) => (true, if outcome.passed { format!("回归通过（版本 {}）", outcome.version) } else { format!("回归失败（版本 {}），原始输出见回归记录", outcome.version) }),
        Err(error) => (false, clip(&format!("{error:#}"))),
    };
    let response = authed(cfg, client.put(url(cfg, &["api", "regression-requests", &claimed.id.to_string(), "result"])?)).json(&json!({"finished": finished, "detail": detail})).send().await.context("回报回归请求结果时连不上平台服务端")?;
    if response.status() != StatusCode::NO_CONTENT {
        bail!("回报回归请求结果失败：平台返回 {}", response.status());
    }
    Ok(true)
}

/// 一直轮询。出错只记日志、下一轮再试，不退出：平台暂时连不上不能让节点的其他功能受影响。
pub async fn serve(cfg: CheckConfig, interval: Duration) {
    if let Err(error) = abandon_stale(&cfg).await {
        tracing::warn!("启动时作废旧回归请求失败（下一次领取前设备可能仍被旧请求占着）：{error:#}");
    }
    loop {
        match poll_once(&cfg).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => tracing::warn!("处理回归请求出错：{error:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}
