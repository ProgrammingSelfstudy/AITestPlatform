//! 候选脚本的自检：在本节点的真机上把脚本跑一遍，通过则回报「自检通过」，失败则回报「已拒绝」。
//!
//! - **只在来源节点自检**：自检要占用本节点的手机，服务端的两个接口也要求脚本来自调用的节点。
//! - **不花模型额度**：自检只跑 pytest，不调用任何模型。
//! - **脚本缺陷和环境问题分开**：脚本自己的失败（断言不成立、导入错误等）才拒绝候选；
//!   前置状态或关房失败、自检超时、手机连不上这类环境问题只报错，脚本保持候选，
//!   不能因为手机掉线就把好脚本判死。
//! - **失败原样给人看**（规则 7）：拒绝时把 pytest 的原始输出末尾记进脚本的状态说明。
//! - **没有账号租约**：自检不领账号，假定手机上已经是登录好的目标账号（`--skip-login`）。
//!   手机没登录时前置状态会失败，按环境问题处理。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use uuid::Uuid;

use crate::setup_runner::{kill_group, SetupSpec};

/// 输出里保留的末尾字符数：足够看清失败原因；服务端状态说明总共 2000 字，还要给之后获准或下线的原因留空间。
const DETAIL_CHARS: usize = 1500;

pub struct CheckConfig {
    pub platform_http: Url,
    pub node_token: String,
    pub node_id: Uuid,
    pub python: String,
    pub automation_dir: PathBuf,
    pub audio_dir: PathBuf,
    pub state_dir: PathBuf,
    pub package: String,
    pub activity: String,
    pub serial: String,
    pub bottom_shift: i32,
    pub timeout: Duration,
    /// 放临时脚本目录的父目录
    pub work_dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Rejected,
}

#[derive(Deserialize)]
struct Source {
    version: i64,
    status: String,
    case_code: String,
    files: BTreeMap<String, String>,
}

pub(crate) fn authed(cfg: &CheckConfig, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    crate::with_node_secret(request.bearer_auth(&cfg.node_token).header("X-Node-Id", cfg.node_id.to_string()))
}

/// 脚本文件名只能是归档过的两种形状。服务端已校验过，这里再校验一次：不信任来源，路径不能逃出临时目录。
fn safe_relative(name: &str) -> bool {
    let path = Path::new(name);
    path.extension().is_some_and(|e| e == "py") && path.components().all(|c| matches!(c, Component::Normal(_))) && path.components().count() <= 2
}

/// 把脚本写成 pytest 能直接收集的 `generated/` 包，返回包所在的目录。
pub(crate) fn write_package(parent: &Path, files: &BTreeMap<String, String>) -> anyhow::Result<PathBuf> {
    let root = parent.join(format!("selfcheck_{}", Uuid::new_v4()));
    let package = root.join("generated");
    std::fs::create_dir_all(package.join("pages")).context("无法创建自检临时目录")?;
    std::fs::write(package.join("__init__.py"), "")?;
    std::fs::write(package.join("pages").join("__init__.py"), "")?;
    for (name, source) in files {
        if !safe_relative(name) {
            bail!("脚本里有不合规的文件名：{name}");
        }
        std::fs::write(package.join(name), source).with_context(|| format!("无法写入 {name}"))?;
    }
    Ok(root)
}

/// 环境问题的特征：这些失败发生在脚本本体之前或之后，和脚本对错无关。
pub(crate) fn environment_failure(output: &str) -> Option<&'static str> {
    ["准备前置状态失败", "关房失败"].into_iter().find(|marker| output.contains(marker))
}

pub(crate) fn tail(text: &str) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    chars[chars.len().saturating_sub(DETAIL_CHARS)..].iter().collect()
}

/// 给人看的结论说明：通过时是 pytest 的最后一行摘要，失败时是退出码加原始输出末尾（规则 7）。
pub(crate) fn describe(passed: bool, code: Option<i32>, output: &str, case_code: &str, version: i64) -> String {
    if passed {
        format!("{}（用例 {case_code}，版本 {version}）", tail(output).lines().last().unwrap_or("通过"))
    } else {
        format!("退出码 {}：\n{}", code.map_or_else(|| "未知".into(), |c| c.to_string()), tail(output))
    }
}

/// 跑 pytest，返回（退出码，合并后的输出）。超时杀整个进程组并报错。
pub(crate) async fn run_pytest(cfg: &CheckConfig, root: &Path) -> anyhow::Result<(Option<i32>, String)> {
    let mut spec = SetupSpec::new(&cfg.python, &cfg.automation_dir, &cfg.serial, &cfg.package, &cfg.activity);
    spec.bottom_shift = cfg.bottom_shift;
    spec.audio_dir = cfg.audio_dir.clone();
    spec.state_dir = cfg.state_dir.clone();
    let mut env = spec.env();
    env.extend([
        ("PYTHONPATH".into(), cfg.automation_dir.to_string_lossy().into_owned()),
        // 只加载我们指定的插件，不让环境里别的 pytest 插件混进来
        ("PYTEST_DISABLE_PLUGIN_AUTOLOAD".into(), "1".into()),
        ("HAFA_ARTIFACT_DIR".into(), root.join("artifacts").to_string_lossy().into_owned()),
    ]);
    let mut command = Command::new(&cfg.python);
    command
        .args(["-m", "pytest", "-p", "hafa_automation.pytest_plugin", "-p", "no:cacheprovider", "-q", "--strict-markers", "generated"])
        .current_dir(root)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().with_context(|| format!("无法启动 {}", cfg.python))?;
    let mut out = child.stdout.take().expect("已配置 piped");
    let mut err = child.stderr.take().expect("已配置 piped");
    let collect = async {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let _ = tokio::join!(out.read_to_end(&mut a), err.read_to_end(&mut b));
        let status = child.wait().await;
        (a, b, status)
    };
    match tokio::time::timeout(cfg.timeout, collect).await {
        Ok((a, b, status)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
            Ok((status.ok().and_then(|s| s.code()), text))
        }
        Err(_) => {
            // collect 被丢弃后 child 仍可用：杀掉整个进程组（含 adb、uiautomator 等子孙）
            kill_group(&child);
            let _ = child.wait().await;
            bail!("自检超过 {} 秒没有结束，已终止。按环境问题处理，脚本保持候选", cfg.timeout.as_secs())
        }
    }
}

/// 对 `script_id` 做一次真机自检并回报平台。返回结果和给人看的说明。
pub async fn self_check(cfg: &CheckConfig, script_id: Uuid) -> anyhow::Result<(Outcome, String)> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let url = |path: &str| cfg.platform_http.join(path).with_context(|| format!("无法拼出平台地址 {path}"));

    let response = authed(cfg, client.get(url(&format!("api/scripts/{script_id}/files"))?)).send().await.context("连不上平台服务端")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::FORBIDDEN => bail!("这个脚本不是本节点生成的，不能在这里自检"),
        StatusCode::NOT_FOUND => bail!("平台上没有这个脚本"),
        StatusCode::UNAUTHORIZED => bail!("节点令牌不对"),
        other => bail!("取脚本失败：平台返回 {other}"),
    }
    let source: Source = response.json().await.context("平台返回的脚本格式不对")?;
    // 先查状态再占手机：已经有结论的脚本不重复自检。
    if source.status != "candidate" {
        bail!("脚本版本 {} 的状态是 {}，只有候选才能自检", source.version, source.status);
    }

    std::fs::create_dir_all(&cfg.work_dir).context("无法创建自检工作目录")?;
    let root = write_package(&cfg.work_dir, &source.files)?;
    let result = run_pytest(cfg, &root).await;
    let _ = std::fs::remove_dir_all(&root);
    let (code, output) = result?;

    if let Some(marker) = environment_failure(&output) {
        bail!("{marker}，属于环境问题，脚本保持候选。原始输出：\n{}", tail(&output));
    }
    let passed = code == Some(0);
    let detail = describe(passed, code, &output, &source.case_code, source.version);

    let body = json!({"passed": passed, "detail": detail});
    let response = authed(cfg, client.put(url(&format!("api/scripts/{script_id}/self-check"))?)).json(&body).send().await.context("回报自检结果时连不上平台服务端")?;
    match response.status() {
        StatusCode::OK => Ok((if passed { Outcome::Passed } else { Outcome::Rejected }, detail)),
        StatusCode::CONFLICT => bail!("脚本状态已经变了，自检结果没有记录：{}", response.text().await.unwrap_or_default()),
        other => bail!("回报自检结果失败：平台返回 {other}"),
    }
}
