//! 根据执行轨迹的截图和页面文字独立复核用例。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use mcp_device::evidence::{IMAGE_TOOL, STEP_TEXT_TOOL, catalog, step_text_catalog};
use protocol::{CaseSpec, FinalResult, JudgeItem, JudgeStatus, Verdict};
use regex_automata::meta::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agy_cli::{self, AgyOpts};
use crate::claude_cli::{self, ClaudeOpts, Failure};
use crate::prompts::{AppKnowledge, render, verified_facts};

pub const JUDGE_SYSTEM: &str = r#"你是测试结果的独立判定员，不参与执行。执行员已经操作完手机并提交了自己的结论，你要根据证据重新判断，不能直接采信它的结论。

规则：
- 按顺序逐条对照「预期结果」，用 evidence 的 image 工具按文件名取图。先按动作找到每条预期对应的截图；状态变化要比较前后两张，不要只看终态。标号遮住文字时优先看同阶段的操作后原图。
- passed / failed 每条必须引用实际查看过的证据（截图文件名或 step:N），说明画面或手机控件树文字中的事实，终态页面文字可补充。缺少支持就记 uncertain。执行员的口述仅供参考。截图没拍到瞬时提示（toast）时，必须先用 step_text 查相关步骤的文字，再决定是否记 uncertain。
- 预期和实际不一致必须判 failed，不允许强行通过。
- 靠截图和页面文字无法判断的预期（如声音是否真的播放、对端用户是否能听到），不要猜，该条记为 uncertain。
- 只要有一条 failed 就整体 failed；没有 failed 但有 uncertain，整体 uncertain；全部有证据支持才 passed。
- 如果执行员的结论和证据矛盾，以证据为准，并在 disagreement 里说明。
- 「人工介入记录」里 mode=sim 表示由脚本模拟了人工动作，mode=skip 表示没有执行人工动作，都不是真人。涉及人工的预期：流程是否按要求暂停、请求是否写清楚，可以依据记录判断；但不要把模拟当成「真人已验证」，需要在 reason 里注明。

最后只输出一个 JSON 对象（不要代码块标记），格式：
{"result": "passed|failed|uncertain", "items": [{"index": 1, "verdict": "passed|failed|uncertain", "screenshots": ["实际查看过的截图文件名或 step:N"], "evidence": "画面或页面原文，或缺少什么证据"}], "disagreement": "与执行员结论的差异，没有则为空字符串", "reason": "一句话总结"}
"#;

fn expected_results(case: &CaseSpec) -> Vec<String> {
    case.expected
        .iter()
        .map(|item| render(&item.text, &case.data))
        .collect()
}

fn app_facts() -> String {
    #[derive(Deserialize)]
    struct Facts {
        verified: Option<Vec<String>>,
    }
    let verified = std::env::var_os("HAFA_APP_INFO")
        .and_then(|path| File::open(path).ok())
        .and_then(|file| serde_yaml_ng::from_reader::<_, Facts>(file).ok())
        .and_then(|facts| facts.verified)
        .unwrap_or_default();
    verified_facts(&AppKnowledge {
        verified,
        ..Default::default()
    })
}

pub fn build_prompt(
    case: &CaseSpec,
    exec_result: &str,
    exec_reason: &str,
    page_text: &str,
    catalog: &[(String, String)],
) -> String {
    build_prompt_with_human(case, exec_result, exec_reason, page_text, catalog, &[], &[])
}

fn build_prompt_with_human(
    case: &CaseSpec,
    exec_result: &str,
    exec_reason: &str,
    page_text: &str,
    catalog: &[(String, String)],
    steps: &[(u32, String)],
    human: &[String],
) -> String {
    let mut lines = vec![
        format!("## 用例 {}：{}", case.code, case.title),
        String::new(),
        "### 预期结果".into(),
    ];
    lines.extend(
        expected_results(case)
            .iter()
            .enumerate()
            .map(|(i, item)| format!("{}. {item}", i + 1)),
    );
    let facts = app_facts();
    lines.extend([
        String::new(),
        "### 执行员提交的结论（仅供参考，不可直接采信）".into(),
        format!("- result: {exec_result}"),
        format!("- reason: {exec_reason}"),
        String::new(),
        "### 终态页面文字（final_page.txt）".into(),
        if page_text.is_empty() {
            "（无）"
        } else {
            page_text
        }
        .into(),
        String::new(),
        if facts.is_empty() {
            "（无已验证事实）".into()
        } else {
            facts
        },
        "### 人工介入记录（need_human）".into(),
    ]);
    if human.is_empty() {
        lines.push("（本次没有调用 need_human）".into());
    } else {
        lines.extend_from_slice(human);
    }
    lines.extend([
        String::new(),
        "### 本次轨迹的截图索引（按时间顺序，按需调用 image 查看，不接受文件路径）".into(),
    ]);
    lines.extend(
        catalog
            .iter()
            .map(|(name, label)| format!("- {name}：{label}")),
    );
    lines.extend([
        String::new(),
        "### 步骤文字索引".into(),
        "调用 step_text(step) 按整数步骤号查看手机控件树文字。这些文字是工具层记录的页面事实，实际查看后可引用 step:N。用它判断截图拍不到的瞬时提示（toast）、文字很小或被遮挡的内容。执行员的口述仅供参考。".into(),
    ]);
    if steps.is_empty() {
        lines.push("（无）".into());
    } else {
        lines.extend(
            steps
                .iter()
                .map(|(idx, label)| format!("- 步骤 {idx} · {label}")),
        );
    }
    lines.join("\n")
}

#[derive(Debug, Clone)]
pub struct Parsed {
    payload: Value,
    pub salvaged: bool,
}

fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn salvage(text: &str) -> Option<Parsed> {
    static INDEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\{\s*"index"\s*:(?:\s*(0|-?[1-9][0-9]*)\b)?"#).expect("序号正则有效")
    });
    static RESULT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""result"\s*:\s*"(passed|failed|uncertain)""#).expect("结论正则有效")
    });
    static REASON: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""reason"\s*:\s*"((?:[^"\\]|\\.)*)""#).expect("理由正则有效")
    });
    static DISAGREEMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""disagreement"\s*:\s*"((?:[^"\\]|\\.)*)""#).expect("分歧正则有效")
    });
    let mut rows = BTreeMap::new();
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    for m in INDEX.captures_iter(text) {
        // 损坏的对象只要出现了同一个序号，也算一次。
        if let Some(n) = m.get_group(1) {
            *occurrences.entry(&text[n.range()]).or_default() += 1;
        }
        let start = m.get_match().expect("匹配了序号字段").start();
        let mut decoder = serde_json::Deserializer::from_str(&text[start..]);
        if let Ok(row) = Value::deserialize(&mut decoder) {
            if let Some(index) = integer(&row["index"]) {
                rows.entry(index).or_insert(row);
            }
        }
    }
    if rows.is_empty() {
        return None;
    }
    for (index, row) in &mut rows {
        if occurrences
            .get(index.to_string().as_str())
            .copied()
            .unwrap_or_default()
            > 1
        {
            *row = json!({"index": index});
        }
    }
    let tail = text
        .rsplit_once("\"evidence\"")
        .map_or(text, |(_, tail)| tail);
    let result = RESULT
        .captures_iter(text)
        .next()
        .and_then(|m| m.get_group(1))
        .map(|s| &text[s.range()]);
    let reason = REASON
        .captures_iter(tail)
        .last()
        .and_then(|m| m.get_group(1))
        .map(|s| &tail[s.range()]);
    let disagreement = DISAGREEMENT
        .captures_iter(tail)
        .last()
        .and_then(|m| m.get_group(1))
        .map_or("", |s| &tail[s.range()]);
    Some(Parsed {
        payload: json!({"result": result, "items": rows.into_values().collect::<Vec<_>>(), "reason": reason, "disagreement": disagreement}),
        salvaged: true,
    })
}

/// 先读整份 JSON；遇到语法错误时再恢复可单独解析的逐项对象。
pub fn parse(text: &str) -> Option<Parsed> {
    let body = text.get(text.find('{')?..=text.rfind('}')?)?;
    match serde_json::from_str(body) {
        Ok(payload) => Some(Parsed {
            payload,
            salvaged: false,
        }),
        Err(_) => salvage(text),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub final_result: FinalResult,
    pub items: Vec<JudgeItem>,
    pub reason: String,
    pub disagreement: String,
}

fn step_has_text(trace: &Value, idx: &str) -> bool {
    let Ok(idx) = idx.parse::<u64>() else {
        return false;
    };
    trace["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|step| step["idx"].as_u64() == Some(idx))
        .is_some_and(|step| {
            ["after_labels", "new_labels"]
                .into_iter()
                .filter_map(|key| step[key].as_array())
                .flatten()
                .filter_map(Value::as_str)
                .any(|label| !label.trim().is_empty())
        })
}

/// 仅有查看记录时，通过项需要引用截图。
pub fn checked(parsed: Option<&Parsed>, expected: &[String], served: &HashSet<String>) -> Checked {
    checked_with_text(parsed, expected, served, &Value::Null, "")
}

/// 逐条采用唯一、证据充分的判定，再按 PoC 的优先级汇总。
fn checked_with_text(
    parsed: Option<&Parsed>,
    expected: &[String],
    served: &HashSet<String>,
    trace: &Value,
    page_text: &str,
) -> Checked {
    let payload = parsed.map_or(&Value::Null, |parsed| &parsed.payload);
    let rows = payload["items"].as_array();
    let has_final_text = !page_text.trim().is_empty();
    let items: Vec<JudgeItem> = expected
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let idx = i as u32 + 1;
            let mut matches = rows
                .into_iter()
                .flatten()
                .filter(|row| integer(&row["index"]) == Some(i128::from(idx)));
            let row = match (matches.next(), matches.next()) {
                (Some(row), None) => row,
                _ => &Value::Null,
            };
            let verdict = match row["verdict"].as_str() {
                Some("passed") => Some(Verdict::Passed),
                Some("failed") => Some(Verdict::Failed),
                Some("uncertain") => Some(Verdict::Uncertain),
                _ => None,
            };
            let note = row["evidence"].as_str().filter(|s| !s.trim().is_empty());
            let references = row["screenshots"].as_array();
            // 截图文件名和 step:N 都由证据工具在成功查看时记入 served。
            let file_refs: Vec<String> = references
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|reference| served.contains(*reference))
                .map(str::to_owned)
                .collect();
            let (mut verdict, mut evidence) = match (verdict, note) {
                (Some(v), Some(note))
                    if v == Verdict::Uncertain
                        || (!file_refs.is_empty()
                            && references.is_some_and(|refs| refs.len() == file_refs.len())) =>
                {
                    (v, note)
                }
                (Some(_), Some(_)) => (
                    Verdict::Uncertain,
                    "该项没有引用可核实的证据，或包含未实际查看的证据",
                ),
                _ => (Verdict::Uncertain, "缺少唯一、有效的逐项判定或证据说明"),
            };
            // 这里只核验文字证据是否有内容。证据是否相关、支持还是反驳预期，由模型判断。
            if verdict == Verdict::Passed
                && !has_final_text
                && file_refs.iter().all(|reference| {
                    reference
                        .strip_prefix("step:")
                        .is_some_and(|idx| !step_has_text(trace, idx))
                })
            {
                verdict = Verdict::Uncertain;
                evidence = "所引步骤与终态页面均无可用文字，转待人工确认";
            }
            JudgeItem {
                idx,
                verdict,
                evidence: evidence.into(),
                file_refs,
            }
        })
        .collect();

    let (final_result, result) = if items.iter().any(|i| i.verdict == Verdict::Failed) {
        (FinalResult::Failed, "failed")
    } else if items.is_empty()
        || items.iter().any(|i| i.verdict == Verdict::Uncertain)
        || payload["result"].as_str() != Some("passed")
        || rows.is_none_or(|rows| rows.len() != expected.len())
    {
        (FinalResult::Uncertain, "uncertain")
    } else {
        (FinalResult::Passed, "passed")
    };
    let reason = (payload["result"].as_str() == Some(result))
        .then(|| payload["reason"].as_str())
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or("按逐项结论及实际查看的证据汇总");
    Checked {
        final_result,
        items,
        reason: reason.into(),
        disagreement: payload["disagreement"].as_str().unwrap_or_default().into(),
    }
}

fn read_run_text(run_dir: &Path, name: &str) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(run_dir.join(name))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

fn human_records(run_dir: &Path, trace: &Value) -> Vec<String> {
    let display = |value: &Value| match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".into(),
        other => other.to_string(),
    };
    if let Some(events) = read_run_text(run_dir, "human_events.json")
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
    {
        return events
            .iter()
            .map(|event| {
                format!(
                    "- 请求：{}\n  处理方式 mode={}，结果：{}，等待 {}s",
                    display(&event["reason"]),
                    display(&event["mode"]),
                    display(&event["note"]),
                    display(&event["waited_s"]),
                )
            })
            .collect();
    }
    trace["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["action"] == "need_human")
        .map(|step| format!("- 请求：{}（没有处理记录）", display(&step["value"])))
        .collect()
}

/// 一次评审期间对运行目录的独占占用：持有目录锁，并清掉上一次的查看记录。
///
/// 证据服务把评审员**实际查看过**的截图和步骤号写进运行目录里的 `served.json`，核对结论时据此判断它引用的证据是不是真的看过。
/// 所以同一个运行目录同一时刻只能有一次评审（否则两次评审的记录会混在一起），开始前也必须清掉旧记录。
/// Claude 和 agy 两条路径共用这一份，核对证据的标准不因评审员不同而不同。
struct EvidenceSession {
    // 同一轨迹目录里的 served.json 只属于一次正在进行的判定。
    _run_lock: File,
}

impl EvidenceSession {
    fn begin(run_dir: &Path) -> io::Result<Self> {
        let run_lock = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(run_dir)?;
        // 安全性：描述符由 run_lock 持有，关闭时释放独占锁。
        if unsafe { libc::flock(run_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        match fs::remove_file(run_dir.join("served.json")) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        Ok(Self { _run_lock: run_lock })
    }
}

struct McpConfig {
    temp_dir: PathBuf,
    _session: EvidenceSession,
}

impl McpConfig {
    fn new(run_dir: &Path, evidence_bin: &Path) -> io::Result<Self> {
        let session = EvidenceSession::begin(run_dir)?;

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().canonicalize()?;
        let temp_dir = loop {
            let dir = base.join(format!(
                "hafa-judge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let config = Self {
            temp_dir,
            _session: session,
        };
        let bin = std::path::absolute(evidence_bin)?;
        let as_utf8 = |path: &Path| {
            path.to_str().map(str::to_owned).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "证据服务的路径必须是 UTF-8")
            })
        };
        let payload = json!({"mcpServers": {"evidence": {
            "command": as_utf8(&bin)?,
            "args": [],
            "env": {"HAFA_RUN_DIR": as_utf8(run_dir)?},
        }}});
        serde_json::to_writer(File::create(config.path())?, &payload).map_err(io::Error::other)?;
        Ok(config)
    }

    fn path(&self) -> PathBuf {
        self.temp_dir.join("mcp.json")
    }
}

impl Drop for McpConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JudgeOutcome {
    pub status: JudgeStatus,
    pub final_result: FinalResult,
    pub items: Vec<JudgeItem>,
    pub reason: String,
    pub disagreement: String,
    pub quota_exhausted: bool,
    pub cost_usd: Option<f64>,
    pub wall_s: f64,
    pub raw: String,
}

/// 评审前准备好的输入：不依赖用哪个评审员。
struct Prepared {
    expected: Vec<String>,
    run_dir: PathBuf,
    trace: Value,
    page: String,
    prompt: String,
}

/// 读出本次轨迹、终态页面文字，拼好提示词。不能评审时返回 `Err`，里面就是该给出的结果（没调模型）。
fn prepare(
    case: &CaseSpec,
    exec_result: &str,
    exec_reason: &str,
    run_dir: &Path,
) -> Result<Prepared, JudgeOutcome> {
    let expected = expected_results(case);
    let mut outcome = blank_outcome(&expected);
    let images = catalog(run_dir);
    let steps = step_text_catalog(run_dir);
    if images.is_empty() && steps.is_empty() {
        outcome.status = JudgeStatus::NotRun;
        outcome.reason = "本次轨迹无可用截图和步骤文字，未调用模型".into();
        return Err(outcome);
    }
    let Ok(run_dir) = run_dir.canonicalize() else {
        outcome.reason = "判定失败，无法定位本次执行的轨迹目录".into();
        return Err(outcome);
    };
    let trace: Value = read_run_text(&run_dir, "trace.json")
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let page = if trace["evidence"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item == "final_page.txt"))
    {
        read_run_text(&run_dir, "final_page.txt")
            .unwrap_or_default()
            .chars()
            .take(4000)
            .collect()
    } else {
        String::new()
    };
    let prompt = build_prompt_with_human(
        case,
        exec_result,
        exec_reason,
        &page,
        &images,
        &steps,
        &human_records(&run_dir, &trace),
    );
    Ok(Prepared {
        expected,
        run_dir,
        trace,
        page,
        prompt,
    })
}

/// 评审员给出了文本结论之后：解析，并按证据服务记下的「实际查看记录」核对它引用的证据。
/// 这是两条评审路径唯一的一份核对逻辑。`who` 只用于说明文字。
fn conclude(outcome: &mut JudgeOutcome, prepared: &Prepared, who: &str) {
    let Some(parsed) = parse(&outcome.raw) else {
        outcome.reason = format!("判定失败，无法解析 {who} 的判定结果，转待人工确认");
        return;
    };
    let served = read_run_text(&prepared.run_dir, "served.json")
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let verdict = checked_with_text(
        Some(&parsed),
        &prepared.expected,
        &served,
        &prepared.trace,
        &prepared.page,
    );
    outcome.status = JudgeStatus::Completed;
    outcome.final_result = verdict.final_result;
    outcome.items = verdict.items;
    outcome.reason = verdict.reason;
    outcome.disagreement = verdict.disagreement;
}

pub async fn run_judge(
    opts: &ClaudeOpts,
    case: &CaseSpec,
    exec_result: &str,
    exec_reason: &str,
    run_dir: &Path,
    evidence_bin: &Path,
) -> JudgeOutcome {
    let prepared = match prepare(case, exec_result, exec_reason, run_dir) {
        Ok(prepared) => prepared,
        Err(outcome) => return outcome,
    };
    let mut outcome = blank_outcome(&prepared.expected);
    let start = Instant::now();
    if let Err(reason) = claude_cli::check_login(opts).await {
        outcome.wall_s = start.elapsed().as_secs_f64();
        outcome.reason = format!("判定调用失败，{reason}，转待人工确认");
        return outcome;
    }
    let config = match McpConfig::new(&prepared.run_dir, evidence_bin) {
        Ok(config) => config,
        Err(e) => {
            outcome.reason = format!("判定失败，无法准备证据 MCP：{e}");
            return outcome;
        }
    };
    let opts = ClaudeOpts {
        mcp_config: Some(config.path()),
        tools: vec![],
        allowed_tools: vec![IMAGE_TOOL.into(), STEP_TEXT_TOOL.into()],
        system_prompt: Some(JUDGE_SYSTEM.into()),
        ..opts.clone()
    };
    let response = claude_cli::run(&opts, &prepared.prompt).await;
    outcome.wall_s = start.elapsed().as_secs_f64();
    outcome.cost_usd = response.cost_usd;
    outcome.raw = response.text;
    if let Some(failure) = response.failure {
        outcome.quota_exhausted = matches!(failure, Failure::Quota(_));
        // 命令行的原始报错可能含登录信息，供节点上报的理由只写错误类型。
        let detail = match failure {
            Failure::Quota(_) => "Claude 会员额度用完或被限流".into(),
            Failure::Auth(_) => "Claude 登录失效".into(),
            Failure::Stalled { secs } => format!("Claude 连续 {secs} 秒无输出"),
            Failure::Timeout { secs } => format!("Claude 超时（总时限 {secs} 秒）"),
            Failure::Spawn(_) => "无法启动 Claude 命令行".into(),
            Failure::Exit { code, .. } => code.map_or_else(
                || "Claude 进程异常退出".into(),
                |c| format!("Claude 进程异常退出（退出码 {c}）"),
            ),
            Failure::Result { .. } => "Claude 命令行返回错误结果".into(),
            Failure::NoResult => "Claude 命令行没有返回结果事件".into(),
        };
        outcome.reason = format!("判定调用失败，{detail}，转待人工确认");
        return outcome;
    }
    conclude(&mut outcome, &prepared, "Claude");
    outcome
}

/// Judge 没跑成时的样子：待人工确认，逐条都是待确认。不得用别的东西顶替（规则 5）。
fn blank_outcome(expected: &[String]) -> JudgeOutcome {
    JudgeOutcome {
        status: JudgeStatus::Failed,
        final_result: FinalResult::Uncertain,
        items: checked(None, expected, &HashSet::new()).items,
        reason: String::new(),
        disagreement: String::new(),
        quota_exhausted: false,
        cost_usd: None,
        wall_s: 0.0,
        raw: String::new(),
    }
}

/// 只给 agy 的工具使用说明。Claude 的命令行把证据工具直接列成 `image` / `step_text`，一句「用 evidence 的 image 工具」就够了；
/// agy 自带读文件、命令行、浏览器等一批内置工具，MCP 要经通用的 `call_mcp_tool` 调用，不说清楚它就会去读文件
/// （真实运行里就发生过：两次 `read_file`，被审计拦下，整次评审作废）。
pub const AGY_TOOL_GUIDE: &str = "【工具使用方式】证据只能通过 MCP 工具获取：调用 call_mcp_tool，ServerName 填 evidence，\
ToolName 填 image（参数 name 是截图文件名，如 1_after.png）或 step_text（参数 step 是整数步骤号）。\
不要使用 read_file、命令行、浏览器、搜索等任何其他工具，也不要尝试按路径读取截图或轨迹文件：它们读不到，也不需要读。\
使用 evidence 之外的任何工具都会使这次评审作废。下面的「按需调用 image 查看」「调用 step_text」都指这两个 MCP 工具。";

/// 用 agy（Gemini）做评审员。和 [`run_judge`] 用同一套提示词、同一份证据服务、同一份核对逻辑；
/// 区别只在怎么调模型：agy 没有系统提示词开关（提示词合并进用户消息），证据 MCP 经临时 HOME 里的专用配置挂上，
/// 调用了证据工具之外的任何工具、或有被自动拒绝的动作，整次评审不可信，转待人工确认。
pub async fn run_judge_agy(
    opts: &AgyOpts,
    case: &CaseSpec,
    exec_result: &str,
    exec_reason: &str,
    run_dir: &Path,
    evidence_bin: &Path,
) -> JudgeOutcome {
    let prepared = match prepare(case, exec_result, exec_reason, run_dir) {
        Ok(prepared) => prepared,
        Err(outcome) => return outcome,
    };
    let mut outcome = blank_outcome(&prepared.expected);
    let start = Instant::now();
    if let Err(reason) = agy_cli::check_login(opts).await {
        outcome.wall_s = start.elapsed().as_secs_f64();
        outcome.reason = format!("判定调用失败，{reason}，转待人工确认");
        return outcome;
    }
    let _session = match EvidenceSession::begin(&prepared.run_dir) {
        Ok(session) => session,
        Err(e) => {
            outcome.reason = format!("判定失败，无法准备证据 MCP：{e}");
            return outcome;
        }
    };
    let command = match std::path::absolute(evidence_bin).ok().filter(|path| path.to_str().is_some()) {
        Some(command) => command,
        None => {
            outcome.reason = "判定失败，无法准备证据 MCP：证据服务的路径必须是 UTF-8".into();
            return outcome;
        }
    };
    let opts = AgyOpts {
        mcp: vec![agy_cli::McpServer {
            name: "evidence".into(),
            command,
            env: BTreeMap::from([("HAFA_RUN_DIR".to_string(), prepared.run_dir.to_string_lossy().into_owned())]),
        }],
        allowed_tools: vec![("evidence".into(), "image".into()), ("evidence".into(), "step_text".into())],
        ..opts.clone()
    };
    let prompt = format!("{AGY_TOOL_GUIDE}\n\n{JUDGE_SYSTEM}\n\n---\n\n{}", prepared.prompt);
    let response = agy_cli::run(&opts, &prompt).await;
    outcome.wall_s = start.elapsed().as_secs_f64();
    outcome.raw = response.text.clone();
    if let Some(failure) = &response.failure {
        outcome.quota_exhausted = matches!(failure, agy_cli::Failure::Quota(_));
        // 原始报错可能带本机信息，供节点上报的理由只写错误类型。
        let detail = match failure {
            agy_cli::Failure::Quota(_) => "agy 额度用完或被限流".to_string(),
            agy_cli::Failure::Auth(_) => "agy 登录失效".into(),
            agy_cli::Failure::Timeout { secs } => format!("agy 超时（总时限 {secs} 秒）"),
            agy_cli::Failure::Spawn(_) => "无法启动 agy 命令行".into(),
            agy_cli::Failure::Exit { code } => code.map_or_else(
                || "agy 进程异常退出".into(),
                |c| format!("agy 进程异常退出（退出码 {c}）"),
            ),
            agy_cli::Failure::Result(_) => "agy 命令行返回错误结果".into(),
            agy_cli::Failure::NoResult => "agy 命令行没有返回结果事件".into(),
        };
        outcome.reason = format!("判定调用失败，{detail}，转待人工确认");
        return outcome;
    }
    // 它想做白名单之外的事：不管最后给出什么结论，这次评审都不可信（可能绕开了证据工具读到没经过服务端记录的内容）
    if !response.violations.is_empty() || !response.denied.is_empty() {
        let mut what = response.violations.clone();
        what.extend(response.denied.iter().map(|action| format!("请求了被拒绝的权限 {action}")));
        outcome.reason = format!("判定失败，agy 在评审中越出了证据工具的范围（{}），评审结果不可信，转待人工确认", what.join("；"));
        return outcome;
    }
    conclude(&mut outcome, &prepared, "agy");
    outcome
}
