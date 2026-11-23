//! 页面发起的回归请求：排队、节点领取、回报结果。
//!
//! 设计要点：
//! - 请求只能发到发起人**自己的节点**（规则 9），由路由层校验个人令牌和节点属主；这里只管状态。
//! - 回归占用一台手机。设备和正常派发互斥靠设备行锁：领取回归和派发 run 都先 `for update` 锁设备行，
//!   派发另外排除有「运行中」回归请求的设备，所以不会出现同一台手机同时被两边占用。
//! - 只有已获准的版本能回归：排队时检查一次，领取时再检查一次（排队期间可能已被下线）。
//! - 节点没回报（崩溃、断线）的请求不会永远占着设备：断线、服务重启、超时都会转成失败并说明原因。

use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// 领取后超过这么久还没回报，当作节点没有回报。比节点自己的回归超时（15 分钟）留出余量。
pub const CLAIM_TIMEOUT_MINUTES: i32 = 30;

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("没有这个脚本")]
    ScriptNotFound,
    #[error("只有已获准的脚本版本能回归，这个版本现在是「{0}」")]
    NotApproved(String),
    #[error("这个脚本在该节点上已经有一个排队或运行中的回归请求")]
    Duplicate,
    #[error("没有这个回归请求")]
    NotFound,
    #[error("{0}")]
    Conflict(&'static str),
    #[error("这个回归请求不是你的节点领走的")]
    NotYours,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// 页面显示用：某个脚本最近一次回归请求。
#[derive(Debug, Serialize)]
pub struct RequestView {
    pub id: Uuid,
    pub node_id: Uuid,
    pub status: String,
    pub detail: String,
    pub requested_at: chrono::DateTime<chrono::Utc>,
}

/// 节点领到的回归任务。
#[derive(Debug, Serialize)]
pub struct Claimed {
    pub id: Uuid,
    pub script_id: Uuid,
    pub project: String,
    pub case_code: String,
}

/// 排一个回归请求。脚本必须是已获准；同一脚本在同一节点上不能重复排。
pub async fn create(pool: &PgPool, script_id: Uuid, node_id: Uuid) -> Result<Uuid, RequestError> {
    let mut tx = pool.begin().await?;
    let status: Option<String> = sqlx::query_scalar("select status from scripts where id = $1 for share").bind(script_id).fetch_optional(&mut *tx).await?;
    let status = status.ok_or(RequestError::ScriptNotFound)?;
    if status != "approved" {
        return Err(RequestError::NotApproved(status));
    }
    let inserted = sqlx::query_scalar("insert into regression_requests (script_id, node_id) values ($1, $2) returning id").bind(script_id).bind(node_id).fetch_one(&mut *tx).await;
    match inserted {
        Ok(id) => {
            tx.commit().await?;
            Ok(id)
        }
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RequestError::Duplicate),
        Err(e) => Err(e.into()),
    }
}

/// 请求所属的节点，用来校验发起人是不是节点属主。
pub async fn node_of(pool: &PgPool, id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("select node_id from regression_requests where id = $1").bind(id).fetch_optional(pool).await
}

/// 取消还在排队的请求。已经被节点领走的不能取消（手机已经在跑了）；不存在和状态不对分开报。
pub async fn cancel(pool: &PgPool, id: Uuid) -> Result<(), RequestError> {
    let done = sqlx::query("update regression_requests set status = 'cancelled', finished_at = clock_timestamp() where id = $1 and status = 'queued'").bind(id).execute(pool).await?;
    if done.rows_affected() > 0 {
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar("select exists (select 1 from regression_requests where id = $1)").bind(id).fetch_one(pool).await?;
    Err(if exists { RequestError::Conflict("这个请求已经被节点领走或已经结束，不能取消") } else { RequestError::NotFound })
}

/// 设备上没有活跃的 run、也没有运行中的回归时，把被标成 `busy` 的设备放回 `idle`。
/// 领取回归时设备被标 `busy`；回归结束、作废、超时都要放回来，否则要等下一次心跳才恢复。`node` 为空表示所有节点。
async fn release_devices(conn: &mut sqlx::PgConnection, node: Option<Uuid>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "update devices d set status = 'idle'
         where d.status = 'busy' and ($1::uuid is null or d.node_id = $1)
           and not exists (select 1 from runs r where r.device_id = d.id and r.status in ('assigned','running','judging','node_lost'))
           and not exists (select 1 from regression_requests q where q.node_id = d.node_id and q.serial = d.serial and q.status = 'running')",
    )
    .bind(node)
    .execute(conn)
    .await?;
    Ok(())
}

/// 把运行中的请求转成失败。`node` 为空表示所有节点（服务重启时用）；带节点和设备时只处理那台设备
/// （节点进程重启后第一件事：它自己肯定没有在跑回归，之前领走的都是没回报的）。
/// 节点短暂断线不算：手机上的回归可能仍在跑，设备锁要保留到回报或超时。
pub async fn fail_running(conn: &mut sqlx::PgConnection, node: Option<Uuid>, serial: Option<&str>, reason: &str) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "update regression_requests set status = 'failed', detail = $3, finished_at = clock_timestamp()
         where status = 'running' and ($1::uuid is null or node_id = $1) and ($2::text is null or serial = $2)",
    )
    .bind(node)
    .bind(serial)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    release_devices(conn, node).await?;
    Ok(done.rows_affected())
}

/// 领走后超过时限没回报的请求转成失败并释放设备。放在后台定时任务里也放在领取前：
/// 不依赖节点还在轮询，也不会因为领取时设备「看起来忙」而被回滚。
pub async fn expire_stale(pool: &PgPool, node: Option<Uuid>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let done = sqlx::query(
        "update regression_requests set status = 'failed', detail = '节点领取后超过时限没有回报结果', finished_at = clock_timestamp()
         where status = 'running' and ($1::uuid is null or node_id = $1) and claimed_at < clock_timestamp() - make_interval(mins => $2)",
    )
    .bind(node)
    .bind(CLAIM_TIMEOUT_MINUTES)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() > 0 {
        release_devices(&mut tx, node).await?;
    }
    tx.commit().await?;
    Ok(done.rows_affected())
}

/// 节点领取一个回归请求，带上它要用的那台设备。
///
/// 只有设备在线、空闲、没有活跃 run、也没有别的回归在占用时才领；否则返回 `None`，请求继续排队。
/// 领取时再确认脚本仍是已获准版本，不是的直接标失败并继续找下一个。
pub async fn claim(pool: &PgPool, node_id: Uuid, serial: &str) -> Result<Option<Claimed>, sqlx::Error> {
    // 先单独提交超时清理：它不能和后面「设备忙就返回」放在同一个事务里，否则早退时会被一起回滚
    expire_stale(pool, Some(node_id)).await?;
    let mut tx = pool.begin().await?;

    // 先锁设备行：和派发 run 串行化。只排除离线设备；「是否被占用」按事实（活跃 run、运行中的回归）判断，
    // 不看 devices.status：这一列里的 busy 本身就可能是上一次没回报的回归留下的
    let device: Option<(Uuid, String)> = sqlx::query_as("select id, status from devices where node_id = $1 and serial = $2 for update").bind(node_id).bind(serial).fetch_optional(&mut *tx).await?;
    let Some((device_id, status)) = device else { return Ok(None) };
    if status == "offline" {
        return Ok(None);
    }
    let occupied: bool = sqlx::query_scalar(
        "select exists (select 1 from runs where device_id = $1 and status in ('assigned','running','judging','node_lost'))
             or exists (select 1 from regression_requests where node_id = $2 and serial = $3 and status = 'running')",
    )
    .bind(device_id)
    .bind(node_id)
    .bind(serial)
    .fetch_one(&mut *tx)
    .await?;
    if occupied {
        return Ok(None);
    }

    loop {
        let row = sqlx::query(
            "select q.id, q.script_id, s.status as script_status, p.name as project, c.code
             from regression_requests q
             join scripts s on s.id = q.script_id
             join test_cases c on c.id = s.case_id
             join projects p on p.id = c.project_id
             where q.node_id = $1 and q.status = 'queued'
             order by q.requested_at, q.id for update of q skip locked limit 1",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let id: Uuid = row.try_get("id")?;
        if row.try_get::<String, _>("script_status")? != "approved" {
            sqlx::query("update regression_requests set status = 'failed', detail = '排队期间脚本已不再是获准版本，没有回归', finished_at = clock_timestamp() where id = $1").bind(id).execute(&mut *tx).await?;
            continue;
        }
        sqlx::query("update regression_requests set status = 'running', serial = $2, claimed_at = clock_timestamp() where id = $1").bind(id).bind(serial).execute(&mut *tx).await?;
        // 真正改一下设备行：只锁不改的话，并发里正在选设备的派发查询沿用旧快照，可能仍把这台设备选走
        sqlx::query("update devices set status = 'busy' where id = $1").bind(device_id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(Some(Claimed { id, script_id: row.try_get("script_id")?, project: row.try_get("project")?, case_code: row.try_get("code")? }));
    }
}

/// 节点回报结果。只有这个节点自己领走、仍在运行中的请求能回报。
/// `finished` 表示得到了回归结论（结论记在 script_regressions）；否则是失败，`detail` 写原因。
pub async fn finish(pool: &PgPool, node_id: Uuid, id: Uuid, finished: bool, detail: &str) -> Result<(), RequestError> {
    let status = if finished { "finished" } else { "failed" };
    let mut tx = pool.begin().await?;
    let row: Option<(Uuid, String)> = sqlx::query_as("select node_id, status from regression_requests where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let Some((owner, current)) = row else { return Err(RequestError::NotFound) };
    if owner != node_id {
        return Err(RequestError::NotYours);
    }
    if current != "running" {
        return Err(RequestError::Conflict("这个回归请求已经不是运行中（服务重启、超时作废或已经回报过），结果没有更新"));
    }
    sqlx::query("update regression_requests set status = $2, detail = $3, finished_at = clock_timestamp() where id = $1").bind(id).bind(status).bind(detail).execute(&mut *tx).await?;
    release_devices(&mut tx, Some(node_id)).await?;
    tx.commit().await?;
    Ok(())
}

/// 每个脚本最近一次回归请求，按脚本编号。
pub async fn latest(pool: &PgPool, script_ids: &[Uuid]) -> Result<std::collections::HashMap<Uuid, RequestView>, sqlx::Error> {
    let rows = sqlx::query(
        "select distinct on (script_id) script_id, id, node_id, status, detail, requested_at
         from regression_requests where script_id = any($1) order by script_id, requested_at desc, id",
    )
    .bind(script_ids)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("script_id")?, RequestView { id: row.try_get("id")?, node_id: row.try_get("node_id")?, status: row.try_get("status")?, detail: row.try_get("detail")?, requested_at: row.try_get("requested_at")? })))
        .collect()
}
