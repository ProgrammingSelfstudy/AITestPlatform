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
  *SERIAL*) echo "adb -s $HAFA_SERIAL shell input tap 1 2: device offline"; echo "1 failed in 0.5s"; exit 1;;
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

// ───── 页面发起的回归：请求、领取、回报、设备互斥 ─────

use node::regress_poll::{abandon_stale, poll_once};
use node::regress_run::regress_script;

/// 请求头里的名字要百分号编码（中文不能直接放进 HTTP 头）。身份只记名字，不是机密。
fn enc(name: &str) -> String {
    name.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// 造一个用户并把节点交给他，返回请求头里用的名字（已编码）。
async fn owner_token(w: &World, name: &str) -> String {
    let user = server::users::ensure_user_for_test(w.st.pool(), name).await.unwrap();
    sqlx::query("update nodes set owner_user_id = $1 where id = $2").bind(user).bind(w.node).execute(w.st.pool()).await.unwrap();
    enc(name)
}

/// 页面发起：POST /api/scripts/{id}/regress，返回状态码和正文。
async fn page_request(w: &World, script: Uuid, token: &str, node: Uuid) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/api/scripts/{script}/regress", w.addr))
        .header("x-user-name", token)
        .header("x-review-token", REVIEW_TOKEN)
        .header("x-requested-with", "ai-test-platform")
        .json(&json!({"node_id": node}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(serde_json::Value::Null))
}

async fn request_row(w: &World, id: &str) -> (String, String, Option<String>) {
    sqlx::query_as("select status, detail, serial from regression_requests where id = $1::uuid").bind(id).fetch_one(w.st.pool()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 页面发起回归_节点领取跑完回报_结论和请求状态都记下() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let cfg = config(&w, &python, Duration::from_secs(20));

    assert!(!poll_once(&cfg).await.unwrap(), "没有请求时什么也不做");
    let (status, body) = page_request(&w, script, &token, w.node).await;
    assert_eq!(status, 201, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(request_row(&w, &id).await.0, "queued");
    assert!(!python_started(&python), "排队不会自己跑");

    assert!(poll_once(&cfg).await.unwrap());
    let (state, detail, serial) = request_row(&w, &id).await;
    assert_eq!((state.as_str(), serial.as_deref()), ("finished", Some("DEV0")), "{detail}");
    assert!(detail.contains("回归通过"), "{detail}");
    let rows = regressions(&w).await;
    assert_eq!((rows.len(), rows[0].0, rows[0].3), (1, script, true));
    assert!(!poll_once(&cfg).await.unwrap(), "领完了");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回归失败是结论_请求算完成_环境问题没有结论_请求算失败() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "FAIL").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    assert!(poll_once(&cfg).await.unwrap());
    let (state, detail, _) = request_row(&w, &id).await;
    assert_eq!(state, "finished", "失败的回归也是得到了结论");
    assert!(detail.contains("回归失败"), "{detail}");
    assert_eq!(regressions(&w).await.iter().map(|r| r.3).collect::<Vec<_>>(), vec![false]);

    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "ENVFAIL").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    assert!(poll_once(&cfg).await.unwrap());
    let (state, detail, _) = request_row(&w, &id).await;
    assert_eq!(state, "failed", "没有得到结论");
    assert!(detail.contains("手机没连上"), "原因要原样给人看：{detail}");
    assert!(regressions(&w).await.is_empty(), "环境问题不记成回归失败");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 只能发到自己的节点_只有获准版本_不能重复排队() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    server::users::ensure_user_for_test(w.st.pool(), "乙").await.unwrap();
    let stranger = enc("乙");
    let script = approved(&w, "PASS").await;
    let candidate_script = candidate(&w, "PASS2").await;

    assert_eq!(page_request(&w, script, &stranger, w.node).await.0, 403, "别人的节点不能用");
    assert_eq!(page_request(&w, script, "u_bad", w.node).await.0, 401);
    assert_eq!(page_request(&w, script, "", w.node).await.0, 401);
    assert_eq!(page_request(&w, script, &token, Uuid::new_v4()).await.0, 404, "没有这个节点");
    assert_eq!(page_request(&w, candidate_script, &token, w.node).await.0, 409, "候选版本不能回归");
    assert_eq!(page_request(&w, Uuid::new_v4(), &token, w.node).await.0, 404);
    let count: i64 = sqlx::query_scalar("select count(*) from regression_requests").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(count, 0, "被拒绝的请求都不能留下记录");

    assert_eq!(page_request(&w, script, &token, w.node).await.0, 201);
    assert_eq!(page_request(&w, script, &token, w.node).await.0, 409, "连点两次只排一份");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回归占着手机时不派任务_设备忙时不领回归() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    page_request(&w, script, &token, w.node).await;

    // 先有一个任务排着：回归领走手机以后，任务不能被派到同一台手机
    runs::create_task(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["TC-04".into()], executor: role(ModelSource::CodexExec, None), judge: role(ModelSource::ClaudeCli, Some(JUDGE_MODEL)) }).await.unwrap();
    let claimed = server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap();
    assert!(claimed.is_some());
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "回归运行期间，这台手机不能被派任务");
    // 节点心跳里报「空闲」也不能把锁覆盖掉
    sqlx::query("update devices set status = 'idle'").execute(w.st.pool()).await.unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none());

    // 回归结束后任务可以派了
    server::regress_requests::finish(w.st.pool(), w.node, claimed.unwrap().id, true, "ok").await.unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_some());
    // 反过来：任务占着手机时，回归不能领
    page_request(&w, script, &token, w.node).await;
    assert!(!poll_once(&cfg).await.unwrap(), "手机上有任务在跑，回归要继续排队");
    assert!(!python_started(&python));
    let queued: i64 = sqlx::query_scalar("select count(*) from regression_requests where status = 'queued'").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(queued, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 排队期间脚本被下线_不回归_不占手机() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    transition(w.st.pool(), script, ScriptStatus::Retired, Some("不要了")).await.unwrap();

    assert!(!poll_once(&cfg).await.unwrap());
    let (state, detail, _) = request_row(&w, &id).await;
    assert_eq!(state, "failed");
    assert!(detail.contains("不再是获准版本"), "{detail}");
    assert!(!python_started(&python) && regressions(&w).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 获准版本换了就不回归_不把别的版本的结果记成这次() {
    use server::scripts::{transition, ScriptStatus};
    let w = world().await;
    let python = fake_python(&w);
    let old = approved(&w, "FAIL").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    transition(w.st.pool(), old, ScriptStatus::Retired, Some("换新版")).await.unwrap();
    let new = approved(&w, "PASS").await;
    let error = message(regress_script(&cfg, "p", "TC-04", Some(old)).await.unwrap_err());
    assert!(error.contains("已不是用例 TC-04 当前的获准版本"), "{error}");
    assert!(!python_started(&python) && regressions(&w).await.is_empty());
    assert!(regress_script(&cfg, "p", "TC-04", Some(new)).await.unwrap().passed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 没有审核口令不能排队也不能取消() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let post = |path: String,口令: Option<&'static str>| {
        let url = format!("http://{}{path}", w.addr);
        let token = token.clone();
        let node = w.node;
        async move {
            let mut request = reqwest::Client::new().post(url).header("x-user-name", token).header("x-requested-with", "ai-test-platform").json(&json!({"node_id": node}));
            if let Some(value) = 口令 {
                request = request.header("x-review-token", value);
            }
            request.send().await.unwrap().status().as_u16()
        }
    };
    assert_eq!(post(format!("/api/scripts/{script}/regress"), None).await, 403);
    assert_eq!(post(format!("/api/scripts/{script}/regress"), Some("口令不对口令不对口令不对")).await, 403);
    let count: i64 = sqlx::query_scalar("select count(*) from regression_requests").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(count, 0);
    assert_eq!(post(format!("/api/scripts/{script}/regress"), Some(REVIEW_TOKEN)).await, 201);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 取消只能取消排队中的_且只能是自己的节点() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    server::users::ensure_user_for_test(w.st.pool(), "乙").await.unwrap();
    let stranger = enc("乙");
    let script = approved(&w, "PASS").await;
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    let cancel = |token: String, id: String| {
        let url = format!("http://{}/api/regression-requests/{id}/cancel", w.addr);
        async move { reqwest::Client::new().post(url).header("x-user-name", token).header("x-review-token", REVIEW_TOKEN).header("x-requested-with", "ai-test-platform").header("content-type", "application/json").send().await.unwrap().status().as_u16() }
    };
    assert_eq!(cancel(stranger, id.clone()).await, 403);
    assert_eq!(request_row(&w, &id).await.0, "queued", "别人取消不了");
    assert_eq!(cancel(token.clone(), id.clone()).await, 204);
    assert_eq!(request_row(&w, &id).await.0, "cancelled");
    assert_eq!(cancel(token.clone(), id.clone()).await, 409, "已经取消，不能再取消（请求存在，只是状态不对）");
    assert_eq!(cancel(token.clone(), Uuid::new_v4().to_string()).await, 404, "没有这个请求");
    // 取消后可以重新排
    assert_eq!(page_request(&w, script, &token, w.node).await.0, 201);
    // 已被节点领走的不能取消
    let claimed = server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    assert_eq!(cancel(token, claimed.id.to_string()).await, 409, "已被节点领走，不能取消");
    assert_eq!(request_row(&w, &claimed.id.to_string()).await.0, "running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 节点重启作废没回报的请求_超时也会释放设备() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();

    abandon_stale(&cfg).await.unwrap();
    let (state, detail, _) = request_row(&w, &id).await;
    assert_eq!(state, "failed");
    assert!(detail.contains("没有回报"), "{detail}");

    // 超时：领走后很久没回报，下一次领取时作废
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    sqlx::query("update regression_requests set claimed_at = clock_timestamp() - interval '2 hours' where id = $1::uuid").bind(&id).execute(w.st.pool()).await.unwrap();
    assert!(server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().is_none());
    assert_eq!(request_row(&w, &id).await.0, "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 别的节点的请求不会被本节点领走_回报也不行() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    sqlx::query("insert into devices (node_id, serial, platform, status) values ($1, 'DEV0', 'android', 'idle')").bind(other).execute(w.st.pool()).await.unwrap();
    let (_, body) = page_request(&w, script, &token, w.node).await;
    assert!(server::regress_requests::claim(w.st.pool(), other, "DEV0").await.unwrap().is_none(), "同序列号的别的节点也领不到");
    let claimed = server::regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    assert_eq!(claimed.id.to_string(), body["id"].as_str().unwrap());
    let stolen = server::regress_requests::finish(w.st.pool(), other, claimed.id, true, "冒充").await;
    assert!(matches!(stolen, Err(server::regress_requests::RequestError::NotYours)), "别的节点不能替它回报：{stolen:?}");
    assert_eq!(request_row(&w, &claimed.id.to_string()).await.0, "running");
}

// ───── 评审（agy）发现并经核实的缺陷的回归测试 ─────

use server::regress_requests::{self, RequestError};

async fn device_status(w: &World, node: Uuid) -> String {
    sqlx::query_scalar("select status from devices where node_id = $1 and serial = 'DEV0'").bind(node).fetch_one(w.st.pool()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 超时清理在设备看起来忙时也要生效_不能被早退回滚() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    assert_eq!(device_status(&w, w.node).await, "busy", "领走回归后设备是 busy（心跳也会这么标）");
    sqlx::query("update regression_requests set claimed_at = clock_timestamp() - interval '2 hours' where id = $1::uuid").bind(&id).execute(w.st.pool()).await.unwrap();

    // 此时设备 busy 完全是这个没回报的请求造成的。领取必须先把它作废并提交，再继续；
    // 以前是看到 busy 就早退，作废的 UPDATE 跟着事务一起回滚，请求永远是运行中、设备永远是 busy
    assert!(regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().is_none(), "没有排队的请求，领不到东西");
    assert_eq!(request_row(&w, &id).await.0, "failed", "作废结果要真的落库，不能被回滚");
    assert_eq!(device_status(&w, w.node).await, "idle", "作废后设备放回空闲");
    // 旧请求清掉后，新请求能排也能领到
    let (status, _) = page_request(&w, script, &token, w.node).await;
    assert_eq!(status, 201);
    assert!(regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 后台定时清理不依赖节点轮询_释放设备() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    // 没超时：不动
    assert_eq!(regress_requests::expire_stale(w.st.pool(), None).await.unwrap(), 0);
    assert_eq!(request_row(&w, &id).await.0, "running");
    sqlx::query("update regression_requests set claimed_at = clock_timestamp() - interval '2 hours' where id = $1::uuid").bind(&id).execute(w.st.pool()).await.unwrap();
    assert_eq!(regress_requests::expire_stale(w.st.pool(), None).await.unwrap(), 1);
    assert_eq!(request_row(&w, &id).await.0, "failed");
    assert_eq!(device_status(&w, w.node).await, "idle", "作废后设备要放回空闲，派发才能再用");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 领取时设备行被真正改写_回报后放回空闲_有任务时不放() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    page_request(&w, script, &token, w.node).await;
    let claimed = regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    assert_eq!(device_status(&w, w.node).await, "busy", "只锁不改的话，并发里选设备的派发查询沿用旧快照，可能仍把这台设备选走");
    regress_requests::finish(w.st.pool(), w.node, claimed.id, true, "ok").await.unwrap();
    assert_eq!(device_status(&w, w.node).await, "idle");
    // 再领一次，回报前设备上有活跃 run（模拟别处占用）：放回空闲时不能把有 run 的设备标成空闲
    page_request(&w, script, &token, w.node).await;
    let second = regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    runs::create_task(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["TC-04".into()], executor: role(ModelSource::CodexExec, None), judge: role(ModelSource::ClaudeCli, Some(JUDGE_MODEL)) }).await.unwrap();
    sqlx::query("update runs set status = 'running', device_id = (select id from devices where serial = 'DEV0'), started_at = clock_timestamp() where status = 'queued'").execute(w.st.pool()).await.unwrap();
    regress_requests::finish(w.st.pool(), w.node, second.id, true, "ok").await.unwrap();
    assert_eq!(device_status(&w, w.node).await, "busy", "设备上有活跃 run，不能标成空闲");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回报请求结果的状态码分得清_不存在_不是你的_已不是运行中() {
    let w = world().await;
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(other).bind(CAPS).execute(w.st.pool()).await.unwrap();
    page_request(&w, script, &token, w.node).await;
    let claimed = regress_requests::claim(w.st.pool(), w.node, "DEV0").await.unwrap().unwrap();
    let pool = w.st.pool();
    assert!(matches!(regress_requests::finish(pool, w.node, Uuid::new_v4(), true, "x").await, Err(RequestError::NotFound)));
    assert!(matches!(regress_requests::finish(pool, other, claimed.id, true, "x").await, Err(RequestError::NotYours)));
    regress_requests::finish(pool, w.node, claimed.id, true, "ok").await.unwrap();
    assert!(matches!(regress_requests::finish(pool, w.node, claimed.id, true, "x").await, Err(RequestError::Conflict(_))), "已经回报过的，迟到的重复回报是状态冲突，不是不存在");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回归期间持有本地设备锁_本地的录屏和收尾没结束就不碰手机() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "PASS").await;
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();

    // 模拟：服务端已经把设备放出来了，但本地的任务收尾还没结束，设备锁还被占着
    let guard = node::hold_device("DEV0").await;
    let poll_cfg = config(&w, &python, Duration::from_secs(20));
    let poll = tokio::spawn(async move { poll_once(&poll_cfg).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!poll.is_finished(), "设备锁被占着，回归不能开始");
    assert!(!python_started(&python), "没拿到设备锁之前不能启动 pytest");
    assert_eq!(request_row(&w, &id).await.0, "running");
    drop(guard);
    assert!(poll.await.unwrap().unwrap());
    assert!(python_started(&python));
    assert_eq!(request_row(&w, &id).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 回归的输出和请求说明里不带设备序列号() {
    let w = world().await;
    let python = fake_python(&w);
    let token = owner_token(&w, "甲").await;
    let script = approved(&w, "SERIAL").await;
    let cfg = config(&w, &python, Duration::from_secs(20));
    let (_, body) = page_request(&w, script, &token, w.node).await;
    let id = body["id"].as_str().unwrap().to_string();
    assert!(poll_once(&cfg).await.unwrap());
    let rows = regressions(&w).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].4.contains("adb -s （设备序列号）") && !rows[0].4.contains("DEV0"), "{}", rows[0].4);
    // 设备序列号本来就单独存在 serial 列里，这是给人看的输出文字，不该再带
    let (_, detail, _) = request_row(&w, &id).await;
    assert!(!detail.contains("DEV0"), "{detail}");
}
