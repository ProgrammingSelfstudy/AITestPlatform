use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use agent::judge::{Checked, JUDGE_SYSTEM, JudgeOutcome, build_prompt, checked, parse, run_judge};
use protocol::{CaseExpected, CaseSpec, FinalResult, JudgeStatus, Verdict};
use serde_json::{Value, json};

#[path = "support/fake_claude.rs"]
mod fake_claude;
use fake_claude::FakeClaude;

const SHOT: &str = "01_final.png";

fn case() -> CaseSpec {
    CaseSpec {
        code: "TC-04".into(),
        title: "选择音乐".into(),
        preconditions: vec![],
        steps: vec![],
        expected: vec![CaseExpected {
            idx: 1,
            text: "显示 {{song}}，账号 {{account.username}}".into(),
        }],
        data: json!({"song": "测试曲目", "account.username": "占位符必须保留"}),
    }
}

fn row(index: u32, verdict: &str, screenshots: Value) -> Value {
    json!({"index": index, "verdict": verdict, "screenshots": screenshots, "evidence": "画面证据"})
}

fn check(result: &str, items: Value, count: usize, served: &[&str]) -> Checked {
    let parsed = parse(&json!({"result": result, "items": items, "reason": "模型总结", "disagreement": "与执行员有分歧"}).to_string()).unwrap();
    checked(
        Some(&parsed),
        &vec!["预期".into(); count],
        &served.iter().map(|s| (*s).into()).collect(),
    )
}

#[test]
fn 提示词展开测试数据并保留账号占位符和证据索引() {
    let prompt = build_prompt(
        &case(),
        "passed",
        "执行员自评",
        "页面有曲目",
        &[(SHOT.into(), "步骤 1 · 终态原图".into())],
    );
    for text in [
        "## 用例 TC-04：选择音乐",
        "1. 显示 测试曲目，账号 {{account.username}}",
        "### 执行员提交的结论（仅供参考，不可直接采信）",
        "- result: passed",
        "- reason: 执行员自评",
        "### 终态页面文字（final_page.txt）\n页面有曲目",
        "- 01_final.png：步骤 1 · 终态原图",
        "### 步骤文字索引",
    ] {
        assert!(prompt.contains(text), "提示词缺少 {text}");
    }
    assert!(!prompt.contains("占位符必须保留"));
}

#[test]
fn 判定规则要求实际查看证据并先查瞬时提示的步骤文字() {
    for rule in [
        "passed / failed 每条必须引用实际查看过的证据（截图文件名或 step:N）",
        "缺少支持就记 uncertain",
        "预期和实际不一致必须判 failed，不允许强行通过。",
        "截图没拍到瞬时提示（toast）时，必须先用 step_text 查相关步骤的文字，再决定是否记 uncertain。",
        r#""screenshots": ["实际查看过的截图文件名或 step:N"]"#,
    ] {
        assert!(JUDGE_SYSTEM.contains(rule), "判定规则缺少 {rule}");
    }
}

#[test]
fn passed没有任何证据引用时降为待确认() {
    let r = check("passed", json!([row(1, "passed", json!([]))]), 1, &[SHOT]);
    assert_eq!(
        (r.final_result, r.items[0].verdict),
        (FinalResult::Uncertain, Verdict::Uncertain)
    );
    assert_eq!(
        r.items[0].evidence,
        "该项没有引用可核实的证据，或包含未实际查看的证据"
    );
}

#[test]
fn 任一引用未查看时降级且只保留已查看引用() {
    for verdict in ["passed", "failed"] {
        let r = check(
            verdict,
            json!([row(1, verdict, json!([SHOT, "02_final.png"]))]),
            1,
            &[SHOT],
        );
        assert_eq!(
            (r.final_result, r.items[0].verdict),
            (FinalResult::Uncertain, Verdict::Uncertain)
        );
        assert_eq!(r.items[0].file_refs, [SHOT]);
    }
    let invalid = check(
        "passed",
        json!([row(1, "passed", json!([SHOT, 2]))]),
        1,
        &[SHOT],
    );
    assert_eq!(invalid.items[0].verdict, Verdict::Uncertain);
}

#[test]
fn 未查看的步骤文字引用使该项待确认() {
    for verdict in ["passed", "failed"] {
        let r = check(
            verdict,
            json!([row(1, verdict, json!([SHOT, "step:8", "step:9"]))]),
            1,
            &[SHOT, "step:8"],
        );
        assert_eq!(
            (r.final_result, r.items[0].verdict),
            (FinalResult::Uncertain, Verdict::Uncertain)
        );
        assert_eq!(r.items[0].file_refs, [SHOT, "step:8"]);
    }
}

#[test]
fn 有效failed优先于条数不符和待确认() {
    for items in [
        json!([row(2, "failed", json!([SHOT]))]),
        json!([
            row(1, "passed", json!(["step:8"])),
            row(2, "failed", json!([SHOT]))
        ]),
    ] {
        let r = check("passed", items, 2, &[SHOT, "step:8"]);
        assert_eq!(r.final_result, FinalResult::Failed);
        assert_eq!(
            r.items.iter().map(|i| i.verdict).collect::<Vec<_>>(),
            [Verdict::Uncertain, Verdict::Failed]
        );
        assert_eq!(r.reason, "按逐项结论及实际查看的证据汇总");
        assert_eq!(r.disagreement, "与执行员有分歧");
    }
}

#[test]
fn 模型逐项条数不符时整体待确认() {
    let extra = check(
        "passed",
        json!([row(1, "passed", json!([SHOT])), {"index": 99}]),
        1,
        &[SHOT],
    );
    assert_eq!(
        (extra.final_result, extra.items[0].verdict),
        (FinalResult::Uncertain, Verdict::Passed)
    );
    let missing = check(
        "passed",
        json!([row(1, "passed", json!([SHOT]))]),
        2,
        &[SHOT],
    );
    assert_eq!(
        (missing.final_result, missing.items[1].verdict),
        (FinalResult::Uncertain, Verdict::Uncertain)
    );
}

#[test]
fn 模型整体报passed但一项待确认时整体待确认() {
    let r = check(
        "passed",
        json!([
            row(1, "passed", json!([SHOT])),
            row(2, "uncertain", json!([]))
        ]),
        2,
        &[SHOT],
    );
    assert_eq!(r.final_result, FinalResult::Uncertain);
    assert_eq!(r.items[1].evidence, "画面证据");
}

#[test]
fn 通过必须兼有模型整体passed和全部有效逐项passed() {
    let items = json!([row(1, "passed", json!([SHOT]))]);
    let uncertain = check("uncertain", items.clone(), 1, &[SHOT]);
    assert_eq!(
        (uncertain.final_result, uncertain.items[0].verdict),
        (FinalResult::Uncertain, Verdict::Passed)
    );
    assert_eq!(uncertain.reason, "模型总结");
    let passed = check("passed", items, 1, &[SHOT]);
    assert_eq!(passed.final_result, FinalResult::Passed);
    assert_eq!(passed.reason, "模型总结");
    assert_eq!(
        check("passed", json!([]), 0, &[]).final_result,
        FinalResult::Uncertain
    );
}

#[test]
fn 无效证据说明和无效序号判为待确认() {
    for item in [
        json!({"index": 1, "verdict": "passed", "screenshots": [SHOT], "evidence": " \t\n"}),
        json!({"index": 1, "verdict": "passed", "screenshots": [SHOT], "evidence": 1}),
        json!({"index": 1, "verdict": "PASS", "screenshots": [SHOT], "evidence": "说明"}),
        json!({"index": true, "verdict": "passed", "screenshots": [SHOT], "evidence": "说明"}),
        json!({"index": 1.0, "verdict": "passed", "screenshots": [SHOT], "evidence": "说明"}),
    ] {
        let r = check("passed", json!([item]), 1, &[SHOT]);
        assert_eq!(
            (r.final_result, r.items[0].verdict),
            (FinalResult::Uncertain, Verdict::Uncertain)
        );
        assert_eq!(r.items[0].evidence, "缺少唯一、有效的逐项判定或证据说明");
    }
}

#[test]
fn 乱_json无法抢救时整体待确认() {
    let parsed = parse("结果\n{这不是 JSON}\n结束");
    assert!(parsed.is_none());
    let verdict = checked(parsed.as_ref(), &["预期".into()], &HashSet::new());
    assert_eq!(
        (verdict.final_result, verdict.items[0].verdict),
        (FinalResult::Uncertain, Verdict::Uncertain)
    );
}

#[test]
fn 解析截取首尾括号且能抢救完整逐项对象() {
    let good = r#"结果如下
{"result":"passed", "items":[{"index":1,"verdict":"passed","screenshots":["01_final.png"],"evidence":"甲"}]}
结束"#;
    assert!(!parse(good).unwrap().salvaged);
    let broken = r#"{"result":"passed","reason":"前面的理由","items":[
        {"index":2,"verdict":"passed","screenshots":["01_final.png"],"evidence":"乙"},
        {"index":1,"verdict":"passed","screenshots":["01_final.png"],"evidence":"甲"}
    }], "reason":"最后的理由","disagreement":"分歧"}"#;
    let parsed = parse(broken).unwrap();
    assert!(parsed.salvaged);
    let out = checked(
        Some(&parsed),
        &["预期甲".into(), "预期乙".into()],
        &HashSet::from([SHOT.into()]),
    );
    assert_eq!(out.final_result, FinalResult::Passed);
    assert_eq!(
        out.items
            .iter()
            .map(|i| (i.idx, i.evidence.as_str()))
            .collect::<Vec<_>>(),
        [(1, "甲"), (2, "乙")]
    );
    assert_eq!(out.reason, "最后的理由");
    assert_eq!(out.disagreement, "分歧");
}

#[test]
fn 同序号重复的完整与损坏判定都不采信() {
    let item = row(1, "passed", json!([SHOT]));
    let full = check("passed", json!([item.clone(), item]), 1, &[SHOT]);
    assert_eq!(full.items[0].verdict, Verdict::Uncertain);
    let broken = r#"{"result":"passed","items":[
        {"index":1,"verdict":"passed","screenshots":["01_final.png"],"evidence":"甲"},
        {"index":1, 损坏},
        {"index":2,"verdict":"passed","screenshots":["01_final.png"],"evidence":"乙"}
    ]}"#;
    let parsed = parse(broken).unwrap();
    assert!(parsed.salvaged);
    let out = checked(
        Some(&parsed),
        &["预期甲".into(), "预期乙".into()],
        &HashSet::from([SHOT.into()]),
    );
    assert_eq!(out.final_result, FinalResult::Uncertain);
    assert_eq!(
        out.items.iter().map(|i| i.verdict).collect::<Vec<_>>(),
        [Verdict::Uncertain, Verdict::Passed]
    );
    assert_eq!(out.items[0].evidence, "缺少唯一、有效的逐项判定或证据说明");
}

impl FakeClaude {
    fn judge(script: &str) -> Self {
        Self::judge_with_login(
            r#"printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"pro"}'"#,
            script,
        )
    }

    fn judge_with_login(auth: &str, script: &str) -> Self {
        let fake = Self::new(script);
        let path = fake.dir.join("claude.sh");
        let model_script = fs::read_to_string(&path).unwrap();
        fs::write(
            path,
            format!(
                r#"if [ "$#" -eq 5 ] && [ "$1" = --setting-sources ] && [ -z "$2" ] &&
    [ "$3" = auth ] && [ "$4" = status ] && [ "$5" = --json ]; then
    {auth}
    exit 0
fi
{model_script}"#
            ),
        )
        .unwrap();
        fake
    }

    fn add_shot(&self) {
        fs::write(self.run_dir.join("shots").join(SHOT), b"\x89PNG\r\n\x1a\n").unwrap();
    }

    async fn run(&self) -> JudgeOutcome {
        self.run_case(&case()).await
    }

    async fn run_case(&self, case: &CaseSpec) -> JudgeOutcome {
        run_judge(
            &self.opts,
            case,
            "passed",
            "执行员自评",
            &self.run_dir,
            Path::new("target/debug/mcp-evidence"),
        )
        .await
    }
}

fn success(text: &str) -> String {
    let event =
        json!({"type": "result", "is_error": false, "result": text, "total_cost_usd": 0.125});
    format!("cat <<'JUDGE_RESULT'\n{event}\nJUDGE_RESULT")
}

fn passed() -> String {
    json!({"result": "passed", "items": [row(1, "passed", json!([SHOT]))], "reason": "有证据", "disagreement": ""}).to_string()
}

#[tokio::test]
async fn 截图和步骤文字清单都为空时不启动_claude() {
    let fake = FakeClaude::new("exit 91");
    fs::write(
        fake.run_dir.join("trace.json"),
        r#"{"steps":[{"idx":1,"action":"need_human","after_labels":[]}]}"#,
    )
    .unwrap();
    let r = fake.run().await;
    assert_eq!(
        (r.status, r.final_result),
        (JudgeStatus::NotRun, FinalResult::Uncertain)
    );
    assert_eq!(r.reason, "本次轨迹无可用截图和步骤文字，未调用模型");
    assert_eq!(r.items[0].verdict, Verdict::Uncertain);
    assert!(!fake.dir.join("claude.sh.prompt").exists());
}

#[tokio::test]
async fn 只引用无文字步骤且终态无文字时通过降为待人工确认() {
    let result = json!({
        "result": "passed", "items": [{
            "index": 1, "verdict": "passed", "screenshots": ["step:8", "step:9"],
            "evidence": "显示测试曲目",
        }],
    })
    .to_string();
    let fake = FakeClaude::judge(&format!(
        "printf '%s\\n' '[\"step:8\",\"step:9\"]' > run/served.json\n{}",
        success(&result)
    ));
    fs::write(
        fake.run_dir.join("trace.json"),
        json!({"evidence": ["final_page.txt"], "steps": [
            {"idx": 7, "action": "tap", "after_labels": ["测试曲目"]},
            {"idx": 8, "action": "tap", "target": "测试曲目", "after_labels": []},
            {"idx": 9, "action": "tap", "after_labels": [" \t "], "new_labels": ["\n"]},
        ]})
        .to_string(),
    )
    .unwrap();
    let mut case = case();
    case.expected[0].text = "显示测试曲目".into();

    fs::write(fake.run_dir.join("final_page.txt"), " \n").unwrap();
    let r = fake.run_case(&case).await;
    assert_eq!(
        (r.status, r.final_result, r.items[0].verdict),
        (
            JudgeStatus::Completed,
            FinalResult::Uncertain,
            Verdict::Uncertain
        )
    );
    assert_eq!(r.items[0].file_refs, ["step:8", "step:9"]);
    assert_eq!(
        r.items[0].evidence,
        "所引步骤与终态页面均无可用文字，转待人工确认"
    );

    fs::write(fake.run_dir.join("final_page.txt"), "测试曲目").unwrap();
    let r = fake.run_case(&case).await;
    assert_eq!(
        (r.final_result, r.items[0].verdict),
        (FinalResult::Passed, Verdict::Passed)
    );
}

#[tokio::test]
async fn 仅引用已查看的有效步骤文字可判passed() {
    let result = json!({
        "result": "passed", "items": [{
            "index": 1, "verdict": "passed", "screenshots": ["step:8"],
            "evidence": "测试曲目",
        }],
    })
    .to_string();
    let fake = FakeClaude::judge(&format!(
        "printf '%s\\n' '[\"step:8\"]' > run/served.json\n{}",
        success(&result)
    ));
    fs::write(
        fake.run_dir.join("trace.json"),
        r#"{"steps":[{"idx":8,"action":"tap","after_labels":[" "],"new_labels":["测试曲目"]}]}"#,
    )
    .unwrap();
    let mut case = case();
    case.expected[0].text = "显示测试曲目".into();
    let r = fake.run_case(&case).await;
    assert_eq!(
        (r.final_result, r.items[0].verdict),
        (FinalResult::Passed, Verdict::Passed)
    );
    assert_eq!(r.items[0].file_refs, ["step:8"]);
}

#[tokio::test]
async fn 只有步骤文字时启动假_claude并接受已查看的失败引用() {
    let actual = "Music size cannot exceed 50MB";
    let result = json!({"result": "failed", "items": [{
        "index": 1, "verdict": "failed", "screenshots": ["step:8"],
        "evidence": actual,
    }]})
    .to_string();
    let fake = FakeClaude::judge(&format!(
        "printf '%s\\n' '[\"step:8\"]' > run/served.json\n{}",
        success(&result)
    ));
    fs::remove_dir(fake.run_dir.join("shots")).unwrap();
    fs::write(
        fake.run_dir.join("trace.json"),
        json!({"steps": [{
            "idx": 8, "action": "tap", "target": "添加音乐",
            "after_labels": [actual], "new_labels": [actual],
        }]})
        .to_string(),
    )
    .unwrap();
    let mut case = case();
    case.code = "TC-06c-F".into();
    case.expected[0].text = "点击后提示 Music size cannot exceed 100MB".into();

    let r = fake.run_case(&case).await;
    assert_eq!(
        (r.status, r.final_result),
        (JudgeStatus::Completed, FinalResult::Failed)
    );
    assert_eq!(r.items[0].verdict, Verdict::Failed);
    assert_eq!(r.items[0].file_refs, ["step:8"]);
    assert_eq!(r.items[0].evidence, actual);
    let prompt = fs::read_to_string(fake.dir.join("claude.sh.prompt")).unwrap();
    for text in [
        "### 步骤文字索引",
        "- 步骤 8 · tap 添加音乐 · 有新出现的文字",
        "调用 step_text(step) 按整数步骤号查看手机控件树文字",
        "工具层记录的页面事实，实际查看后可引用 step:N",
        "截图拍不到的瞬时提示（toast）、文字很小或被遮挡的内容",
        "执行员的口述仅供参考",
    ] {
        assert!(prompt.contains(text), "提示词缺少 {text}");
    }
    assert!(!prompt.contains(actual));
}

#[tokio::test]
async fn 只启用证据_mcp且沿用角色参数和本次取图记录() {
    let mut fake = FakeClaude::judge(&format!(
        r#"
previous=''
for arg do
    if [ "$previous" = "--mcp-config" ]; then
        cp "$arg" "$0.config"
        printf '%s\n' "$arg" > "$0.config_path"
    fi
    previous="$arg"
done
printf '%s\n' '["{SHOT}"]' > run/served.json
{}"#,
        success(&passed())
    ));
    fake.add_shot();
    fake.opts.model = Some("角色指定的模型".into());
    fake.opts.effort = Some("high".into());
    fake.opts.mcp_config = Some("其它服务.json".into());
    fake.opts.system_prompt = Some("应覆盖的指令".into());
    fake.opts.tools = vec!["Read".into()];
    fake.opts.allowed_tools = vec!["mcp__device__tap".into()];
    fs::write(
        fake.run_dir.join("trace.json"),
        json!({"evidence": ["final_page.txt"], "steps": []}).to_string(),
    )
    .unwrap();
    fs::write(
        fake.run_dir.join("final_page.txt"),
        format!("{}页尾", "页".repeat(4000)),
    )
    .unwrap();
    fs::write(
        fake.run_dir.join("human_events.json"),
        r#"[{"reason":"确认音乐","mode":"sim","note":"脚本已确认","waited_s":3}]"#,
    )
    .unwrap();

    let r = fake.run().await;
    assert_eq!(
        (r.status, r.final_result, r.cost_usd),
        (JudgeStatus::Completed, FinalResult::Passed, Some(0.125)),
        "{r:?}"
    );
    assert_eq!(r.items[0].file_refs, [SHOT]);
    assert_eq!(r.items[0].evidence, "画面证据");
    assert_eq!(r.raw, passed());
    assert!(r.wall_s > 0.0);
    let args = fs::read_to_string(fake.dir.join("claude.sh.args")).unwrap();
    for arg in [
        "--strict-mcp-config\n",
        "--tools\n\n",
        "--allowed-tools\nmcp__evidence__image,mcp__evidence__step_text\n",
        "--model\n角色指定的模型\n",
        "--effort\nhigh\n",
        JUDGE_SYSTEM,
    ] {
        assert!(args.contains(arg), "缺少参数 {arg}");
    }
    let run = fake.run_dir.canonicalize().unwrap();
    assert!(!args.contains(run.to_str().unwrap()));
    assert!(
        !args.contains("应覆盖的指令")
            && !args.contains("mcp__device__tap")
            && !args.contains("其它服务.json")
    );
    let mcp: Value =
        serde_json::from_slice(&fs::read(fake.dir.join("claude.sh.config")).unwrap()).unwrap();
    assert_eq!(
        mcp,
        json!({"mcpServers": {"evidence": {
            "command": std::path::absolute("target/debug/mcp-evidence").unwrap(), "args": [], "env": {"HAFA_RUN_DIR": run},
        }}})
    );
    let temp = fs::read_to_string(fake.dir.join("claude.sh.config_path")).unwrap();
    assert!(!Path::new(temp.trim()).exists(), "临时配置需要清理");
    let prompt = fs::read_to_string(fake.dir.join("claude.sh.prompt")).unwrap();
    assert!(prompt.contains("页".repeat(4000).as_str()) && !prompt.contains("页尾"));
    assert!(prompt.contains("- 请求：确认音乐\n  处理方式 mode=sim，结果：脚本已确认，等待 3s"));
}

#[tokio::test]
async fn 旧查看记录清零且终态文字必须列在轨迹中() {
    let fake = FakeClaude::judge(&success(&passed()));
    fake.add_shot();
    fs::write(fake.run_dir.join("served.json"), json!([SHOT]).to_string()).unwrap();
    fs::write(fake.run_dir.join("final_page.txt"), "未列入轨迹的文字").unwrap();
    fs::write(
        fake.run_dir.join("trace.json"),
        r#"{"steps":[{"idx":1,"action":"need_human","value":"请人工检查"}]}"#,
    )
    .unwrap();
    let r = fake.run().await;
    assert_eq!(
        (r.status, r.final_result),
        (JudgeStatus::Completed, FinalResult::Uncertain)
    );
    assert!(r.items[0].file_refs.is_empty());
    assert!(!fake.run_dir.join("served.json").exists());
    let prompt = fs::read_to_string(fake.dir.join("claude.sh.prompt")).unwrap();
    assert!(!prompt.contains("未列入轨迹的文字"));
    assert!(prompt.contains("- 请求：请人工检查（没有处理记录）"));
}

#[tokio::test]
async fn 会员登录检查失败时明确报错并跳过判定() {
    for auth in [
        r#"printf '%s\n' '{"loggedIn":false,"authMethod":"none"}'
echo '虚构的登录诊断详情' >&2
exit 1"#,
        r#"printf '%s\n' '{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}'"#,
        r#"printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":null}'"#,
    ] {
        let fake = FakeClaude::judge_with_login(auth, &success(&passed()));
        fake.add_shot();
        let r = fake.run().await;
        assert_eq!(
            (r.status, r.final_result, r.quota_exhausted),
            (JudgeStatus::Failed, FinalResult::Uncertain, false)
        );
        assert_eq!(
            r.reason,
            "判定调用失败，未检测到 Claude 会员登录，请在本机用 `claude auth login` 登录会员账号，转待人工确认"
        );
        assert!(r.raw.is_empty());
        assert!(!fake.dir.join("claude.sh.prompt").exists());
    }
}

#[tokio::test]
async fn 会员额度耗尽标记且判定失败待确认() {
    let fake = FakeClaude::judge(
        r#"echo '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}'
echo '{"type":"result","is_error":true,"result":"已达上限"}'"#,
    );
    fake.add_shot();
    let r = fake.run().await;
    assert!(r.quota_exhausted);
    assert_eq!(
        (r.status, r.final_result),
        (JudgeStatus::Failed, FinalResult::Uncertain)
    );
    assert!(r.reason.contains("额度"));
}

#[tokio::test]
async fn 命令成功但结果不可解析时判定失败待确认() {
    let fake = FakeClaude::judge(&success("{结果损坏}"));
    fake.add_shot();
    let r = fake.run().await;
    assert_eq!(
        (r.status, r.final_result, r.quota_exhausted),
        (JudgeStatus::Failed, FinalResult::Uncertain, false)
    );
    assert!(r.reason.contains("无法解析"));
    assert_eq!(r.raw, "{结果损坏}");
}

#[tokio::test]
async fn 命令超时判定失败待确认() {
    let mut fake = FakeClaude::judge("sleep 10");
    fake.add_shot();
    fake.opts.total_timeout = Duration::from_millis(200);
    let r = fake.run().await;
    assert_eq!(
        (r.status, r.final_result, r.quota_exhausted),
        (JudgeStatus::Failed, FinalResult::Uncertain, false)
    );
    assert!(r.reason.contains("超时"), "{r:?}");
}
