//! 节点侧自检的端到端测试：进程内起真实服务端和数据库，Python 用假的可执行脚本（不碰真机、不花额度）。
//! 假 python 按测试文件里的标记模拟 pytest 的各种结局。

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use node::selfcheck_run::{self_check, CheckConfig, Outcome};
use protocol::{CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, ModelSource, RoleModel, Verdict};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::scripts::{create_candidate, file_digests, NewScript};
use server::test_support::test_state;
use server::{app, AppState, Config};
use uuid::Uuid;

const TOKEN: &str = "selfcheck-run-token";
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
    let dir = std::env::temp_dir().join(format!("node_selfcheck_{}", Uuid::new_v4()));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 脚本通过_候选变自检通过_环境按配置传给脚本() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "PASS").await;
    let (outcome, detail) = self_check(&config(&w, &python, Duration::from_secs(20)), id).await.unwrap();
    assert_eq!(outcome, Outcome::Passed);
    assert!(detail.contains("1 passed"), "{detail}");
    let (status, note) = status_of(&w, id).await;
    assert_eq!(status, "self_check_passed");
    assert!(note.unwrap().contains("1 passed"));

    let seen = fs::read_to_string(format!("{}.seen", python.display())).unwrap();
    assert!(seen.contains("serial=DEV0"), "{seen}");
    assert!(seen.contains(&format!("pythonpath={}", w.dir.display())), "{seen}");
    assert!(seen.contains("disable=1"), "只加载指定的插件：{seen}");
    assert!(seen.contains("-p hafa_automation.pytest_plugin") && seen.contains("--strict-markers"), "{seen}");
    assert!(fs::read_dir(w.dir.join("work")).unwrap().next().is_none(), "临时目录必须清掉");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 脚本自己失败_候选被拒绝_原始输出记进说明() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "FAIL").await;
    let (outcome, detail) = self_check(&config(&w, &python, Duration::from_secs(20)), id).await.unwrap();
    assert_eq!(outcome, Outcome::Rejected);
    assert!(detail.contains("退出码 1") && detail.contains("目标歌曲没找到"), "{detail}");
    let (status, note) = status_of(&w, id).await;
    assert_eq!(status, "rejected");
    assert!(note.unwrap().contains("目标歌曲没找到"), "原始失败输出要原样记下");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 环境问题不拒绝脚本_保持候选并带上原始输出() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "ENVFAIL").await;
    let error = message(self_check(&config(&w, &python, Duration::from_secs(20)), id).await.unwrap_err());
    assert!(error.contains("环境问题") && error.contains("手机没连上"), "{error}");
    assert_eq!(status_of(&w, id).await.0, "candidate", "手机掉线不能把好脚本判死");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 超时按环境问题处理_终止进程_保持候选() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "SLEEP").await;
    let started = std::time::Instant::now();
    let error = message(self_check(&config(&w, &python, Duration::from_secs(1)), id).await.unwrap_err());
    assert!(error.contains("超过 1 秒") && error.contains("保持候选"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10), "必须按时终止，不能等脚本睡完");
    assert_eq!(status_of(&w, id).await.0, "candidate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 已有结论的脚本不重复自检_不占手机() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "PASS").await;
    self_check(&config(&w, &python, Duration::from_secs(20)), id).await.unwrap();
    fs::remove_file(format!("{}.seen", python.display())).unwrap();
    let error = message(self_check(&config(&w, &python, Duration::from_secs(20)), id).await.unwrap_err());
    assert!(error.contains("self_check_passed") && error.contains("只有候选"), "{error}");
    assert!(!PathBuf::from(format!("{}.seen", python.display())).exists(), "状态不对时不能启动 python");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 不是本节点的脚本不能自检() {
    let w = world().await;
    let python = fake_python(&w);
    let id = candidate(&w, "PASS").await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    let mut cfg = config(&w, &python, Duration::from_secs(20));
    cfg.node_id = other;
    let error = message(self_check(&cfg, id).await.unwrap_err());
    assert!(error.contains("不是本节点"), "{error}");
    assert_eq!(status_of(&w, id).await.0, "candidate");
    assert!(!PathBuf::from(format!("{}.seen", python.display())).exists(), "没有资格时不能启动 python");
}
