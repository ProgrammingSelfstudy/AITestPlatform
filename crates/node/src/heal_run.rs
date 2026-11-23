//! 自愈：已获准的脚本回归失败后，在发起人自己的节点上分诊，只有确定是「定位失效」才产出修复候选。
//!
//! - **只分诊失败的回归**：最近一次回归必须是失败，且这次失败没有分诊过（同一次失败只花一次额度）。
//! - **先拿新证据**：在本节点手机上把脚本重跑一遍，取失败那一刻的控件文字。重跑通过说明原失败可能是偶发，
//!   记为「分不清」，不改任何东西；设备掉线、前置失败这类环境问题不记任何分诊，只报错。
//! - **谁发起谁付额度（规则 9）、不使用 API Key（规则 1）**：模型调用用来源执行那次任务里 Judge 的角色配置和本机会员命令行，
//!   脚本和轨迹都只在来源节点上取；额度用完立即停，不记分诊，不改用其他计费方式。
//! - **修复只产出候选**：分诊结论经服务端归档，修复版是一个新的候选版本（记下来源版本），要重新自检、人工获准；
//!   获准的原版本在人决定之前不动。业务行为错误和分不清不改脚本，失败的回归记录原样保留。

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::claude_cli::ClaudeOpts;
use agent::scriptgen::{HealContext, HealVerdict, ScriptgenError, ScriptgenInput, ScriptgenOpts, builtin_contract, heal};
use anyhow::{Context, bail};
use protocol::{CaseSpec, FinalResult, JudgeItem, ModelSource, RoleModel};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::scriptgen_run::read_trace;
use crate::selfcheck_run::{authed, environment_failure, run_pytest, tail, write_package, CheckConfig};

pub struct HealConfig {
    pub check: CheckConfig,
    pub runs_dir: PathBuf,
    /// 命令行路径和时限。模型和强度由来源任务里 Judge 的角色决定。
    pub claude: ClaudeOpts,
    /// 被测应用的界面元素表，没有就传 `Value::Null`
    pub ui_map: Value,
}

#[derive(Debug)]
pub struct Report {
    pub verdict: HealVerdict,
    pub reason: String,
    /// 修复候选（脚本编号和版本），只有「定位失效」才有
    pub healed: Option<(Uuid, i64)>,
}

#[derive(Deserialize)]
struct Failure {
    regression_id: Uuid,
    passed: bool,
    triaged: bool,
}

#[derive(Deserialize)]
struct HealSource {
    status: String,
    version: i64,
    source_run_id: Uuid,
    files: std::collections::BTreeMap<String, String>,
    regression: Option<Failure>,
}

#[derive(Deserialize)]
struct RunSource {
    final_result: Option<FinalResult>,
    judge_status: Option<String>,
    case_spec: CaseSpec,
    judge_items: Vec<JudgeItem>,
    judge: RoleModel,
}

/// 失败现场的控件文字上限：够模型判断，又不把整页塞进去。
const ELEMENTS_LIMIT: usize = 6000;

/// 读失败那一刻保存的控件文字（插件写的 `*-elements.txt`），没有就返回空串。
fn read_elements(root: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(root.join("artifacts")) else { return String::new() };
    let mut names: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with("-elements.txt")).collect();
    names.sort();
    // 取用例本体失败（call）那份；没有就取第一份
    let chosen = names.iter().find(|p| p.to_string_lossy().contains("-call-")).or(names.first());
    let text = chosen.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.chars().take(ELEMENTS_LIMIT).collect()
}

fn explain(error: ScriptgenError) -> anyhow::Error {
    match error {
        ScriptgenError::QuotaExhausted => anyhow::anyhow!("Claude 会员额度已用完，已暂停；这次失败没有分诊，不会改用其他计费方式。额度恢复后再重试"),
        other => anyhow::anyhow!("自愈分诊失败：{other}。这次失败没有分诊"),
    }
}

fn get_json_error(status: StatusCode, what: &str) -> anyhow::Error {
    match status {
        StatusCode::FORBIDDEN => anyhow::anyhow!("{what}：这个脚本不是本节点生成的，不能在这里自愈（谁发起谁付额度）"),
        StatusCode::NOT_FOUND => anyhow::anyhow!("{what}：平台上没有这个脚本或执行"),
        StatusCode::UNAUTHORIZED => anyhow::anyhow!("节点令牌不对"),
        other => anyhow::anyhow!("{what}：平台返回 {other}"),
    }
}

/// 对已获准的 `script_id` 做一次自愈分诊并把结论回报平台。
pub async fn heal_script(cfg: &HealConfig, script_id: Uuid) -> anyhow::Result<Report> {
    let check = &cfg.check;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let url = |path: &str| check.platform_http.join(path).with_context(|| format!("无法拼出平台地址 {path}"));

    // 1. 取脚本和最近一次回归，先把不该花额度的情况都挡掉。
    let response = authed(check, client.get(url(&format!("api/scripts/{script_id}/heal-source"))?)).send().await.context("连不上平台服务端")?;
    if response.status() != StatusCode::OK {
        return Err(get_json_error(response.status(), "取脚本失败"));
    }
    let script: HealSource = response.json().await.context("平台返回的脚本格式不对")?;
    if script.status != "approved" {
        bail!("脚本版本 {} 的状态是 {}，只有已获准的版本才需要自愈", script.version, script.status);
    }
    let failure = match script.regression {
        None => bail!("脚本版本 {} 还没有回归记录，没有可分诊的失败", script.version),
        Some(f) if f.passed => bail!("脚本版本 {} 最近一次回归是通过的，不需要自愈", script.version),
        Some(f) if f.triaged => bail!("脚本版本 {} 最近一次失败已经分诊过了；再回归一次得到新的失败后才能再分诊", script.version),
        Some(f) => f,
    };

    // 2. 来源执行：用例、判定证据、角色。
    let response = authed(check, client.get(url(&format!("api/runs/{}/script-source", script.source_run_id))?)).send().await.context("连不上平台服务端")?;
    if response.status() != StatusCode::OK {
        return Err(get_json_error(response.status(), "取来源执行失败"));
    }
    let source: RunSource = response.json().await.context("平台返回的脚本来源格式不对")?;
    if source.judge_status.as_deref() != Some("completed") || source.final_result != Some(FinalResult::Passed) {
        bail!("脚本的来源执行不是 Judge 整体通过，不能据此自愈");
    }
    if source.judge.source != ModelSource::ClaudeCli {
        bail!("来源任务的 Judge 用的不是 claude_cli，自愈只支持 Claude 会员命令行，不会改用其他来源");
    }
    let trace = read_trace(&cfg.runs_dir, script.source_run_id)?;

    // 3. 新证据：在本节点手机上重跑一遍。
    std::fs::create_dir_all(&check.work_dir).context("无法创建自愈工作目录")?;
    let root = write_package(&check.work_dir, &script.files)?;
    let rerun = run_pytest(check, &root).await;
    let elements = read_elements(&root);
    let _ = std::fs::remove_dir_all(&root);
    let (code, output) = rerun?;
    if let Some(marker) = environment_failure(&output) {
        bail!("{marker}，属于环境问题，这次失败没有分诊。原始输出：\n{}", tail(&output));
    }

    // 4. 分诊。重跑通过就不花额度：原失败可能是偶发，交人工。
    let (verdict, reason, fix) = if code == Some(0) {
        (HealVerdict::Unsure, "重新运行这个脚本通过了，原来的失败可能是偶发或环境波动，没有修改脚本，请人判断是否需要再观察".to_string(), None)
    } else {
        let contract = builtin_contract();
        let opts = ScriptgenOpts { role: source.judge.clone(), claude: cfg.claude.clone() };
        let context = HealContext { files: &script.files, failure: &tail(&output), elements: &elements };
        let result = heal(
            &opts,
            ScriptgenInput {
                case: &source.case_spec,
                trace: &trace,
                judge_result: FinalResult::Passed,
                judge_items: &source.judge_items,
                ui_map: &cfg.ui_map,
                contract: &contract,
                redactions: &[],
                previous_attempt: None,
            },
            &context,
        )
        .await
        .map_err(explain)?;
        (result.verdict, result.reason, result.fix)
    };

    // 5. 回报结论；定位失效时带上修复文件，由服务端归档成候选。
    let mut body = json!({"regression_id": failure.regression_id, "verdict": verdict.as_str(), "reason": reason});
    if let Some(fix) = &fix {
        body["files"] = json!(fix.files);
        body["sha256"] = json!(fix.sha256);
        body["notes"] = json!(fix.notes);
        body["source"] = json!("claude_cli");
        body["model"] = json!(fix.model);
        body["effort"] = json!(fix.effort);
    }
    let response = authed(check, client.put(url(&format!("api/scripts/{script_id}/triage"))?)).json(&body).send().await.context("回报分诊结论时连不上平台服务端")?;
    match response.status() {
        StatusCode::CREATED => {
            let reply: Value = response.json().await.context("平台返回的分诊结果格式不对")?;
            let healed = match reply["healed"].as_object() {
                Some(record) => Some((
                    record["id"].as_str().and_then(|s| s.parse().ok()).context("分诊结果里没有修复脚本编号")?,
                    record["version"].as_i64().context("分诊结果里没有版本号")?,
                )),
                None => None,
            };
            Ok(Report { verdict, reason, healed })
        }
        StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => bail!("平台没有接受分诊结论：{}", response.text().await.unwrap_or_default()),
        other => bail!("回报分诊结论失败：平台返回 {other}"),
    }
}
