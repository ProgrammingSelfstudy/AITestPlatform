//! 角色可用性检测：配完角色后，在节点本机真的探一下这个来源能不能用。
//!
//! - Claude / Gemini（agy）：先查本机登录（只接受会员 / 个人账号），再真调一次模型（一句极短的提示）。
//! - Codex：只查 `codex login status`（只接受 ChatGPT 登录），**没有发起模型调用**，结果说明里如实写出来。
//! - DeepSeek：运行器还在开发中，如实报失败，不假装可用。
//!
//! 失败时给用户看原始错误（截断到 300 字），不得带 Key；这里没有任何路径会读到 Key。

use std::process::Stdio;
use std::time::Duration;

use protocol::{CheckStatus, ModelSource};
use tokio::process::Command;

use crate::{agy_cli, claude_cli};

const PROMPT: &str = "这是一次可用性检测。请只回复两个字母：OK";
const MAX_DETAIL: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub status: CheckStatus,
    pub detail: String,
}

fn short(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_DETAIL {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_DETAIL).collect();
    format!("{cut}…（已截断）")
}

fn probe(status: CheckStatus, detail: impl Into<String>) -> Probe {
    Probe { status, detail: detail.into() }
}

/// 探测一个来源 + 模型。`model` 来自角色配置，代码里不写死。
pub async fn check(source: ModelSource, model: Option<&str>) -> Probe {
    check_with(source, model, &claude_cli::ClaudeOpts::default(), &agy_cli::AgyOpts::default(), "codex").await
}

/// 命令行路径可注入，测试里换成假脚本。
pub async fn check_with(source: ModelSource, model: Option<&str>, claude: &claude_cli::ClaudeOpts, agy: &agy_cli::AgyOpts, codex_bin: &str) -> Probe {
    match source {
        ModelSource::ClaudeCli => check_claude(model, claude).await,
        ModelSource::Agy => check_agy(model, agy).await,
        ModelSource::CodexExec => check_codex(codex_bin).await,
        ModelSource::DeepSeek => probe(CheckStatus::Failed, "DeepSeek 的运行器还在开发中，暂时不能检测"),
    }
}

async fn check_claude(model: Option<&str>, base: &claude_cli::ClaudeOpts) -> Probe {
    if let Err(reason) = claude_cli::check_login(base).await {
        return probe(CheckStatus::NotLoggedIn, reason);
    }
    let opts = claude_cli::ClaudeOpts { model: model.map(str::to_owned), total_timeout: Duration::from_secs(90), ..base.clone() };
    let out = claude_cli::run_text_only(&opts, PROMPT).await;
    match out.failure {
        None if !out.text.trim().is_empty() => probe(CheckStatus::Ok, "已真实调用一次模型，响应正常"),
        None => probe(CheckStatus::Failed, "调用成功但模型没有返回内容"),
        Some(claude_cli::Failure::Quota(m)) => probe(CheckStatus::QuotaExhausted, short(&m)),
        Some(claude_cli::Failure::Auth(m)) => probe(CheckStatus::NotLoggedIn, short(&m)),
        Some(claude_cli::Failure::Exit { code, stderr }) => probe(CheckStatus::Failed, short(&format!("命令行异常退出（退出码 {code:?}）：{stderr}"))),
        Some(claude_cli::Failure::Result { message, .. }) => probe(CheckStatus::Failed, short(&message)),
        Some(other) => probe(CheckStatus::Failed, short(&format!("{other:?}"))),
    }
}

async fn check_agy(model: Option<&str>, base: &agy_cli::AgyOpts) -> Probe {
    if let Err(reason) = agy_cli::check_login(base).await {
        return probe(CheckStatus::NotLoggedIn, reason);
    }
    let opts = agy_cli::AgyOpts { model: model.map(str::to_owned), total_timeout: Duration::from_secs(120), ..base.clone() };
    let out = agy_cli::run(&opts, PROMPT).await;
    match out.failure {
        None if !out.violations.is_empty() || !out.denied.is_empty() => probe(CheckStatus::Failed, "模型在检测里调用了不该用的工具，已拦下"),
        None if !out.text.trim().is_empty() => probe(CheckStatus::Ok, "已真实调用一次模型，响应正常"),
        None => probe(CheckStatus::Failed, "调用成功但模型没有返回内容"),
        Some(agy_cli::Failure::Quota(m)) => probe(CheckStatus::QuotaExhausted, short(&m)),
        Some(agy_cli::Failure::Auth(m)) => probe(CheckStatus::NotLoggedIn, short(&m)),
        Some(other) => probe(CheckStatus::Failed, short(&format!("{other:?}"))),
    }
}

async fn check_codex(bin: &str) -> Probe {
    let mut cmd = Command::new(bin);
    cmd.args(["login", "status"]).stdin(Stdio::null()).kill_on_drop(true);
    // 剔除会绕过会员登录的环境变量
    for name in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
        cmd.env_remove(name);
    }
    let out = match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
        Err(_) => return probe(CheckStatus::Failed, "检查 Codex 登录超时（20 秒）"),
        Ok(Err(e)) => return probe(CheckStatus::Failed, format!("无法运行 codex 命令行：{e}")),
        Ok(Ok(out)) => out,
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() && text.contains("ChatGPT") {
        probe(CheckStatus::Ok, "已用 ChatGPT 登录（只检查了登录状态，没有发起模型调用）")
    } else if out.status.success() {
        probe(CheckStatus::NotLoggedIn, "不是 ChatGPT 登录（平台只接受本机会员登录，不使用 API Key）")
    } else {
        probe(CheckStatus::NotLoggedIn, short(&format!("未登录：{text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deepseek还没实现时如实报失败() {
        let p = check(ModelSource::DeepSeek, Some("m")).await;
        assert_eq!(p.status, CheckStatus::Failed);
        assert!(p.detail.contains("开发中"));
    }

    #[tokio::test]
    async fn codex命令不存在时报失败并给出原因() {
        let p = check_with(ModelSource::CodexExec, None, &Default::default(), &Default::default(), "/不存在的/codex").await;
        assert_eq!(p.status, CheckStatus::Failed);
        assert!(p.detail.contains("无法运行"));
    }

    #[test]
    fn 过长的原始错误被截断() {
        let long = "错".repeat(1000);
        let s = short(&long);
        assert!(s.chars().count() < 320 && s.contains("已截断"));
    }
}
