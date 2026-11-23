//! 按版本回归：取某用例当前**已获准**的脚本，在本节点的真机上跑一遍，把结果记到平台。
//!
//! 和自检的区别：
//! - 自检决定一个候选能不能用，失败会改脚本的状态；回归不改脚本状态，**失败是回归的结论**
//!   （产品退化，或者脚本随界面改版失效了），如实记下，由人判断是哪一种。
//! - 回归不要求脚本来自本节点：获准的脚本是项目资产，任何认证节点都可以在自己的设备上跑。
//! - 只记脚本本身跑出的结论；设备掉线、前置状态或关房失败、超时是环境问题，只报错，不记。
//! - 不花模型额度，只跑 pytest。没有账号租约，手机上要已经登录好目标账号。

use std::time::Instant;

use anyhow::{bail, Context};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::selfcheck_run::{authed, describe, environment_failure, run_pytest, scrub, tail, write_package, CheckConfig};

#[derive(Debug)]
pub struct Outcome {
    pub version: i64,
    pub passed: bool,
    pub detail: String,
}

#[derive(Deserialize)]
struct Approved {
    id: Uuid,
    version: i64,
    case_code: String,
    files: std::collections::BTreeMap<String, String>,
}

/// 回归 `project` 里的 `case_code`，用它当前已获准的版本。
pub async fn regress(cfg: &CheckConfig, project: &str, case_code: &str) -> anyhow::Result<Outcome> {
    regress_script(cfg, project, case_code, None).await
}

/// 同 [`regress`]；`expect` 给了就必须正好是这个脚本：页面请求回归的是排队时那个获准版本，
/// 排队期间换了版本就不回归，免得把「另一个版本的结果」记成人点的那次。
pub async fn regress_script(cfg: &CheckConfig, project: &str, case_code: &str, expect: Option<Uuid>) -> anyhow::Result<Outcome> {
    let client = crate::platform_client(std::time::Duration::from_secs(30))?;
    let mut url = cfg.platform_http.clone();
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("平台地址不能带路径"))?.pop_if_empty().extend(["api", "projects", project, "cases", case_code, "approved-script"]);
    let response = authed(cfg, client.get(url)).send().await.context("连不上平台服务端")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => bail!("用例 {case_code} 没有已获准的脚本，没有可回归的版本（先自检并获准一个）"),
        StatusCode::UNAUTHORIZED => bail!("节点令牌不对"),
        other => bail!("取已获准脚本失败：平台返回 {other}"),
    }
    let script: Approved = response.json().await.context("平台返回的脚本格式不对")?;
    if let Some(expect) = expect.filter(|expect| *expect != script.id) {
        bail!("请求回归的脚本 {expect} 已不是用例 {case_code} 当前的获准版本（现在是版本 {}），没有回归", script.version);
    }

    std::fs::create_dir_all(&cfg.work_dir).context("无法创建回归工作目录")?;
    let root = write_package(&cfg.work_dir, &script.files)?;
    let started = Instant::now();
    let result = run_pytest(cfg, &root).await;
    let _ = std::fs::remove_dir_all(&root);
    let (code, output) = result?;
    let elapsed = started.elapsed();

    if let Some(marker) = environment_failure(&output) {
        bail!("{marker}，属于环境问题，不记录这次回归。原始输出：\n{}", scrub(cfg, &tail(&output)));
    }
    let passed = code == Some(0);
    let detail = scrub(cfg, &describe(passed, code, &output, &script.case_code, script.version));
    let body = json!({"serial": cfg.serial, "passed": passed, "detail": detail, "duration_ms": elapsed.as_millis().min(i32::MAX as u128) as u64});
    let mut report = cfg.platform_http.clone();
    report.path_segments_mut().map_err(|_| anyhow::anyhow!("平台地址不能带路径"))?.pop_if_empty().extend(["api", "scripts", &script.id.to_string(), "regressions"]);
    let response = authed(cfg, client.put(report)).json(&body).send().await.context("回报回归结果时连不上平台服务端")?;
    match response.status() {
        StatusCode::CREATED => Ok(Outcome { version: script.version, passed, detail }),
        StatusCode::CONFLICT => bail!("脚本在回归期间被下线了，结果没有记录：{}", response.text().await.unwrap_or_default()),
        other => bail!("回报回归结果失败：平台返回 {other}"),
    }
}
