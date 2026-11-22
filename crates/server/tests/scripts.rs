//! 脚本归档的数据库测试（每个测试一个独立 schema）。

use std::collections::BTreeMap;
use std::time::Duration;

use protocol::{CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, ModelSource, RoleModel, Verdict};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::scripts::{self, content_digest, file_digests, NewScript, ScriptError, ScriptStatus};
use server::test_support::test_state;
use server::{AppState, Config};
use uuid::Uuid;

const CAPS: &str = r#"{"node_version":"t","os":"macos","model_sources":[{"source":"claude_cli","vision":true},{"source":"codex_exec","vision":true}]}"#;
const TEST_FILE: &str = "test_tc04.py";
const PAGE_FILE: &str = "pages/tc04_pages.py";

fn nc(code: &str, step: &str) -> NewCase {
    NewCase {
        code: code.into(),
        title: format!("{code} 标题"),
        level: String::new(),
        platforms: vec!["android".into()],
        preconditions: vec![],
        steps: vec![step.into()],
        expected: (1..=2).map(|i| CaseExpected { idx: i, text: format!("预期{i}") }).collect(),
        data: json!({}),
        refs: vec![],
        note: String::new(),
        should_fail: false,
    }
}

fn role(source: ModelSource) -> RoleModel {
    RoleModel { source, model: None, effort: None }
}

struct World {
    st: AppState,
    node: Uuid,
}

async fn world() -> World {
    let st = test_state(Config { node_token: "t".into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(node).bind(CAPS).execute(st.pool()).await.unwrap();
    for i in 0..3 {
        sqlx::query("insert into devices (node_id, serial, platform, status) values ($1, $2, 'android', 'idle')").bind(node).bind(format!("DEV{i}")).execute(st.pool()).await.unwrap();
    }
    import(st.pool(), "p", &[nc("TC-04", "步骤"), nc("TC-05", "别的用例")], &BTreeMap::new()).await.unwrap();
    World { st, node }
}

async fn case_id(w: &World, code: &str) -> Uuid {
    sqlx::query_scalar("select id from test_cases where code = $1").bind(code).fetch_one(w.st.pool()).await.unwrap()
}

/// 把指定用例的一次执行走完，判定给定的结果和逐条明细，返回 run 编号。
async fn finished_run(w: &World, code: &str, final_result: FinalResult, verdicts: [Verdict; 2]) -> Uuid {
    runs::create_task(
        w.st.pool(),
        &NewTask { project: "p".into(), node_id: w.node, case_codes: vec![code.into()], executor: role(ModelSource::CodexExec), judge: role(ModelSource::ClaudeCli) },
    )
    .await
    .unwrap();
    let claimed = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    runs::mark_started(w.st.pool(), w.node, claimed.run_id).await.unwrap();
    runs::executor_done(w.st.pool(), w.node, &ExecutorDone { run_id: claimed.run_id, result: Verdict::Passed, reason: "过了".into(), cost: None }).await.unwrap();
    let items = verdicts.iter().enumerate().map(|(i, v)| JudgeItem { idx: i as u32 + 1, verdict: *v, evidence: "证据".into(), file_refs: vec![] }).collect();
    runs::judge_done(w.st.pool(), w.node, &JudgeDone { run_id: claimed.run_id, judge_status: JudgeStatus::Completed, final_result, items }).await.unwrap();
    claimed.run_id
}

async fn passed_run(w: &World, code: &str) -> Uuid {
    finished_run(w, code, FinalResult::Passed, [Verdict::Passed, Verdict::Passed]).await
}

fn files(test_body: &str) -> BTreeMap<String, String> {
    [(TEST_FILE.to_string(), test_body.to_string()), (PAGE_FILE.to_string(), "class Page:\n    pass\n".to_string())].into()
}

fn new_script(case: Uuid, run: Uuid, files: BTreeMap<String, String>) -> NewScript {
    NewScript {
        case_id: case,
        source_run_id: run,
        file_sha256: file_digests(&files),
        files,
        notes: "测试".into(),
        gen_source: "claude_cli".into(),
        gen_model: Some("m".into()),
        gen_effort: Some("high".into()),
    }
}

fn source_error(e: ScriptError) -> String {
    match e {
        ScriptError::SourceRun(msg) => msg,
        other => panic!("应该是来源执行错误，实际是：{other}"),
    }
}

// ---------------------------------------------------------------- 归档

#[tokio::test]
async fn 归档成功_版本从1开始_源码能取回() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let input = new_script(case, run, files("def test_a():\n    assert True\n"));
    let created = scripts::create_candidate(w.st.pool(), &input).await.unwrap();
    assert_eq!((created.version, created.case_version, created.status), (1, 1, ScriptStatus::Candidate));
    assert_eq!(created.source_run_id, run, "必须能追溯到来源执行");
    assert_eq!(created.file_names, vec![PAGE_FILE.to_string(), TEST_FILE.to_string()], "按路径排序");
    assert_eq!(created.content_sha256, content_digest(&input.files));
    assert_eq!((created.gen_source.as_str(), created.gen_model.as_deref(), created.gen_effort.as_deref()), ("claude_cli", Some("m"), Some("high")));
    assert_eq!(scripts::load_files(w.st.pool(), created.id).await.unwrap().unwrap(), input.files, "取回的源码必须和归档时一致");
    assert_eq!(scripts::get(w.st.pool(), created.id).await.unwrap().unwrap().id, created.id);
    assert!(scripts::get(w.st.pool(), Uuid::new_v4()).await.unwrap().is_none());
}

#[tokio::test]
async fn 同样内容只归档一次_不同内容版本递增() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let first = scripts::create_candidate(w.st.pool(), &new_script(case, run, files("def test_a():\n    assert 1\n"))).await.unwrap();
    match scripts::create_candidate(w.st.pool(), &new_script(case, run, files("def test_a():\n    assert 1\n"))).await.unwrap_err() {
        ScriptError::Duplicate { id, version } => assert_eq!((id, version), (first.id, 1), "重复要指回已有版本"),
        other => panic!("应该是重复，实际是：{other}"),
    }
    let second = scripts::create_candidate(w.st.pool(), &new_script(case, run, files("def test_a():\n    assert 2\n"))).await.unwrap();
    assert_eq!(second.version, 2);
    let listed: Vec<i32> = scripts::list_by_case(w.st.pool(), case).await.unwrap().iter().map(|s| s.version).collect();
    assert_eq!(listed, vec![2, 1], "新的在前");
    assert!(scripts::list_by_case(w.st.pool(), case_id(&w, "TC-05").await).await.unwrap().is_empty(), "别的用例不受影响");
}

#[tokio::test]
async fn 并发归档同一用例版本号不重复() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let mut handles = Vec::new();
    for n in 0..6 {
        let pool = w.st.pool().clone();
        let input = new_script(case, run, files(&format!("def test_a():\n    assert {n}\n")));
        handles.push(tokio::spawn(async move { scripts::create_candidate(&pool, &input).await.unwrap().version }));
    }
    let mut versions = Vec::new();
    for handle in handles {
        versions.push(handle.await.unwrap());
    }
    versions.sort_unstable();
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6], "并发归档也不能出现重复或跳号");
}

// ---------------------------------------------------------------- 入库前的硬检查

#[tokio::test]
async fn 来源执行不是通过_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let failed = finished_run(&w, "TC-04", FinalResult::Failed, [Verdict::Passed, Verdict::Failed]).await;
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, failed, files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("不是通过"), "{msg}");
    let uncertain = finished_run(&w, "TC-04", FinalResult::Uncertain, [Verdict::Passed, Verdict::Uncertain]).await;
    assert!(scripts::create_candidate(w.st.pool(), &new_script(case, uncertain, files("x = 1\n"))).await.is_err(), "待人工确认也不能生成脚本");
    assert!(scripts::list_by_case(w.st.pool(), case).await.unwrap().is_empty(), "被拒绝的不能留下记录");
}

#[tokio::test]
async fn 来源执行不存在或属于别的用例_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, Uuid::new_v4(), files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("不存在"), "{msg}");
    let other = passed_run(&w, "TC-05").await;
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, other, files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("不是这个用例"), "{msg}");
    assert!(matches!(scripts::create_candidate(w.st.pool(), &new_script(Uuid::new_v4(), other, files("x = 1\n"))).await.unwrap_err(), ScriptError::CaseNotFound));
}

#[tokio::test]
async fn 某条预期没有通过项_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    sqlx::query("delete from judge_items where run_id = $1 and idx = 2").bind(run).execute(w.st.pool()).await.unwrap();
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("预期 2"), "要指出是哪一条：{msg}");
    sqlx::query("update judge_items set verdict = 'uncertain' where run_id = $1 and idx = 1").bind(run).execute(w.st.pool()).await.unwrap();
    assert!(scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.is_err(), "有一条不是通过也不行");
}

#[tokio::test]
async fn judge没有完成_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    // 库里有约束：Judge 失败时最终结果必须是待人工确认，所以两列要一起改成合法状态。
    sqlx::query("update runs set judge_status = 'failed', final_result = 'uncertain' where id = $1").bind(run).execute(w.st.pool()).await.unwrap();
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("Judge"), "{msg}");
}

#[tokio::test]
async fn 用例内容变了_旧版本的执行不能生成新脚本() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    import(w.st.pool(), "p", &[nc("TC-04", "步骤改了")], &BTreeMap::new()).await.unwrap();
    let version: i32 = sqlx::query_scalar("select version from test_cases where id = $1").bind(case).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(version, 2, "前提：用例版本已经自增");
    let msg = source_error(scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.unwrap_err());
    assert!(msg.contains("用例版本"), "{msg}");
}

#[tokio::test]
async fn 摘要和内容对不上_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let mut tampered = new_script(case, run, files("x = 1\n"));
    tampered.file_sha256.insert(TEST_FILE.into(), "0".repeat(64));
    match scripts::create_candidate(w.st.pool(), &tampered).await.unwrap_err() {
        ScriptError::HashMismatch(names) => assert!(names.contains(TEST_FILE), "要指出是哪个文件：{names}"),
        other => panic!("应该是摘要不一致，实际是：{other}"),
    }
    let mut missing = new_script(case, run, files("x = 1\n"));
    missing.file_sha256.remove(PAGE_FILE);
    assert!(matches!(scripts::create_candidate(w.st.pool(), &missing).await.unwrap_err(), ScriptError::HashMismatch(_)), "少一个摘要也不行");
}

#[tokio::test]
async fn 文件名或大小不合规_拒绝() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    for bad in ["../evil.py", "/etc/cron.d/x", "conftest.py", "test_tc05.py"] {
        let mut all = files("x = 1\n");
        all.insert(bad.into(), "x = 1\n".into());
        assert!(matches!(scripts::create_candidate(w.st.pool(), &new_script(case, run, all)).await.unwrap_err(), ScriptError::InvalidFiles(_)), "{bad}");
    }
    let only_page: BTreeMap<String, String> = [(PAGE_FILE.to_string(), "x = 1\n".to_string())].into();
    assert!(matches!(scripts::create_candidate(w.st.pool(), &new_script(case, run, only_page)).await.unwrap_err(), ScriptError::InvalidFiles(_)), "缺测试文件");
    assert!(scripts::list_by_case(w.st.pool(), case).await.unwrap().is_empty());
}

// ---------------------------------------------------------------- 状态流转

#[tokio::test]
async fn 状态必须经过自检才能获准() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let script = scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.unwrap();

    let err = scripts::transition(w.st.pool(), script.id, ScriptStatus::Approved, None).await.unwrap_err();
    assert!(matches!(err, ScriptError::BadTransition { from: ScriptStatus::Candidate, to: ScriptStatus::Approved }), "不能跳过自检：{err}");
    assert_eq!(scripts::get(w.st.pool(), script.id).await.unwrap().unwrap().status, ScriptStatus::Candidate, "失败的迁移不能改状态");

    let checked = scripts::transition(w.st.pool(), script.id, ScriptStatus::SelfCheckPassed, Some("自检 3 项通过")).await.unwrap();
    assert_eq!((checked.status, checked.status_note.as_deref()), (ScriptStatus::SelfCheckPassed, Some("自检 3 项通过")));
    assert!(checked.updated_at >= script.updated_at);
    assert_eq!(scripts::transition(w.st.pool(), script.id, ScriptStatus::Approved, None).await.unwrap().status, ScriptStatus::Approved);
    assert!(scripts::transition(w.st.pool(), script.id, ScriptStatus::Rejected, None).await.is_err(), "获准后只能下线");
    assert_eq!(scripts::transition(w.st.pool(), script.id, ScriptStatus::Retired, Some("用例改版")).await.unwrap().status, ScriptStatus::Retired);
    assert!(scripts::transition(w.st.pool(), script.id, ScriptStatus::Approved, None).await.is_err(), "下线是终点");
    assert!(matches!(scripts::transition(w.st.pool(), Uuid::new_v4(), ScriptStatus::Rejected, None).await.unwrap_err(), ScriptError::NotFound));
}

#[tokio::test]
async fn 拒绝是终点并带理由() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let script = scripts::create_candidate(w.st.pool(), &new_script(case, run, files("x = 1\n"))).await.unwrap();
    let rejected = scripts::transition(w.st.pool(), script.id, ScriptStatus::Rejected, Some("自检失败：断言 3 不成立")).await.unwrap();
    assert_eq!((rejected.status, rejected.status_note.as_deref()), (ScriptStatus::Rejected, Some("自检失败：断言 3 不成立")));
    assert!(scripts::transition(w.st.pool(), script.id, ScriptStatus::SelfCheckPassed, None).await.is_err());
}

// ---------------------------------------------------------------- 库约束兜底：绕过模块直接写库也写不进脏数据

#[tokio::test]
async fn 库约束拒绝脏数据() {
    let w = world().await;
    let case = case_id(&w, "TC-04").await;
    let run = passed_run(&w, "TC-04").await;
    let good = "a".repeat(64);
    let insert = |files: &'static str, sha: String, status: &'static str, source: &'static str| {
        let pool = w.st.pool().clone();
        async move {
            sqlx::query(
                "insert into scripts (case_id, version, case_version, source_run_id, files, content_sha256, gen_source, status) values ($1, 1, 1, $2, $3::jsonb, $4, $5, $6)",
            )
            .bind(case)
            .bind(run)
            .bind(files)
            .bind(sha)
            .bind(source)
            .bind(status)
            .execute(&pool)
            .await
        }
    };
    assert!(insert(r#"{"test_tc04.py":"x"}"#, good.clone(), "approved", "claude_cli").await.is_ok(), "对照：合法数据能写入");
    for (what, result) in [
        ("未知状态", insert(r#"{"t":"x"}"#, "b".repeat(64), "weird", "claude_cli").await),
        ("摘要格式不对", insert(r#"{"t":"x"}"#, "XYZ".into(), "candidate", "claude_cli").await),
        ("空文件表", insert("{}", "c".repeat(64), "candidate", "claude_cli").await),
        ("文件表不是对象", insert(r#"["x"]"#, "d".repeat(64), "candidate", "claude_cli").await),
        ("未知模型来源", insert(r#"{"t":"x"}"#, "e".repeat(64), "candidate", "gpt_api_key").await),
        ("同用例同版本", insert(r#"{"t":"y"}"#, "f".repeat(64), "candidate", "claude_cli").await),
        ("同用例同内容", insert(r#"{"t":"z"}"#, good.clone(), "candidate", "claude_cli").await),
    ] {
        assert!(result.is_err(), "{what} 必须被库拒绝");
    }
}
