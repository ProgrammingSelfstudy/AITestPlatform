use std::fs;
use std::collections::BTreeMap;

use agent::prompts::SCRIPTHEAL_SYSTEM;
use agent::scriptcheck::UiContract;
use agent::scriptgen::{
    HealContext, HealVerdict, ScriptTrace, ScriptgenError, ScriptgenInput, ScriptgenOpts, assert_count, heal,
};
use protocol::{CaseExpected, CaseSpec, FinalResult, JudgeItem, ModelSource, RoleModel, Verdict};
use serde_json::{Value, json};

#[path = "support/fake_claude.rs"]
mod fake_claude;
use fake_claude::FakeClaude;

const CONTRACT: &str = include_str!("fixtures/scriptgen/ui_contract.json");
const TRACE: &str = include_str!("fixtures/scriptgen/trace.json");
const TEST_NAME: &str = "test_tc04.py";
const PAGE_NAME: &str = "pages/tc04_pages.py";
const TEST_POC: &str = include_str!("fixtures/scriptgen/generated/test_tc04.py");
const PAGE_POC: &str = include_str!("fixtures/scriptgen/generated/pages/tc04_pages.py");
const USER: &str = "fixture_login_name";
const PASSWORD: &str = "demoPass\"with\\Slash99";

struct Inputs {
    case: CaseSpec,
    trace: ScriptTrace,
    items: Vec<JudgeItem>,
    map: Value,
    contract: UiContract,
    redactions: Vec<(String, String)>,
}

impl Inputs {
    fn new() -> Self {
        Self {
            case: CaseSpec {
                code: "TC-04".into(),
                title: "滑动并添加歌曲".into(),
                preconditions: vec!["已登录、进房".into()],
                steps: vec!["点 More，选歌 {{song}}，不展开 {{account.username}}".into()],
                expected: vec![CaseExpected {
                    idx: 1,
                    text: "显示曲目 {{song}}".into(),
                }],
                data: json!({"song": "测试曲目", "account.username": USER}),
            },
            trace: serde_json::from_str(TRACE).unwrap(),
            items: vec![JudgeItem {
                idx: 1,
                verdict: Verdict::Passed,
                evidence: format!("页面显示 Music。本次相关上下文 {USER} 和 {PASSWORD}"),
                file_refs: vec!["shots/01_after.png".into(), "step:1".into()],
            }],
            map: json!({"More 面板": "Music、Interaction Tools", "测试说明": PASSWORD}),
            contract: UiContract::from_json(CONTRACT.as_bytes()).unwrap(),
            redactions: vec![
                (USER.into(), "{{account.username}}".into()),
                (PASSWORD.into(), "{{account.password}}".into()),
            ],
        }
    }

    fn input(&self) -> ScriptgenInput<'_> {
        ScriptgenInput {
            case: &self.case,
            trace: &self.trace,
            judge_result: FinalResult::Passed,
            judge_items: &self.items,
            ui_map: &self.map,
            contract: &self.contract,
            redactions: &self.redactions,
            previous_attempt: None,
        }
    }
}

fn opts(fake: &FakeClaude) -> ScriptgenOpts {
    ScriptgenOpts {
        role: RoleModel {
            source: ModelSource::ClaudeCli,
            model: Some("由角色指定的假模型".into()),
            effort: Some("high".into()),
        },
        claude: fake.opts.clone(),
    }
}

fn fake_events(events: &[String]) -> FakeClaude {
    let mut script = String::from(
        r#"
count=0
if [ -f "$0.count" ]; then read -r count < "$0.count"; fi
count=$((count + 1))
printf '%s\n' "$count" > "$0.count"
cp "$0.prompt" "$0.prompt.$count"
cp "$0.args" "$0.args.$count"
case "$count" in
"#,
    );
    for (index, events) in events.iter().enumerate() {
        script.push_str(&format!(
            "  {})\ncat <<'SCRIPTGEN_EVENTS'\n{events}\nSCRIPTGEN_EVENTS\n;;\n",
            index + 1
        ));
    }
    script.push_str("*)\nexit 97\n;;\nesac");
    FakeClaude::new(&script)
}

fn success(text: &str) -> String {
    json!({"type": "result", "is_error": false, "result": text}).to_string()
}


fn captured(fake: &FakeClaude, part: &str, turn: u8) -> String {
    fs::read_to_string(fake.dir.join(format!("claude.sh.{part}.{turn}"))).unwrap()
}

fn count(fake: &FakeClaude) -> u8 {
    fs::read_to_string(fake.dir.join("claude.sh.count"))
        .map_or(0, |text| text.trim().parse().unwrap())
}

fn assert_credentials_absent(text: &str) {
    for secret in [
        USER,
        PASSWORD,
        "demoPass",
        "Slash99",
        "old-trace@example.test",
        "/Users/fixture/private",
        "13579024",
    ] {
        assert!(!text.contains(secret), "模型输入含模拟凭证或设备信息");
    }
}


fn original() -> BTreeMap<String, String> {
    [(TEST_NAME.to_string(), TEST_POC.to_string()), (PAGE_NAME.to_string(), PAGE_POC.to_string())].into()
}

/// 模型给的自愈回复。`files` 为空表示不带文件。
fn reply(verdict: &str, reason: &str, files: Option<(&str, &str)>) -> String {
    let mut value = json!({"verdict": verdict, "reason": reason, "notes": "改了定位"});
    if let Some((test, page)) = files {
        value["files"] = json!({ TEST_NAME: test, PAGE_NAME: page });
    }
    success(&value.to_string())
}

/// 在页面文件末尾加一行注释：内容变了，但断言一条没少，静态校验也不受影响。
fn relocated_page() -> String {
    format!("{PAGE_POC}\n# 重新按文案定位\n")
}

async fn run(fake: &FakeClaude, failure: &str, elements: &str) -> Result<agent::scriptgen::HealResult, ScriptgenError> {
    let files = original();
    heal(&opts(fake), Inputs::new().input(), &HealContext { files: &files, failure, elements }).await
}

#[tokio::test]
async fn 定位失效且修复合规_产出带哈希的修复文件() {
    let page = relocated_page();
    let fake = fake_events(&[reply("locator_broken", "elements 里 Add 按钮的文案变成了 Confirm", Some((TEST_POC, &page)))]);
    let result = run(&fake, "AssertionError: 没找到 Add", "Confirm\nCancel").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::LocatorBroken);
    assert!(result.reason.contains("Confirm"));
    let fix = result.fix.expect("定位失效要带修复");
    assert_eq!(fix.files[PAGE_NAME], page);
    assert_eq!(fix.files[TEST_NAME], TEST_POC);
    assert_eq!(fix.sha256.len(), 2);
    assert_eq!(fix.effort.as_deref(), Some("high"));
    let args = captured(&fake, "args", 1);
    assert!(args.contains(SCRIPTHEAL_SYSTEM), "必须用自愈的系统提示词");
    assert!(args.contains("--tools\n\n"), "自愈不给模型任何工具");
    let prompt = captured(&fake, "prompt", 1);
    assert!(prompt.contains("没找到 Add") && prompt.contains("Confirm\\nCancel"), "失败输出和控件文字要送进去");
    assert_eq!(count(&fake), 1);
}

#[tokio::test]
async fn 业务行为错误不产出文件_即使模型带了也丢弃() {
    let page = relocated_page();
    let fake = fake_events(&[reply("business_bug", "页面显示添加失败的提示：Song not added", Some((TEST_POC, &page)))]);
    let result = run(&fake, "AssertionError: 计数不对", "Song not added").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::BusinessBug);
    assert!(result.fix.is_none(), "业务行为错误时脚本是对的，不能产出修复");
    assert!(result.reason.contains("Song not added"));
    assert_eq!(count(&fake), 1);
}

#[tokio::test]
async fn 分不清就是分不清_不产出文件() {
    let fake = fake_events(&[reply("unsure", "elements 为空，无法判断", None)]);
    let result = run(&fake, "超时", "").await.unwrap();
    assert_eq!((result.verdict, result.fix.is_none()), (HealVerdict::Unsure, true));
}

#[tokio::test]
async fn 修复和原脚本一样等于没修_重试一轮仍一样就降为分不清() {
    let same = reply("locator_broken", "定位变了", Some((TEST_POC, PAGE_POC)));
    let fake = fake_events(&[same.clone(), same]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::Unsure);
    assert!(result.fix.is_none());
    assert!(result.reason.contains("没有实际修改") && result.reason.contains("原判断"), "{}", result.reason);
    assert_eq!(count(&fake), 2, "只重试一轮");
    assert!(captured(&fake, "prompt", 2).contains("没有通过校验"), "违规要回传给模型");
}

#[tokio::test]
async fn 修复删掉断言就拒绝_不能靠放宽断言让脚本通过() {
    let weaker: String = {
        let mut removed = false;
        TEST_POC
            .lines()
            .filter(|line| {
                let hit = !removed && line.trim_start().starts_with("assert ");
                removed |= hit;
                !hit
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(assert_count(&[(TEST_NAME.to_string(), weaker.clone())].into()) < assert_count(&[(TEST_NAME.to_string(), TEST_POC.to_string())].into()));
    let bad = reply("locator_broken", "定位变了", Some((&weaker, PAGE_POC)));
    let fake = fake_events(&[bad.clone(), bad]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!((result.verdict, result.fix.is_none()), (HealVerdict::Unsure, true));
    assert!(result.reason.contains("断言"), "{}", result.reason);
}

#[tokio::test]
async fn 定位失效但没给文件_或违规一轮后改正() {
    let page = relocated_page();
    // 第一轮没带文件（违规），第二轮改正
    let fake = fake_events(&[reply("locator_broken", "定位变了", None), reply("locator_broken", "定位变了", Some((TEST_POC, &page)))]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::LocatorBroken);
    assert!(result.fix.is_some());
    assert_eq!(count(&fake), 2);
    // 两轮都没带文件：降为分不清
    let fake = fake_events(&[reply("locator_broken", "定位变了", None), reply("locator_broken", "定位变了", None)]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!((result.verdict, result.fix.is_none()), (HealVerdict::Unsure, true));
}

#[tokio::test]
async fn 无法解析或结论不认识_降为分不清() {
    let fake = fake_events(&[success("我觉得是定位问题"), success("还是说不清")]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::Unsure);
    assert_eq!(count(&fake), 2);
    let fake = fake_events(&[reply("flaky", "偶发", None)]);
    let result = run(&fake, "失败", "x").await.unwrap();
    assert_eq!(result.verdict, HealVerdict::Unsure);
    assert_eq!(count(&fake), 1, "结论不在三种之内不重试");
}

#[tokio::test]
async fn 额度用尽直接报错_不降级为分不清() {
    let fake = fake_events(&[json!({"type": "result", "is_error": true, "result": "You've reached your weekly limit"}).to_string()]);
    let result = run(&fake, "失败", "x").await;
    assert!(matches!(result, Err(ScriptgenError::QuotaExhausted)), "{result:?}");
    assert_eq!(count(&fake), 1);
}

#[tokio::test]
async fn 凭证不会进入模型输入_模型回复里带敏感值也不采用() {
    let fake = fake_events(&[reply("business_bug", &format!("账号 {USER} 看到的提示不对"), None)]);
    let result = run(&fake, &format!("失败 {USER} {PASSWORD}"), &format!("欢迎 {USER}")).await.unwrap();
    assert_credentials_absent(&captured(&fake, "prompt", 1));
    assert_eq!(result.verdict, HealVerdict::Unsure, "原因含敏感值不能原样存进平台");
}

#[tokio::test]
async fn 来源执行不是整体通过时不调用模型() {
    let fake = fake_events(&[reply("unsure", "x", None)]);
    let inputs = Inputs::new();
    let mut input = inputs.input();
    input.judge_result = FinalResult::Failed;
    let files = original();
    let result = heal(&opts(&fake), input, &HealContext { files: &files, failure: "f", elements: "" }).await;
    assert!(matches!(result, Err(ScriptgenError::Input(_))), "{result:?}");
    assert_eq!(count(&fake), 0);
}

#[test]
fn 断言计数只数语句级assert() {
    let files: BTreeMap<String, String> = [("a.py".to_string(), "def f():\n    assert x\n    assert(y)\n    assert_ok(z)\n    # assert w\n    asserted = 1\n".to_string())].into();
    assert_eq!(assert_count(&files), 2);
}
