//! 自愈分诊的端到端测试：进程内起真实服务端和数据库，Python 用假的可执行脚本（不碰真机、不花额度）。
//! 假 python 按测试文件里的标记模拟 pytest 的各种结局。

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use node::heal_run::{heal_script, HealConfig};
use node::regress_run::regress;
use agent::claude_cli::ClaudeOpts;
use agent::scriptgen::HealVerdict;
use node::selfcheck_run::CheckConfig;
use protocol::{CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, ModelSource, RoleModel, Verdict};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::scripts::{create_candidate, file_digests, NewScript};
use server::test_support::test_state;
use server::{app, AppState, Config};
use uuid::Uuid;

const TOKEN: &str = "heal-run-token";
const JUDGE_MODEL: &str = "角色指定的判定模型";
const CAPS: &str = r#"{"node_version":"t","os":"macos","model_sources":[{"source":"claude_cli","vision":true},{"source":"codex_exec","vision":true}]}"#;

/// 假 python：先确认 generated 包结构，再按测试文件里的标记模拟 pytest。
const FAKE_PYTHON: &str = r#"#!/bin/sh
f=generated/test_tc04.py
[ -f generated/__init__.py ] && [ -f generated/pages/__init__.py ] && [ -f "$f" ] || { echo "包结构不对"; exit 9; }
n=$(cat "$0.n" 2>/dev/null || echo 0); echo $((n+1)) > "$0.n"
fail() {
  mkdir -p "$HAFA_ARTIFACT_DIR"
  echo "Confirm 按钮 (content-desc)" > "$HAFA_ARTIFACT_DIR/TC-04-abc123-call-elements.txt"
  echo "AssertionError: 目标歌曲没找到"; echo "1 failed in 0.5s"; exit 1
}
case "$(cat "$f")" in
  *ENVLATER*) [ "$n" = 0 ] && fail; echo "准备前置状态失败（退出码 1）：手机没连上"; exit 1;;
  *FLAKY*) [ "$n" = 0 ] && fail; echo "1 passed in 0.5s"; exit 0;;
  *FAIL*) fail;;
  *) echo "1 passed in 0.5s"; exit 0;;
esac
"#;

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

const REVIEW_TOKEN: &str = "regress-review-token-0123456789";

async fn world() -> World {
    // 页面发起的写接口要审核口令（和创建任务同一套）；口令从环境变量读，所有测试用同一个值，只设一次
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("PLATFORM_REVIEW_TOKEN", REVIEW_TOKEN) });
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
    let dir = std::env::temp_dir().join(format!("node_regress_{}", Uuid::new_v4()));
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


fn fake_python(w: &World) -> PathBuf {
    let path = w.dir.join("fake_python.sh");
    fs::write(&path, FAKE_PYTHON).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(w: &World, python: &PathBuf, timeout: Duration) -> CheckConfig {
    CheckConfig {
        platform_http: format!("http://{}/", w.addr).parse().unwrap(),
        node_token: TOKEN.into(),
        node_id: w.node,
        python: python.to_string_lossy().into(),
        automation_dir: w.dir.clone(),
        audio_dir: w.dir.clone(),
        state_dir: w.dir.clone(),
        package: "com.example".into(),
        activity: "com.example.Main".into(),
        serial: "DEV0".into(),
        bottom_shift: 0,
        timeout,
        work_dir: w.dir.join("work"),
    }
}

/// 归档一条候选，测试文件里放标记，返回脚本编号。
async fn candidate(w: &World, marker: &str) -> Uuid {
    let run = passed_run(w).await;
    let case_id: Uuid = sqlx::query_scalar("select case_id from runs where id = $1").bind(run).fetch_one(w.st.pool()).await.unwrap();
    let files = [("test_tc04.py".to_string(), format!("# {marker}\n")), ("pages/tc04_pages.py".to_string(), format!("# {marker}\n"))].into();
    let new = NewScript {
        case_id,
        source_run_id: run,
        file_sha256: file_digests(&files),
        files,
        notes: String::new(),
        gen_source: "claude_cli".into(),
        gen_model: None,
        gen_effort: None,
    };
    create_candidate(w.st.pool(), &new).await.unwrap().id
}

fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}


/// 归档、自检通过并获准一条脚本（测试文件里放标记），返回编号。
async fn approved(w: &World, marker: &str) -> Uuid {
    use server::scripts::{transition, ScriptStatus};
    let id = candidate(w, marker).await;
    transition(w.st.pool(), id, ScriptStatus::SelfCheckPassed, Some("ok")).await.unwrap();
    transition(w.st.pool(), id, ScriptStatus::Approved, Some("ok")).await.unwrap();
    id
}

async fn regressions(w: &World) -> Vec<(Uuid, Uuid, String, bool, String)> {
    sqlx::query_as("select script_id, node_id, serial, passed, detail from script_regressions order by created_at").fetch_all(w.st.pool()).await.unwrap()
}


const TRACE: &str = include_str!("../../agent/tests/fixtures/scriptgen/trace.json");
const TEST_POC: &str = include_str!("../../agent/tests/fixtures/scriptgen/generated/test_tc04.py");
const PAGE_POC: &str = include_str!("../../agent/tests/fixtures/scriptgen/generated/pages/tc04_pages.py");

/// 把夹具轨迹放到本节点的 runs 目录里，就像这次执行真的是在本节点跑的。
fn put_trace(w: &World, run: Uuid) {
    let dir = w.dir.join("runs").join(run.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("trace.json"), TRACE).unwrap();
}

/// 假的 Claude 命令行：记录收到的提示词和调用次数，然后原样输出给定的事件。
struct FakeClaude {
    script: PathBuf,
    opts: ClaudeOpts,
}

impl FakeClaude {
    fn new(w: &World, events: &[String]) -> Self {
        let script = w.dir.join(format!("claude_{}.sh", Uuid::new_v4()));
        let body = format!("set -eu\ncat > \"$0.prompt\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\necho x >> \"$0.calls\"\ncat <<'SCRIPTGEN_EVENTS'\n{}\nSCRIPTGEN_EVENTS\n", events.join("\n"));
        fs::write(&script, body).unwrap();
        let opts = ClaudeOpts { bin: "/bin/sh".into(), pre_args: vec![script.to_string_lossy().into()], cwd: Some(w.dir.clone()), stall_timeout: Duration::from_secs(5), total_timeout: Duration::from_secs(10), ..Default::default() };
        Self { script, opts }
    }

    fn calls(&self) -> usize {
        fs::read_to_string(format!("{}.calls", self.script.display())).map(|t| t.lines().count()).unwrap_or(0)
    }

    fn prompt(&self) -> String {
        fs::read_to_string(format!("{}.prompt", self.script.display())).unwrap_or_default()
    }
}

fn reply(verdict: &str, reason: &str, files: Option<(&str, &str)>) -> String {
    let mut value = json!({"verdict": verdict, "reason": reason, "notes": "改了定位"});
    if let Some((test, page)) = files {
        value["files"] = json!({"test_tc04.py": test, "pages/tc04_pages.py": page});
    }
    json!({"type": "result", "is_error": false, "result": value.to_string()}).to_string()
}

fn heal_config(w: &World, python: &PathBuf, claude: &FakeClaude) -> HealConfig {
    HealConfig { check: config(w, python, Duration::from_secs(20)), runs_dir: w.dir.join("runs"), claude: claude.opts.clone(), ui_map: serde_json::Value::Null }
}

/// 获准一条脚本、放好本节点的轨迹，并回归一次（记下回归结果）。返回（脚本编号，来源 run）。
async fn regressed(w: &World, python: &PathBuf, marker: &str) -> Uuid {
    let id = approved(w, marker).await;
    let run: Uuid = sqlx::query_scalar("select source_run_id from scripts where id = $1").bind(id).fetch_one(w.st.pool()).await.unwrap();
    put_trace(w, run);
    regress(&config(w, python, Duration::from_secs(20)), "p", "TC-04").await.unwrap();
    id
}

async fn triages(w: &World) -> Vec<(Uuid, String, Option<Uuid>)> {
    sqlx::query_as("select script_id, verdict, healed_script_id from script_triages order by created_at").fetch_all(w.st.pool()).await.unwrap()
}

async fn script_state(w: &World, id: Uuid) -> String {
    sqlx::query_scalar("select status from scripts where id = $1").bind(id).fetch_one(w.st.pool()).await.unwrap()
}

async fn script_count(w: &World) -> i64 {
    sqlx::query_scalar("select count(*) from scripts").fetch_one(w.st.pool()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 定位失效_产出修复候选_原版本仍是已获准_记下来源() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let claude = FakeClaude::new(&w, &[reply("locator_broken", "elements 里按钮文案变成了 Confirm", Some((TEST_POC, PAGE_POC)))]);

    let report = heal_script(&heal_config(&w, &python, &claude), id).await.unwrap();
    assert_eq!(report.verdict, HealVerdict::LocatorBroken);
    let (healed, version) = report.healed.expect("要有修复候选");
    assert_eq!(version, 2);
    assert_eq!(script_state(&w, id).await, "approved", "原获准版本在人决定之前不动");
    let (status, from): (String, Option<Uuid>) = sqlx::query_as("select status, healed_from from scripts where id = $1").bind(healed).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((status.as_str(), from), ("candidate", Some(id)), "修复版是候选，要重新自检、人工获准");
    assert_eq!(triages(&w).await, vec![(id, "locator_broken".to_string(), Some(healed))]);
    let prompt = claude.prompt();
    assert!(prompt.contains("Confirm 按钮") && prompt.contains("目标歌曲没找到"), "新证据（控件文字和失败输出）要送给模型");
    assert_eq!(claude.calls(), 1);
    assert_eq!(regressions(&w).await.len(), 1, "自愈重跑不另记回归");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 业务行为错误_不改脚本_失败记录保留() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let claude = FakeClaude::new(&w, &[reply("business_bug", "页面提示添加失败，应用行为不符合预期", None)]);
    let report = heal_script(&heal_config(&w, &python, &claude), id).await.unwrap();
    assert_eq!((report.verdict, report.healed), (HealVerdict::BusinessBug, None));
    assert_eq!(script_count(&w).await, 1, "不能新增脚本版本");
    assert_eq!(script_state(&w, id).await, "approved");
    let rows = regressions(&w).await;
    assert_eq!((rows.len(), rows[0].3), (1, false), "失败的回归记录要原样保留");
    assert_eq!(triages(&w).await, vec![(id, "business_bug".to_string(), None)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 重跑通过就不花额度_记为分不清() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FLAKY").await;
    let claude = FakeClaude::new(&w, &[reply("locator_broken", "不该被调用", Some((TEST_POC, PAGE_POC)))]);
    let report = heal_script(&heal_config(&w, &python, &claude), id).await.unwrap();
    assert_eq!((report.verdict, report.healed), (HealVerdict::Unsure, None));
    assert!(report.reason.contains("偶发"), "{}", report.reason);
    assert_eq!(claude.calls(), 0, "重跑通过时不能调用模型");
    assert_eq!(script_count(&w).await, 1);
    assert_eq!(triages(&w).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 重跑遇到环境问题_不分诊_不花额度() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "ENVLATER").await;
    let claude = FakeClaude::new(&w, &[reply("unsure", "不该被调用", None)]);
    let error = message(heal_script(&heal_config(&w, &python, &claude), id).await.unwrap_err());
    assert!(error.contains("环境问题") && error.contains("手机没连上"), "{error}");
    assert_eq!(claude.calls(), 0);
    assert!(triages(&w).await.is_empty(), "环境问题不能记成分诊结论");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 额度用尽_不记分诊_不改用其他方式() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let quota = json!({"type": "result", "is_error": true, "result": "You've reached your weekly limit"}).to_string();
    let claude = FakeClaude::new(&w, &[quota]);
    let error = message(heal_script(&heal_config(&w, &python, &claude), id).await.unwrap_err());
    assert!(error.contains("额度已用完") && error.contains("不会改用其他计费方式"), "{error}");
    assert!(triages(&w).await.is_empty(), "额度用完时这次失败没有分诊，恢复后还能再来");
    assert_eq!((script_state(&w, id).await.as_str(), script_count(&w).await), ("approved", 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 不该分诊的情况都不调用模型() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let claude = FakeClaude::new(&w, &[reply("unsure", "不该被调用", None)]);
    let cfg = heal_config(&w, &python, &claude);

    // 候选：不是已获准
    let candidate_id = candidate(&w, "FAIL").await;
    assert!(message(heal_script(&cfg, candidate_id).await.unwrap_err()).contains("只有已获准的版本"));
    transition(w.st.pool(), candidate_id, ScriptStatus::SelfCheckPassed, None).await.unwrap();
    transition(w.st.pool(), candidate_id, ScriptStatus::Approved, None).await.unwrap();
    // 已获准但没回归过
    assert!(message(heal_script(&cfg, candidate_id).await.unwrap_err()).contains("还没有回归记录"));
    // 回归通过
    let w2 = world().await;
    let python2 = fake_python(&w2);
    let claude2 = FakeClaude::new(&w2, &[reply("unsure", "不该被调用", None)]);
    let ok_id = regressed(&w2, &python2, "PASS").await;
    assert!(message(heal_script(&heal_config(&w2, &python2, &claude2), ok_id).await.unwrap_err()).contains("通过的，不需要自愈"));
    // 不存在的脚本
    assert!(message(heal_script(&cfg, Uuid::new_v4()).await.unwrap_err()).contains("没有这个脚本"));
    assert_eq!((claude.calls(), claude2.calls()), (0, 0));
    assert!(triages(&w).await.is_empty() && triages(&w2).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 同一次失败只分诊一次_再回归失败后才能再来() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let claude = FakeClaude::new(&w, &[reply("unsure", "证据不足", None)]);
    let cfg = heal_config(&w, &python, &claude);
    heal_script(&cfg, id).await.unwrap();
    assert!(message(heal_script(&cfg, id).await.unwrap_err()).contains("已经分诊过了"));
    assert_eq!(claude.calls(), 1, "第二次不能再花额度");
    regress(&cfg.check, "p", "TC-04").await.unwrap();
    heal_script(&cfg, id).await.unwrap();
    assert_eq!(triages(&w).await.len(), 2);
    assert_eq!(claude.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 脚本不是本节点的_不能自愈() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    let claude = FakeClaude::new(&w, &[reply("unsure", "不该被调用", None)]);
    let mut cfg = heal_config(&w, &python, &claude);
    cfg.check.node_id = other;
    let error = message(heal_script(&cfg, id).await.unwrap_err());
    assert!(error.contains("不是本节点生成的"), "{error}");
    assert_eq!(claude.calls(), 0);
}

// ───── 服务端守住的规则（不信任节点上报的结论）─────

use server::triage::{submit, Fix, Submission, TriageError, Verdict as TriageVerdict};

fn fix_of(test: &str, page: &str) -> Fix {
    let files: std::collections::BTreeMap<String, String> = [("test_tc04.py".to_string(), test.to_string()), ("pages/tc04_pages.py".to_string(), page.to_string())].into();
    Fix { file_sha256: file_digests(&files), files, notes: String::new(), gen_source: "claude_cli".into(), gen_model: None, gen_effort: None }
}

async fn latest_regression_id(w: &World, script: Uuid) -> Uuid {
    sqlx::query_scalar("select id from script_regressions where script_id = $1 order by created_at desc limit 1").bind(script).fetch_one(w.st.pool()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 服务端拒绝不合规的分诊_且失败时什么都不留下() {
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let reg = latest_regression_id(&w, id).await;
    let pool = w.st.pool();
    let sub = |regression_id, verdict, reason: &str, fix| Submission { regression_id, verdict, reason: reason.into(), fix };

    // 定位失效必须带修复；其他结论不能带修复；原因必填
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::LocatorBroken, "x", None)).await, Err(TriageError::Invalid(_))));
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::BusinessBug, "x", Some(fix_of(TEST_POC, PAGE_POC)))).await, Err(TriageError::Invalid(_))));
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::Unsure, "  ", None)).await, Err(TriageError::Invalid(_))));
    // 不是最近一次失败
    assert!(matches!(submit(pool, w.node, id, sub(Uuid::new_v4(), TriageVerdict::Unsure, "x", None)).await, Err(TriageError::Stale)));
    // 修复内容和原脚本一样：归档拒绝，分诊也不能留下（同一个事务）
    let parent = server::scripts::load_files(pool, id).await.unwrap().unwrap();
    let same = fix_of(&parent["test_tc04.py"], &parent["pages/tc04_pages.py"]);
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::LocatorBroken, "x", Some(same))).await, Err(TriageError::Candidate(_))));
    // 摘要对不上
    let mut bad = fix_of(TEST_POC, PAGE_POC);
    bad.file_sha256.insert("test_tc04.py".into(), "0".repeat(64));
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::LocatorBroken, "x", Some(bad))).await, Err(TriageError::Candidate(_))));
    assert!(triages(&w).await.is_empty() && script_count(&w).await == 1, "被拒绝的分诊不能留下任何记录");

    // 合规的通过，且同一次失败不能再分诊
    assert!(submit(pool, w.node, id, sub(reg, TriageVerdict::Unsure, "证据不足", None)).await.unwrap().is_none());
    assert!(matches!(submit(pool, w.node, id, sub(reg, TriageVerdict::Unsure, "再来", None)).await, Err(TriageError::AlreadyTriaged)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 只有已获准且最近回归失败的脚本才能分诊() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let id = regressed(&w, &python, "FAIL").await;
    let reg = latest_regression_id(&w, id).await;
    transition(w.st.pool(), id, ScriptStatus::Retired, Some("下线")).await.unwrap();
    let result = submit(w.st.pool(), w.node, id, Submission { regression_id: reg, verdict: TriageVerdict::Unsure, reason: "x".into(), fix: None }).await;
    assert!(matches!(result, Err(TriageError::NotApproved(_))), "下线后不能分诊：{result:?}");
}
