//! 从 Judge 已通过的轨迹生成 pytest 和 Page Object。生成与校验只返回内存中的产物。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mcp_device::trace::redact_text;
use protocol::{CaseSpec, FinalResult, JudgeItem, ModelSource, RoleModel, Verdict};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::claude_cli::{self, ClaudeOpts, Failure};
use crate::prompts::{SCRIPTGEN_SYSTEM, render};
use crate::scriptcheck::{self, UiContract, Violation, contains_sensitive, redact_sensitive};

pub const ROLE: &str = "scriptgen";
const MAX_FILES: usize = 6;
const MAX_FILE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct ScriptgenOpts {
    pub role: RoleModel,
    /// 复用 Judge 的命令行路径和时限配置。模型、强度、提示词及工具由生成角色控制。
    pub claude: ClaudeOpts,
}

pub struct ScriptgenInput<'a> {
    pub case: &'a CaseSpec,
    pub trace: &'a ScriptTrace,
    pub judge_result: FinalResult,
    pub judge_items: &'a [JudgeItem],
    pub ui_map: &'a Value,
    pub contract: &'a UiContract,
    /// 调用方已知的（敏感值，占位符），用于旧轨迹、Judge 证据等附加来源的补充脱敏。
    pub redactions: &'a [(String, String)],
}

/// 只加载生成用到的轨迹字段，兼容旧轨迹缺少部分证据字段的情况。
#[derive(Deserialize)]
pub struct ScriptTrace {
    pub case: Option<String>,
    pub steps: Vec<ScriptTraceStep>,
}

#[derive(Deserialize)]
pub struct ScriptTraceStep {
    pub idx: u32,
    pub action: String,
    pub target: Option<String>,
    pub prev_label: Option<String>,
    pub point: Option<(i32, i32)>,
    pub value: Option<String>,
    #[serde(default)]
    pub new_labels: Vec<String>,
    #[serde(default)]
    pub after_labels: Vec<String>,
    pub shot: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GeneratedScripts {
    pub files: BTreeMap<String, String>,
    pub notes: String,
    pub sha256: BTreeMap<String, String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug)]
pub enum ScriptgenError {
    Input(&'static str),
    QuotaExhausted,
    Claude(&'static str),
    Violations(Vec<Violation>),
}

impl fmt::Display for ScriptgenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(f, "{ROLE} 输入无效：{message}"),
            Self::QuotaExhausted => write!(f, "{ROLE} 的 Claude 会员额度已用完或被限流"),
            Self::Claude(message) => write!(f, "{ROLE} 调用 Claude 失败：{message}"),
            Self::Violations(violations) => {
                write!(f, "{ROLE} 静态校验失败，共 {} 条违规", violations.len())
            }
        }
    }
}

impl std::error::Error for ScriptgenError {}

fn credential_key(key: &str) -> bool {
    matches!(
        key,
        "账号" | "帐号" | "用户名" | "密码" | "口令" | "令牌" | "设备序列号"
    ) || key.to_ascii_lowercase().split(['.', '-', '_']).any(|part| {
        matches!(
            part,
            "account"
                | "accounts"
                | "username"
                | "password"
                | "passwd"
                | "pwd"
                | "email"
                | "token"
                | "serial"
                | "credentials"
                | "secret"
                | "apikey"
        )
    }) || key.eq_ignore_ascii_case("api_key")
}

fn collect_redactions(value: &Value, sensitive: bool, pairs: &mut Vec<(String, String)>) {
    match value {
        Value::Object(entries) => {
            for (key, value) in entries {
                collect_redactions(value, sensitive || credential_key(key), pairs);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_redactions(item, sensitive, pairs);
            }
        }
        Value::String(text) if sensitive && !text.is_empty() && !text.starts_with("{{account.") => {
            pairs.push((text.clone(), "（已脱敏）".into()));
        }
        Value::Number(value) if sensitive => pairs.push((value.to_string(), "（已脱敏）".into())),
        _ => {}
    }
}

fn redact_prompt(value: &mut Value, pairs: &[(String, String)]) {
    let scrub = |text: &str| redact_sensitive(&redact_text(text, pairs));
    match value {
        Value::String(text) => *text = scrub(text),
        Value::Array(items) => {
            for item in items {
                redact_prompt(item, pairs);
            }
        }
        Value::Object(entries) => {
            for (key, mut val) in std::mem::take(entries) {
                redact_prompt(&mut val, pairs);
                entries.insert(scrub(&key), val);
            }
        }
        Value::Number(number) => {
            let original = number.to_string();
            let redacted = scrub(&original);
            if redacted != original {
                *value = Value::String(redacted);
            }
        }
        _ => {}
    }
}

fn screenshot(name: &str) -> Option<&str> {
    let base = name.rsplit(['/', '\\']).next()?;
    (base.len() <= 128
        && base.ends_with(".png")
        && base.len() > 4
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
    .then_some(base)
}

fn reference(text: &str) -> Option<&str> {
    if text
        .strip_prefix("step:")
        .is_some_and(|s| s.parse::<u32>().is_ok_and(|n| n > 0))
    {
        Some(text)
    } else {
        screenshot(text)
    }
}

struct Prepared {
    prompt: String,
    names: [String; 2],
    redactions: Vec<(String, String)>,
}

fn prepare(input: &ScriptgenInput<'_>) -> Result<Prepared, ScriptgenError> {
    let case = input.case;
    let key = scriptcheck::case_key(&case.code)
        .ok_or(ScriptgenError::Input("用例编号不能转换成脚本文件名"))?;
    let expected_ids: BTreeSet<_> = case.expected.iter().map(|item| item.idx).collect();
    let judged_ids: BTreeSet<_> = input.judge_items.iter().map(|item| item.idx).collect();
    if input.judge_result != FinalResult::Passed
        || expected_ids.is_empty()
        || expected_ids.contains(&0)
        || expected_ids.len() != case.expected.len()
        || judged_ids.len() != input.judge_items.len()
        || judged_ids != expected_ids
        || input
            .judge_items
            .iter()
            .any(|item| item.verdict != Verdict::Passed)
    {
        return Err(ScriptgenError::Input(
            "需要 Judge 整体通过，且每条预期都有唯一的通过判定",
        ));
    }
    if input.trace.steps.is_empty()
        || input
            .trace
            .case
            .as_ref()
            .is_some_and(|code| code != &case.code)
    {
        return Err(ScriptgenError::Input("需要与用例编号对应的非空轨迹"));
    }
    let mut redactions = input.redactions.to_vec();
    collect_redactions(&case.data, false, &mut redactions);
    collect_redactions(input.ui_map, false, &mut redactions);
    let safe_data = Value::Object(
        case.data
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, value)| {
                !credential_key(key)
                    && (value.is_string() || value.is_number() || value.is_boolean())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let render = |text: &str| render(text, &safe_data);
    let expected: Vec<_> = case
        .expected
        .iter()
        .map(|item| json!({"idx": item.idx, "text": item.text, "rendered": render(&item.text)}))
        .collect();
    let judge: Vec<_> = input.judge_items.iter().map(|item| json!({
        "idx": item.idx,
        "verdict": item.verdict,
        "evidence": item.evidence,
        "references": item.file_refs.iter().filter_map(|name| reference(name)).collect::<Vec<_>>(),
    })).collect();
    let steps: Vec<_> = input
        .trace
        .steps
        .iter()
        .map(|step| {
            json!({
                "idx": step.idx,
                "action": step.action,
                "target": step.target,
                "prev_label": step.prev_label,
                "point": step.point,
                "value": step.value,
                "new_labels": step.new_labels,
                "after_labels": step.after_labels,
                "shot": step.shot.as_deref().and_then(screenshot),
            })
        })
        .collect();
    let names = [format!("pages/{key}_pages.py"), format!("test_{key}.py")];
    let mut prompt = json!({
        "case": {
            "code": case.code,
            "title": case.title,
            "preconditions": case.preconditions.iter().map(|s| render(s)).collect::<Vec<_>>(),
            "steps": case.steps.iter().map(|s| render(s)).collect::<Vec<_>>(),
            "expected": expected,
        },
        "trace": steps,
        "judge": judge,
        "ui_map": input.ui_map,
        "ui_contract": {
            "methods": input.contract.methods,
            "allowed_imports": input.contract.allowed_imports.iter()
                .map(|name| name.replace("{case}", &key))
                .collect::<Vec<_>>(),
        },
        "required_files": names,
    });
    redact_prompt(&mut prompt, &redactions);
    Ok(Prepared {
        prompt: format!("### 生成输入\n{prompt:#}"),
        names,
        redactions,
    })
}

/// 构建与实际送给模型相同的首轮用户提示词。
pub fn build_prompt(input: &ScriptgenInput<'_>) -> Result<String, ScriptgenError> {
    Ok(prepare(input)?.prompt)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    #[serde(deserialize_with = "deserialize_files")]
    files: Vec<(String, String)>,
    notes: String,
}

// 保留重复键和原始文件数，校验完成前不折叠到映射。
fn deserialize_files<'de, D: Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<(String, String)>, D::Error> {
    struct FileMap;
    impl<'de> Visitor<'de> for FileMap {
        type Value = Vec<(String, String)>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("文件名到 Python 源码的 JSON 对象")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut files = Vec::new();
            while let Some(file) = map.next_entry()? {
                if files.len() <= MAX_FILES {
                    files.push(file);
                }
            }
            Ok(files)
        }
    }
    decoder.deserialize_map(FileMap)
}

fn parse(text: &str) -> Option<Payload> {
    if let Ok(payload) = serde_json::from_str(text) {
        return Some(payload);
    }
    if serde_json::from_str::<IgnoredAny>(text).is_ok() {
        return None;
    }
    // 类似 Judge 的 JSON 抢救：跳过包装噪声，必须能恢复唯一且完整的对象。
    let mut rest = text;
    let mut recovered = None;
    while let Some(start) = rest.find('{') {
        let candidate = &rest[start..];
        let mut decoder = serde_json::Deserializer::from_str(candidate).into_iter::<IgnoredAny>();
        if decoder.next().is_some_and(|result| result.is_ok()) {
            let end = decoder.byte_offset();
            if let Ok(payload) = serde_json::from_str(&candidate[..end]) {
                if recovered.replace(payload).is_some() {
                    return None;
                }
            }
            rest = &candidate[end..];
        } else {
            rest = &candidate[1..];
        }
    }
    recovered
}

fn validate_payload(
    text: &str,
    prepared: &Prepared,
    input: &ScriptgenInput<'_>,
) -> Result<(BTreeMap<String, String>, String), Vec<Violation>> {
    let output_error = |message| vec![Violation::new("输出", 1, 0, message)];
    let payload =
        parse(text).ok_or_else(|| output_error("未找到唯一且完整的 files/notes JSON 对象"))?;
    if payload.files.len() > MAX_FILES {
        return Err(output_error("文件数量超过 6 个"));
    }
    let mut files = BTreeMap::new();
    for (name, source) in payload.files {
        if !prepared.names.contains(&name) {
            return Err(output_error("文件名必须与本次用例的两个指定文件名一致"));
        }
        if source.len() > MAX_FILE_BYTES {
            return Err(vec![Violation::new(&name, 1, 0, "文件超过 64KB")]);
        }
        if files.insert(name, source).is_some() {
            return Err(output_error("文件名重复"));
        }
    }
    if files.len() != prepared.names.len() {
        return Err(output_error("缺少本次用例的测试或 Page Object 文件"));
    }
    let mut violations = scriptcheck::check_with_redactions(
        &files,
        input.contract,
        &input.case.code,
        input.case.expected.len(),
        &prepared.redactions,
    );
    if contains_sensitive(&payload.notes, &prepared.redactions) {
        violations.push(Violation::new("notes", 1, 5, "生成备注含敏感值"));
    }
    if violations.is_empty() {
        Ok((files, payload.notes))
    } else {
        Err(violations)
    }
}

fn model_failure(failure: Failure) -> ScriptgenError {
    // 命令行异常文本可能回显输入或本机信息。只回传错误类别。
    match failure {
        Failure::Quota(_) => ScriptgenError::QuotaExhausted,
        Failure::Auth(_) => ScriptgenError::Claude("本机会员登录失效"),
        Failure::Stalled { .. } => ScriptgenError::Claude("命令行无输出，已判卡住"),
        Failure::Timeout { .. } => ScriptgenError::Claude("调用超时"),
        Failure::Spawn(_) => ScriptgenError::Claude("无法启动命令行"),
        Failure::Exit { .. } => ScriptgenError::Claude("命令行异常退出"),
        Failure::Result { .. } => ScriptgenError::Claude("命令行返回错误"),
        Failure::NoResult => ScriptgenError::Claude("命令行没有返回生成结果"),
    }
}

/// 内置的 Ui 契约（PoC 签名夹具），与开发入口 `scriptgen_dev` 的默认契约相同。
/// 它不是从当前自动化运行时读出来的，运行时新增或改名的方法这里不会知道；
/// 要和运行时对齐，需要调用方用 [`UiContract::from_runtime_json`] 自己构造。
pub fn builtin_contract() -> UiContract {
    UiContract::from_json(include_str!("../tests/fixtures/scriptgen/ui_contract.json").as_bytes())
        .expect("内置的 Ui 契约夹具必须是有效 JSON")
}

/// 首次生成与最多一轮修正。只有通过文件形态和源码校验的结果才会返回。
pub async fn generate(
    opts: &ScriptgenOpts,
    input: ScriptgenInput<'_>,
) -> Result<GeneratedScripts, ScriptgenError> {
    if opts.role.source != ModelSource::ClaudeCli {
        return Err(ScriptgenError::Input(
            "scriptgen 角色必须使用 Claude 会员命令行",
        ));
    }
    let prepared = prepare(&input)?;
    let claude = ClaudeOpts {
        model: opts.role.model.clone(),
        effort: opts.role.effort.clone(),
        system_prompt: Some(SCRIPTGEN_SYSTEM.into()),
        mcp_config: None,
        tools: vec![],
        allowed_tools: vec![],
        ..opts.claude.clone()
    };
    let mut prompt = prepared.prompt.clone();
    let mut repair = false;
    loop {
        let response = claude_cli::run_text_only(&claude, &prompt).await;
        if response
            .rate_limit
            .is_some_and(|limit| !limit.status.starts_with("allowed"))
        {
            return Err(ScriptgenError::QuotaExhausted);
        }
        if let Some(failure) = response.failure {
            return Err(model_failure(failure));
        }
        match validate_payload(&response.text, &prepared, &input) {
            Ok((files, notes)) => {
                let sha256 = files
                    .iter()
                    .map(|(name, source)| {
                        let hash = Sha256::digest(source.as_bytes())
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect();
                        (name.clone(), hash)
                    })
                    .collect();
                return Ok(GeneratedScripts {
                    files,
                    notes,
                    sha256,
                    model: response.model.or_else(|| opts.role.model.clone()),
                    effort: opts.role.effort.clone(),
                });
            }
            Err(violations) if repair => return Err(ScriptgenError::Violations(violations)),
            Err(violations) => {
                let feedback = serde_json::to_string_pretty(&violations).expect("违规列表可序列化");
                prompt.push_str("\n\n### 上一轮的静态违规，请修正后重新输出完整 JSON\n");
                prompt.push_str(&feedback);
                repair = true;
            }
        }
    }
}
