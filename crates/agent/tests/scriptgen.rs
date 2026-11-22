use std::fs;
use std::process::Command;

use agent::prompts::SCRIPTGEN_SYSTEM;
use agent::scriptcheck::UiContract;
use agent::scriptgen::{
    ScriptTrace, ScriptgenError, ScriptgenInput, ScriptgenOpts, build_prompt, generate,
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
const BAD_UI: &str = r#"import pytest

@pytest.mark.case("TC-04")
def test_tc04(ui):
    assert ui.unknown("Music"), "预期显示 Music"
"#;
const TOO_FEW: &str = r#"import pytest

@pytest.mark.case("TC-04")
def test_tc04(ui):
    ui.has("Music")
"#;

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

fn payload(test: &str) -> String {
    json!({"files": { TEST_NAME: test, PAGE_NAME: PAGE_POC }, "notes": "采用页面文案"}).to_string()
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

#[tokio::test]
async fn 正常生成保留原文件哈希角色模型与强度() {
    let init = json!({"type": "system", "subtype": "init", "model": "假命令行报告的实际模型"});
    let fake = fake_events(&[format!("{init}\n{}", success(&payload(TEST_POC)))]);
    let output = generate(&opts(&fake), Inputs::new().input()).await.unwrap();
    assert_eq!(output.files[TEST_NAME], TEST_POC);
    assert_eq!(output.files[PAGE_NAME], PAGE_POC);
    assert_eq!(
        output.sha256[TEST_NAME],
        "d747b25c60cdb0d1423ad61ebc7d80b67a532cb38167d9ec11a3b7ea921984cd"
    );
    assert_eq!(
        output.sha256[PAGE_NAME],
        "187e6a32f06344fbf925a490d06c10a873cb46d7b4bd1fe853b0ed6b122c36c8"
    );
    assert_eq!(output.notes, "采用页面文案");
    assert_eq!(output.model.as_deref(), Some("假命令行报告的实际模型"));
    assert_eq!(output.effort.as_deref(), Some("high"));
    let args = captured(&fake, "args", 1);
    for arg in [
        "--model\n由角色指定的假模型\n",
        "--effort\nhigh\n",
        "--tools\n\n",
        "--setting-sources\n\n",
        "--mcp-config\n{\"mcpServers\":{}}\n--strict-mcp-config",
        "--no-session-persistence",
        SCRIPTGEN_SYSTEM,
    ] {
        assert!(args.contains(arg), "缺少参数 {arg}");
    }
    assert_eq!(count(&fake), 1);
    assert_eq!(
        fs::read_dir(&fake.run_dir).unwrap().count(),
        1,
        "只保留原有 shots 目录"
    );
}

#[tokio::test]
async fn 含额外花括号的包装噪声可抢救完整_json() {
    let fence = "\u{60}\u{60}\u{60}";
    let wrapped = format!(
        "回复 {{仅说明}}\n{fence}json\n{}\n{fence}\n收尾 {{}}",
        payload(TEST_POC)
    );
    let fake = fake_events(&[success(&wrapped)]);
    let output = generate(&opts(&fake), Inputs::new().input()).await.unwrap();
    assert_eq!(output.files[TEST_NAME], TEST_POC);
    assert_eq!(output.model.as_deref(), Some("由角色指定的假模型"));
    assert_eq!(count(&fake), 1);
}

async fn rejects_output(text: &str, rule: u8, reason: &str) {
    let fake = fake_events(&[success(text), success(text)]);
    let result = generate(&opts(&fake), Inputs::new().input()).await;
    assert!(
        matches!(&result, Err(ScriptgenError::Violations(issues)) if issues.iter()
        .any(|issue| issue.rule == rule && issue.message.contains(reason))),
        "{result:?}"
    );
    assert_eq!(count(&fake), 2);
    assert_eq!(fs::read_dir(&fake.run_dir).unwrap().count(), 1);
}

#[tokio::test]
async fn 拒绝非法文件名大小写和其他用例的文件() {
    for file in [
        "pages/../other.py",
        "pages/TC04_pages.py",
        "pages/tc06_pages.py",
        "/tmp/test_tc04.py",
    ] {
        rejects_output(
            &json!({"files": {file: PAGE_POC, TEST_NAME: TEST_POC}, "notes": ""}).to_string(),
            0,
            "文件名",
        )
        .await;
    }
}

#[tokio::test]
async fn 拒绝超过六个文件并保留重复文件名用于校验() {
    let files: serde_json::Map<_, _> = (0..7)
        .map(|n| (format!("test_tc{n}.py"), json!("pass")))
        .collect();
    rejects_output(
        &json!({"files": files, "notes": ""}).to_string(),
        0,
        "数量超过 6",
    )
    .await;
    let duplicate = format!(
        r#"{{"files":{{"{TEST_NAME}":{},"{PAGE_NAME}":{},"{TEST_NAME}":"pass"}},"notes":""}}"#,
        json!(TEST_POC),
        json!(PAGE_POC)
    );
    rejects_output(&duplicate, 0, "文件名重复").await;
}

#[tokio::test]
async fn 文件按字节计算六十四_kb_上限() {
    let ok = format!("{PAGE_POC}#{}\n", " ".repeat(65536 - PAGE_POC.len() - 2));
    let fake = fake_events(&[success(
        &json!({"files":{PAGE_NAME: ok, TEST_NAME: TEST_POC}, "notes":""}).to_string(),
    )]);
    assert!(generate(&opts(&fake), Inputs::new().input()).await.is_ok());
    let large = format!("#{}\n", "字".repeat(22000));
    rejects_output(
        &json!({"files": {PAGE_NAME: large, TEST_NAME: TEST_POC}, "notes": ""}).to_string(),
        0,
        "文件超过 64KB",
    )
    .await;
}

#[tokio::test]
async fn 拒绝损坏的重复的和多余字段的_json() {
    let ambiguous = format!("候选一 {}\n候选二 {}", payload(TEST_POC), payload(TEST_POC));
    for text in [
        "{ \"files\":".into(),
        ambiguous,
        payload(TEST_POC).replacen("\"notes\":", "\"extra\":0,\"notes\":", 1),
    ] {
        rejects_output(&text, 0, "唯一且完整").await;
    }
}

#[tokio::test]
async fn 额度用尽映射专用错误且只调用一次() {
    let events = [
        json!({"type": "result", "is_error": true, "result": "You've reached your weekly limit"})
            .to_string(),
        json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected"}}).to_string(),
    ];
    for event in events {
        let fake = fake_events(&[event, success(&payload(TEST_POC))]);
        let result = generate(&opts(&fake), Inputs::new().input()).await;
        assert!(
            matches!(result, Err(ScriptgenError::QuotaExhausted)),
            "{result:?}"
        );
        assert_eq!(count(&fake), 1);
    }
}

#[tokio::test]
async fn 会员登录失效也不会重试或换来源() {
    let auth = json!({"type": "result", "is_error": true, "result": "not logged in"}).to_string();
    let fake = fake_events(&[auth, success(&payload(TEST_POC))]);
    let result = generate(&opts(&fake), Inputs::new().input()).await;
    assert!(
        matches!(result, Err(ScriptgenError::Claude("本机会员登录失效"))),
        "{result:?}"
    );
    assert_eq!(count(&fake), 1);
}

#[tokio::test]
async fn 逐条未通过或来源不对时不调用模型() {
    let fake = fake_events(&[success(&payload(TEST_POC))]);
    let mut input = Inputs::new();
    input.items[0].verdict = Verdict::Uncertain;
    assert!(matches!(
        generate(&opts(&fake), input.input()).await,
        Err(ScriptgenError::Input(_))
    ));
    let mut opts = opts(&fake);
    opts.role.source = ModelSource::CodexExec;
    assert!(matches!(
        generate(&opts, Inputs::new().input()).await,
        Err(ScriptgenError::Input(_))
    ));
    assert_eq!(count(&fake), 0);
}

#[tokio::test]
async fn 静态违规随提示词回传一轮后通过() {
    let fake = fake_events(&[success(&payload(BAD_UI)), success(&payload(TEST_POC))]);
    let output = generate(&opts(&fake), Inputs::new().input()).await.unwrap();
    assert_eq!(count(&fake), 2);
    assert_eq!(output.files[TEST_NAME], TEST_POC);
    let repair = captured(&fake, "prompt", 2);
    for text in [
        "上一轮的静态违规",
        TEST_NAME,
        "\"line\": 5",
        "\"rule\": 4",
        "Ui 方法不在契约中",
    ] {
        assert!(repair.contains(text), "修正说明缺少 {text}");
    }
}

#[tokio::test]
async fn 两轮失败仅返回最后一轮的结构化违规() {
    let fake = fake_events(&[
        success(&payload(BAD_UI)),
        success(&payload(TOO_FEW)),
        success(&payload(TEST_POC)),
    ]);
    let result = generate(&opts(&fake), Inputs::new().input()).await;
    assert!(
        matches!(&result, Err(ScriptgenError::Violations(issues)) if
        matches!(issues.as_slice(), [issue] if issue.file == TEST_NAME && issue.line == 4 && issue.rule == 8)),
        "{result:?}"
    );
    assert_eq!(count(&fake), 2);
    assert_eq!(fs::read_dir(&fake.run_dir).unwrap().count(), 1);
}

#[test]
fn 提示词包含契约轨迹和所有生成与静态规则() {
    let input = Inputs::new();
    let prompt = build_prompt(&input.input()).unwrap();
    for method in input.contract.methods.keys() {
        assert!(
            prompt.contains(&format!("\"{method}\"")),
            "缺少契约方法 {method}"
        );
    }
    for data in [
        "More",
        "295",
        "1527",
        "01_after.png",
        "step:1",
        "new_labels",
        "显示曲目 测试曲目",
        "More 面板",
        "generated.pages.tc04_pages",
        "{{account.username}}",
        "{{account.password}}",
    ] {
        assert!(prompt.contains(data), "缺少模型输入 {data}");
    }
    assert_credentials_absent(&prompt);
    assert!(!prompt.contains("不应送入模型的字段"));
    for rule in [
        r#"{"files":{"pages/<case>_pages.py":"...","test_<case>.py":"..."},"notes":"..."}"#,
        "可以解析的 Python 源码",
        "位置及关键字参数",
        "content-desc",
        "坐标兜底",
        "说明原因",
        "预期原文",
        "@pytest.mark.case",
        "显式 assert",
        "账号、密码、邮箱、token、adb、设备序列号",
        "登录、进房",
        "由 setup 保证",
        "循环必须有上限",
        "while True",
        "break",
        "3 秒",
        "wait_text",
        "白名单",
        "exec、eval、compile、open、__import__、subprocess、os、sys、socket、requests、urllib、shutil、pathlib",
    ] {
        assert!(SCRIPTGEN_SYSTEM.contains(rule), "系统提示词缺少 {rule}");
    }
}

#[test]
fn 脚本生成提示词明确子集边界和安全约束() {
    for rule in [
        "try 必须有 except 具体异常名，可用 as 绑定异常，再接可选的 else 和 finally",
        "禁止裸 except: 和 except BaseException",
        "except Exception 也须满足断言约束",
        "未含断言的 try 可以捕获等待超时并返回 False",
        "assert 或调用以 assert_ 开头的函数",
        "每个 except 路径都必须执行 raise（re-raise）或 pytest.fail(...)",
        "try 体内的断言不计入预期条数",
        "finally 不得吞掉断言",
        "for 用 range、有限字面量或可识别的页面快照",
        "while 需要可静态检查的计数和上限，while True 还须在达到上限时 break",
        "推导式必须遍历有限源",
        "range 参数使用整数常量或整数常量表达式",
        "enumerate、sorted、reversed、list、tuple、set、zip、map、filter 只能包装有限源",
        "禁止 itertools.count、itertools.cycle、无次数上限的 itertools.repeat",
        "两参数 iter(x, sentinel)、while 驱动的生成器以及对 ui 的无上限轮询",
        "任何绝对路径（包括 /sdcard/... 等设备文件路径）",
        "不得直接调用 adb",
        "测试音频由 setup 准备",
        "固定等待 sleep 和 ui.wait 只用 0 到 3 秒的数值常量",
    ] {
        assert!(SCRIPTGEN_SYSTEM.contains(rule), "系统提示词缺少 {rule}");
    }
}

#[tokio::test]
async fn 原始轨迹凭证和模型错误产物都不会进入任一轮输入() {
    let mut bad: Value = serde_json::from_str(&payload(BAD_UI)).unwrap();
    bad["notes"] = json!("model-leak@example.test");
    let fake = fake_events(&[success(&bad.to_string()), success(&payload(TEST_POC))]);
    let output = generate(&opts(&fake), Inputs::new().input()).await.unwrap();
    assert_eq!(output.notes, "采用页面文案");
    for turn in 1..=2 {
        let prompt = captured(&fake, "prompt", turn);
        assert_credentials_absent(&prompt);
        assert!(!prompt.contains("model-leak@example.test"));
        assert_credentials_absent(&captured(&fake, "args", turn));
    }
    assert!(captured(&fake, "prompt", 2).contains("生成备注含敏感值"));
}

#[test]
fn 开发入口注入契约且剔除继承的凭证环境变量() {
    let fake = FakeClaude::new(&format!(
        r#"
for key in ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL OPENAI_API_KEY CLAUDE_CODE_USE_BEDROCK NODE_TOKEN HAFA_ACCOUNTS_KEY; do
    if printenv "$key" >/dev/null; then exit 81; fi
done
[ -n "$HOME" ] && [ -n "$PATH" ]
cat <<'SCRIPTGEN_EVENTS'
{}
SCRIPTGEN_EVENTS"#,
        success(&payload(&TEST_POC.replacen(
            "    room = RoomPage(ui)",
            "    ui.fixture_only()\n    room = RoomPage(ui)",
            1
        )))
    ));
    let input = Inputs::new();
    let case = fake.dir.join("case.json");
    let contract = fake.dir.join("contract.json");
    let info = fake.dir.join("app_info.yaml");
    fs::write(&case, serde_json::to_vec(&input.case).unwrap()).unwrap();
    fs::write(
        fake.run_dir.join("trace.json"),
        r#"{"steps":[{"idx":1,"action":"tap","target":"More"}]}"#,
    )
    .unwrap();
    fs::write(fake.run_dir.join("judge.json"), json!({
        "final_result":"passed","items":[{"idx":1,"verdict":"passed","evidence":"显示 Music","file_refs":["step:1"]}]
    }).to_string()).unwrap();
    let mut injected: Value = serde_json::from_str(CONTRACT).unwrap();
    injected["methods"]["fixture_only"] = json!({"parameters":[]});
    fs::write(&contract, injected.to_string()).unwrap();
    fs::write(&info, "ui_map:\n  More: 测试知识\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scriptgen_dev"))
        .arg(&fake.run_dir)
        .arg(&case)
        .arg("--fake-claude")
        .arg(fake.dir.join("claude.sh"))
        .arg("--contract")
        .arg(&contract)
        .arg("--app-info")
        .arg(&info)
        .envs(
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "OPENAI_API_KEY",
                "CLAUDE_CODE_USE_BEDROCK",
                "NODE_TOKEN",
                "HAFA_ACCOUNTS_KEY",
            ]
            .map(|key| (key, "模拟环境值")),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let scripts: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        scripts["files"][TEST_NAME]
            .as_str()
            .unwrap()
            .contains("ui.fixture_only()")
    );
    let prompt = fs::read_to_string(fake.dir.join("claude.sh.prompt")).unwrap();
    assert!(prompt.contains("测试知识"));
    assert_eq!(fs::read_dir(&fake.run_dir).unwrap().count(), 3);
}
