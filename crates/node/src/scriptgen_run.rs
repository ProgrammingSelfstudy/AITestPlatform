//! 节点侧脚本生成：用本节点的一次「Judge 整体通过」的执行生成回归脚本候选，上传到服务端归档。
//!
//! - **谁发起谁付额度（规则 9）**：生成要调 Claude 会员命令行，所以只在执行这次 run 的节点上跑。
//!   轨迹只读本节点本地的 `runs_dir/<run_id>/trace.json`，没有就报错，不去别处找；
//!   服务端的两个接口也都要求这次执行属于调用的节点。
//! - **不使用 API Key，额度用完就停（规则 1）**：额度用完立即返回错误并如实告知，不改用其他计费方式。
//! - **模型来自角色配置（规则 3）**：用这次任务里 Judge 的角色配置，代码里不写死模型名；
//!   角色不是 `claude_cli` 就明确报错，不悄悄换来源。
//! - **不浪费额度**：调用模型之前先做资格预检，最终结果不是通过的执行不会送模型。
//! - 脱敏：轨迹落盘时已按登记的凭证脱敏；执行结束后登记值不再保留，所以这里只剩生成核心自带的
//!   形态过滤（邮箱、令牌、绝对路径、设备序列号）作为第二层，没有第三层。

use std::path::{Path, PathBuf};

use agent::claude_cli::ClaudeOpts;
use agent::scriptgen::{PreviousAttempt, ScriptTrace, ScriptgenError, ScriptgenInput, ScriptgenOpts, builtin_contract, generate};
use anyhow::{Context, bail};
use protocol::{CaseSpec, FinalResult, JudgeItem, ModelSource, RoleModel};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct GenConfig {
    pub platform_http: Url,
    pub node_token: String,
    pub node_id: Uuid,
    pub runs_dir: PathBuf,
    /// 命令行路径和时限。模型和强度不在这里，由任务里的 Judge 角色决定。
    pub claude: ClaudeOpts,
    /// 被测应用的界面元素表，没有就传 `Value::Null`
    pub ui_map: Value,
    /// 修正模式：基于这条被自检拒绝的脚本和它的失败输出重新生成
    pub fix_from: Option<Uuid>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 新归档的候选
    Created { id: Uuid, version: i64 },
    /// 同样内容之前已经归档过，指回已有版本
    Existing { id: Uuid, version: i64 },
}

/// 服务端给节点的来源视图（`GET /api/runs/{id}/script-source`）。
#[derive(Deserialize)]
struct Source {
    final_result: Option<FinalResult>,
    judge_status: Option<String>,
    case_spec: CaseSpec,
    judge_items: Vec<JudgeItem>,
    /// 这次任务里 Judge 的角色配置，生成角色据此解析
    judge: RoleModel,
}

fn authed(cfg: &GenConfig, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    crate::with_node_secret(request.bearer_auth(&cfg.node_token).header("X-Node-Id", cfg.node_id.to_string()))
}

fn url(cfg: &GenConfig, path: &str) -> anyhow::Result<Url> {
    cfg.platform_http.join(path).with_context(|| format!("无法拼出平台地址 {path}"))
}

/// 读本节点本地的轨迹。没有就报错：只能在执行这次 run 的节点上生成。
pub(crate) fn read_trace(runs_dir: &Path, run_id: Uuid) -> anyhow::Result<ScriptTrace> {
    let path = runs_dir.join(run_id.to_string()).join("trace.json");
    let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("本节点没有这次执行的轨迹文件（{}）：{e}。脚本只能在执行它的节点上生成", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("轨迹文件格式不对：{}", path.display()))
}

/// 把生成核心的错误转成给人看的中文。额度用完单独说明，且不提供换计费方式的出路。
fn explain(error: ScriptgenError) -> anyhow::Error {
    match error {
        ScriptgenError::QuotaExhausted => anyhow::anyhow!("Claude 会员额度已用完，已暂停；不会改用其他计费方式。额度恢复后再重试"),
        ScriptgenError::Violations(violations) => {
            let shown: Vec<String> = violations.iter().take(8).map(|v| format!("{} 第 {} 行，规则 {}：{}", v.file, v.line, v.rule, v.message)).collect();
            let more = violations.len().saturating_sub(shown.len());
            anyhow::anyhow!("生成的脚本没有通过静态校验，不会上传：\n{}{}", shown.join("\n"), if more > 0 { format!("\n……还有 {more} 处") } else { String::new() })
        }
        other => anyhow::anyhow!("生成脚本失败：{other}"),
    }
}

#[derive(Deserialize)]
struct PreviousSource {
    status: String,
    status_note: Option<String>,
    case_code: String,
    files: std::collections::BTreeMap<String, String>,
}

/// 取被自检拒绝的上一版：必须是已拒绝、有失败输出、且是同一个用例，否则不带它去花额度。
async fn fetch_previous(cfg: &GenConfig, client: &reqwest::Client, id: Uuid, case_code: &str) -> anyhow::Result<PreviousAttempt> {
    let response = authed(cfg, client.get(url(cfg, &format!("api/scripts/{id}/files"))?)).send().await.context("连不上平台服务端")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::FORBIDDEN => bail!("要修正的脚本不是本节点生成的"),
        StatusCode::NOT_FOUND => bail!("平台上没有要修正的脚本"),
        other => bail!("取要修正的脚本失败：平台返回 {other}"),
    }
    let previous: PreviousSource = response.json().await.context("平台返回的脚本格式不对")?;
    if previous.status != "rejected" {
        bail!("要修正的脚本状态是 {}，只有被自检拒绝的脚本才需要按失败输出修正", previous.status);
    }
    if previous.case_code != case_code {
        bail!("要修正的脚本属于用例 {}，和这次执行的用例 {case_code} 不是同一个", previous.case_code);
    }
    let failure = previous.status_note.filter(|n| !n.trim().is_empty()).context("要修正的脚本没有记录失败输出")?;
    Ok(PreviousAttempt { files: previous.files, failure })
}

/// 为 `run_id` 生成回归脚本候选并上传归档。
pub async fn generate_and_upload(cfg: &GenConfig, run_id: Uuid) -> anyhow::Result<Outcome> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;

    // 1. 取来源：完整用例、最终结果、逐条判定、Judge 角色。
    let response = authed(cfg, client.get(url(cfg, &format!("api/runs/{run_id}/script-source"))?)).send().await.context("连不上平台服务端")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::FORBIDDEN => bail!("这次执行不属于本节点，不能在这里生成脚本（谁发起谁付额度）"),
        StatusCode::NOT_FOUND => bail!("平台上没有这次执行"),
        StatusCode::UNAUTHORIZED => bail!("节点令牌不对"),
        other => bail!("取脚本来源失败：平台返回 {other}"),
    }
    let source: Source = response.json().await.context("平台返回的脚本来源格式不对")?;

    // 2. 资格预检：不合格的执行不送模型，不花额度。服务端归档时还会再查一遍。
    if source.judge_status.as_deref() != Some("completed") {
        bail!("这次执行的 Judge 没有完成，不能生成脚本");
    }
    if source.final_result != Some(FinalResult::Passed) {
        bail!("这次执行的最终结果不是通过，不能生成脚本");
    }

    // 3. 生成角色：用这次任务里 Judge 的角色配置，不写死模型，也不换来源。
    if source.judge.source != ModelSource::ClaudeCli {
        bail!("这次任务的 Judge 用的不是 claude_cli，脚本生成只支持 Claude 会员命令行，不会改用其他来源");
    }
    let previous = match cfg.fix_from {
        Some(id) => Some(fetch_previous(cfg, &client, id, &source.case_spec.code).await?),
        None => None,
    };
    let trace = read_trace(&cfg.runs_dir, run_id)?;
    let contract = builtin_contract();
    let opts = ScriptgenOpts { role: source.judge.clone(), claude: cfg.claude.clone() };
    let generated = generate(
        &opts,
        ScriptgenInput {
            case: &source.case_spec,
            trace: &trace,
            judge_result: FinalResult::Passed,
            judge_items: &source.judge_items,
            ui_map: &cfg.ui_map,
            contract: &contract,
            redactions: &[],
            previous_attempt: previous.as_ref(),
        },
    )
    .await
    .map_err(explain)?;
    tracing::info!("脚本候选生成完成：{} 个文件", generated.files.len());

    // 4. 上传归档。
    let body = json!({
        "files": generated.files,
        "sha256": generated.sha256,
        "notes": generated.notes,
        "source": "claude_cli",
        "model": generated.model,
        "effort": generated.effort,
    });
    let response = authed(cfg, client.put(url(cfg, &format!("api/runs/{run_id}/scripts"))?)).json(&body).send().await.context("上传脚本时连不上平台服务端")?;
    let status = response.status();
    match status {
        StatusCode::CREATED | StatusCode::OK => {
            let record: Value = response.json().await.context("平台返回的归档结果格式不对")?;
            let id: Uuid = record["id"].as_str().and_then(|s| s.parse().ok()).context("归档结果里没有脚本编号")?;
            let version = record["version"].as_i64().context("归档结果里没有版本号")?;
            Ok(if status == StatusCode::CREATED { Outcome::Created { id, version } } else { Outcome::Existing { id, version } })
        }
        StatusCode::UNPROCESSABLE_ENTITY => bail!("平台拒绝归档：{}", response.text().await.unwrap_or_default()),
        other => bail!("上传脚本失败：平台返回 {other}"),
    }
}
