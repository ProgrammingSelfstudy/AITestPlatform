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
use crate::selfcheck_run::{authed, scrub, CheckConfig};

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
    let client = crate::platform_client(Duration::from_secs(30))?;
    let response = authed(cfg, client.put(url(cfg, &["api", "node", "regression-requests", "abandon"])?)).json(&json!({"serial": cfg.serial})).send().await.context("连不上平台服务端")?;
    if response.status() != StatusCode::NO_CONTENT {
        bail!("作废旧的回归请求失败：平台返回 {}", response.status());
    }
    Ok(())
}

/// 领一个请求并跑完，返回是否处理了一个。没有请求或设备正忙返回 `false`。
pub async fn poll_once(cfg: &CheckConfig) -> anyhow::Result<bool> {
    let client = crate::platform_client(Duration::from_secs(30))?;
    let response = authed(cfg, client.put(url(cfg, &["api", "node", "regression-requests", "claim"])?)).json(&json!({"serial": cfg.serial})).send().await.context("连不上平台服务端")?;
    let claimed: Claimed = match response.status() {
        StatusCode::NO_CONTENT => return Ok(false),
        StatusCode::OK => response.json().await.context("平台返回的回归请求格式不对")?,
        other => bail!("领取回归请求失败：平台返回 {other}"),
    };
    tracing::info!("领到回归请求 {}：{} 的 {}", claimed.id, claimed.project, claimed.case_code);
    // 本节点自己的任务执行器和这里共用同一把按序列号的设备锁：服务端重启、取消 run 之后会立刻释放数据库里的设备锁，
    // 但本地的录屏和收尾可能还没结束，没有这把锁两边会同时用 adb 操作同一台手机
    let (finished, detail) = {
        let _device = crate::recorder::lock_device(&cfg.serial).await;
        match regress_script(cfg, &claimed.project, &claimed.case_code, Some(claimed.script_id)).await {
            Ok(outcome) => (true, if outcome.passed { format!("回归通过（版本 {}）", outcome.version) } else { format!("回归失败（版本 {}），原始输出见回归记录", outcome.version) }),
            Err(error) => (false, clip(&scrub(cfg, &format!("{error:#}")))),
        }
    };
    report_result(cfg, &client, claimed.id, finished, &detail).await?;
    Ok(true)
}

/// 回报请求结果。瞬时的网络错误和 5xx 重试（最多 3 次，间隔 1 秒、2 秒）：结果没送到，请求就一直是「运行中」，
/// 占着手机直到超时。服务端明确拒绝（404/403/409）不重试，那是请求已经被作废，重试没有意义。
async fn report_result(cfg: &CheckConfig, client: &reqwest::Client, id: Uuid, finished: bool, detail: &str) -> anyhow::Result<()> {
    let target = url(cfg, &["api", "regression-requests", &id.to_string(), "result"])?;
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(attempt)).await;
        }
        match authed(cfg, client.put(target.clone())).json(&json!({"finished": finished, "detail": detail})).send().await {
            Ok(response) if response.status() == StatusCode::NO_CONTENT => return Ok(()),
            Ok(response) if response.status().is_server_error() => last = format!("平台返回 {}", response.status()),
            Ok(response) => bail!("回报回归请求结果被平台拒绝：{}（{}）", response.status(), response.text().await.unwrap_or_default()),
            Err(error) => last = format!("连不上平台服务端：{error}"),
        }
    }
    bail!("回报回归请求结果失败（已重试 3 次）：{last}。请求会一直显示运行中，直到超时作废")
}

/// 一直轮询。出错只记日志、下一轮再试，不退出：平台暂时连不上不能让节点的其他功能受影响。
pub async fn serve(cfg: CheckConfig, interval: Duration) {
    // 启动时作废自己之前没回报的请求。没成功就不开始领：旧请求还占着设备，领也领不到，先把它清掉
    let mut wait = Duration::from_secs(2);
    while let Err(error) = abandon_stale(&cfg).await {
        tracing::warn!("启动时作废旧回归请求失败，{} 秒后重试：{error:#}", wait.as_secs());
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// 按顺序回复给定状态码的假平台。返回地址和收到的请求次数。
    async fn fake_platform(statuses: Vec<u16>) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let status = statuses[n.min(statuses.len() - 1)];
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    let reason = if status == 204 { "No Content" } else { "X" };
                    let _ = socket.write_all(format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
                });
            }
        });
        (addr, hits)
    }

    fn config(addr: std::net::SocketAddr) -> CheckConfig {
        CheckConfig {
            platform_http: format!("http://{addr}/").parse().unwrap(),
            node_token: "t".into(),
            node_id: Uuid::nil(),
            python: "python3".into(),
            automation_dir: PathBuf::new(),
            audio_dir: PathBuf::new(),
            state_dir: PathBuf::new(),
            package: String::new(),
            activity: String::new(),
            serial: "DEV0".into(),
            bottom_shift: 0,
            timeout: Duration::from_secs(1),
            work_dir: PathBuf::new(),
        }
    }

    async fn report(addr: std::net::SocketAddr) -> anyhow::Result<()> {
        let cfg = config(addr);
        let client = crate::platform_client(Duration::from_secs(5)).unwrap();
        report_result(&cfg, &client, Uuid::nil(), true, "ok").await
    }

    #[tokio::test]
    async fn 回报遇到瞬时的服务端错误会重试_最终成功() {
        let (addr, hits) = fake_platform(vec![503, 500, 204]).await;
        report(addr).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3, "前两次 5xx 要重试，第三次成功");
    }

    #[tokio::test]
    async fn 回报被服务端明确拒绝不重试() {
        for status in [404u16, 403, 409] {
            let (addr, hits) = fake_platform(vec![status, 204]).await;
            let error = report(addr).await.unwrap_err().to_string();
            assert!(error.contains("被平台拒绝"), "{error}");
            assert_eq!(hits.load(Ordering::SeqCst), 1, "{status} 是请求已被作废，重试没有意义");
        }
    }

    #[tokio::test]
    async fn 一直失败最多试三次() {
        let (addr, hits) = fake_platform(vec![503]).await;
        let error = report(addr).await.unwrap_err().to_string();
        assert!(error.contains("已重试 3 次"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }
}
