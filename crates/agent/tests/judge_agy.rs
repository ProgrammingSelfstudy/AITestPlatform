//! 用 agy（Gemini）做评审员的判定测试。`agy` 用 `/bin/sh` 脚本模拟，不连 Google、不花额度。
//!
//! 重点：**核对证据的逻辑和 Claude 评审员是同一份**，不因为评审员不同而放宽——
//! 引用了证据服务没记录过的证据，通过就降为待人工确认；越出证据工具范围的评审整次不可信。
//! 假脚本模拟证据服务：从临时 HOME 里专用的 MCP 配置读出运行目录，写 `served.json`（真实的证据服务就是这么记的）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent::agy_cli::AgyOpts;
use agent::judge::{AGY_TOOL_GUIDE, JUDGE_SYSTEM, JudgeOutcome, run_judge_agy};
use protocol::{CaseExpected, CaseSpec, FinalResult, JudgeStatus, Verdict};
use serde_json::{Value, json};

const SHOT: &str = "01_final.png";
const LOGIN_OK: &str = "applyAuthResult: email=someone@example.com, authMethod=consumer, quotaProject=";

fn case() -> CaseSpec {
    CaseSpec {
        code: "TC-04".into(),
        title: "选择音乐".into(),
        preconditions: vec![],
        steps: vec![],
        expected: vec![CaseExpected { idx: 1, text: "显示 {{song}}，账号 {{account.username}}".into() }],
        data: json!({"song": "测试曲目", "account.username": "占位符必须保留"}),
    }
}

struct Fake {
    dir: PathBuf,
    run_dir: PathBuf,
    opts: AgyOpts,
}

impl Fake {
    /// `model_part` 是模型调用（不带 `models` 参数）时脚本做的事；登录检查（`models`）由这里统一处理。
    fn new(login_line: &str, model_part: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("hafa-agyjudge-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let run_dir = dir.join("run");
        fs::create_dir_all(run_dir.join("shots")).unwrap();
        let real_home = dir.join("real_home");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
        let script = dir.join("agy.sh");
        fs::write(
            &script,
            format!(
                r#"set -u
case " $* " in *" models "*)
    LOG=; while [ $# -gt 0 ]; do case "$1" in --log-file) LOG="$2";; esac; shift; done
    echo '{login_line}' > "$LOG"; echo gemini-4-neon-high; exit 0;;
esac
cat > "$0.stdin"; printf '%s\n' "$@" > "$0.args"; echo x >> "$0.calls"
RUN=$(sed -n 's/.*"HAFA_RUN_DIR":"\([^"]*\)".*/\1/p' "$HOME/.gemini/config/mcp_config.json")
{model_part}
"#
            ),
        )
        .unwrap();
        let opts = AgyOpts {
            bin: "/bin/sh".into(),
            pre_args: vec![script.to_string_lossy().into()],
            model: Some("角色指定的假模型".into()),
            real_home: Some(real_home),
            total_timeout: Duration::from_secs(20),
            ..AgyOpts::default()
        };
        Self { dir, run_dir, opts }
    }

    fn add_shot(&self) {
        fs::write(self.run_dir.join("shots").join(SHOT), b"\x89PNG\r\n\x1a\n").unwrap();
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(format!("agy.sh.{name}"))).unwrap_or_default()
    }

    async fn run(&self) -> JudgeOutcome {
        run_judge_agy(&self.opts, &case(), "passed", "执行员自评", &self.run_dir, Path::new("target/debug/mcp-evidence")).await
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn row(verdict: &str, screenshots: Value) -> Value {
    json!({"index": 1, "verdict": verdict, "screenshots": screenshots, "evidence": "画面证据"})
}

fn answer(result: &str, items: Value) -> String {
    json!({"result": result, "items": items, "reason": "有证据", "disagreement": ""}).to_string()
}

/// 模型部分：先模拟证据服务记下「实际查看」的证据，再输出事件。
fn model_part(served: &[&str], events: &[Value]) -> String {
    let events = events.iter().map(|event| event.to_string()).collect::<Vec<_>>().join("\n");
    let served = serde_json::to_string(served).unwrap();
    format!("echo '{served}' > \"$RUN/served.json\"\ncat <<'AGY_EVENTS'\n{events}\nAGY_EVENTS")
}

fn result_event(text: &str) -> Value {
    json!({"event": "result", "result": {"conversation_id": "c", "status": "SUCCESS", "response": text, "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})
}

fn tool_event(index: i64, tool: &str, mcp: Option<(&str, &str)>) -> Value {
    let parameters = mcp.map_or(json!({"AbsolutePath": "/x"}), |(s, t)| json!({"ServerName": s, "ToolName": t, "Arguments": {}}));
    json!({"event": "step_update", "step_update": {"step_index": index, "state": "DONE", "step_type": "tool", "tool_name": tool, "tool_info": {"parameters": parameters}}})
}

#[tokio::test]
async fn 引用了实际查看的证据_判通过_证据引用被保留() {
    let fake = Fake::new(LOGIN_OK, &model_part(&[SHOT], &[tool_event(1, "call_mcp_tool", Some(("evidence", "image"))), result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
    fake.add_shot();
    let outcome = fake.run().await;
    assert_eq!(outcome.status, JudgeStatus::Completed, "{outcome:?}");
    assert_eq!(outcome.final_result, FinalResult::Passed);
    assert_eq!(outcome.items[0].verdict, Verdict::Passed);
    assert_eq!(outcome.items[0].file_refs, vec![SHOT.to_string()]);
    assert_eq!(outcome.cost_usd, None, "agy 没有折算费用");
}

#[tokio::test]
async fn 引用了没有查看过的证据_通过降为待人工确认_和_claude_评审员同一标准() {
    // 服务端的 served.json 里没有它引用的截图：它在编造引用
    let fake = Fake::new(LOGIN_OK, &model_part(&[], &[result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
    fake.add_shot();
    let outcome = fake.run().await;
    assert_eq!(outcome.status, JudgeStatus::Completed);
    assert_eq!(outcome.final_result, FinalResult::Uncertain, "不能因为评审员是 agy 就放宽");
    assert_eq!(outcome.items[0].verdict, Verdict::Uncertain);
    assert!(outcome.items[0].file_refs.is_empty());
}

#[tokio::test]
async fn 上一次评审留下的查看记录会被清掉_不能拿来替这次的引用背书() {
    // 模型部分：什么证据也没查看、不写 served.json，直接给出引用了截图的「通过」
    let events = result_event(&answer("passed", json!([row("passed", json!([SHOT]))])));
    let fake = Fake::new(LOGIN_OK, &format!("cat <<'E'\n{events}\nE"));
    fake.add_shot();
    fs::write(fake.run_dir.join("served.json"), format!("[\"{SHOT}\"]")).unwrap();
    let outcome = fake.run().await;
    assert_eq!(outcome.final_result, FinalResult::Uncertain, "旧的 served.json 必须在评审开始前清掉：{outcome:?}");
    assert_eq!(outcome.items[0].verdict, Verdict::Uncertain);
}

#[tokio::test]
async fn 越出证据工具范围_整次评审不可信_转待人工确认() {
    // 它去读了文件（工作目录里的文件不用放行就能读，所以靠审计发现）。即使最后给出了「通过」也不能采信
    let fake = Fake::new(LOGIN_OK, &model_part(&[SHOT], &[tool_event(1, "read_file", None), result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
    fake.add_shot();
    let outcome = fake.run().await;
    assert_eq!(outcome.status, JudgeStatus::Failed);
    assert_eq!(outcome.final_result, FinalResult::Uncertain);
    assert!(outcome.items.iter().all(|item| item.verdict == Verdict::Uncertain));
    assert!(outcome.reason.contains("越出了证据工具的范围") && outcome.reason.contains("read_file"), "{}", outcome.reason);

    // 别的 MCP 服务
    let other = Fake::new(LOGIN_OK, &model_part(&[SHOT], &[tool_event(1, "call_mcp_tool", Some(("other", "image"))), result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
    other.add_shot();
    assert_eq!(other.run().await.final_result, FinalResult::Uncertain);
}

#[tokio::test]
async fn 有被自动拒绝的动作_同样不可信() {
    let mut event = result_event(&answer("passed", json!([row("passed", json!([SHOT]))])));
    event["result"]["denied_actions"] = json!([{"action": "command"}]);
    let fake = Fake::new(LOGIN_OK, &model_part(&[SHOT], &[event]));
    fake.add_shot();
    let outcome = fake.run().await;
    assert_eq!(outcome.status, JudgeStatus::Failed);
    assert_eq!(outcome.final_result, FinalResult::Uncertain);
    assert!(outcome.reason.contains("command"), "{}", outcome.reason);
}

#[tokio::test]
async fn 额度用完标记出来_判定失败待确认_不换别的方式() {
    let event = json!({"event": "result", "result": {"status": "ERROR", "error": "RESOURCE_EXHAUSTED: quota exceeded"}});
    let fake = Fake::new(LOGIN_OK, &model_part(&[], &[event]));
    fake.add_shot();
    let outcome = fake.run().await;
    assert!(outcome.quota_exhausted);
    assert_eq!((outcome.status, outcome.final_result), (JudgeStatus::Failed, FinalResult::Uncertain));
    assert!(outcome.reason.contains("agy 额度用完"), "{}", outcome.reason);
    assert_eq!(fake.file("calls").lines().count(), 1, "不重试");
}

#[tokio::test]
async fn 登录检查没过_不调模型() {
    for line in ["applyAuthResult: email=a@b.c, authMethod=api_key, quotaProject=", "没有认证方式"] {
        let fake = Fake::new(line, &model_part(&[SHOT], &[result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
        fake.add_shot();
        let outcome = fake.run().await;
        assert_eq!((outcome.status, outcome.final_result), (JudgeStatus::Failed, FinalResult::Uncertain));
        assert!(outcome.reason.starts_with("判定调用失败，") && outcome.reason.ends_with("转待人工确认"), "{}", outcome.reason);
        assert!(!outcome.reason.contains("a@b.c"), "不能带账号信息");
        assert_eq!(fake.file("calls"), "", "登录检查没过，模型一次都不能调：{line}");
    }
}

#[tokio::test]
async fn 没有任何证据时不启动_agy() {
    let fake = Fake::new(LOGIN_OK, &model_part(&[], &[result_event("{}")]));
    // 没有 add_shot，也没有步骤文字
    let outcome = fake.run().await;
    assert_eq!(outcome.status, JudgeStatus::NotRun);
    assert_eq!(fake.file("calls"), "");
}

#[tokio::test]
async fn 提示词合并了判定规则_走标准输入_不进参数_证据_mcp_按这次运行目录配置() {
    let part = format!("{}\ncp \"$HOME/.gemini/config/mcp_config.json\" \"$0.mcp\"", model_part(&[SHOT], &[result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))]));
    let fake = Fake::new(LOGIN_OK, &part);
    fake.add_shot();
    let outcome = fake.run().await;
    assert_eq!(outcome.final_result, FinalResult::Passed, "{outcome:?}");
    let stdin: Value = serde_json::from_str(fake.file("stdin").lines().next().unwrap()).unwrap();
    let prompt = stdin["message"]["content"].as_str().unwrap();
    assert!(prompt.starts_with(AGY_TOOL_GUIDE), "agy 专用的工具使用说明在最前面：不说清楚它会去读文件");
    assert!(prompt.contains(JUDGE_SYSTEM), "判定规则要合并进用户消息：agy 没有系统提示词开关");
    assert!(prompt.contains("call_mcp_tool") && prompt.contains("ServerName 填 evidence") && prompt.contains("不要使用 read_file"), "{AGY_TOOL_GUIDE}");
    // 和 Claude 评审员同一个提示词：测试数据展开，账号占位符原样保留、账号值不出现
    assert!(prompt.contains("1. 显示 测试曲目，账号 {{account.username}}"), "{prompt}");
    assert!(!prompt.contains("占位符必须保留"), "账号值不能进提示词");
    assert!(prompt.contains(SHOT), "证据索引要在提示词里");
    let args = fake.file("args");
    assert!(!args.contains("测试曲目") && !args.contains("判定员"), "提示词不能出现在命令行参数里：{args}");
    let mcp: Value = serde_json::from_str(&fake.file("mcp")).unwrap();
    let env: BTreeMap<String, String> = serde_json::from_value(mcp["mcpServers"]["evidence"]["env"].clone()).unwrap();
    assert_eq!(PathBuf::from(&env["HAFA_RUN_DIR"]), fake.run_dir.canonicalize().unwrap(), "证据服务看的是这次的运行目录");
}

#[tokio::test]
async fn 同一个运行目录同时只能有一次评审() {
    let slow = Fake::new(LOGIN_OK, &format!("sleep 3\n{}", model_part(&[SHOT], &[result_event(&answer("passed", json!([row("passed", json!([SHOT]))])))])));
    slow.add_shot();
    let second = AgyOpts { ..slow.opts.clone() };
    let first = tokio::spawn({
        let (opts, run_dir) = (slow.opts.clone(), slow.run_dir.clone());
        async move { run_judge_agy(&opts, &case(), "passed", "自评", &run_dir, Path::new("target/debug/mcp-evidence")).await }
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let blocked = run_judge_agy(&second, &case(), "passed", "自评", &slow.run_dir, Path::new("target/debug/mcp-evidence")).await;
    assert_eq!((blocked.status, blocked.final_result), (JudgeStatus::Failed, FinalResult::Uncertain));
    assert!(blocked.reason.contains("无法准备证据 MCP"), "{}", blocked.reason);
    assert_eq!(first.await.unwrap().final_result, FinalResult::Passed, "第一次评审不受影响");
}
