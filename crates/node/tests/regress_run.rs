//! 节点侧回归的端到端测试：进程内起真实服务端和数据库，Python 用假的可执行脚本（不碰真机、不花额度）。
//! 假 python 按测试文件里的标记模拟 pytest 的各种结局。

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use node::regress_run::regress;
use node::selfcheck_run::CheckConfig;
use protocol::{CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, ModelSource, RoleModel, Verdict};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::scripts::{create_candidate, file_digests, NewScript};
use server::test_support::test_state;
use server::{app, AppState, Config};
use uuid::Uuid;

const TOKEN: &str = "regress-run-token";
const JUDGE_MODEL: &str = "角色指定的判定模型";
const CAPS: &str = r#"{"node_version":"t","os":"macos","model_sources":[{"source":"claude_cli","vision":true},{"source":"codex_exec","vision":true}]}"#;

/// 假 python：先确认 generated 包结构，再按测试文件里的标记模拟 pytest。
const FAKE_PYTHON: &str = r#"#!/bin/sh
f=generated/test_tc04.py
[ -f generated/__init__.py ] && [ -f generated/pages/__init__.py ] && [ -f "$f" ] || { echo "包结构不对"; exit 9; }
echo "serial=$HAFA_SERIAL pythonpath=$PYTHONPATH disable=$PYTEST_DISABLE_PLUGIN_AUTOLOAD artifact=$HAFA_ARTIFACT_DIR args=$*" > "$0.seen"
case "$(cat "$f")" in
  *ENVFAIL*) echo "准备前置状态失败（退出码 1）：手机没连上"; exit 1;;
  *SLEEP*) sleep 30; exit 0;;
  *FAIL*) echo "AssertionError: 目标歌曲没找到"; echo "1 failed in 0.5s"; exit 1;;
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

async fn status_of(w: &World, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("select status, status_note from scripts where id = $1").bind(id).fetch_one(w.st.pool()).await.unwrap()
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

fn python_started(python: &PathBuf) -> bool {
    PathBuf::from(format!("{}.seen", python.display())).exists()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 已获准的版本通过_记录一次通过_脚本状态不变() {
    let w = world().await;
    let python = fake_python(&w);
    let id = approved(&w, "PASS").await;
    let outcome = regress(&config(&w, &python, Duration::from_secs(20)), "p", "TC-04").await.unwrap();
    assert!(outcome.passed && outcome.version == 1, "{outcome:?}");
    let rows = regressions(&w).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0, rows[0].1, rows[0].2.as_str(), rows[0].3), (id, w.node, "DEV0", true));
    assert!(rows[0].4.contains("1 passed"), "{}", rows[0].4);
    assert_eq!(status_of(&w, id).await.0, "approved", "回归不改脚本状态");
    assert!(fs::read_dir(w.dir.join("work")).unwrap().next().is_none(), "临时目录必须清掉");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回归失败是结论_如实记录原始输出_脚本仍是已获准() {
    let w = world().await;
    let python = fake_python(&w);
    let id = approved(&w, "FAIL").await;
    let outcome = regress(&config(&w, &python, Duration::from_secs(20)), "p", "TC-04").await.unwrap();
    assert!(!outcome.passed);
    assert!(outcome.detail.contains("退出码 1") && outcome.detail.contains("目标歌曲没找到"), "{}", outcome.detail);
    let rows = regressions(&w).await;
    assert_eq!((rows.len(), rows[0].3), (1, false));
    assert!(rows[0].4.contains("目标歌曲没找到"), "原始失败输出要原样记下");
    assert_eq!(status_of(&w, id).await.0, "approved", "回归失败不能自动下线脚本，由人判断是产品退化还是脚本失效");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 环境问题和超时不记录_只报错() {
    let w = world().await;
    let python = fake_python(&w);
    approved(&w, "ENVFAIL").await;
    let error = message(regress(&config(&w, &python, Duration::from_secs(20)), "p", "TC-04").await.unwrap_err());
    assert!(error.contains("环境问题") && error.contains("手机没连上"), "{error}");
    assert!(regressions(&w).await.is_empty(), "手机掉线不是脚本的结论，不能记成失败");

    let w = world().await;
    let python = fake_python(&w);
    approved(&w, "SLEEP").await;
    let started = std::time::Instant::now();
    let error = message(regress(&config(&w, &python, Duration::from_secs(1)), "p", "TC-04").await.unwrap_err());
    assert!(error.contains("超过 1 秒"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(regressions(&w).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 没有获准版本不能回归_不占手机() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "PASS").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let error = message(regress(&cfg, "p", "TC-04").await.unwrap_err());
    assert!(error.contains("没有已获准的脚本"), "{error}");
    transition(w.st.pool(), id, ScriptStatus::SelfCheckPassed, None).await.unwrap();
    assert!(message(regress(&cfg, "p", "TC-04").await.unwrap_err()).contains("没有已获准的脚本"), "自检通过但没获准也不行");
    assert!(message(regress(&cfg, "p", "TC-99").await.unwrap_err()).contains("没有已获准的脚本"));
    assert!(!python_started(&python), "没有获准版本时不能启动 python");
    assert!(regressions(&w).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 下线后不能回归_换成新获准版本后用新版本() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let old = approved(&w, "FAIL").await;
    transition(w.st.pool(), old, ScriptStatus::Retired, Some("换新版")).await.unwrap();
    let cfg = config(&w, &python, Duration::from_secs(20));
    assert!(message(regress(&cfg, "p", "TC-04").await.unwrap_err()).contains("没有已获准的脚本"));
    let new = approved(&w, "PASS").await;
    let outcome = regress(&cfg, "p", "TC-04").await.unwrap();
    assert!(outcome.passed, "必须用当前获准的版本，不是旧的失败版本：{outcome:?}");
    assert_eq!(regressions(&w).await[0].0, new);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 别的节点也能回归获准的脚本_结果记在它名下() {
    let w = world().await;
    let python = fake_python(&w);
    approved(&w, "PASS").await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    let mut cfg = config(&w, &python, Duration::from_secs(20));
    cfg.node_id = other;
    assert!(regress(&cfg, "p", "TC-04").await.unwrap().passed);
    assert_eq!(regressions(&w).await[0].1, other);
}
