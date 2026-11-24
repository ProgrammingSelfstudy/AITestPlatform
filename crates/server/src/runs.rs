//! 任务与 run 的状态机、派发、结果落库。
//!
//! 几条硬规则（都有测试，其中几条同时由数据库约束兜底）：
//! - **run 创建时就绑定到一个节点**，只有这个节点的连接能领走它、推进它：不存在跨节点调度（谁发起谁付额度）；
//! - 同一台设备同一时刻只有一个活跃 run（库里的部分唯一索引）；节点掉线宽限期内的 run 也占着设备；
//! - **最终结果由服务端按逐条判定重新算**，不信节点报上来的：有 failed 就是 failed，没有 failed 但有 uncertain 或数量对不上就是 uncertain，
//!   全部 passed 才是 passed；Judge 没跑成就是 uncertain，**绝不用执行员的自评顶替**；
//! - 节点只能操作自己的 run；非法状态流转明确报错，不静默忽略。

use chrono::{DateTime, Utc};
use protocol::{
    BlockedReason, CaseSpec, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeOpinions, JudgeStatus, ModelSource, ResumeDecision, ResumeItem, RoleModel, RunBlocked, Step, Verdict,
};
use serde_json::Value;
use sqlx::{Acquire, PgConnection, PgPool, Row};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

use crate::accounts::{self, AccountPool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Assigned,
    Running,
    Judging,
    Finished,
    Blocked,
    Cancelled,
    NodeLost,
}

impl RunStatus {
    pub(crate) const ALL: [Self; 8] = [
        Self::Queued, Self::Assigned, Self::Running, Self::Judging,
        Self::Finished, Self::Blocked, Self::Cancelled, Self::NodeLost,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Assigned => "assigned",
            RunStatus::Running => "running",
            RunStatus::Judging => "judging",
            RunStatus::Finished => "finished",
            RunStatus::Blocked => "blocked",
            RunStatus::Cancelled => "cancelled",
            RunStatus::NodeLost => "node_lost",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter()
            .find(|x| x.as_str() == s)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Blocked | Self::Cancelled)
    }

    /// 占着设备的状态（与库里的部分唯一索引一致）。
    pub fn holds_device(self) -> bool {
        matches!(self, Self::Assigned | Self::Running | Self::Judging | Self::NodeLost)
    }
}

/// 状态流转是否合法。
pub fn can_transition(from: RunStatus, to: RunStatus) -> bool {
    use RunStatus::*;
    matches!(
        (from, to),
        (Queued, Assigned | Cancelled)
            | (Assigned, Running | Queued | Blocked | Cancelled)
            | (Running, Judging | NodeLost | Blocked | Cancelled)
            | (Judging, Finished | NodeLost | Blocked | Cancelled)
            | (NodeLost, Running | Judging | Blocked | Cancelled)
    )
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("没有这个 run：{0}")]
    NotFound(Uuid),
    #[error("run {run} 当前是 {from}，不能{action}")]
    Illegal { run: Uuid, from: String, action: &'static str },
    #[error("{0}")]
    Invalid(String),
    #[error("数据库错误：{0}")]
    Db(#[from] sqlx::Error),
}

type Res<T> = Result<T, RunError>;

fn src_str(s: ModelSource) -> &'static str {
    match s {
        ModelSource::ClaudeCli => "claude_cli",
        ModelSource::CodexExec => "codex_exec",
        ModelSource::Agy => "agy",
        ModelSource::DeepSeek => "deepseek",
    }
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Passed => "passed",
        Verdict::Failed => "failed",
        Verdict::Uncertain => "uncertain",
    }
}

fn reason_str(r: BlockedReason) -> &'static str {
    match r {
        BlockedReason::Setup => "setup",
        BlockedReason::Link => "link",
        BlockedReason::NodeLost => "node_lost",
        BlockedReason::Stuck => "stuck",
        BlockedReason::Quota => "quota",
        BlockedReason::Other => "other",
    }
}

// ---------------------------------------------------------------- 创建任务

#[derive(Debug, Clone)]
pub struct NewTask {
    pub project: String,
    /// 绑定的节点：任务只能在这个节点上、用这个节点的模型来源执行
    pub node_id: Uuid,
    pub case_codes: Vec<String>,
    pub executor: RoleModel,
    pub judge: RoleModel,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskCreated {
    pub task_id: Uuid,
    /// 包含已跳过的 run（它们一创建就是结束状态）
    pub run_ids: Vec<Uuid>,
    /// 条件不足、没有派发的用例和原因
    pub skipped: Vec<SkippedCase>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SkippedCase {
    pub code: String,
    pub reason: String,
}

/// 条件不足就不跑：返回跳过的原因（中文，直接给人看）。`platforms` 是用例的运行平台，`node_platforms` 是这个节点上登记过的设备平台。
/// - 运行环境是 h5 / 浏览器：平台还没有这两种执行器，一律跳过；
/// - 运行环境是 app：节点上一台对应平台的设备都没登记过，跳过（设备只是暂时离线、忙着的不算，会排队等）。
pub(crate) fn skip_reason(platforms: &[String], node_platforms: &[String]) -> Option<String> {
    match crate::modules::assets::run_env_of(platforms) {
        "browser" => return Some("这条用例要在浏览器里跑，平台还没有浏览器执行器，已跳过".into()),
        "h5" => return Some("这条用例是 H5 页面，平台还没有 H5 执行器，已跳过".into()),
        _ => {}
    }
    let wanted: Vec<String> = platforms.iter().map(|p| p.to_lowercase()).filter(|p| p == "android" || p == "ios").collect();
    let usable = |p: &str| node_platforms.iter().any(|n| n.eq_ignore_ascii_case(p));
    let ok = if wanted.is_empty() { !node_platforms.is_empty() } else { wanted.iter().any(|p| usable(p)) };
    if ok {
        return None;
    }
    let need = if wanted.is_empty() { "手机".to_string() } else { wanted.iter().map(|p| if p == "ios" { "iOS" } else { "Android" }).collect::<Vec<_>>().join(" / ") };
    Some(format!("这条用例要在 {need} 设备上跑，这个节点上还没有登记过这样的设备，已跳过"))
}

/// 创建任务：每个用例一个 run（排队），并把用例内容快照进 run。
/// 校验：节点存在且有所选的模型来源、项目存在、用例都存在且不是受阻状态。
pub async fn create_task(pool: &PgPool, t: &NewTask) -> Res<TaskCreated> {
    create_task_with_judges(pool, t, &[]).await
}

/// 同 `create_task`，另外带「其他判定员」：它们和主判定员同时评审同一份证据，和任务在同一个事务里写入（不会出现被领取了才补上的空档）。
pub async fn create_task_with_judges(pool: &PgPool, t: &NewTask, extra_judges: &[RoleModel]) -> Res<TaskCreated> {
    if t.case_codes.is_empty() {
        return Err(RunError::Invalid("至少要选一个用例".into()));
    }
    let mut tx = pool.begin().await?;
    let caps: Option<Value> = sqlx::query_scalar("select caps from nodes where id = $1").bind(t.node_id).fetch_optional(&mut *tx).await?;
    let Some(caps) = caps else { return Err(RunError::Invalid(format!("没有这个节点：{}", t.node_id))) };
    let have: Vec<String> = caps["model_sources"].as_array().map(|a| a.iter().filter_map(|m| m["source"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    for (role, r) in [("执行员", &t.executor), ("Judge", &t.judge)].into_iter().chain(extra_judges.iter().map(|j| ("其他判定员", j))) {
        if !have.iter().any(|s| s == src_str(r.source)) {
            return Err(RunError::Invalid(format!("该节点没有可用的 {}，不能用作{role}（节点现有：{}）", src_str(r.source), have.join("、"))));
        }
    }
    let project_id: Option<Uuid> = sqlx::query_scalar("select id from projects where name = $1").bind(&t.project).fetch_optional(&mut *tx).await?;
    let Some(project_id) = project_id else { return Err(RunError::Invalid(format!("没有这个项目：{}", t.project))) };

    let task_id: Uuid = sqlx::query_scalar("insert into tasks (project_id, node_id, executor, judge, extra_judges) values ($1,$2,$3,$4,$5) returning id")
        .bind(project_id)
        .bind(t.node_id)
        .bind(serde_json::to_value(&t.executor).expect("可序列化"))
        .bind(serde_json::to_value(&t.judge).expect("可序列化"))
        .bind(serde_json::to_value(extra_judges).expect("可序列化"))
        .fetch_one(&mut *tx)
        .await?;
    let mut run_ids = vec![];
    let mut problems = vec![];
    let mut skipped = vec![];
    // 这个节点上登记过的设备平台（不管现在在线还是忙）：判断用例要的设备有没有
    let node_platforms: Vec<String> = sqlx::query_scalar("select distinct platform from devices where node_id = $1").bind(t.node_id).fetch_all(&mut *tx).await?;
    for code in &t.case_codes {
        let Some(r) = sqlx::query("select id, status, blocked_reason, version, review_status, platforms from test_cases where project_id = $1 and code = $2").bind(project_id).bind(code).fetch_optional(&mut *tx).await? else {
            problems.push(format!("{code} 不存在"));
            continue;
        };
        let status: String = r.get("status");
        if status != "active" {
            let why: Option<String> = r.get("blocked_reason");
            problems.push(format!("{code} 是 {status} 状态{}", why.map(|w| format!("（{w}）")).unwrap_or_default()));
            continue;
        }
        // 生成的用例要先经评审员复核和人工确认（approved）才能执行；导入的老用例默认 approved
        let review: String = r.get("review_status");
        if review != "approved" {
            problems.push(format!("{code} 还没有通过复核和人工确认（当前：{review}）"));
            continue;
        }
        let case_id: Uuid = r.get("id");
        let (spec, version) = crate::cases::spec_by_id(&mut tx, case_id).await?;
        // 条件不足：不派发，记一条已结束的「已跳过」run，原因写清楚
        if let Some(reason) = skip_reason(&r.get::<Vec<String>, _>("platforms"), &node_platforms) {
            let id: Uuid = sqlx::query_scalar(
                "insert into runs (task_id, case_id, case_version, case_spec, node_id, status, final_result, blocked_reason, executor_reason, finished_at)
                 values ($1,$2,$3,$4,$5,'blocked','blocked','skipped',$6,now()) returning id",
            )
            .bind(task_id)
            .bind(case_id)
            .bind(version)
            .bind(serde_json::to_value(&spec).expect("可序列化"))
            .bind(t.node_id)
            .bind(&reason)
            .fetch_one(&mut *tx)
            .await?;
            run_ids.push(id);
            skipped.push(SkippedCase { code: code.clone(), reason });
            continue;
        }
        let id: Uuid = sqlx::query_scalar("insert into runs (task_id, case_id, case_version, case_spec, node_id) values ($1,$2,$3,$4,$5) returning id")
            .bind(task_id)
            .bind(case_id)
            .bind(version)
            .bind(serde_json::to_value(&spec).expect("可序列化"))
            .bind(t.node_id)
            .fetch_one(&mut *tx)
            .await?;
        run_ids.push(id);
    }
    if !problems.is_empty() {
        // 任何一个用例有问题就整个任务不创建（事务回滚）
        return Err(RunError::Invalid(format!("不能创建任务：{}", problems.join("；"))));
    }
    tx.commit().await?;
    Ok(TaskCreated { task_id, run_ids, skipped })
}

/// 用例要用测试账号，但平台没有配置账号池：这些 run 永远领不到账号，与其一直排队不如直接跳过并说清楚。
/// 只在页面 / 管理员建任务的入口调用（命令行和测试里建任务的行为不变）。返回新跳过的用例。
pub async fn skip_account_runs(pool: &PgPool, task_id: Uuid) -> Res<Vec<SkippedCase>> {
    if AccountPool::configured().is_some() {
        return Ok(vec![]);
    }
    let reason = "这条用例要用测试账号，但平台还没有配置账号池，已跳过";
    let rows = sqlx::query(
        "update runs set status = 'blocked', final_result = 'blocked', blocked_reason = 'skipped', executor_reason = $2, finished_at = now()
         where task_id = $1 and status = 'queued' and strpos(case_spec::text, '{{account.') > 0
         returning (case_spec->>'code') as code",
    )
    .bind(task_id)
    .bind(reason)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| SkippedCase { code: r.get::<Option<String>, _>("code").unwrap_or_default(), reason: reason.into() }).collect())
}

// ---------------------------------------------------------------- 领任务

#[derive(Debug, Clone)]
pub struct Claimed {
    pub run_id: Uuid,
    pub task_id: Uuid,
    pub device_id: Uuid,
    pub device_serial: String,
    pub case: CaseSpec,
    pub case_version: i32,
    pub executor: RoleModel,
    pub judge: RoleModel,
    /// 其他判定员（和主判定员同时评审同一份证据）；空就是只有一个判定员
    pub extra_judges: Vec<RoleModel>,
    pub lease_id: Option<Uuid>,
    pub needs_cleanup: bool,
}

/// 给节点领取最早可派发的 run，设备和所需的账号一起分配。
pub async fn claim_next(pool: &PgPool, node_id: Uuid) -> Res<Option<Claimed>> {
    let accounts = AccountPool::configured();
    claim_next_with_accounts(pool, node_id, accounts.as_ref()).await
}

pub(crate) async fn claim_next_with_accounts(pool: &PgPool, node_id: Uuid, accounts: Option<&AccountPool>) -> Res<Option<Claimed>> {
    let mut after_seq = i64::MIN;
    'queued: loop {
        let mut tx = pool.begin().await?;
        let run = sqlx::query(
            "select r.id, r.task_id, r.case_version, r.case_spec, r.seq, t.executor, t.judge, t.extra_judges,
                    strpos(r.case_spec::text, '{{account.') > 0 as needs_account
             from runs r join tasks t on t.id = r.task_id
             where r.node_id = $1 and r.status = 'queued' and r.seq > $2
               and ($3 or strpos(r.case_spec::text, '{{account.') = 0)
             order by r.seq for update of r skip locked limit 1",
        )
        .bind(node_id).bind(after_seq).bind(accounts.is_some()).fetch_optional(&mut *tx).await?;
        let Some(run) = run else { return Ok(None) };
        let run_id: Uuid = run.get("id");
        let accounts = accounts.filter(|_| run.get("needs_account"));
        let mut after_serial: Option<String> = None;
        let (device_id, device_serial, lease) = loop {
            // 每次只锁一台设备。借不到账号就回滚到保存点，并发领取可继续使用这台设备。
            let mut attempt = tx.begin().await?;
            let device: Option<(Uuid, String)> = sqlx::query_as(
                "select d.id, d.serial from devices d join nodes n on n.id = d.node_id
                 where d.node_id = $1 and n.online and d.status = 'idle'
                   and ($2::text is null or d.serial > $2)
                   and not exists (select 1 from runs x where x.device_id = d.id and x.status in ('assigned','running','judging','node_lost'))
                   and not exists (select 1 from regression_requests q where q.node_id = d.node_id and q.serial = d.serial and q.status = 'running')
                 order by d.serial for update of d skip locked limit 1",
            )
            .bind(node_id)
            .bind(after_serial.as_deref())
            .fetch_optional(&mut *attempt)
            .await?;
            let Some((device_id, serial)) = device else {
                attempt.rollback().await?;
                if after_serial.is_none() {
                    return Ok(None);
                }
                // 这些设备都借不到账号，跳过这个 run 并继续检查队列。
                after_seq = run.get("seq");
                tx.rollback().await?;
                continue 'queued;
            };
            let lease = if let Some(accounts) = accounts {
                let lease = accounts::acquire_with_settings(
                    &mut attempt,
                    run_id,
                    device_id,
                    accounts::DEFAULT_TTL,
                    &accounts.settings,
                )
                .await?;
                let Some(lease) = lease else {
                    after_serial = Some(serial);
                    attempt.rollback().await?;
                    continue;
                };
                Some(lease)
            } else {
                None
            };
            attempt.commit().await?;
            break (device_id, serial, lease);
        };
        let lease_id = lease.as_ref().map(|l| l.id);
        sqlx::query("update runs set status = 'assigned', device_id = $2, lease_id = $3, assigned_at = now() where id = $1")
            .bind(run_id).bind(device_id).bind(lease_id).execute(&mut *tx).await?;
        sqlx::query("update devices set status = 'busy' where id = $1").bind(device_id).execute(&mut *tx).await?;
        let dec = |v: Value| serde_json::from_value::<RoleModel>(v).map_err(|e| sqlx::Error::Decode(Box::new(e)));
        let claimed = Claimed {
            run_id,
            task_id: run.get("task_id"),
            device_id,
            device_serial,
            case: serde_json::from_value(run.get::<Value, _>("case_spec")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            case_version: run.get("case_version"),
            executor: dec(run.get("executor"))?,
            judge: dec(run.get("judge"))?,
            extra_judges: serde_json::from_value(run.get::<Value, _>("extra_judges")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            lease_id,
            needs_cleanup: lease.is_some_and(|l| l.needs_cleanup),
        };
        tx.commit().await?;
        return Ok(Some(claimed));
    }
}

// ---------------------------------------------------------------- 状态推进（节点上报）

async fn current_status(conn: &mut PgConnection, run_id: Uuid) -> Res<(RunStatus, Uuid)> {
    let r = sqlx::query("select status, node_id from runs where id = $1").bind(run_id).fetch_optional(&mut *conn).await?;
    let Some(r) = r else { return Err(RunError::NotFound(run_id)) };
    Ok((RunStatus::parse(&r.get::<String, _>("status")).expect("库里有 check"), r.get("node_id")))
}

/// 更新没命中任何行时，说清楚为什么：不存在、不是这个节点的、还是当前状态不允许。
async fn why_not(conn: &mut PgConnection, run_id: Uuid, node_id: Uuid, action: &'static str) -> RunError {
    match current_status(conn, run_id).await {
        Err(e) => e,
        Ok((_, owner)) if owner != node_id => RunError::Invalid(format!("run {run_id} 不属于这个节点，不能{action}")),
        Ok((s, _)) => RunError::Illegal { run: run_id, from: s.as_str().into(), action },
    }
}

/// 把 run 占着的设备还回去：节点在线就是空闲，不在线就是离线。
async fn release_device(conn: &mut PgConnection, run_id: Uuid) -> Res<()> {
    sqlx::query(
        "update devices d set status = case when n.online then 'idle' else 'offline' end
         from nodes n, runs r where r.id = $1 and d.id = r.device_id and n.id = d.node_id",
    )
    .bind(run_id)
    .execute(conn)
    .await?;
    Ok(())
}

async fn release_resources(conn: &mut PgConnection, run_id: Uuid, dirty: bool) -> Res<()> {
    release_device(conn, run_id).await?;
    let lease_id: Option<Uuid> = sqlx::query_scalar("select lease_id from runs where id = $1")
        .bind(run_id).fetch_one(&mut *conn).await?;
    if let Some(id) = lease_id {
        accounts::release(conn, id, dirty).await?;
    }
    Ok(())
}

/// 节点开始执行：assigned → running。
pub async fn mark_started(pool: &PgPool, node_id: Uuid, run_id: Uuid) -> Res<()> {
    mark_started_with_receipt(pool, node_id, run_id, None).await
}

pub(crate) async fn mark_started_with_receipt(pool: &PgPool, node_id: Uuid, run_id: Uuid, receipt: Option<Uuid>) -> Res<()> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "update runs set status = 'running', started_at = coalesce(started_at, now()), start_receipt = $3
         where id = $1 and node_id = $2 and (status = 'assigned' or (status = 'running' and start_receipt = $3))",
    )
        .bind(run_id)
        .bind(node_id)
        .bind(receipt)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(why_not(&mut tx, run_id, node_id, "开始执行").await);
    }
    tx.commit().await?;
    Ok(())
}

/// 记录一步。`(run_id, idx)` 幂等：重传返回 `false`，不重复写。
pub async fn record_step(pool: &PgPool, node_id: Uuid, s: &Step) -> Res<bool> {
    if s.idx == 0 {
        return Err(RunError::Invalid("步骤编号从 1 开始".into()));
    }
    let mut tx = pool.begin().await?;
    let (status, owner) = current_status(&mut tx, s.run_id).await?;
    if owner != node_id {
        return Err(RunError::Invalid(format!("run {} 不属于这个节点，不能记录步骤", s.run_id)));
    }
    if !matches!(status, RunStatus::Running | RunStatus::Judging) {
        return Err(RunError::Illegal { run: s.run_id, from: status.as_str().into(), action: "记录步骤" });
    }
    let n = sqlx::query("insert into run_steps (run_id, idx, tool, args, result, file_ref, at_ms) values ($1,$2,$3,$4,$5,$6,$7) on conflict (run_id, idx) do nothing")
        .bind(s.run_id)
        .bind(s.idx as i32)
        .bind(&s.tool)
        .bind(&s.args)
        .bind(&s.result)
        .bind(&s.file_ref)
        .bind((s.at_ms > 0).then_some(s.at_ms))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(n > 0)
}

/// 执行员结束：running → judging。执行员自评只是记录，不是最终结果。
pub async fn executor_done(pool: &PgPool, node_id: Uuid, d: &ExecutorDone) -> Res<()> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("update runs set status = 'judging', executor_result = $3, executor_reason = $4, cost = $5 where id = $1 and node_id = $2 and status = 'running'")
        .bind(d.run_id)
        .bind(node_id)
        .bind(verdict_str(d.result))
        .bind(&d.reason)
        .bind(&d.cost)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(why_not(&mut tx, d.run_id, node_id, "提交执行结果").await);
    }
    tx.commit().await?;
    Ok(())
}

/// 按逐条判定算出最终结果。`expected_count` 是用例的预期条数。
///
/// 有 failed 就是 failed；没有 failed 但有 uncertain、或判定条数和预期条数对不上、或有重复 / 越界的编号，就是 uncertain；
/// 全部 passed 且恰好覆盖每一条预期才是 passed。**漏判的预期不能被当成通过。**
pub fn compute_final(items: &[protocol::JudgeItem], expected_count: usize) -> FinalResult {
    if items.iter().any(|i| i.verdict == Verdict::Failed) {
        return FinalResult::Failed;
    }
    let mut seen = std::collections::BTreeSet::new();
    let well_formed = items.iter().all(|i| i.idx >= 1 && (i.idx as usize) <= expected_count && seen.insert(i.idx));
    if well_formed && items.len() == expected_count && items.iter().all(|i| i.verdict == Verdict::Passed) {
        FinalResult::Passed
    } else {
        FinalResult::Uncertain
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JudgeOutcome {
    pub final_result: FinalResult,
    /// 节点报上来的最终结果和服务端按逐条判定算出来的不一致，已以服务端为准
    pub overridden: bool,
}

/// 一个判定员的最终结果，由服务端按逐条判定复算；节点还校验了原始条数和模型的整体结论，这两项在逐条结果里复算不出来，
/// 所以节点说「待确认」而逐条全通过时，保留「待确认」。没跑成（非 Completed）一律待确认。
fn checked_final(status: JudgeStatus, node_final: FinalResult, items: &[JudgeItem], expected_len: usize) -> FinalResult {
    if status != JudgeStatus::Completed {
        return FinalResult::Uncertain;
    }
    match (compute_final(items, expected_len), node_final) {
        (FinalResult::Passed, FinalResult::Uncertain) => FinalResult::Uncertain,
        (computed, _) => computed,
    }
}

fn judge_status_str(s: JudgeStatus) -> &'static str {
    match s {
        JudgeStatus::Completed => "completed",
        JudgeStatus::Failed => "failed",
        JudgeStatus::NotRun => "not_run",
    }
}

fn final_str(f: FinalResult) -> &'static str {
    match f {
        FinalResult::Passed => "passed",
        FinalResult::Failed => "failed",
        _ => "uncertain",
    }
}

/// 多个判定员时，节点在 `JudgeDone` 之前上报每个人各自的评审结果：存下来（每人一条，单独记录，不合并）。
/// 逐条判定由服务端复算后再存；最终结果由 `judge_done` 按「一致才采用」汇总。
pub async fn record_judge_opinions(pool: &PgPool, node_id: Uuid, o: &JudgeOpinions) -> Res<()> {
    if o.opinions.is_empty() {
        return Err(RunError::Invalid("判定员评审结果不能是空的".into()));
    }
    let mut seen = std::collections::HashSet::new();
    if !o.opinions.iter().all(|x| seen.insert(x.seq)) {
        return Err(RunError::Invalid("判定员评审结果的序号重复了".into()));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select case_spec, status, node_id from runs where id = $1 for update").bind(o.run_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Err(RunError::NotFound(o.run_id)) };
    if row.get::<Uuid, _>("node_id") != node_id {
        return Err(RunError::Invalid(format!("run {} 不属于这个节点，不能提交判定", o.run_id)));
    }
    if row.get::<String, _>("status") != "judging" {
        return Err(RunError::Illegal { run: o.run_id, from: row.get("status"), action: "提交判定" });
    }
    let spec: CaseSpec = serde_json::from_value(row.get::<Value, _>("case_spec")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    for x in &o.opinions {
        if x.judge_status == JudgeStatus::Completed && x.final_result == FinalResult::Blocked {
            return Err(RunError::Invalid("Judge 完成时最终结果不能是 blocked".into()));
        }
        let fin = checked_final(x.judge_status, x.final_result, &x.items, spec.expected.len());
        // 没跑成的判定员不存逐条结果（和单判定员一样）
        let items = if x.judge_status == JudgeStatus::Completed { serde_json::to_value(&x.items).expect("可序列化") } else { serde_json::json!([]) };
        sqlx::query(
            "insert into judge_opinions (run_id, seq, source, model, effort, judge_status, final_result, items) values ($1,$2,$3,$4,$5,$6,$7,$8)
             on conflict (run_id, seq) do update set source = excluded.source, model = excluded.model, effort = excluded.effort,
               judge_status = excluded.judge_status, final_result = excluded.final_result, items = excluded.items",
        )
        .bind(o.run_id)
        .bind(x.seq as i32)
        .bind(src_str(x.judge.source))
        .bind(&x.judge.model)
        .bind(&x.judge.effort)
        .bind(judge_status_str(x.judge_status))
        .bind(final_str(fin))
        .bind(items)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Judge 完成：judging → finished。逐项判定由服务端复算，并保留 Judge 对整体的待确认结论。
/// 有多个判定员（先上报过 `JudgeOpinions`）时，最终结果由服务端汇总：所有人都完成且结论一致才采用，否则待人工确认。
pub async fn judge_done(pool: &PgPool, node_id: Uuid, j: &JudgeDone) -> Res<JudgeOutcome> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select case_spec, status, node_id from runs where id = $1 for update").bind(j.run_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Err(RunError::NotFound(j.run_id)) };
    if row.get::<Uuid, _>("node_id") != node_id {
        return Err(RunError::Invalid(format!("run {} 不属于这个节点，不能提交判定", j.run_id)));
    }
    if row.get::<String, _>("status") != "judging" {
        return Err(RunError::Illegal { run: j.run_id, from: row.get("status"), action: "提交判定" });
    }
    let spec: CaseSpec = serde_json::from_value(row.get::<Value, _>("case_spec")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    let stored = sqlx::query("select seq, judge_status, final_result, items from judge_opinions where run_id = $1 order by seq")
        .bind(j.run_id)
        .fetch_all(&mut *tx)
        .await?;
    let (js, computed, items): (JudgeStatus, FinalResult, Vec<JudgeItem>) = if stored.len() > 1 {
        // 多个判定员：各人的结果已经在 judge_opinions 里（服务端复算过），这里只做汇总；逐条判定表里放主判定员（seq 0）的
        let each: Vec<(JudgeStatus, FinalResult)> = stored
            .iter()
            .map(|r| {
                let status = match r.get::<String, _>("judge_status").as_str() {
                    "completed" => JudgeStatus::Completed,
                    "failed" => JudgeStatus::Failed,
                    _ => JudgeStatus::NotRun,
                };
                let fin = match r.get::<String, _>("final_result").as_str() {
                    "passed" => FinalResult::Passed,
                    "failed" => FinalResult::Failed,
                    _ => FinalResult::Uncertain,
                };
                (status, fin)
            })
            .collect();
        let (status, fin) = protocol::consensus(&each);
        let primary = stored.iter().find(|r| r.get::<i32, _>("seq") == 0);
        let items = primary
            .filter(|r| r.get::<String, _>("judge_status") == "completed")
            .and_then(|r| serde_json::from_value::<Vec<JudgeItem>>(r.get::<Value, _>("items")).ok())
            .unwrap_or_default();
        (status, fin, items)
    } else {
        let completed = j.judge_status == JudgeStatus::Completed;
        if completed && j.final_result == FinalResult::Blocked {
            return Err(RunError::Invalid("Judge 完成时最终结果不能是 blocked".into()));
        }
        (j.judge_status, checked_final(j.judge_status, j.final_result, &j.items, spec.expected.len()), if completed { j.items.clone() } else { vec![] })
    };
    let overridden = computed != j.final_result;
    for i in &items {
        let text = spec.expected.iter().find(|e| e.idx == i.idx).map(|e| e.text.clone()).unwrap_or_default();
        sqlx::query("insert into judge_items (run_id, idx, expected_text, verdict, evidence, file_refs) values ($1,$2,$3,$4,$5,$6) on conflict (run_id, idx) do update set verdict = excluded.verdict, evidence = excluded.evidence, file_refs = excluded.file_refs")
            .bind(j.run_id)
            .bind(i.idx as i32)
            .bind(text)
            .bind(verdict_str(i.verdict))
            .bind(&i.evidence)
            .bind(&i.file_refs)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("update runs set status = 'finished', judge_status = $2, final_result = $3, finished_at = now() where id = $1")
        .bind(j.run_id)
        .bind(judge_status_str(js))
        .bind(final_str(computed))
        .execute(&mut *tx)
        .await?;
    release_resources(&mut tx, j.run_id, false).await?;
    tx.commit().await?;
    Ok(JudgeOutcome { final_result: computed, overridden })
}

/// 节点上报受阻：任何活跃状态 → blocked。
pub async fn run_blocked(pool: &PgPool, node_id: Uuid, b: &RunBlocked) -> Res<()> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "update runs set status = 'blocked', final_result = 'blocked', blocked_reason = $3, executor_reason = coalesce(executor_reason, $4), finished_at = now()
         where id = $1 and node_id = $2 and status in ('assigned','running','judging','node_lost')",
    )
    .bind(b.run_id)
    .bind(node_id)
    .bind(reason_str(b.reason))
    .bind(&b.detail)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(why_not(&mut tx, b.run_id, node_id, "标记受阻").await);
    }
    release_resources(&mut tx, b.run_id, true).await?;
    tx.commit().await?;
    Ok(())
}

/// 取消。排队中的直接取消；已经分配 / 执行中的也标取消并释放设备（通知节点停下来由派发层做）。
pub async fn cancel(pool: &PgPool, run_id: Uuid) -> Res<()> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("update runs set status = 'cancelled', finished_at = now() where id = $1 and status in ('queued','assigned','running','judging','node_lost')")
        .bind(run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        let (s, _) = current_status(&mut tx, run_id).await?;
        return Err(RunError::Illegal { run: run_id, from: s.as_str().into(), action: "取消" });
    }
    release_resources(&mut tx, run_id, true).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- 节点掉线与恢复

async fn requeue_assigned(conn: &mut PgConnection, node_id: Option<Uuid>) -> Res<()> {
    let leases: Vec<Option<Uuid>> = sqlx::query_scalar(
        "with assigned as (
           select id, lease_id from runs where status = 'assigned' and ($1::uuid is null or node_id = $1) for update
         )
         update runs r set status = 'queued', device_id = null, assigned_at = null, lease_id = null
         from assigned a where r.id = a.id returning a.lease_id",
    )
    .bind(node_id).fetch_all(&mut *conn).await?;
    for id in leases.into_iter().flatten() {
        accounts::release_requeued(conn, id).await?;
    }
    Ok(())
}

/// 节点掉线（在节点离线的同一个事务里调用）：
/// - 还没开始执行的（assigned）退回排队并释放租约；
/// - 正在执行 / 判定的转 `node_lost`，**不直接判失败**，宽限期内重连还能续跑；这期间它仍然占着设备。
pub async fn node_lost(conn: &mut PgConnection, node_id: Uuid) -> Res<()> {
    requeue_assigned(conn, Some(node_id)).await?;
    sqlx::query("update runs set status = 'node_lost', node_lost_at = now() where node_id = $1 and status in ('running','judging')").bind(node_id).execute(&mut *conn).await?;
    Ok(())
}

/// 服务进程启动时：所有节点的执行中 run 一并转 `node_lost`（还没开始的退回排队）。
/// 服务重启（或崩溃）没有走「节点离线」那条路径，不处理的话节点重连后会被当成「没有这个 run」而被放弃。
pub async fn all_nodes_lost(conn: &mut PgConnection) -> Res<()> {
    requeue_assigned(conn, None).await?;
    sqlx::query("update runs set status = 'node_lost', node_lost_at = now() where status in ('running','judging')").execute(&mut *conn).await?;
    Ok(())
}

/// 节点重连时，对它声称仍在跑的 run 逐个决定继续还是放弃，并处理「服务端以为在跑、节点却没有」的孤儿：
/// - 在宽限期内的 node_lost run → 继续（已有执行员结果的回到 judging，否则回到 running）；
/// - 其它（超过宽限期、已被取消、不是它的）→ 放弃；
/// - 这个节点上服务端以为还在活跃、但节点没报的 run（节点重启丢了本地进度）→ 受阻（node_lost）。
pub async fn decide_resume(conn: &mut PgConnection, node_id: Uuid, running: &[Uuid], grace: Duration) -> Res<Vec<ResumeItem>> {
    let mut out = vec![];
    for run_id in running {
        let n = sqlx::query(
            "update runs set status = case when executor_result is not null then 'judging' else 'running' end, node_lost_at = null
             where id = $1 and node_id = $2 and status = 'node_lost' and node_lost_at > now() - make_interval(secs => $3)",
        )
        .bind(run_id)
        .bind(node_id)
        .bind(grace.as_secs_f64())
        .execute(&mut *conn)
        .await?
        .rows_affected();
        out.push(ResumeItem { run_id: *run_id, decision: if n > 0 { ResumeDecision::Continue } else { ResumeDecision::Abort } });
    }
    let reported: Vec<Uuid> = running.to_vec();
    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "update runs set status = 'blocked', final_result = 'blocked', blocked_reason = 'node_lost', finished_at = now(),
                executor_reason = coalesce(executor_reason, '节点重连后没有这个 run 的本地进度（节点可能重启过）')
         where node_id = $1 and status in ('running','judging','node_lost') and not (id = any($2)) returning id",
    )
    .bind(node_id)
    .bind(&reported)
    .fetch_all(&mut *conn)
    .await?;
    for id in orphans {
        release_resources(conn, id, true).await?;
    }
    Ok(out)
}

/// 宽限期过了还没回来的 `node_lost` run → 受阻（node_lost）。返回处理了几个。定期调用。
pub async fn expire_node_lost(pool: &PgPool, grace: Duration) -> Res<u64> {
    let mut tx = pool.begin().await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "update runs set status = 'blocked', final_result = 'blocked', blocked_reason = 'node_lost', finished_at = now(),
                executor_reason = coalesce(executor_reason, '节点掉线超过宽限期没有回来')
         where status = 'node_lost' and node_lost_at <= now() - make_interval(secs => $1) returning id",
    )
    .bind(grace.as_secs_f64())
    .fetch_all(&mut *tx)
    .await?;
    for id in &ids {
        release_resources(&mut tx, *id, true).await?;
    }
    tx.commit().await?;
    Ok(ids.len() as u64)
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RunView {
    pub run_id: Uuid,
    pub code: String,
    pub status: String,
    pub executor_result: Option<String>,
    pub judge_status: Option<String>,
    pub final_result: Option<String>,
    pub blocked_reason: Option<String>,
    pub case_version: i32,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub device_serial: Option<String>,
}

pub async fn task_runs(pool: &PgPool, task_id: Uuid) -> Result<Vec<RunView>, sqlx::Error> {
    sqlx::query_as(
        "select r.id as run_id, r.case_spec->>'code' as code, r.status, r.executor_result, r.judge_status,
                r.final_result, r.blocked_reason, r.case_version, r.queued_at, r.started_at, r.finished_at, d.serial as device_serial
         from runs r left join devices d on d.id = r.device_id where r.task_id = $1 order by r.seq",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::JudgeItem;
    use RunStatus::*;

    fn item(idx: u32, v: Verdict) -> JudgeItem {
        JudgeItem { idx, verdict: v, evidence: String::new(), file_refs: vec![] }
    }

    #[test]
    fn 终态不能再流转() {
        for t in [Finished, Blocked, Cancelled] {
            for to in [Queued, Assigned, Running, Judging, Finished, Blocked, Cancelled, NodeLost] {
                assert!(!can_transition(t, to), "{t:?} → {to:?} 不应合法");
            }
        }
    }

    #[test]
    fn 主流程合法_跳步非法() {
        assert!(can_transition(Queued, Assigned) && can_transition(Assigned, Running) && can_transition(Running, Judging) && can_transition(Judging, Finished));
        assert!(!can_transition(Queued, Running), "没分配设备不能直接执行");
        assert!(!can_transition(Running, Finished), "不经过判定不能直接完成：不能用执行员自评当最终结果");
        assert!(!can_transition(Assigned, Judging));
        assert!(!can_transition(Queued, Finished));
    }

    #[test]
    fn 掉线与恢复的流转() {
        assert!(can_transition(Running, NodeLost) && can_transition(Judging, NodeLost));
        assert!(can_transition(NodeLost, Running) && can_transition(NodeLost, Judging) && can_transition(NodeLost, Blocked));
        assert!(!can_transition(NodeLost, Finished), "掉线的 run 必须先恢复再判定");
        assert!(can_transition(Assigned, Queued), "没开始的 run 掉线后退回排队");
    }

    #[test]
    fn 状态字符串互转() {
        for s in [Queued, Assigned, Running, Judging, Finished, Blocked, Cancelled, NodeLost] {
            assert_eq!(RunStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(RunStatus::parse("乱写"), None);
        assert!(NodeLost.holds_device() && !Queued.holds_device() && !Finished.holds_device());
    }

    #[test]
    fn 最终结果_全部通过且覆盖全部预期才是passed() {
        let ok = [item(1, Verdict::Passed), item(2, Verdict::Passed)];
        assert_eq!(compute_final(&ok, 2), FinalResult::Passed);
        assert_eq!(compute_final(&ok[..1], 2), FinalResult::Uncertain, "漏判了一条预期，不能当通过");
        assert_eq!(compute_final(&[], 2), FinalResult::Uncertain, "一条判定都没有更不能通过");
        assert_eq!(compute_final(&ok, 1), FinalResult::Uncertain, "判定条数超过预期条数说明数据不对，不能放行");
    }

    #[test]
    fn 最终结果_有failed就是failed() {
        let v = [item(1, Verdict::Passed), item(2, Verdict::Failed), item(3, Verdict::Uncertain)];
        assert_eq!(compute_final(&v, 3), FinalResult::Failed);
        assert_eq!(compute_final(&v[1..2], 3), FinalResult::Failed, "漏判的情况下只要有 failed 仍然是 failed：失败不能被漏判掩盖");
    }

    #[test]
    fn 最终结果_有uncertain但没failed是uncertain() {
        assert_eq!(compute_final(&[item(1, Verdict::Passed), item(2, Verdict::Uncertain)], 2), FinalResult::Uncertain);
    }

    #[test]
    fn 最终结果_重复或越界编号不能凑数() {
        assert_eq!(compute_final(&[item(1, Verdict::Passed), item(1, Verdict::Passed)], 2), FinalResult::Uncertain, "同一条预期判两次不能凑够数");
        assert_eq!(compute_final(&[item(1, Verdict::Passed), item(5, Verdict::Passed)], 2), FinalResult::Uncertain, "越界编号不能凑数");
        assert_eq!(compute_final(&[item(0, Verdict::Passed), item(1, Verdict::Passed)], 2), FinalResult::Uncertain, "编号 0 不合法");
    }
}
