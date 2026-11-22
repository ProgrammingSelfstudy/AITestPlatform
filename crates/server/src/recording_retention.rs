use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, ensure};
use protocol::recording::{recording_name, recording_seq};
use sqlx::PgPool;
use tokio::fs;
use uuid::Uuid;

pub const DEFAULT_KEEP_DAYS: i32 = 14;
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct PruneReport {
    pub runs: usize,
    pub segments: i64,
    pub removed_bytes: u64,
    pub remaining_bytes: u64,
}

#[derive(sqlx::FromRow)]
struct Inventory {
    run_id: Uuid,
    size: i64,
    segments: i64,
    eligible: bool,
    expired: bool,
}

pub async fn prune(pool: &PgPool, data: &Path, keep_days: i32, max_bytes: u64, apply: bool) -> anyhow::Result<PruneReport> {
    ensure!((1..=36500).contains(&keep_days) && max_bytes > 0, "留存天数须在 1–36500 之间，容量须大于零");
    let inventory: Vec<Inventory> = sqlx::query_as(
        "select r.id as run_id, sum(greatest(f.size, 0))::bigint as size, count(*) as segments,
                coalesce(r.status in ('finished', 'blocked', 'cancelled') and r.finished_at < now() - interval '10 minutes', false) as eligible,
                coalesce(r.finished_at < now() - make_interval(days => $1), false) as expired
         from runs r join files f on f.run_id = r.id and f.kind = 'recording'
         group by r.id order by r.finished_at nulls last, r.id",
    )
    .bind(keep_days)
    .fetch_all(pool)
    .await
    .context("无法统计已登记的录屏")?;

    let mut report = PruneReport {
        remaining_bytes: inventory.iter().map(|row| row.size as u64).fold(0, u64::saturating_add),
        ..PruneReport::default()
    };
    for row in inventory {
        if !row.eligible || (!row.expired && report.remaining_bytes <= max_bytes) {
            continue;
        }
        let (segments, size) = if apply {
            prune_run(pool, data, row.run_id).await.with_context(|| format!("无法清理 run {} 的录屏", row.run_id))?
        } else {
            (row.segments, row.size as u64)
        };
        report.runs += usize::from(segments > 0);
        report.segments += segments;
        report.removed_bytes = report.removed_bytes.saturating_add(size);
        report.remaining_bytes = report.remaining_bytes.saturating_sub(size);
    }
    Ok(report)
}

async fn prune_run(pool: &PgPool, data: &Path, run_id: Uuid) -> anyhow::Result<(i64, u64)> {
    let mut tx = pool.begin().await?;
    let eligible: Option<bool> = sqlx::query_scalar(
        "select status in ('finished', 'blocked', 'cancelled') and coalesce(finished_at < now() - interval '10 minutes', false)
         from runs where id = $1 for update",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    if eligible != Some(true) {
        return Ok((0, 0));
    }
    let rows: Vec<(i32, String, i64)> = sqlx::query_as("select seq, path, size from files where run_id = $1 and kind = 'recording'")
        .bind(run_id)
        .fetch_all(&mut *tx)
        .await?;
    if rows.is_empty() {
        return Ok((0, 0));
    }

    let dir = crate::files::run_dir(data, run_id, false).await.context("找不到安全的录屏目录；未修改文件登记")?;
    let prefix = crate::files::prefix(run_id);
    let mut paths = Vec::new();
    let mut total = 0u64;
    for (seq, path, size) in &rows {
        let name = recording_name(*seq);
        ensure!(recording_seq(&name) == Some(*seq) && path.strip_prefix(&prefix) == Some(name.as_str()), "录屏登记路径不合法");
        let target = dir.join(name);
        match fs::symlink_metadata(&target).await {
            Ok(meta) => {
                ensure!(meta.is_file() && meta.len() == u64::try_from(*size)?, "录屏不是常规文件或大小与登记不符");
                paths.push(target);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        total = total.saturating_add(u64::try_from(*size)?);
    }
    for path in paths {
        match fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sqlx::query("delete from files where run_id = $1 and kind = 'recording'")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((rows.len() as i64, total))
}
