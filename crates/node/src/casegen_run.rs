//! 节点侧用例生成：领取页面发起的生成请求，用本节点「测试用例」角色的模型根据文档和截图生成用例，回报给服务端。
//!
//! - **谁发起谁付额度（规则 9）**：只领**本节点**的请求（节点身份由令牌和 node_id 确认），模型调用用本机会员登录。
//! - 角色和模型来自本机 `roles.json`（规则 3），请求里没指定角色就用第一个「测试用例」角色；
//!   一个都没有就如实回报「没有配置」，不悄悄换成别的角色或来源。
//! - 额度用完立即回报失败并说明，不重试、不换计费方式（规则 1）。
//! - 服务端是用例格式的唯一裁判：这里只回报模型的原始输出，不合格由服务端整批拒收。
//! - 日志和回报说明里不放文档和截图内容，只放错误类型。

use std::future::Future;
use std::pin::Pin;
use std::path::PathBuf;
use std::time::Duration;

use agent::casegen::{self, Image, Input};
use anyhow::{bail, Context};
use base64::Engine as _;
use protocol::{ModelSource, RoleSlot};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::roles::{real_probe, RoleStore};

pub struct CasegenConfig {
    pub platform_http: Url,
    pub node_token: String,
    pub node_id: Uuid,
    /// 本机状态目录（角色配置在这里）
    pub state_dir: PathBuf,
}

#[derive(Deserialize)]
struct Shot {
    content_type: String,
    data_b64: String,
}

#[derive(Deserialize)]
struct Claimed {
    id: Uuid,
    project: String,
    module: String,
    doc: String,
    ui_links: Vec<String>,
    screenshots: Vec<Shot>,
    writer_role: Option<String>,
}

fn authed(cfg: &CasegenConfig, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    crate::with_node_secret(request.bearer_auth(&cfg.node_token).header("X-Node-Id", cfg.node_id.to_string()))
}

fn url(cfg: &CasegenConfig, segments: &[&str]) -> anyhow::Result<Url> {
    let mut url = cfg.platform_http.clone();
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("平台地址不能带路径"))?.pop_if_empty().extend(segments);
    Ok(url)
}

/// 生成函数：来源、模型、强度、输入 → 结果。默认调真实命令行，测试里换成假的。
pub type Generate = Box<dyn for<'a> Fn(ModelSource, Option<String>, Option<String>, &'a Input<'a>) -> Pin<Box<dyn Future<Output = casegen::Outcome> + Send + 'a>> + Send + Sync>;

pub fn real_generate() -> Generate {
    Box::new(|source, model, effort, input| Box::pin(casegen::generate(source, model, effort, input)))
}

/// 领一个请求并处理完，返回是否处理了一个。
pub async fn poll_once(cfg: &CasegenConfig, generate: &Generate) -> anyhow::Result<bool> {
    let client = crate::platform_client(Duration::from_secs(60))?;
    let response = authed(cfg, client.put(url(cfg, &["api", "node", "casegen-requests", "claim"])?)).send().await.context("连不上平台服务端")?;
    let claimed: Claimed = match response.status() {
        StatusCode::NO_CONTENT => return Ok(false),
        StatusCode::OK => response.json().await.context("平台返回的生成请求格式不对")?,
        other => bail!("领取生成请求失败：平台返回 {other}"),
    };
    tracing::info!("领到用例生成请求 {}：{} / {}", claimed.id, claimed.project, claimed.module);

    let (ok, detail, cases_text) = match handle(cfg, &claimed, generate).await {
        Ok((text, note)) => (true, note, Some(text)),
        Err(reason) => (false, reason, None),
    };
    report(cfg, &client, claimed.id, ok, &detail, cases_text.as_deref()).await?;
    Ok(true)
}

async fn handle(cfg: &CasegenConfig, claimed: &Claimed, generate: &Generate) -> Result<(String, String), String> {
    // 每次领到都重新读本机角色配置：用户刚在页面里改过就用新的
    let roles = RoleStore::open(&cfg.state_dir, real_probe()).configs();
    let role = roles
        .iter()
        .find(|r| r.slot == RoleSlot::CaseWriter && claimed.writer_role.as_deref().is_none_or(|id| r.id == id))
        .ok_or_else(|| "这个节点没有配置「测试用例」角色（或请求指定的角色不存在），请先在「角色与模型」页配置并检测".to_string())?;
    let mut images = vec![];
    for (i, shot) in claimed.screenshots.iter().enumerate() {
        let bytes = base64::engine::general_purpose::STANDARD.decode(&shot.data_b64).map_err(|_| format!("第 {} 张截图解码失败", i + 1))?;
        images.push(Image { media_type: shot.content_type.clone(), bytes });
    }
    if images.is_empty() {
        return Err("请求里没有截图".into());
    }
    let input = Input { module: &claimed.module, doc: &claimed.doc, ui_links: &claimed.ui_links, images: &images };
    let outcome = generate(role.source, Some(role.default.model.clone()), role.default.effort.clone(), &input).await;
    if outcome.quota_exhausted {
        tracing::warn!("测试用例角色额度用完，已停止，不换计费方式");
    }
    match outcome.failure {
        Some(reason) => Err(reason),
        None => Ok((outcome.text, format!("角色「{}」生成完成，用时 {:.0} 秒", role.name, outcome.wall_s))),
    }
}

/// 回报结果。瞬时的网络错误和 5xx 重试（最多 3 次，间隔 1 秒、2 秒）；服务端明确拒绝不重试。
async fn report(cfg: &CasegenConfig, client: &reqwest::Client, id: Uuid, ok: bool, detail: &str, cases_text: Option<&str>) -> anyhow::Result<()> {
    let target = url(cfg, &["api", "casegen-requests", &id.to_string(), "result"])?;
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(attempt)).await;
        }
        match authed(cfg, client.put(target.clone())).json(&json!({"ok": ok, "detail": detail, "cases_text": cases_text})).send().await {
            Ok(response) if response.status() == StatusCode::OK => return Ok(()),
            Ok(response) if response.status().is_server_error() => last = format!("平台返回 {}", response.status()),
            Ok(response) => bail!("回报生成结果被平台拒绝：{}（{}）", response.status(), response.text().await.unwrap_or_default()),
            Err(error) => last = format!("连不上平台服务端：{error}"),
        }
    }
    bail!("回报生成结果失败（已重试 3 次）：{last}。请求会一直显示运行中，直到超时作废")
}

/// 一直轮询。出错只记日志、下一轮再试，不退出。
pub async fn serve(cfg: CasegenConfig, interval: Duration) {
    let generate = real_generate();
    loop {
        match poll_once(&cfg, &generate).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => tracing::warn!("处理用例生成请求出错：{error:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}
