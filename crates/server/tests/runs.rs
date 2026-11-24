//! 任务与 run 状态机的数据库测试（每个测试一个独立 schema）。

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use protocol::{
    BlockedReason, CaseExpected, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeOpinion, JudgeOpinions, JudgeStatus, ModelSource, RoleModel, RunBlocked, Step, Verdict,
};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask, RunError};
use server::test_support::test_state;
use server::{AppState, Config};
use sqlx::Row;
use uuid::Uuid;

const CAPS: &str = r#"{"node_version":"t","os":"macos","model_sources":[{"source":"claude_cli","vision":true},{"source":"codex_exec","vision":true}]}"#;

fn nc(code: &str, n_expected: u32, step: &str) -> NewCase {
    NewCase {
        code: code.into(),
        title: format!("{code} 标题"),
        level: String::new(),
        platforms: vec!["android".into()],
        preconditions: vec![],
        steps: vec![step.into()],
        expected: (1..=n_expected).map(|i| CaseExpected { idx: i, text: format!("预期{i}") }).collect(),
        data: json!({}),
        refs: vec![],
        note: String::new(),
        should_fail: false,
    }
}

fn ex() -> RoleModel {
    RoleModel { source: ModelSource::CodexExec, model: None, effort: None }
}

fn jd() -> RoleModel {
    RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None }
}

struct World {
    st: AppState,
    node: Uuid,
    devices: Vec<Uuid>,
}

/// 一个在线节点、`n` 台空闲设备，以及普通、受阻和需要账号的用例。
async fn world(n_devices: usize) -> World {
    let st = test_state(Config { node_token: "t".into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let node = seed_node(&st, n_devices, "A").await;
    let devices = sqlx::query_scalar("select id from devices where node_id = $1 order by serial").bind(node).fetch_all(st.pool()).await.unwrap();
    let blocked: BTreeMap<String, String> = [("C".to_string(), "房主不能下麦".to_string())].into();
    let mut account_case = nc("ACCOUNT", 2, "需要账号");
    account_case.data = json!({"account": "{{account.username}}"});
    import(st.pool(), "p", &[nc("A", 2, "步骤A"), nc("B", 3, "步骤B"), nc("C", 1, "步骤C"), account_case], &blocked).await.unwrap();
    World { st, node, devices }
}

async fn seed_node(st: &AppState, n_devices: usize, tag: &str) -> Uuid {
    let node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)").bind(node).bind(CAPS).execute(st.pool()).await.unwrap();
    for i in 0..n_devices {
        sqlx::query("insert into devices (node_id, serial, platform, status) values ($1, $2, 'android', 'idle')").bind(node).bind(format!("{tag}-DEV{i}")).execute(st.pool()).await.unwrap();
    }
    node
}

async fn task(w: &World, codes: &[&str]) -> runs::TaskCreated {
    runs::create_task(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: codes.iter().map(|c| c.to_string()).collect(), executor: ex(), judge: jd() }).await.unwrap()
}

async fn status_of(st: &AppState, run: Uuid) -> String {
    sqlx::query_scalar("select status from runs where id = $1").bind(run).fetch_one(st.pool()).await.unwrap()
}

async fn device_status(st: &AppState, dev: Uuid) -> String {
    sqlx::query_scalar("select status from devices where id = $1").bind(dev).fetch_one(st.pool()).await.unwrap()
}

fn step(run: Uuid, idx: u32) -> Step {
    Step { run_id: run, idx, at_ms: 0, tool: "tap".into(), args: json!({"element_id": 2}), result: json!({"ok": true}), file_ref: Some(format!("shots/{idx:02}.png")) }
}

fn item(idx: u32, v: Verdict) -> JudgeItem {
    JudgeItem { idx, verdict: v, evidence: format!("证据{idx}"), file_refs: vec![] }
}

fn judge(run: Uuid, fin: FinalResult, items: Vec<JudgeItem>) -> JudgeDone {
    JudgeDone { run_id: run, judge_status: JudgeStatus::Completed, final_result: fin, items }
}

/// 把一个 run 推进到 running。
async fn to_running(w: &World, run: Uuid) {
    runs::mark_started(w.st.pool(), w.node, run).await.unwrap();
}

async fn to_judging(w: &World, run: Uuid) {
    to_running(w, run).await;
    runs::executor_done(w.st.pool(), w.node, &ExecutorDone { run_id: run, result: Verdict::Passed, reason: "r".into(), cost: None }).await.unwrap();
}

// ---------------------------------------------------------------- 创建任务

#[tokio::test]
async fn 创建任务_每个用例一个排队的run并快照用例() {
    let w = world(1).await;
    let t = task(&w, &["ACCOUNT", "B"]).await;
    assert_eq!(t.run_ids.len(), 2);
    let r = sqlx::query("select status, case_version, case_spec from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(r.get::<String, _>("status"), "queued");
    assert_eq!(r.get::<i32, _>("case_version"), 1);
    let spec: serde_json::Value = r.get("case_spec");
    assert_eq!(spec["code"], "ACCOUNT");
    assert_eq!(spec["expected"].as_array().unwrap().len(), 2);
    assert_eq!(spec["data"]["account"], "{{account.username}}", "快照里只有占位符");
}

#[tokio::test]
async fn 用例之后被改了_run仍用创建时的快照() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let blocked = BTreeMap::new();
    import(w.st.pool(), "p", &[nc("A", 2, "被改过的步骤A"), nc("B", 3, "步骤B"), nc("C", 1, "步骤C")], &blocked).await.unwrap();
    let ver: i32 = sqlx::query_scalar("select version from test_cases where code = 'A'").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(ver, 2, "前提：用例确实升到了第 2 版");
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!(c.run_id, t.run_ids[0]);
    assert_eq!((c.case.steps[0].as_str(), c.case_version), ("步骤A", 1), "下发的必须是创建任务时的用例，不是现在的");
}

#[tokio::test]
async fn 创建任务_校验失败要整体不创建() {
    let w = world(1).await;
    let mk = |codes: &[&str]| NewTask { project: "p".into(), node_id: w.node, case_codes: codes.iter().map(|c| c.to_string()).collect(), executor: ex(), judge: jd() };
    // 受阻的用例不能创建，并说明原因；其它合法的也不能先建出来
    let e = runs::create_task(w.st.pool(), &mk(&["A", "C"])).await.unwrap_err().to_string();
    assert!(e.contains("C 是 blocked") && e.contains("房主不能下麦"), "{e}");
    let e = runs::create_task(w.st.pool(), &mk(&["A", "不存在"])).await.unwrap_err().to_string();
    assert!(e.contains("不存在 不存在"), "{e}");
    let (t, r): (i64, i64) = (
        sqlx::query_scalar("select count(*) from tasks").fetch_one(w.st.pool()).await.unwrap(),
        sqlx::query_scalar("select count(*) from runs").fetch_one(w.st.pool()).await.unwrap(),
    );
    assert_eq!((t, r), (0, 0), "有一个用例有问题，整个任务都不能创建");
    assert!(runs::create_task(w.st.pool(), &mk(&[])).await.is_err(), "空任务不行");
    let mut bad = mk(&["A"]);
    bad.node_id = Uuid::new_v4();
    assert!(runs::create_task(w.st.pool(), &bad).await.unwrap_err().to_string().contains("没有这个节点"));
    let mut bad = mk(&["A"]);
    bad.project = "没有".into();
    assert!(runs::create_task(w.st.pool(), &bad).await.is_err());
}

#[tokio::test]
async fn 创建任务_节点没有所选的模型来源就拒绝() {
    let w = world(1).await;
    // 这个节点只有 codex_exec
    let only_codex = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2::jsonb, true)")
        .bind(only_codex)
        .bind(r#"{"node_version":"t","os":"macos","model_sources":[{"source":"codex_exec","vision":true}]}"#)
        .execute(w.st.pool())
        .await
        .unwrap();
    let e = runs::create_task(w.st.pool(), &NewTask { project: "p".into(), node_id: only_codex, case_codes: vec!["A".into()], executor: ex(), judge: jd() }).await.unwrap_err().to_string();
    assert!(e.contains("没有可用的 claude_cli") && e.contains("Judge"), "额度归属：不能用节点上没有的模型来源：{e}");
}

// ---------------------------------------------------------------- 领任务与设备锁

#[tokio::test]
async fn 领任务_最早排队的run配一台空闲设备() {
    let w = world(2).await;
    let t = task(&w, &["A", "B"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!(c.run_id, t.run_ids[0], "先来先领");
    assert_eq!(c.device_serial, "A-DEV0");
    assert_eq!((c.executor, c.judge), (ex(), jd()));
    assert_eq!(status_of(&w.st, c.run_id).await, "assigned");
    assert_eq!(device_status(&w.st, c.device_id).await, "busy");
}

#[tokio::test]
async fn 设备锁_一台设备同一时刻只给一个run() {
    let w = world(1).await;
    let t = task(&w, &["A", "B"]).await;
    let first = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "只有一台设备，第二个 run 必须继续排队");
    assert_eq!(status_of(&w.st, t.run_ids[1]).await, "queued");
    // 第一个结束、设备释放后，第二个才能领到
    to_judging(&w, first.run_id).await;
    runs::judge_done(w.st.pool(), w.node, &judge(first.run_id, FinalResult::Passed, vec![item(1, Verdict::Passed), item(2, Verdict::Passed)])).await.unwrap();
    let second = runs::claim_next(w.st.pool(), w.node).await.unwrap().expect("设备释放后应能领到");
    assert_eq!((second.run_id, second.device_id), (t.run_ids[1], first.device_id));
}

#[tokio::test]
async fn 没有排队任务_没有空闲设备_节点或设备不在线都领不到() {
    let w = world(1).await;
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "没有排队的 run");
    task(&w, &["A"]).await;
    sqlx::query("update devices set status = 'offline'").execute(w.st.pool()).await.unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "设备离线不能分配");
    sqlx::query("update devices set status = 'idle'").execute(w.st.pool()).await.unwrap();
    sqlx::query("update nodes set online = false").execute(w.st.pool()).await.unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "节点离线不能分配");
}

#[tokio::test]
async fn run只属于创建时绑定的节点_别的节点领不到() {
    let w = world(1).await;
    let other = seed_node(&w.st, 2, "B").await;
    task(&w, &["A"]).await;
    assert!(runs::claim_next(w.st.pool(), other).await.unwrap().is_none(), "不存在跨节点调度：别的节点有空闲设备也领不走这个 run");
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_some());
}

#[tokio::test]
async fn 并发领取_不会重复拿到同一个run或同一台设备() {
    let w = world(3).await;
    task(&w, &["A", "B"]).await;
    task(&w, &["A", "B"]).await;
    task(&w, &["A"]).await; // 一共 5 个 run，3 台设备
    let mut hs = vec![];
    for _ in 0..10 {
        let (pool, node) = (w.st.pool().clone(), w.node);
        hs.push(tokio::spawn(async move { runs::claim_next(&pool, node).await }));
    }
    let mut got = vec![];
    for h in hs {
        if let Some(c) = h.await.unwrap().unwrap() {
            got.push((c.run_id, c.device_id));
        }
    }
    assert_eq!(got.len(), 3, "3 台设备，应恰好领走 3 个：{got:?}");
    assert_eq!(got.iter().map(|g| g.0).collect::<HashSet<_>>().len(), 3, "同一个 run 被领了两次");
    assert_eq!(got.iter().map(|g| g.1).collect::<HashSet<_>>().len(), 3, "同一台设备被分给了两个 run");
}

#[tokio::test]
async fn 数据库兜底_同一台设备不能有两个活跃run() {
    let w = world(1).await;
    let t = task(&w, &["A", "B"]).await;
    let dev = w.devices[0];
    sqlx::query("update runs set status = 'assigned', device_id = $2 where id = $1").bind(t.run_ids[0]).bind(dev).execute(w.st.pool()).await.unwrap();
    let r = sqlx::query("update runs set status = 'assigned', device_id = $2 where id = $1").bind(t.run_ids[1]).bind(dev).execute(w.st.pool()).await;
    assert!(r.is_err(), "即使应用层有 bug，库里的唯一索引也要拦住设备被占两次");
}

#[tokio::test]
async fn 同一任务里的run严格按创建顺序领取() {
    // 回归：同一任务的 run 在同一个事务里创建，queued_at 相同；排序曾退化成按随机 id，顺序是乱的
    let w = world(1).await;
    let t = task(&w, &["A"; 20]).await;
    let mut got = vec![];
    for _ in 0..20 {
        let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
        got.push(c.run_id);
        to_judging(&w, c.run_id).await;
        runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, vec![item(1, Verdict::Passed), item(2, Verdict::Passed)])).await.unwrap();
    }
    assert_eq!(got, t.run_ids, "必须按创建顺序领取");
    let views: Vec<Uuid> = runs::task_runs(w.st.pool(), t.task_id).await.unwrap().iter().map(|v| v.run_id).collect();
    assert_eq!(views, t.run_ids, "查询出来的顺序也要一致");
}

// ---------------------------------------------------------------- 完整流程

#[tokio::test]
async fn 完整流程_领取_开始_步骤_执行完_判定_完成() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    runs::mark_started(w.st.pool(), w.node, c.run_id).await.unwrap();
    assert_eq!(status_of(&w.st, c.run_id).await, "running");
    for i in 1..=3 {
        assert!(runs::record_step(w.st.pool(), w.node, &step(c.run_id, i)).await.unwrap(), "第 {i} 步应是新记录");
    }
    assert!(!runs::record_step(w.st.pool(), w.node, &step(c.run_id, 2)).await.unwrap(), "重传同一步必须幂等，不重复写");
    let n: i64 = sqlx::query_scalar("select count(*) from run_steps where run_id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 3);

    runs::executor_done(w.st.pool(), w.node, &ExecutorDone { run_id: c.run_id, result: Verdict::Passed, reason: "我觉得过了".into(), cost: Some(json!({"cost_usd": 0.2})) }).await.unwrap();
    assert_eq!(status_of(&w.st, c.run_id).await, "judging");
    let r = sqlx::query("select executor_result, final_result from runs where id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<Option<String>, _>("executor_result"), r.get::<Option<String>, _>("final_result")), (Some("passed".into()), None), "执行员自评只是记录，此时还没有最终结果");

    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, vec![item(1, Verdict::Passed), item(2, Verdict::Passed)])).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Passed, false));
    let r = sqlx::query("select status, final_result, judge_status, finished_at is not null as fin from runs where id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<String, _>("status"), r.get::<Option<String>, _>("final_result"), r.get::<bool, _>("fin")), ("finished".into(), Some("passed".into()), true));
    assert_eq!(device_status(&w.st, c.device_id).await, "idle", "结束后设备要释放");
    let items = sqlx::query("select expected_text, evidence from judge_items where run_id = $1 order by idx").bind(c.run_id).fetch_all(w.st.pool()).await.unwrap();
    assert_eq!(items[1].get::<String, _>("expected_text"), "预期2", "预期原文取自 run 创建时的快照");
    let views = runs::task_runs(w.st.pool(), t.task_id).await.unwrap();
    assert_eq!((views[0].code.as_str(), views[0].final_result.as_deref()), ("A", Some("passed")));
}

// ---------------------------------------------------------------- 判定不能强行通过

#[tokio::test]
async fn 节点说通过但逐条判定里有failed_服务端改成failed() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, t.run_ids[0]).await;
    let out = runs::judge_done(w.st.pool(), w.node, &judge(t.run_ids[0], FinalResult::Passed, vec![item(1, Verdict::Passed), item(2, Verdict::Failed)])).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Failed, true), "最终结果由服务端按逐条判定重算，不信节点报的");
    let f: String = sqlx::query_scalar("select final_result from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(f, "failed");
}

#[tokio::test]
async fn 漏判了预期_不能被当成通过() {
    let w = world(1).await;
    let t = task(&w, &["B"]).await; // 3 条预期
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, t.run_ids[0]).await;
    let out = runs::judge_done(w.st.pool(), w.node, &judge(t.run_ids[0], FinalResult::Passed, vec![item(1, Verdict::Passed), item(2, Verdict::Passed)])).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Uncertain, true), "3 条预期只判了 2 条，必须是待确认");
}

#[tokio::test]
async fn judge整体待确认时逐项全通过也不会升级() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let run = t.run_ids[0];
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, run).await;
    let report = judge(run, FinalResult::Uncertain, vec![item(1, Verdict::Passed), item(2, Verdict::Passed)]);
    let out = runs::judge_done(w.st.pool(), w.node, &report).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Uncertain, false));
    let stored: String = sqlx::query_scalar("select final_result from runs where id = $1")
        .bind(run).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(stored, "uncertain");
}

#[tokio::test]
async fn judge没跑成_最终结果一律待确认_不用执行员自评顶替() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, t.run_ids[0]).await; // 执行员自评是 passed
    let j = JudgeDone { run_id: t.run_ids[0], judge_status: JudgeStatus::Failed, final_result: FinalResult::Passed, items: vec![item(1, Verdict::Passed), item(2, Verdict::Passed)] };
    let out = runs::judge_done(w.st.pool(), w.node, &j).await.unwrap();
    assert_eq!(out.final_result, FinalResult::Uncertain, "Judge 没跑成时，执行员的 passed 绝不能变成最终结果");
    let n: i64 = sqlx::query_scalar("select count(*) from judge_items where run_id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 0, "Judge 失败时节点附带的逐条判定不可信，不入库");
}

#[tokio::test]
async fn 数据库兜底_judge失败却写passed会被约束拒绝() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, t.run_ids[0]).await;
    let r = sqlx::query("update runs set status = 'finished', judge_status = 'failed', final_result = 'passed' where id = $1").bind(t.run_ids[0]).execute(w.st.pool()).await;
    assert!(r.is_err(), "就算应用代码有 bug，库里的 check 也要拦住「Judge 失败却通过」");
    let r = sqlx::query("update runs set status = 'finished', final_result = 'blocked' where id = $1").bind(t.run_ids[0]).execute(w.st.pool()).await;
    assert!(r.is_err(), "finished 的最终结果不能是 blocked");
}

#[tokio::test]
async fn judge完成时最终结果不能是blocked() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    runs::claim_next(w.st.pool(), w.node).await.unwrap();
    to_judging(&w, t.run_ids[0]).await;
    assert!(matches!(runs::judge_done(w.st.pool(), w.node, &judge(t.run_ids[0], FinalResult::Blocked, vec![])).await, Err(RunError::Invalid(_))));
}

// ---------------------------------------------------------------- 非法流转与跨节点操作

#[tokio::test]
async fn 非法状态流转明确报错_不静默忽略() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let run = t.run_ids[0];
    let p = w.st.pool();
    assert!(matches!(runs::mark_started(p, w.node, run).await, Err(RunError::Illegal { ref from, .. }) if from == "queued"), "没分配设备不能开始");
    assert!(matches!(runs::record_step(p, w.node, &step(run, 1)).await, Err(RunError::Illegal { .. })), "排队中不能记步骤");
    runs::claim_next(p, w.node).await.unwrap();
    assert!(matches!(runs::executor_done(p, w.node, &ExecutorDone { run_id: run, result: Verdict::Passed, reason: String::new(), cost: None }).await, Err(RunError::Illegal { .. })), "没开始就不能提交执行结果");
    assert!(matches!(runs::judge_done(p, w.node, &judge(run, FinalResult::Passed, vec![])).await, Err(RunError::Illegal { .. })), "没经过执行就不能直接判定完成");
    to_running(&w, run).await;
    assert!(matches!(runs::mark_started(p, w.node, run).await, Err(RunError::Illegal { .. })), "不能重复开始");
    assert!(matches!(runs::record_step(p, w.node, &step(run, 0)).await, Err(RunError::Invalid(_))), "步骤编号从 1 开始");
    assert!(matches!(runs::mark_started(p, w.node, Uuid::new_v4()).await, Err(RunError::NotFound(_))));
}

#[tokio::test]
async fn 节点只能操作自己的run() {
    let w = world(1).await;
    let other = seed_node(&w.st, 1, "B").await;
    let t = task(&w, &["A"]).await;
    let run = t.run_ids[0];
    let p = w.st.pool();
    runs::claim_next(p, w.node).await.unwrap();
    for e in [
        runs::mark_started(p, other, run).await.unwrap_err(),
        runs::record_step(p, other, &step(run, 1)).await.unwrap_err(),
        runs::executor_done(p, other, &ExecutorDone { run_id: run, result: Verdict::Passed, reason: String::new(), cost: None }).await.unwrap_err(),
        runs::judge_done(p, other, &judge(run, FinalResult::Passed, vec![])).await.unwrap_err(),
        runs::run_blocked(p, other, &RunBlocked { run_id: run, reason: BlockedReason::Other, detail: String::new() }).await.unwrap_err(),
    ] {
        assert!(e.to_string().contains("不属于这个节点"), "别的节点不能碰这个 run：{e}");
    }
    assert_eq!(status_of(&w.st, run).await, "assigned", "越权操作一次都没生效");
}

// ---------------------------------------------------------------- 受阻与取消

#[tokio::test]
async fn 受阻_记原因_最终结果blocked_释放设备() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, t.run_ids[0]).await;
    runs::run_blocked(w.st.pool(), w.node, &RunBlocked { run_id: t.run_ids[0], reason: BlockedReason::Link, detail: "reconnect_loop".into() }).await.unwrap();
    let r = sqlx::query("select status, final_result, blocked_reason, executor_reason from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<String, _>("status"), r.get::<Option<String>, _>("final_result"), r.get::<Option<String>, _>("blocked_reason"), r.get::<Option<String>, _>("executor_reason")),
               ("blocked".into(), Some("blocked".into()), Some("link".into()), Some("reconnect_loop".into())));
    assert_eq!(device_status(&w.st, c.device_id).await, "idle");
    assert!(matches!(runs::run_blocked(w.st.pool(), w.node, &RunBlocked { run_id: t.run_ids[0], reason: BlockedReason::Other, detail: String::new() }).await, Err(RunError::Illegal { .. })), "终态不能再改");
}

#[tokio::test]
async fn 数据库兜底_blocked必须有原因() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let r = sqlx::query("update runs set status = 'blocked', final_result = 'blocked' where id = $1").bind(t.run_ids[0]).execute(w.st.pool()).await;
    assert!(r.is_err(), "统计完成率要区分链路 / 环境故障，blocked 没有原因不行");
}

#[tokio::test]
async fn 取消_排队中和执行中都行_终态不行() {
    let w = world(2).await;
    let t = task(&w, &["A", "B"]).await;
    runs::cancel(w.st.pool(), t.run_ids[1]).await.unwrap();
    assert_eq!(status_of(&w.st, t.run_ids[1]).await, "cancelled");
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!(c.run_id, t.run_ids[0], "已取消的不会被领走");
    to_running(&w, c.run_id).await;
    runs::cancel(w.st.pool(), c.run_id).await.unwrap();
    assert_eq!(device_status(&w.st, c.device_id).await, "idle", "取消执行中的 run 要释放设备");
    assert!(matches!(runs::cancel(w.st.pool(), c.run_id).await, Err(RunError::Illegal { .. })), "已取消的不能再取消");
}

// ---------------------------------------------------------------- 节点掉线（DB 层，WebSocket 联动见 runs_ws.rs）

async fn lose(w: &World) {
    let mut tx = w.st.pool().begin().await.unwrap();
    runs::node_lost(&mut tx, w.node).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn 掉线_执行中转node_lost而不是失败_仍占着设备() {
    let w = world(1).await;
    let t = task(&w, &["A", "B"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, c.run_id).await;
    lose(&w).await;
    assert_eq!(status_of(&w.st, c.run_id).await, "node_lost", "掉线不能直接判失败");
    // 宽限期内别的 run 不能把这台设备领走
    sqlx::query("update devices set status = 'idle'").execute(w.st.pool()).await.unwrap(); // 模拟节点重连后设备同步成空闲
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none(), "node_lost 的 run 还占着设备，别的排队任务不能领走它");
    assert_eq!(status_of(&w.st, t.run_ids[1]).await, "queued");
}

#[tokio::test]
async fn 掉线_还没开始的退回排队可以重新领() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    lose(&w).await;
    assert_eq!(status_of(&w.st, c.run_id).await, "queued", "没开始执行的 run 没有副作用，掉线后退回排队");
    let dev: Option<Uuid> = sqlx::query_scalar("select device_id from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(dev, None);
    sqlx::query("update devices set status = 'idle'").execute(w.st.pool()).await.unwrap();
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_some());
}

#[tokio::test]
async fn 重连_宽限期内继续_已有执行员结果的回到判定阶段() {
    let w = world(2).await;
    let t = task(&w, &["A", "B"]).await;
    let a = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, a.run_id).await; // 还在执行
    let b = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_judging(&w, b.run_id).await; // 已在判定
    lose(&w).await;
    let mut tx = w.st.pool().begin().await.unwrap();
    let r = runs::decide_resume(&mut tx, w.node, &[t.run_ids[0], t.run_ids[1]], Duration::from_secs(120)).await.unwrap();
    tx.commit().await.unwrap();
    assert!(r.iter().all(|x| x.decision == protocol::ResumeDecision::Continue), "{r:?}");
    assert_eq!((status_of(&w.st, a.run_id).await.as_str(), status_of(&w.st, b.run_id).await.as_str()), ("running", "judging"));
}

#[tokio::test]
async fn 重连_超过宽限期放弃() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, c.run_id).await;
    lose(&w).await;
    sqlx::query("update runs set node_lost_at = now() - interval '10 minutes'").execute(w.st.pool()).await.unwrap();
    let mut tx = w.st.pool().begin().await.unwrap();
    let r = runs::decide_resume(&mut tx, w.node, &[t.run_ids[0]], Duration::from_secs(120)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(r[0].decision, protocol::ResumeDecision::Abort, "掉线太久了，不能再续跑");
    assert_eq!(status_of(&w.st, c.run_id).await, "node_lost", "放弃的决定由 expire 统一收尾，这里不改状态");
}

#[tokio::test]
async fn 重连_节点没有的run判受阻() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, c.run_id).await;
    // 服务端以为在跑，节点重启后却一个都没报
    let mut tx = w.st.pool().begin().await.unwrap();
    runs::decide_resume(&mut tx, w.node, &[], Duration::from_secs(120)).await.unwrap();
    tx.commit().await.unwrap();
    let r = sqlx::query("select status, blocked_reason, final_result from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<String, _>("status"), r.get::<Option<String>, _>("blocked_reason"), r.get::<Option<String>, _>("final_result")), ("blocked".into(), Some("node_lost".into()), Some("blocked".into())));
    assert_eq!(device_status(&w.st, c.device_id).await, "idle", "孤儿 run 判受阻后要释放设备");
}

#[tokio::test]
async fn 宽限期过期统一收尾() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_running(&w, c.run_id).await;
    lose(&w).await;
    assert_eq!(runs::expire_node_lost(w.st.pool(), Duration::from_secs(120)).await.unwrap(), 0, "还在宽限期内不处理");
    sqlx::query("update runs set node_lost_at = now() - interval '10 minutes'").execute(w.st.pool()).await.unwrap();
    assert_eq!(runs::expire_node_lost(w.st.pool(), Duration::from_secs(120)).await.unwrap(), 1);
    let r = sqlx::query("select status, blocked_reason from runs where id = $1").bind(t.run_ids[0]).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<String, _>("status"), r.get::<Option<String>, _>("blocked_reason")), ("blocked".into(), Some("node_lost".into())));
}

#[tokio::test]
async fn 创建任务_条件不足的用例直接跳过_不派发_不占设备_也不算通过() {
    let w = world(1).await;
    let mut h5 = nc("H5", 1, "打开链接");
    h5.platforms = vec!["h5".into()];
    let mut web = nc("WEB", 1, "打开网页");
    web.platforms = vec!["browser".into()];
    let mut ios = nc("IOS", 1, "iOS 步骤");
    ios.platforms = vec!["ios".into()];
    import(w.st.pool(), "p", &[h5, web, ios], &BTreeMap::new()).await.unwrap();

    let t = task(&w, &["A", "H5", "WEB", "IOS"]).await;
    assert_eq!(t.run_ids.len(), 4, "跳过的也要有记录，页面才能显示原因");
    let skipped: Vec<(&str, &str)> = t.skipped.iter().map(|s| (s.code.as_str(), s.reason.as_str())).collect();
    assert_eq!(skipped.iter().map(|(c, _)| *c).collect::<Vec<_>>(), ["H5", "WEB", "IOS"]);
    assert!(skipped[0].1.contains("H5 执行器"), "{skipped:?}");
    assert!(skipped[1].1.contains("浏览器执行器"), "{skipped:?}");
    assert!(skipped[2].1.contains("iOS 设备"), "节点上只有安卓设备：{skipped:?}");

    // 正常的 app 用例照常排队；跳过的是已结束的 blocked，原因 skipped，没有设备，不算通过
    assert_eq!(status_of(&w.st, t.run_ids[0]).await, "queued");
    for run in &t.run_ids[1..] {
        let r = sqlx::query("select status, final_result, blocked_reason, device_id, finished_at, executor_reason from runs where id = $1").bind(run).fetch_one(w.st.pool()).await.unwrap();
        assert_eq!(r.get::<String, _>("status"), "blocked");
        assert_eq!(r.get::<Option<String>, _>("final_result").as_deref(), Some("blocked"), "不是 passed");
        assert_eq!(r.get::<Option<String>, _>("blocked_reason").as_deref(), Some("skipped"));
        assert!(r.get::<Option<Uuid>, _>("device_id").is_none() && r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at").is_some());
        assert!(r.get::<Option<String>, _>("executor_reason").is_some_and(|x| x.contains("已跳过")));
    }
    // 节点只会领到正常的那一条；之后没有可领的了，设备也没被跳过的占着
    let claimed = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!(claimed.run_id, t.run_ids[0]);
    assert!(runs::claim_next(w.st.pool(), w.node).await.unwrap().is_none());
}

#[tokio::test]
async fn 创建任务_节点上一台设备都没登记过_app_用例跳过() {
    let w = world(0).await;
    let t = task(&w, &["A", "B"]).await;
    assert_eq!(t.skipped.len(), 2);
    assert!(t.skipped[0].reason.contains("还没有登记过这样的设备"), "{:?}", t.skipped);
    // 设备只是离线 / 忙着：登记过就不算缺，照常排队
    let w = world(1).await;
    sqlx::query("update devices set status = 'offline' where node_id = $1").bind(w.node).execute(w.st.pool()).await.unwrap();
    let t = task(&w, &["A"]).await;
    assert!(t.skipped.is_empty());
    assert_eq!(status_of(&w.st, t.run_ids[0]).await, "queued");
}


// ---------------------------------------------------------------- 多个判定员同时评审

fn opinion(seq: u32, status: JudgeStatus, fin: FinalResult, items: Vec<JudgeItem>) -> JudgeOpinion {
    JudgeOpinion { seq, judge: RoleModel { source: ModelSource::ClaudeCli, model: Some(format!("m{seq}")), effort: None }, judge_status: status, final_result: fin, items }
}

fn all_passed() -> Vec<JudgeItem> {
    vec![item(1, Verdict::Passed), item(2, Verdict::Passed)]
}

/// 建一个带「其他判定员」的任务，领取并推进到判定阶段。
async fn judging_with_extras(w: &World, extras: &[RoleModel]) -> (Uuid, runs::Claimed) {
    let t = runs::create_task_with_judges(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["A".into()], executor: ex(), judge: jd() }, extras).await.unwrap();
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    to_judging(w, c.run_id).await;
    (t.task_id, c)
}

#[tokio::test]
async fn 其他判定员随任务保存_领取时一起下发() {
    let w = world(1).await;
    // 其他判定员的来源节点上没有：和主判定员、执行员一样，建任务时就拒绝
    let missing = vec![RoleModel { source: ModelSource::Agy, model: Some("gemini-x".into()), effort: None }];
    let err = runs::create_task_with_judges(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["A".into()], executor: ex(), judge: jd() }, &missing).await.unwrap_err();
    assert!(err.to_string().contains("其他判定员") && err.to_string().contains("agy"), "{err}");
    let extras = vec![RoleModel { source: ModelSource::ClaudeCli, model: Some("opus".into()), effort: None }, jd()];
    let t = runs::create_task_with_judges(w.st.pool(), &NewTask { project: "p".into(), node_id: w.node, case_codes: vec!["A".into()], executor: ex(), judge: jd() }, &extras).await.unwrap();
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!(c.task_id, t.task_id);
    assert_eq!(c.extra_judges, extras);
    // 不带其他判定员：和以前一样，空
    let t2 = task(&w, &["B"]).await;
    runs::cancel(w.st.pool(), c.run_id).await.unwrap();
    let c2 = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    assert_eq!((c2.task_id, c2.extra_judges.len()), (t2.task_id, 0));
}

#[tokio::test]
async fn 多个判定员_结论一致才采用_各人的结果单独记录() {
    let w = world(1).await;
    let (_, c) = judging_with_extras(&w, &[jd(), jd()]).await;
    let opinions = JudgeOpinions { run_id: c.run_id, opinions: vec![
        opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
        opinion(1, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
        opinion(2, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
    ] };
    runs::record_judge_opinions(w.st.pool(), w.node, &opinions).await.unwrap();
    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, all_passed())).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Passed, false));
    let n: i64 = sqlx::query_scalar("select count(*) from judge_opinions where run_id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 3, "三个判定员各一条");
    let models: Vec<Option<String>> = sqlx::query_scalar("select model from judge_opinions where run_id = $1 order by seq").bind(c.run_id).fetch_all(w.st.pool()).await.unwrap();
    assert_eq!(models, [Some("m0".into()), Some("m1".into()), Some("m2".into())]);
    let items: i64 = sqlx::query_scalar("select count(*) from judge_items where run_id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(items, 2, "逐条判定表里放主判定员的");
}

#[tokio::test]
async fn 多个判定员_有分歧或有人没跑成_一律待人工确认_不能强行通过() {
    // 1 个说通过、1 个说失败（逐条里有 failed）：分歧 → 待确认
    let w = world(1).await;
    let (_, c) = judging_with_extras(&w, &[jd()]).await;
    let failing = vec![item(1, Verdict::Passed), item(2, Verdict::Failed)];
    runs::record_judge_opinions(w.st.pool(), w.node, &JudgeOpinions { run_id: c.run_id, opinions: vec![
        opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
        opinion(1, JudgeStatus::Completed, FinalResult::Failed, failing),
    ] }).await.unwrap();
    // 节点（被篡改或有 bug）硬说通过：以服务端汇总为准
    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, all_passed())).await.unwrap();
    assert_eq!((out.final_result, out.overridden), (FinalResult::Uncertain, true));
    assert_eq!(status_of(&w.st, c.run_id).await, "finished");
    let fin: Option<String> = sqlx::query_scalar("select final_result from runs where id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(fin.as_deref(), Some("uncertain"));

    // 其他人都说通过，但有一个没跑成：待确认，评审状态是 failed
    let w = world(1).await;
    let (_, c) = judging_with_extras(&w, &[jd(), jd()]).await;
    runs::record_judge_opinions(w.st.pool(), w.node, &JudgeOpinions { run_id: c.run_id, opinions: vec![
        opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
        opinion(1, JudgeStatus::Failed, FinalResult::Uncertain, vec![]),
        opinion(2, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
    ] }).await.unwrap();
    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, all_passed())).await.unwrap();
    assert_eq!(out.final_result, FinalResult::Uncertain);
    let r = sqlx::query("select judge_status, final_result from runs where id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!((r.get::<Option<String>, _>("judge_status").as_deref(), r.get::<Option<String>, _>("final_result").as_deref()), (Some("failed"), Some("uncertain")));

    // 一个判定员说通过，但他逐条里漏判了一条预期：服务端复算成待确认，汇总也不是通过
    let w = world(1).await;
    let (_, c) = judging_with_extras(&w, &[jd()]).await;
    runs::record_judge_opinions(w.st.pool(), w.node, &JudgeOpinions { run_id: c.run_id, opinions: vec![
        opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()),
        opinion(1, JudgeStatus::Completed, FinalResult::Passed, vec![item(1, Verdict::Passed)]),
    ] }).await.unwrap();
    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, all_passed())).await.unwrap();
    assert_eq!(out.final_result, FinalResult::Uncertain, "漏判不能当通过");
}

#[tokio::test]
async fn 多个判定员的结果_只能由这个节点在判定阶段提交_序号不能重复() {
    let w = world(1).await;
    let t = task(&w, &["A"]).await;
    let c = runs::claim_next(w.st.pool(), w.node).await.unwrap().unwrap();
    let good = JudgeOpinions { run_id: c.run_id, opinions: vec![opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()), opinion(1, JudgeStatus::Completed, FinalResult::Passed, all_passed())] };
    // 还没到判定阶段：不行
    assert!(matches!(runs::record_judge_opinions(w.st.pool(), w.node, &good).await, Err(RunError::Illegal { .. })));
    to_judging(&w, c.run_id).await;
    // 别的节点：不行
    let other = seed_node(&w.st, 1, "B").await;
    assert!(matches!(runs::record_judge_opinions(w.st.pool(), other, &good).await, Err(RunError::Invalid(_))));
    // 空的、序号重复的、完成却是 blocked 的：不行
    assert!(matches!(runs::record_judge_opinions(w.st.pool(), w.node, &JudgeOpinions { run_id: c.run_id, opinions: vec![] }).await, Err(RunError::Invalid(_))));
    let dup = JudgeOpinions { run_id: c.run_id, opinions: vec![opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed()), opinion(0, JudgeStatus::Completed, FinalResult::Passed, all_passed())] };
    assert!(matches!(runs::record_judge_opinions(w.st.pool(), w.node, &dup).await, Err(RunError::Invalid(_))));
    let blocked = JudgeOpinions { run_id: c.run_id, opinions: vec![opinion(0, JudgeStatus::Completed, FinalResult::Blocked, all_passed())] };
    assert!(matches!(runs::record_judge_opinions(w.st.pool(), w.node, &blocked).await, Err(RunError::Invalid(_))));
    // 重传同样的结果：幂等
    runs::record_judge_opinions(w.st.pool(), w.node, &good).await.unwrap();
    runs::record_judge_opinions(w.st.pool(), w.node, &good).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from judge_opinions where run_id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 2);
    let _ = t;
}

#[tokio::test]
async fn 只有一个判定员_行为和以前完全一样_不产生多判定员记录() {
    let w = world(1).await;
    let (_, c) = judging_with_extras(&w, &[]).await;
    let out = runs::judge_done(w.st.pool(), w.node, &judge(c.run_id, FinalResult::Passed, all_passed())).await.unwrap();
    assert_eq!(out.final_result, FinalResult::Passed);
    let n: i64 = sqlx::query_scalar("select count(*) from judge_opinions where run_id = $1").bind(c.run_id).fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 0);
}
