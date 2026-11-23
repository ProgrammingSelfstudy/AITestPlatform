//! 角色可用性检测：配完角色后，在节点本机真的探一下这个来源能不能用。
//!
//! - Claude / Gemini（agy）：先查本机登录（只接受会员 / 个人账号），再真调一次模型（一句极短的提示）。
//! - Codex：先查 `codex login status`（只接受 ChatGPT 登录），再用 `codex exec` 真调一次模型（只读沙箱、不落会话）。
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
        ModelSource::CodexExec => check_codex(codex_bin, model).await,
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

async fn check_codex(bin: &str, model: Option<&str>) -> Probe {
    // 登录状态：只接受 ChatGPT 登录（规则 1）
    let mut cmd = Command::new(bin);
    cmd.args(["login", "status"]).stdin(Stdio::null()).kill_on_drop(true);
    strip_api_keys(&mut cmd);
    let out = match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
        Err(_) => return probe(CheckStatus::Failed, "检查 Codex 登录超时（20 秒）"),
        Ok(Err(e)) => return probe(CheckStatus::Failed, format!("无法运行 codex 命令行：{e}")),
        Ok(Ok(out)) => out,
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return probe(CheckStatus::NotLoggedIn, short(&format!("未登录：{text}")));
    }
    if !text.contains("ChatGPT") {
        return probe(CheckStatus::NotLoggedIn, "不是 ChatGPT 登录（平台只接受本机会员登录，不使用 API Key）");
    }
    // 真调一次模型：只读沙箱、不落会话、推理强度压到最低，提示词走标准输入
    let dir = std::env::temp_dir().join(format!("hafa-probe-{}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return probe(CheckStatus::Failed, format!("无法准备临时目录：{e}"));
    }
    let last = dir.join("last.txt");
    let mut cmd = Command::new(bin);
    cmd.args(["exec", "--ignore-user-config", "--skip-git-repo-check", "--ephemeral", "-s", "read-only", "-c", "model_reasoning_effort=\"low\"", "-C"])
        .arg(&dir)
        .arg("--output-last-message")
        .arg(&last)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(model) = model {
        cmd.args(["-m", model]);
    }
    strip_api_keys(&mut cmd);
    let result = run_with_stdin(cmd, PROMPT, Duration::from_secs(120)).await;
    let reply = std::fs::read_to_string(&last).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Err(detail) => probe(CheckStatus::Failed, detail),
        Ok((true, _)) if !reply.trim().is_empty() => probe(CheckStatus::Ok, "已真实调用一次模型，响应正常"),
        Ok((true, _)) => probe(CheckStatus::Failed, "调用成功但模型没有返回内容"),
        Ok((false, stderr)) if claude_cli::looks_like_quota(&stderr) => probe(CheckStatus::QuotaExhausted, short(&stderr)),
        Ok((false, stderr)) => probe(CheckStatus::Failed, short(&format!("codex exec 失败：{stderr}"))),
    }
}

/// 剔除会绕过会员登录的环境变量
fn strip_api_keys(cmd: &mut Command) {
    for name in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
        cmd.env_remove(name);
    }
}

/// 启动子进程，把提示词写进标准输入，等它结束。返回（是否成功退出，stderr）；起不来或超时返回错误说明。
async fn run_with_stdin(mut cmd: Command, prompt: &str, limit: Duration) -> Result<(bool, String), String> {
    use tokio::io::AsyncWriteExt as _;
    let mut child = cmd.spawn().map_err(|e| format!("无法启动 codex 命令行：{e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
    }
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Err(_) => Err(format!("codex exec 超时（{} 秒），已结束进程", limit.as_secs())),
        Ok(Err(e)) => Err(format!("等待 codex 命令行出错：{e}")),
        Ok(Ok(out)) => Ok((out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())),
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

    /// 假 codex：`login status` 和 `exec` 的行为由脚本参数决定
    fn fake_codex(login: &str, exec_body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("probe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("codex.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = login ]; then {login}; fi\n\
                 out=\"\"; while [ $# -gt 0 ]; do if [ \"$1\" = --output-last-message ]; then out=\"$2\"; fi; shift; done\n{exec_body}\n"
            ),
        )
        .unwrap();
        (dir, script)
    }

    async fn run_codex(login: &str, exec_body: &str) -> Probe {
        let (dir, script) = fake_codex(login, exec_body);
        // 经 /bin/sh 调用，避免新建可执行文件被系统扫描变慢
        let wrapper = dir.join("codex");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec /bin/sh {} \"$@\"\n", script.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let p = check_with(ModelSource::CodexExec, Some("m"), &Default::default(), &Default::default(), wrapper.to_str().unwrap()).await;
        let _ = std::fs::remove_dir_all(&dir);
        p
    }

    #[tokio::test]
    async fn codex真调成功才算可用() {
        let p = run_codex("echo 'Logged in using ChatGPT'; exit 0", "echo OK > \"$out\"; exit 0").await;
        assert_eq!((p.status, p.detail.as_str()), (CheckStatus::Ok, "已真实调用一次模型，响应正常"));
    }

    #[tokio::test]
    async fn codex登录了但调用失败要报失败并带原始错误() {
        let p = run_codex("echo 'Logged in using ChatGPT'; exit 0", "echo '模型不存在: m' >&2; exit 1").await;
        assert_eq!(p.status, CheckStatus::Failed);
        assert!(p.detail.contains("模型不存在"), "{}", p.detail);
    }

    #[tokio::test]
    async fn codex额度用完单独分类() {
        let p = run_codex("echo 'Logged in using ChatGPT'; exit 0", "echo 'You have reached your usage limit' >&2; exit 1").await;
        assert_eq!(p.status, CheckStatus::QuotaExhausted, "{}", p.detail);
    }

    #[tokio::test]
    async fn codex调用成功但没有回复也不算可用() {
        let p = run_codex("echo 'Logged in using ChatGPT'; exit 0", "exit 0").await;
        assert_eq!(p.status, CheckStatus::Failed);
    }

    #[tokio::test]
    async fn codex用的是api_key登录时拒绝且不发起模型调用() {
        let p = run_codex("echo 'Logged in using an API key'; exit 0", "echo SHOULD_NOT_RUN > \"$out\"; exit 0").await;
        assert_eq!(p.status, CheckStatus::NotLoggedIn);
    }

    /// 对本机真实命令行跑一遍检测（默认忽略，会花极少量额度）：
    /// `cargo test -p agent --lib probe::tests::真实探测 -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn 真实探测() {
        let models = [(ModelSource::ClaudeCli, "claude-opus-5-5"), (ModelSource::Agy, "gemini-4-neon-high"), (ModelSource::CodexExec, "gpt-6.2-astra")];
        for (source, model) in models {
            let p = check(source, Some(model)).await;
            println!("{source:?}：{:?}｜{}", p.status, p.detail);
        }
    }

    #[test]
    fn 过长的原始错误被截断() {
        let long = "错".repeat(1000);
        let s = short(&long);
        assert!(s.chars().count() < 320 && s.contains("已截断"));
    }
}
