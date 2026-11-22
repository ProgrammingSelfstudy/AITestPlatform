//! 节点侧脚本生成的端到端测试：进程内起真实服务端和数据库，Claude 用假的（不花会员额度）。

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use agent::claude_cli::ClaudeOpts;
use node::scriptgen_run::{generate_and_upload, GenConfig, Outcome};
use protocol::{CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, ModelSource, RoleModel, Verdict};
use serde_json::{json, Value};
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::test_support::test_state;
use server::{app, AppState, Config};
use uuid::Uuid;

const TOKEN: &str = "scriptgen-run-token";
const TEST_NAME: &str = "test_tc04.py";
const PAGE_NAME: &str = "pages/tc04_pages.py";
const TEST_POC: &str = include_str!("../../agent/tests/fixtures/scriptgen/generated/test_tc04.py");
const PAGE_POC: &str = include_str!("../../agent/tests/fixtures/scriptgen/generated/pages/tc04_pages.py");
const TRACE: &str = include_str!("../../agent/tests/fixtures/scriptgen/trace.json");
const JUDGE_MODEL: &str = "角色指定的判定模型-甲";
const CAPS: &str = r#"{"node_version":"t","os":"macos","model_sources":[{"source":"claude_cli","vision":true},{"source":"codex_exec","vision":true}]}"#;
const BAD_UI: &str = "import pytest\n\n@pytest.mark.case(\"TC-04\")\ndef test_tc04(ui):\n    assert ui.unknown(\"Music\"), \"预期显示 Music\"\n";

struct World {
    st: AppState,
    addr: SocketAddr,
    node: Uuid,
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

async fn world() -> World {
    let st = test_state(Config { node_token: TOKEN.into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(node).bind(CAPS).execute(st.pool()).await.unwrap();
    sqlx::query("insert into devices (node_id, serial, platform, status) values ($1, 'DEV0', 'android', 'idle')").bind(node).execute(st.pool()).await.unwrap();
    let case = NewCase {
        code: "TC-04".into(),
        title: "滑动并添加歌曲".into(),
        level: String::new(),
        platforms: vec!["android".into()],
        preconditions: vec!["已登录、进房".into()],
        steps: vec!["点 More，选歌 {{song}}".into()],
        expected: vec![CaseExpected { idx: 1, text: "显示曲目 {{song}}".into() }],
        data: json!({"song": "测试曲目"}),
        refs: vec![],
        note: String::new(),
        should_fail: false,
    };
    import(st.pool(), "p", &[case], &Default::default()).await.unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(st.clone());
    tokio::spawn(async move {
        let _ = axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router).await;
    });
    let dir = std::env::temp_dir().join(format!("node_scriptgen_{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    World { st, addr, node, dir }
}

fn role(source: ModelSource, model: Option<&str>) -> RoleModel {
    RoleModel { source, model: model.map(String::from), effort: Some("high".into()) }
}

/// 把 TC-04 走完一次：Judge 角色和判定结果可指定。返回 run 编号。
async fn finished_run(w: &World, judge: RoleModel, final_result: FinalResult, verdict: Verdict) -> Uuid {
    runs::create_task(
        w.st.pool(),
        &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["TC-04".into()], executor: role(ModelSource::CodexExec, None), judge },
    )
    .await
    .unwrap();
    let claimed = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    runs::mark_started(w.st.pool(), w.node, claimed.run_id).await.unwrap();
    runs::executor_done(w.st.pool(), w.node, &ExecutorDone { run_id: claimed.run_id, result: Verdict::Passed, reason: "过了".into(), cost: None }).await.unwrap();
    let items = vec![JudgeItem { idx: 1, verdict, evidence: "页面显示 Music".into(), file_refs: vec!["shots/01_after.png".into(), "step:1".into()] }];
    runs::judge_done(w.st.pool(), w.node, &JudgeDone { run_id: claimed.run_id, judge_status: JudgeStatus::Completed, final_result, items }).await.unwrap();
    claimed.run_id
}

async fn passed_run(w: &World) -> Uuid {
    finished_run(w, role(ModelSource::ClaudeCli, Some(JUDGE_MODEL)), FinalResult::Passed, Verdict::Passed).await
}

/// 把夹具轨迹放到本节点的 runs 目录里，就像这次执行真的是在本节点跑的。
fn put_trace(w: &World, run: Uuid) {
    let dir = w.dir.join("runs").join(run.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("trace.json"), TRACE).unwrap();
}

/// 假的 Claude 命令行：记录收到的提示词、参数和调用次数，然后原样输出给定的事件。
struct FakeClaude {
    script: PathBuf,
    opts: ClaudeOpts,
}

impl FakeClaude {
    fn new(w: &World, events: &[String]) -> Self {
        let script = w.dir.join(format!("claude_{}.sh", Uuid::new_v4()));
        let body = format!(
            "set -eu\ncat > \"$0.prompt\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\necho x >> \"$0.calls\"\ncat <<'SCRIPTGEN_EVENTS'\n{}\nSCRIPTGEN_EVENTS\n",
            events.join("\n")
        );
        fs::write(&script, body).unwrap();
        let opts = ClaudeOpts {
            bin: "/bin/sh".into(),
            pre_args: vec![script.to_string_lossy().into()],
            cwd: Some(w.dir.clone()),
            stall_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        Self { script, opts }
    }

    fn calls(&self) -> usize {
        fs::read_to_string(format!("{}.calls", self.script.display())).map(|t| t.lines().count()).unwrap_or(0)
    }

    fn args(&self) -> String {
        fs::read_to_string(format!("{}.args", self.script.display())).unwrap_or_default()
    }
}

fn success(files: Value) -> String {
    json!({"type": "result", "is_error": false, "result": json!({"files": files, "notes": "采用页面文案"}).to_string()}).to_string()
}

fn good_events() -> Vec<String> {
    vec![success(json!({ TEST_NAME: TEST_POC, PAGE_NAME: PAGE_POC }))]
}

fn config(w: &World, claude: &FakeClaude) -> GenConfig {
    GenConfig {
        platform_http: format!("http://{}/", w.addr).parse().unwrap(),
        node_token: TOKEN.into(),
        node_id: w.node,
        runs_dir: w.dir.join("runs"),
        claude: claude.opts.clone(),
        ui_map: Value::Null,
        fix_from: None,
    }
}

async fn script_count(w: &World) -> i64 {
    sqlx::query_scalar("select count(*) from scripts").fetch_one(w.st.pool()).await.unwrap()
}

fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

// ---------------------------------------------------------------- 正常链路

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 通过的执行生成候选并归档_角色来自任务的judge配置() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &good_events());

    let outcome = generate_and_upload(&config(&w, &claude), run).await.unwrap();
    let Outcome::Created { id, version } = outcome else { panic!("应该是新建，实际是 {outcome:?}") };
    assert_eq!(version, 1);
    assert_eq!(claude.calls(), 1);
    assert!(claude.args().contains(JUDGE_MODEL), "模型必须来自任务里 Judge 的角色配置，不是写死的：{}", claude.args());

    let row = sqlx::query("select status, source_run_id, gen_source, gen_model, files from scripts where id = $1").bind(id).fetch_one(w.st.pool()).await.unwrap();
    use sqlx::Row;
    assert_eq!(row.get::<String, _>("status"), "candidate", "归档后只是候选，不能直接获准");
    assert_eq!(row.get::<Uuid, _>("source_run_id"), run);
    assert_eq!(row.get::<String, _>("gen_source"), "claude_cli");
    let files: Value = row.get("files");
    assert_eq!(files[TEST_NAME], TEST_POC, "归档的源码必须和生成的一致");
    assert!(files.get(PAGE_NAME).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 同样内容再生成一次指回已有版本() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &good_events());
    let first = generate_and_upload(&config(&w, &claude), run).await.unwrap();
    let second = generate_and_upload(&config(&w, &claude), run).await.unwrap();
    let (Outcome::Created { id: a, .. }, Outcome::Existing { id: b, version }) = (first, second) else { panic!("第二次应该指回已有版本") };
    assert_eq!((a, version), (b, 1));
    assert_eq!(script_count(&w).await, 1, "不能重复归档");
}

// ---------------------------------------------------------------- 不花额度的拒绝

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 最终结果不是通过_不送模型不花额度() {
    let w = world().await;
    let run = finished_run(&w, role(ModelSource::ClaudeCli, Some(JUDGE_MODEL)), FinalResult::Failed, Verdict::Failed).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &good_events());
    let text = message(generate_and_upload(&config(&w, &claude), run).await.unwrap_err());
    assert!(text.contains("不是通过"), "{text}");
    assert_eq!(claude.calls(), 0, "资格不合格必须在调用模型之前就拒绝，不能花额度");
    assert_eq!(script_count(&w).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn judge角色不是claude_cli_明确报错不换来源() {
    let w = world().await;
    let run = finished_run(&w, role(ModelSource::CodexExec, None), FinalResult::Passed, Verdict::Passed).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &good_events());
    let text = message(generate_and_upload(&config(&w, &claude), run).await.unwrap_err());
    assert!(text.contains("claude_cli") && text.contains("不会改用其他来源"), "{text}");
    assert_eq!(claude.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 本节点没有轨迹_只能在执行它的节点上生成() {
    let w = world().await;
    let run = passed_run(&w).await; // 故意不放轨迹
    let claude = FakeClaude::new(&w, &good_events());
    let text = message(generate_and_upload(&config(&w, &claude), run).await.unwrap_err());
    assert!(text.contains("本节点没有这次执行的轨迹"), "{text}");
    assert_eq!(claude.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 别的节点的执行不能在这里生成() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    let claude = FakeClaude::new(&w, &good_events());
    let mut cfg = config(&w, &claude);
    cfg.node_id = other;
    let text = message(generate_and_upload(&cfg, run).await.unwrap_err());
    assert!(text.contains("不属于本节点"), "{text}");
    assert_eq!(claude.calls(), 0, "别人的执行不能用本节点的额度去生成");
    cfg.node_id = w.node;
    cfg.node_token = "wrong-token".into();
    assert!(message(generate_and_upload(&cfg, run).await.unwrap_err()).contains("令牌"));
}

// ---------------------------------------------------------------- 生成失败的处理

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 额度用完立即停止_如实告知且不换计费方式() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &[json!({"type": "result", "is_error": true, "result": "You've reached your weekly limit"}).to_string()]);
    let text = message(generate_and_upload(&config(&w, &claude), run).await.unwrap_err());
    assert!(text.contains("额度已用完") && text.contains("不会改用其他计费方式"), "{text}");
    assert_eq!(claude.calls(), 1, "额度用完不能重试");
    assert_eq!(script_count(&w).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 生成的脚本没过静态校验_不上传() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &[success(json!({ TEST_NAME: BAD_UI }))]);
    let text = message(generate_and_upload(&config(&w, &claude), run).await.unwrap_err());
    assert!(text.contains("没有通过静态校验") && text.contains("规则"), "要指出违反了哪条规则：{text}");
    assert_eq!(script_count(&w).await, 0, "不合规的脚本不能进库");
}

// ---------------------------------------------------------------- 按自检失败修正

/// 先归档一条候选，再按需改成指定状态。返回脚本编号。
async fn archived_as(w: &World, run: Uuid, status: Option<(server::scripts::ScriptStatus, &str)>) -> Uuid {
    use server::scripts::{create_candidate, file_digests, transition, NewScript};
    let case_id: Uuid = sqlx::query_scalar("select case_id from runs where id = $1").bind(run).fetch_one(w.st.pool()).await.unwrap();
    let files = [(TEST_NAME.to_string(), TEST_POC.to_string()), (PAGE_NAME.to_string(), PAGE_POC.to_string())].into();
    let new = NewScript { case_id, source_run_id: run, file_sha256: file_digests(&files), files, notes: String::new(), gen_source: "claude_cli".into(), gen_model: None, gen_effort: None };
    let id = create_candidate(w.st.pool(), &new).await.unwrap().id;
    if let Some((to, note)) = status {
        transition(w.st.pool(), id, to, Some(note)).await.unwrap();
    }
    id
}

fn prompt_of(claude: &FakeClaude) -> String {
    fs::read_to_string(format!("{}.prompt", claude.script.display())).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 修正模式把上一版源码和原始失败输出交给模型() {
    use server::scripts::ScriptStatus;
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let old = archived_as(&w, run, Some((ScriptStatus::Rejected, "AssertionError: 目标歌曲没找到-独特标记"))).await;
    let claude = FakeClaude::new(&w, &[success(json!({ TEST_NAME: format!("{TEST_POC}\n# 已修正\n"), PAGE_NAME: PAGE_POC }))]);
    let mut cfg = config(&w, &claude);
    cfg.fix_from = Some(old);

    let Outcome::Created { version, .. } = generate_and_upload(&cfg, run).await.unwrap() else { panic!("应该新建版本") };
    assert_eq!(version, 2, "修正后是新版本，不覆盖旧版本");
    let prompt = prompt_of(&claude);
    assert!(prompt.contains("previous_attempt") && prompt.contains("目标歌曲没找到-独特标记"), "失败输出必须原样交给模型");
    assert!(prompt.contains("MusicLibraryPage") || prompt.contains("def test_"), "上一版源码也要交给模型");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 没有修正模式时提示词不带上一版() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let claude = FakeClaude::new(&w, &good_events());
    generate_and_upload(&config(&w, &claude), run).await.unwrap();
    assert!(!prompt_of(&claude).contains("previous_attempt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 要修正的脚本不是已拒绝_或没有失败输出_不调用模型() {
    let w = world().await;
    let run = passed_run(&w).await;
    put_trace(&w, run);
    let candidate = archived_as(&w, run, None).await;
    let claude = FakeClaude::new(&w, &good_events());
    let mut cfg = config(&w, &claude);
    cfg.fix_from = Some(candidate);
    let error = message(generate_and_upload(&cfg, run).await.unwrap_err());
    assert!(error.contains("candidate") && error.contains("只有被自检拒绝"), "{error}");

    sqlx::query("update scripts set status = 'rejected', status_note = '  ' where id = $1").bind(candidate).execute(w.st.pool()).await.unwrap();
    let error = message(generate_and_upload(&cfg, run).await.unwrap_err());
    assert!(error.contains("没有记录失败输出"), "{error}");
    assert_eq!(claude.calls(), 0, "条件不满足不能花额度");

    cfg.fix_from = Some(Uuid::new_v4());
    assert!(message(generate_and_upload(&cfg, run).await.unwrap_err()).contains("没有要修正的脚本"));
    assert_eq!(claude.calls(), 0);
}
