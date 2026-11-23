//! 对**真实 `agy`** 的集成测试：默认忽略，需要本机 agy 个人账号登录，会花一点 Gemini 额度。
//! 运行：`cargo build -p mcp-device && cargo test -p agent --test agy_real -- --ignored --nocapture`
//!
//! 用合成证据（四个象限分别是红蓝黄绿的 PNG，加一个猜不到的暗号），所以不涉及任何真实截图或账号。
//! 它验证的是假脚本测不到的东西：真实的事件格式、真实的权限规则、真实的登录态链接、真实的证据 MCP 能连上。
//! `agy` 升级后行为变了，先跑这个。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use agent::agy_cli::{AgyOpts, McpServer, check_login, run};
use serde_json::Value;

const SHOT_1: &[u8] = include_bytes!("fixtures/agy/shots/1_after.png");
const SHOT_2: &[u8] = include_bytes!("fixtures/agy/shots/2_after.png");
const TRACE: &str = include_str!("fixtures/agy/trace.json");

const PROMPT: &str = r#"你是测试结果的独立判定员。证据只能通过名为 evidence 的 MCP 工具获取：image(name) 查看截图，step_text(step) 查看某一步的页面控件文字。

可用截图：1_after.png（步骤 1 操作后）、2_after.png（步骤 2 操作后）。步骤文字可用：1、2。

请判断两条预期：
1. 步骤 1 操作后的截图里，左上角是红色。
2. 步骤 1 操作后，页面控件文字里出现了一个形如「动物名-数字」的暗号，请原样写出这个暗号。

必须真正调用工具取得证据，不要凭空猜测，也不要读取文件系统。最后只输出一个 JSON 对象：
{"items":[{"index":1,"verdict":"passed|failed|uncertain","screenshots":["实际查看的截图或 step:N"],"evidence":"画面或文字里的事实"},{"index":2,"verdict":"passed|failed|uncertain","screenshots":["..."],"evidence":"..."}],"quadrants":"你在 1_after.png 里看到的四个象限颜色（左上、右上、左下、右下）"}"#;

fn evidence_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mcp-evidence");
    assert!(path.exists(), "先运行 `cargo build -p mcp-device` 生成证据服务：{path:?}");
    path.canonicalize().unwrap()
}

#[tokio::test]
#[ignore = "需要本机 agy 个人账号登录，会花一点 Gemini 额度"]
async fn 真实_agy_登录检查和经证据工具评审() {
    let model = std::env::var("AGY_TEST_MODEL").unwrap_or_else(|_| "gemini-4-neon-high".into());
    let mut opts = AgyOpts { model: Some(model), total_timeout: Duration::from_secs(300), ..AgyOpts::default() };
    check_login(&opts).await.expect("本机需要 agy 个人账号登录");

    let run_dir = std::env::temp_dir().join(format!("hafa-agyreal-{}", std::process::id()));
    fs::create_dir_all(run_dir.join("shots")).unwrap();
    fs::write(run_dir.join("shots/1_after.png"), SHOT_1).unwrap();
    fs::write(run_dir.join("shots/2_after.png"), SHOT_2).unwrap();
    fs::write(run_dir.join("trace.json"), TRACE).unwrap();
    opts.mcp = vec![McpServer { name: "evidence".into(), command: evidence_binary(), env: BTreeMap::from([("HAFA_RUN_DIR".to_string(), run_dir.to_string_lossy().into_owned())]) }];
    opts.allowed_tools = vec![("evidence".into(), "image".into()), ("evidence".into(), "step_text".into())];

    let outcome = run(&opts, PROMPT).await;
    println!("耗时 {:.1}s，工具调用 {:?}，用量 {:?}", outcome.wall_s, outcome.tool_calls, outcome.usage);
    let served = fs::read_to_string(run_dir.join("served.json")).unwrap_or_default();
    let _ = fs::remove_dir_all(&run_dir);

    assert!(outcome.ok(), "{outcome:?}");
    assert!(outcome.violations.is_empty() && outcome.denied.is_empty());
    let answer: Value = serde_json::from_str(&outcome.text).unwrap_or_else(|_| panic!("没有返回 JSON：{}", outcome.text));
    assert!(answer["quadrants"].as_str().unwrap().contains("红"), "它确实看到了图：{answer}");
    assert!(answer["items"][1]["evidence"].as_str().unwrap().contains("ZEBRA-4821"), "它确实读到了步骤文字：{answer}");
    // 证据服务自己的记录：它实际取过哪些证据，和它引用的一致
    assert!(served.contains("1_after.png") && served.contains("step:1"), "served.json：{served}");
    assert!(!served.contains("2_after.png"), "没取过的证据不能出现在记录里：{served}");
}
