//! 自愈分诊：获准脚本回归失败后，节点上的模型判断是「定位失效」「业务行为错误」还是「分不清」。
//!
//! 服务端不信任这个结论本身，只守住不能被绕过的几条：
//! - 只有已获准的脚本、只针对**最近一次失败的回归**，且同一次失败只分诊一次（不重复花额度）。
//! - 只有「定位失效」能带修复文件，修复只产出**候选**（走和普通候选一样的归档硬检查，记下来源版本）；
//!   它不会替换获准版本，也不会改原脚本的状态，仍要自检、人工获准。
//! - 「业务行为错误」和「分不清」不动任何脚本：失败的回归记录和原版本都保留，由人处理。

use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::scripts::{self, NewScript, ScriptError, ScriptRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    LocatorBroken,
    BusinessBug,
    Unsure,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocatorBroken => "locator_broken",
            Self::BusinessBug => "business_bug",
            Self::Unsure => "unsure",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "locator_broken" => Some(Self::LocatorBroken),
            "business_bug" => Some(Self::BusinessBug),
            "unsure" => Some(Self::Unsure),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TriageError {
    #[error("没有这个脚本")]
    NotFound,
    #[error("只有已获准的脚本版本能分诊，这个版本现在是「{0}」")]
    NotApproved(String),
    #[error("这个脚本还没有回归失败的记录，不需要分诊")]
    NoFailure,
    #[error("要分诊的不是最近一次回归（之后又回归过），请重新取最新的失败")]
    Stale,
    #[error("这次失败已经分诊过了")]
    AlreadyTriaged,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("修复候选没有通过归档检查：{0}")]
    Candidate(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// 节点要分诊的失败：最近一次回归。
#[derive(Debug, Serialize)]
pub struct Failure {
    pub regression_id: Uuid,
    pub passed: bool,
    pub detail: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub triaged: bool,
}

/// 页面显示用：某个脚本最近一次分诊。
#[derive(Debug, Serialize)]
pub struct TriageView {
    pub regression_id: Uuid,
    pub verdict: Verdict,
    pub reason: String,
    pub healed_script_id: Option<Uuid>,
    pub healed_version: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 脚本最近一次回归，以及它是否已分诊。没回归过返回 `None`。
pub async fn latest_failure(pool: &PgPool, script_id: Uuid) -> Result<Option<Failure>, sqlx::Error> {
    let row = sqlx::query(
        "select r.id, r.passed, r.detail, r.created_at, exists (select 1 from script_triages t where t.regression_id = r.id) as triaged
         from script_regressions r where r.script_id = $1 order by r.created_at desc, r.id limit 1",
    )
    .bind(script_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| Ok(Failure { regression_id: row.try_get("id")?, passed: row.try_get("passed")?, detail: row.try_get("detail")?, created_at: row.try_get("created_at")?, triaged: row.try_get("triaged")? })).transpose()
}

/// 节点提交分诊结论。`fix` 只在「定位失效」时给，是修复后的文件（归档时用来源执行补全）。
pub struct Submission {
    pub regression_id: Uuid,
    pub verdict: Verdict,
    pub reason: String,
    pub fix: Option<Fix>,
}

pub struct Fix {
    pub files: std::collections::BTreeMap<String, String>,
    pub file_sha256: std::collections::BTreeMap<String, String>,
    pub notes: String,
    pub gen_source: String,
    pub gen_model: Option<String>,
    pub gen_effort: Option<String>,
}

/// 记一次分诊；「定位失效」同时在同一个事务里归档修复候选。返回修复候选（如果有）。
pub async fn submit(pool: &PgPool, node_id: Uuid, script_id: Uuid, submission: Submission) -> Result<Option<ScriptRecord>, TriageError> {
    let reason = submission.reason.trim();
    if reason.is_empty() || reason.chars().count() > 1000 {
        return Err(TriageError::Invalid("分诊原因必填，最多 1000 字"));
    }
    match (submission.verdict, submission.fix.is_some()) {
        (Verdict::LocatorBroken, false) => return Err(TriageError::Invalid("「定位失效」必须带修复后的脚本，没有就应该报「分不清」")),
        (Verdict::BusinessBug | Verdict::Unsure, true) => return Err(TriageError::Invalid("只有「定位失效」能带修复脚本，业务行为错误和分不清不改脚本")),
        _ => {}
    }

    let mut tx = pool.begin().await?;
    let parent = sqlx::query("select status, case_id, source_run_id from scripts where id = $1 for update").bind(script_id).fetch_optional(&mut *tx).await?;
    let parent = parent.ok_or(TriageError::NotFound)?;
    let status: String = parent.try_get("status")?;
    if status != "approved" {
        return Err(TriageError::NotApproved(status));
    }
    let latest: Option<(Uuid, bool)> = sqlx::query_as("select id, passed from script_regressions where script_id = $1 order by created_at desc, id limit 1").bind(script_id).fetch_optional(&mut *tx).await?;
    let Some((latest_id, passed)) = latest else { return Err(TriageError::NoFailure) };
    if passed {
        return Err(TriageError::NoFailure);
    }
    if latest_id != submission.regression_id {
        return Err(TriageError::Stale);
    }

    let healed = match submission.fix {
        Some(fix) => {
            let new = NewScript {
                case_id: parent.try_get("case_id")?,
                source_run_id: parent.try_get("source_run_id")?,
                files: fix.files,
                file_sha256: fix.file_sha256,
                notes: fix.notes,
                gen_source: fix.gen_source,
                gen_model: fix.gen_model,
                gen_effort: fix.gen_effort,
            };
            match scripts::create_in_tx(&mut tx, &new, Some(script_id)).await {
                Ok(record) => Some(record),
                Err(ScriptError::Duplicate { version, .. }) => return Err(TriageError::Candidate(format!("内容和版本 {version} 相同，没有实际修改"))),
                Err(ScriptError::Db(e)) => return Err(e.into()),
                Err(e) => return Err(TriageError::Candidate(e.to_string())),
            }
        }
        None => None,
    };
    let inserted = sqlx::query("insert into script_triages (script_id, regression_id, node_id, verdict, reason, healed_script_id) values ($1, $2, $3, $4, $5, $6)")
        .bind(script_id)
        .bind(latest_id)
        .bind(node_id)
        .bind(submission.verdict.as_str())
        .bind(reason)
        .bind(healed.as_ref().map(|h| h.id))
        .execute(&mut *tx)
        .await;
    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Err(TriageError::AlreadyTriaged),
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok(healed)
}

/// 每个脚本最近一次分诊，按脚本编号。
pub async fn latest(pool: &PgPool, script_ids: &[Uuid]) -> Result<std::collections::HashMap<Uuid, TriageView>, sqlx::Error> {
    let rows = sqlx::query(
        "select distinct on (t.script_id) t.script_id, t.regression_id, t.verdict, t.reason, t.healed_script_id, h.version as healed_version, t.created_at
         from script_triages t left join scripts h on h.id = t.healed_script_id
         where t.script_id = any($1) order by t.script_id, t.created_at desc, t.id",
    )
    .bind(script_ids)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let verdict: String = row.try_get("verdict")?;
            Ok((
                row.try_get("script_id")?,
                TriageView {
                    regression_id: row.try_get("regression_id")?,
                    verdict: Verdict::parse(&verdict).ok_or_else(|| sqlx::Error::Decode(format!("未知的分诊结论：{verdict}").into()))?,
                    reason: row.try_get("reason")?,
                    healed_script_id: row.try_get("healed_script_id")?,
                    healed_version: row.try_get("healed_version")?,
                    created_at: row.try_get("created_at")?,
                },
            ))
        })
        .collect()
}
