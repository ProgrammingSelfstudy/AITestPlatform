//! 本机应用环境检查：装没装、版本、怎么登录的、有哪些模型。
//!
//! **只跑本地命令，不调模型、不花额度。** 登录只接受会员 / 个人账号（规则 1）：
//! - Claude：`claude auth status`（会员登录才算通过）；命令行**没有列模型的命令**，所以给别名建议，也允许手填完整模型名，
//!   配完靠「检测」真调一次验证。
//! - Codex：`codex login status`（只接受 ChatGPT 登录）；模型来自 `codex debug models` 的完整目录。
//! - Gemini（agy）：个人账号登录检查；模型来自 `agy models`（模型名里已带推理强度）。
//! - DeepSeek：没有命令行，用 API Key，运行器还在开发中，不检查、不列模型。
//!
//! 返回的说明里不放账号信息（邮箱、组织名等）。

use std::process::Stdio;
use std::time::Duration;

use protocol::{AppLogin, AppModel, AppScan, ModelSource};
use serde_json::Value;
use tokio::process::Command;

use crate::{agy_cli, claude_cli};

const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(40);

/// Claude 命令行的 `--model` 接受别名（指向该系列最新的模型）。命令行不能列出模型，这里只是建议，用户也能手填完整模型名。
const CLAUDE_ALIASES: [(&str, &str); 3] = [("opus", "Opus（最新）"), ("sonnet", "Sonnet（最新）"), ("haiku", "Haiku（最新）")];

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn base(source: ModelSource) -> AppScan {
    AppScan {
        source,
        installed: false,
        version: None,
        login: AppLogin::NotLoggedIn,
        login_detail: String::new(),
        models: vec![],
        models_note: String::new(),
        scanned_at: now(),
    }
}

/// 跑一个命令拿标准输出。找不到命令返回 `None`（没装）；其它失败返回 `Some(Err)`。
async fn run(bin: &str, args: &[&str], limit: Duration) -> Option<Result<(bool, String), String>> {
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    // 剔除会绕过会员登录的环境变量
    for name in ["OPENAI_API_KEY", "CODEX_API_KEY", "ANTHROPIC_API_KEY"] {
        cmd.env_remove(name);
    }
    match tokio::time::timeout(limit, cmd.output()).await {
        Err(_) => Some(Err(format!("运行 {bin} 超时（{} 秒）", limit.as_secs()))),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Ok(Err(e)) => Some(Err(format!("无法运行 {bin}：{e}"))),
        Ok(Ok(out)) => Some(Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))),
    }
}

fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(80).collect()
}

/// `codex debug models` 的目录：只取对用户可见（`visibility == "list"`）的，按优先级排序。
pub fn parse_codex_catalog(json: &str) -> Result<Vec<AppModel>, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("模型目录不是合法 JSON：{e}"))?;
    let list = value.get("models").and_then(Value::as_array).or_else(|| value.as_array()).ok_or("模型目录里没有 models 列表")?;
    let mut models: Vec<(i64, AppModel)> = list
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter_map(|m| {
            let id = m.get("slug").and_then(Value::as_str)?.to_string();
            let display = m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let efforts = m
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|l| l.get("effort").and_then(Value::as_str).map(str::to_owned)).collect())
                .unwrap_or_default();
            let default_effort = m.get("default_reasoning_level").and_then(Value::as_str).map(str::to_owned);
            Some((m.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX), AppModel { id, display, efforts, default_effort }))
        })
        .collect();
    models.sort_by_key(|(priority, _)| *priority);
    Ok(models.into_iter().map(|(_, m)| m).collect())
}

/// `agy models` 的输出：第一行是提示，其余每行「模型名<TAB>显示名」。
pub fn parse_agy_models(text: &str) -> Vec<AppModel> {
    text.lines()
        .filter_map(|line| {
            let (id, display) = line.split_once('\t')?;
            let id = id.trim();
            (!id.is_empty() && !id.contains(' ')).then(|| AppModel { id: id.into(), display: display.trim().into(), efforts: vec![], default_effort: None })
        })
        .collect()
}

pub async fn scan(source: ModelSource) -> AppScan {
    scan_with(source, &Bins::default()).await
}

/// 命令名可注入，测试里换成假脚本。
#[derive(Clone)]
pub struct Bins {
    pub claude: String,
    pub codex: String,
    pub agy: String,
}

impl Default for Bins {
    fn default() -> Self {
        Self { claude: "claude".into(), codex: "codex".into(), agy: "agy".into() }
    }
}

pub async fn scan_with(source: ModelSource, bins: &Bins) -> AppScan {
    match source {
        ModelSource::ClaudeCli => scan_claude(bins).await,
        ModelSource::CodexExec => scan_codex(bins).await,
        ModelSource::Agy => scan_agy(bins).await,
        ModelSource::DeepSeek => AppScan {
            login: AppLogin::NotApplicable,
            login_detail: "DeepSeek 用 API Key，没有本机登录；运行器还在开发中，暂时不能配置".into(),
            models_note: "DeepSeek 的运行器还在开发中".into(),
            ..base(source)
        },
    }
}

async fn scan_claude(bins: &Bins) -> AppScan {
    let mut scan = base(ModelSource::ClaudeCli);
    match run(&bins.claude, &["--version"], VERSION_TIMEOUT).await {
        None => {
            scan.login_detail = "本机没有找到 claude 命令行，请先安装 Claude Code".into();
            return scan;
        }
        Some(Err(e)) => {
            scan.login_detail = e;
            return scan;
        }
        Some(Ok((_, text))) => {
            scan.installed = true;
            scan.version = Some(first_line(&text));
        }
    }
    let opts = claude_cli::ClaudeOpts { bin: bins.claude.clone(), ..Default::default() };
    match claude_cli::check_login(&opts).await {
        Ok(()) => {
            scan.login = AppLogin::Ok;
            scan.login_detail = "已用 Claude 会员登录".into();
        }
        Err(reason) => scan.login_detail = reason.to_string(),
    }
    scan.models = CLAUDE_ALIASES.iter().map(|(id, display)| AppModel { id: (*id).into(), display: (*display).into(), efforts: vec![], default_effort: None }).collect();
    scan.models_note = "Claude 命令行不能列出模型：这里是别名建议（指向该系列最新的模型），也可以手填完整模型名；配完点「检测」真调一次来验证".into();
    scan
}

async fn scan_codex(bins: &Bins) -> AppScan {
    let mut scan = base(ModelSource::CodexExec);
    match run(&bins.codex, &["--version"], VERSION_TIMEOUT).await {
        None => {
            scan.login_detail = "本机没有找到 codex 命令行，请先安装 Codex CLI".into();
            return scan;
        }
        Some(Err(e)) => {
            scan.login_detail = e;
            return scan;
        }
        Some(Ok((_, text))) => {
            scan.installed = true;
            scan.version = Some(first_line(&text));
        }
    }
    match run(&bins.codex, &["login", "status"], VERSION_TIMEOUT).await {
        Some(Ok((true, text))) if text.contains("ChatGPT") => {
            scan.login = AppLogin::Ok;
            scan.login_detail = "已用 ChatGPT 登录".into();
        }
        Some(Ok((true, _))) => {
            scan.login = AppLogin::WrongAuth;
            scan.login_detail = "不是 ChatGPT 登录（平台只接受本机会员登录，不使用 API Key）".into();
        }
        Some(Ok((false, text))) => scan.login_detail = format!("未登录：{}", first_line(&text)),
        Some(Err(e)) => scan.login_detail = e,
        None => {}
    }
    match run(&bins.codex, &["debug", "models"], CATALOG_TIMEOUT).await {
        Some(Ok((true, text))) => match parse_codex_catalog(&text) {
            Ok(models) => {
                scan.models_note = format!("来自 codex debug models，共 {} 个可用模型", models.len());
                scan.models = models;
            }
            Err(e) => scan.models_note = format!("读取模型目录失败：{e}"),
        },
        Some(Ok((false, text))) => scan.models_note = format!("读取模型目录失败：{}", first_line(&text)),
        Some(Err(e)) => scan.models_note = e,
        None => {}
    }
    scan
}

async fn scan_agy(bins: &Bins) -> AppScan {
    let mut scan = base(ModelSource::Agy);
    match run(&bins.agy, &["--version"], VERSION_TIMEOUT).await {
        None => {
            scan.login_detail = "本机没有找到 agy 命令行，请先安装并登录 Antigravity CLI".into();
            return scan;
        }
        Some(Err(e)) => {
            scan.login_detail = e;
            return scan;
        }
        Some(Ok((_, text))) => {
            scan.installed = true;
            scan.version = Some(first_line(&text));
        }
    }
    let opts = agy_cli::AgyOpts { bin: bins.agy.clone(), ..Default::default() };
    match agy_cli::check_login(&opts).await {
        Ok(()) => {
            scan.login = AppLogin::Ok;
            scan.login_detail = "已用个人账号登录".into();
        }
        Err(reason) => {
            scan.login = if reason.contains("不是个人账号") { AppLogin::WrongAuth } else { AppLogin::NotLoggedIn };
            scan.login_detail = reason.to_string();
        }
    }
    match run(&bins.agy, &["models"], CATALOG_TIMEOUT).await {
        Some(Ok((true, text))) => {
            scan.models = parse_agy_models(&text);
            scan.models_note = format!("来自 agy models，共 {} 个模型（模型名里已带推理强度，不再单独选强度）", scan.models.len());
        }
        Some(Ok((false, text))) => scan.models_note = format!("读取模型列表失败：{}", first_line(&text)),
        Some(Err(e)) => scan.models_note = e,
        None => {}
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_CATALOG: &str = r#"{"models":[
      {"slug":"gpt-b","display_name":"B","visibility":"list","priority":2,"default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low","description":"x"},{"effort":"high","description":"y"}]},
      {"slug":"gpt-a","display_name":"A","visibility":"list","priority":1,"default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low","description":"x"}]},
      {"slug":"hidden","display_name":"H","visibility":"hide","priority":0,"supported_reasoning_levels":[]}
    ]}"#;

    #[test]
    fn codex目录只取可见模型_按优先级排序_带强度档位() {
        let models = parse_codex_catalog(CODEX_CATALOG).unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-a", "gpt-b"], "隐藏的不要，按 priority 升序");
        assert_eq!(models[1].efforts, ["low", "high"]);
        assert_eq!((models[1].default_effort.as_deref(), models[0].display.as_str()), (Some("medium"), "A"));
        assert!(parse_codex_catalog("不是 json").is_err());
        assert!(parse_codex_catalog("{}").is_err());
    }

    #[test]
    fn agy模型列表跳过提示行() {
        let text = "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\ngemini-4-neon-low\tGemini 4 Neon (Low)\n\n";
        let models = parse_agy_models(text);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gemini-4-neon-high", "gemini-4-neon-low"]);
        assert!(models.iter().all(|m| m.efforts.is_empty()));
    }

    #[tokio::test]
    async fn 没装的应用如实报没装() {
        let bins = Bins { claude: "/不存在/claude".into(), codex: "/不存在/codex".into(), agy: "/不存在/agy".into() };
        for source in [ModelSource::ClaudeCli, ModelSource::CodexExec, ModelSource::Agy] {
            let scan = scan_with(source, &bins).await;
            assert!(!scan.installed && scan.version.is_none() && scan.models.is_empty(), "{source:?}");
            assert!(scan.login_detail.contains("没有找到"), "{}", scan.login_detail);
        }
    }

    #[tokio::test]
    async fn deepseek不检查本机且说明开发中() {
        let scan = scan(ModelSource::DeepSeek).await;
        assert_eq!((scan.installed, scan.login), (false, AppLogin::NotApplicable));
        assert!(scan.models.is_empty() && scan.models_note.contains("开发中"));
    }

    /// 对本机真实命令行跑一遍（默认忽略，不调模型、不花额度）：
    /// `cargo test -p agent --lib apps::tests::真实检查 -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn 真实检查() {
        for source in [ModelSource::ClaudeCli, ModelSource::CodexExec, ModelSource::Agy] {
            let s = scan(source).await;
            println!("{source:?}：安装={} 版本={:?} 登录={:?}（{}）模型 {} 个｜{}", s.installed, s.version, s.login, s.login_detail, s.models.len(), s.models_note);
            println!("  前几个：{:?}", s.models.iter().take(4).map(|m| (&m.id, &m.efforts)).collect::<Vec<_>>());
        }
    }
}
