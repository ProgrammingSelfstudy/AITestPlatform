//! P3 脚本归档：回归脚本的版本、来源和状态。
//!
//! 只负责存储和状态流转，**不负责生成和静态校验**（那在 `agent::scriptgen` / `scriptcheck`，服务端不依赖它）。
//! 因此入库前服务端再做一遍不依赖模型的硬检查：来源执行必须是 Judge 整体通过、每条预期都有通过项，
//! 用例版本必须没变，文件名必须落在白名单内，文件摘要必须和内容一致。
//! 脚本内容是否含凭证由生成端的静态校验保证，这里不重复判断，也不声称判断过。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// 与 `agent::scriptgen` 的上限保持一致。
const MAX_FILES: usize = 6;
const MAX_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptStatus {
    /// 刚生成、静态校验通过，还没自检
    Candidate,
    /// 在受控环境自检通过
    SelfCheckPassed,
    /// 获准：平台可以选这个版本来回归
    Approved,
    Rejected,
    /// 获准后下线
    Retired,
}

impl ScriptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::SelfCheckPassed => "self_check_passed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Retired => "retired",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "candidate" => Self::Candidate,
            "self_check_passed" => Self::SelfCheckPassed,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "retired" => Self::Retired,
            _ => return None,
        })
    }

    /// 只能顺着「候选 → 自检通过 → 获准 → 下线」走，随时可拒绝（获准后只能下线）；拒绝和下线是终点。
    /// 不允许跳过自检直接获准。
    fn can_move_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Candidate, Self::SelfCheckPassed)
                | (Self::Candidate, Self::Rejected)
                | (Self::SelfCheckPassed, Self::Approved)
                | (Self::SelfCheckPassed, Self::Rejected)
                | (Self::Approved, Self::Retired)
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("用例不存在")]
    CaseNotFound,
    #[error("脚本文件不合规：{0}")]
    InvalidFiles(String),
    #[error("文件摘要和内容不一致：{0}")]
    HashMismatch(String),
    #[error("来源执行不符合要求：{0}")]
    SourceRun(String),
    #[error("同样内容已归档为版本 {version}")]
    Duplicate { id: Uuid, version: i32 },
    #[error("脚本不存在")]
    NotFound,
    #[error("状态不能从 {} 改为 {}", from.as_str(), to.as_str())]
    BadTransition { from: ScriptStatus, to: ScriptStatus },
    #[error("同一用例已有获准的版本 {version}，请先下线它再获准新版本")]
    AnotherApproved { version: i32 },
    #[error("这个脚本依据的是用例版本 {script}，用例现在是版本 {current}，用例内容变过，旧脚本不能获准")]
    CaseChanged { script: i32, current: i32 },
    #[error("数据库错误：{0}")]
    Db(#[from] sqlx::Error),
}

/// 待归档的候选。`file_sha256` 是生成端给出的单文件摘要，服务端会重算比对。
#[derive(Debug, Clone)]
pub struct NewScript {
    pub case_id: Uuid,
    pub source_run_id: Uuid,
    pub files: BTreeMap<String, String>,
    pub file_sha256: BTreeMap<String, String>,
    pub notes: String,
    /// 生成时用的模型来源：`claude_cli` / `codex_exec`
    pub gen_source: String,
    pub gen_model: Option<String>,
    pub gen_effort: Option<String>,
}

/// 脚本记录。不含源码，源码用 [`load_files`] 单独取，列表接口不会把整套脚本带出去。
#[derive(Debug, Clone, Serialize)]
pub struct ScriptRecord {
    pub id: Uuid,
    pub case_id: Uuid,
    pub version: i32,
    pub case_version: i32,
    pub source_run_id: Uuid,
    pub file_names: Vec<String>,
    pub content_sha256: String,
    pub gen_source: String,
    pub gen_model: Option<String>,
    pub gen_effort: Option<String>,
    pub notes: String,
    pub status: ScriptStatus,
    pub status_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 查询列清单。用宏而不是运行时拼接，所有 SQL 在编译期就是静态字符串。
macro_rules! columns {
    () => {
        "id, case_id, version, case_version, source_run_id, \
         (select coalesce(array_agg(k order by k), '{}') from jsonb_object_keys(files) k) as file_names, \
         content_sha256, gen_source, gen_model, gen_effort, notes, status, status_note, created_at, updated_at"
    };
}

fn record(row: &PgRow) -> Result<ScriptRecord, sqlx::Error> {
    let status: String = row.try_get("status")?;
    Ok(ScriptRecord {
        id: row.try_get("id")?,
        case_id: row.try_get("case_id")?,
        version: row.try_get("version")?,
        case_version: row.try_get("case_version")?,
        source_run_id: row.try_get("source_run_id")?,
        file_names: row.try_get("file_names")?,
        content_sha256: row.try_get("content_sha256")?,
        gen_source: row.try_get("gen_source")?,
        gen_model: row.try_get("gen_model")?,
        gen_effort: row.try_get("gen_effort")?,
        notes: row.try_get("notes")?,
        status: ScriptStatus::parse(&status).ok_or_else(|| sqlx::Error::Decode(format!("未知的脚本状态：{status}").into()))?,
        status_note: row.try_get("status_note")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// 用例编号转成脚本文件名里的 key：去掉连字符、转小写，只允许小写字母、数字、下划线。与 `scriptcheck::case_key` 一致。
pub fn case_key(code: &str) -> Option<String> {
    let key = code.replace('-', "").to_ascii_lowercase();
    (!key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')).then_some(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 每个文件按原始 UTF-8 内容算 SHA-256，与生成端的 `sha256` 字段同一算法。
pub fn file_digests(files: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    files.iter().map(|(name, source)| (name.clone(), hex(&Sha256::digest(source.as_bytes())))).collect()
}

/// 整套脚本的内容摘要：按路径排序，逐个文件把「路径 NUL 单文件摘要 换行」连起来再取 SHA-256。
/// 路径里不会有 NUL 和换行（文件名白名单保证），所以拼接没有歧义。
pub fn content_digest(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (name, digest) in file_digests(files) {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

/// 文件名白名单：必须有 `test_<key>.py`，可以再有 `pages/<key>_pages.py`，别的一律不收。
fn check_files(case_code: &str, files: &BTreeMap<String, String>) -> Result<(), ScriptError> {
    let bad = |msg: String| Err(ScriptError::InvalidFiles(msg));
    let Some(key) = case_key(case_code) else {
        return bad("用例编号不能转换成脚本文件名".into());
    };
    if files.is_empty() || files.len() > MAX_FILES {
        return bad(format!("文件数必须在 1 到 {MAX_FILES} 之间"));
    }
    let test_name = format!("test_{key}.py");
    let page_name = format!("pages/{key}_pages.py");
    if !files.contains_key(&test_name) {
        return bad(format!("缺少 {test_name}"));
    }
    for (name, source) in files {
        if *name != test_name && *name != page_name {
            return bad(format!("只接收 {test_name} 和 {page_name}，不接收 {name}"));
        }
        if source.trim().is_empty() {
            return bad(format!("{name} 是空的"));
        }
        if source.len() > MAX_FILE_BYTES {
            return bad(format!("{name} 超过 {} KiB", MAX_FILE_BYTES / 1024));
        }
        if source.contains('\0') {
            return bad(format!("{name} 含有 NUL 字符"));
        }
    }
    Ok(())
}

/// 归档一个候选脚本。整个过程在一个事务里，先锁住用例行，所以并发归档同一用例时版本号不会重复。
pub async fn create_candidate(pool: &PgPool, new: &NewScript) -> Result<ScriptRecord, ScriptError> {
    let mut tx = pool.begin().await?;

    let case = sqlx::query("select code, version from test_cases where id = $1 for update").bind(new.case_id).fetch_optional(&mut *tx).await?;
    let Some(case) = case else { return Err(ScriptError::CaseNotFound) };
    let code: String = case.try_get("code")?;
    let case_version: i32 = case.try_get("version")?;

    check_files(&code, &new.files)?;
    let actual = file_digests(&new.files);
    if actual != new.file_sha256 {
        let wrong: Vec<&str> = actual.keys().chain(new.file_sha256.keys()).filter(|name| actual.get(*name) != new.file_sha256.get(*name)).map(String::as_str).collect();
        return Err(ScriptError::HashMismatch(wrong.join("、")));
    }

    // 来源执行：同一用例、用例版本没变、Judge 已完成且整体通过、每条预期都有通过项。
    let run = sqlx::query("select case_id, case_version, final_result, judge_status, case_spec -> 'expected' as expected from runs where id = $1")
        .bind(new.source_run_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(run) = run else { return Err(ScriptError::SourceRun("执行记录不存在".into())) };
    if run.try_get::<Uuid, _>("case_id")? != new.case_id {
        return Err(ScriptError::SourceRun("这次执行不是这个用例的".into()));
    }
    let run_case_version: i32 = run.try_get("case_version")?;
    if run_case_version != case_version {
        return Err(ScriptError::SourceRun(format!("这次执行用的是用例版本 {run_case_version}，当前是 {case_version}，旧版本的执行不能生成新脚本")));
    }
    if run.try_get::<Option<String>, _>("judge_status")?.as_deref() != Some("completed") {
        return Err(ScriptError::SourceRun("Judge 没有完成".into()));
    }
    if run.try_get::<Option<String>, _>("final_result")?.as_deref() != Some("passed") {
        return Err(ScriptError::SourceRun("最终结果不是通过".into()));
    }
    let expected: serde_json::Value = run.try_get("expected")?;
    let expected_idx: Vec<i64> = expected.as_array().map(|items| items.iter().filter_map(|item| item["idx"].as_i64()).collect()).unwrap_or_default();
    if expected_idx.is_empty() {
        return Err(ScriptError::SourceRun("这次执行没有预期条目".into()));
    }
    let passed: Vec<i32> = sqlx::query_scalar("select idx from judge_items where run_id = $1 and verdict = 'passed'").bind(new.source_run_id).fetch_all(&mut *tx).await?;
    if let Some(missing) = expected_idx.iter().find(|idx| !passed.iter().any(|p| i64::from(*p) == **idx)) {
        return Err(ScriptError::SourceRun(format!("预期 {missing} 没有通过项，不能据此生成脚本")));
    }

    let digest = content_digest(&new.files);
    if let Some(dup) = sqlx::query("select id, version from scripts where case_id = $1 and content_sha256 = $2").bind(new.case_id).bind(&digest).fetch_optional(&mut *tx).await? {
        return Err(ScriptError::Duplicate { id: dup.try_get("id")?, version: dup.try_get("version")? });
    }
    let version: i32 = sqlx::query_scalar("select coalesce(max(version), 0) + 1 from scripts where case_id = $1").bind(new.case_id).fetch_one(&mut *tx).await?;

    let row = sqlx::query(concat!(
        "insert into scripts (case_id, version, case_version, source_run_id, files, content_sha256, gen_source, gen_model, gen_effort, notes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning ",
        columns!()
    ))
        .bind(new.case_id)
        .bind(version)
        .bind(case_version)
        .bind(new.source_run_id)
        .bind(serde_json::to_value(&new.files).expect("字符串表一定能序列化"))
        .bind(&digest)
        .bind(&new.gen_source)
        .bind(&new.gen_model)
        .bind(&new.gen_effort)
        .bind(&new.notes)
        .fetch_one(&mut *tx)
        .await?;
    let created = record(&row)?;
    tx.commit().await?;
    Ok(created)
}

/// 改状态。只允许 [`ScriptStatus::can_move_to`] 规定的路径；`note` 记录原因（如自检失败的输出摘要、拒绝理由）。
pub async fn transition(pool: &PgPool, id: Uuid, to: ScriptStatus, note: Option<&str>) -> Result<ScriptRecord, ScriptError> {
    let mut tx = pool.begin().await?;
    let current: Option<(String, Uuid, i32)> = sqlx::query_as("select status, case_id, case_version from scripts where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let Some((current, case_id, case_version)) = current else { return Err(ScriptError::NotFound) };
    let from = ScriptStatus::parse(&current).ok_or_else(|| sqlx::Error::Decode(format!("未知的脚本状态：{current}").into()))?;
    if !from.can_move_to(to) {
        return Err(ScriptError::BadTransition { from, to });
    }
    if to == ScriptStatus::Approved {
        // 锁住用例行：两个脚本同时获准时串行化；库里还有唯一索引兜底。
        let live: i32 = sqlx::query_scalar("select version from test_cases where id = $1 for update").bind(case_id).fetch_one(&mut *tx).await?;
        if live != case_version {
            return Err(ScriptError::CaseChanged { script: case_version, current: live });
        }
        let other: Option<i32> = sqlx::query_scalar("select version from scripts where case_id = $1 and status = 'approved' and id <> $2").bind(case_id).bind(id).fetch_optional(&mut *tx).await?;
        if let Some(version) = other {
            return Err(ScriptError::AnotherApproved { version });
        }
    }
    let row = sqlx::query(concat!("update scripts set status = $2, status_note = $3, updated_at = clock_timestamp() where id = $1 returning ", columns!()))
        .bind(id).bind(to.as_str()).bind(note).fetch_one(&mut *tx).await?;
    let updated = record(&row)?;
    tx.commit().await?;
    Ok(updated)
}

/// 状态说明的字数上限（库里的约束是 2000）。
pub const NOTE_LIMIT: usize = 2000;
/// 人给的原因最多这么长；留出空间给之前的自检输出。
pub const REASON_LIMIT: usize = 300;

/// 在原有说明（如自检输出）后面追加这次人的决定，总长不超过 [`NOTE_LIMIT`]；放不下时截掉原说明的开头，保留末尾。
pub fn append_note(old: Option<&str>, label: &str, reason: &str) -> String {
    let tail = format!("\n—— {label}：{reason}");
    let room = NOTE_LIMIT.saturating_sub(tail.chars().count());
    let old = old.unwrap_or("").trim();
    let count = old.chars().count();
    let kept: String = if count > room { format!("…{}", old.chars().skip(count - room + 1).collect::<String>()) } else { old.to_string() };
    if kept.is_empty() { tail.trim_start().to_string() } else { format!("{kept}{tail}") }
}

/// 一个项目里所有用例的全部脚本版本，按用例编号、版本从新到旧。带用例编号和标题，给页面直接展示。
pub async fn list_by_project(pool: &PgPool, project: &str) -> Result<Vec<(String, String, ScriptRecord)>, sqlx::Error> {
    // 列清单里的列名会和 test_cases 的同名列冲突，所以把脚本表单独包成子查询
    sqlx::query(concat!(
        "select c.code as case_code, c.title as case_title, s.* from (select ", columns!(), " from scripts) s ",
        "join test_cases c on c.id = s.case_id join projects p on p.id = c.project_id ",
        "where p.name = $1 order by c.code, s.version desc"
    ))
    .bind(project)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| Ok((row.try_get("case_code")?, row.try_get("case_title")?, record(row)?)))
    .collect()
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<ScriptRecord>, sqlx::Error> {
    sqlx::query(concat!("select ", columns!(), " from scripts where id = $1")).bind(id).fetch_optional(pool).await?.map(|row| record(&row)).transpose()
}

/// 一个用例的全部脚本版本，新的在前。
pub async fn list_by_case(pool: &PgPool, case_id: Uuid) -> Result<Vec<ScriptRecord>, sqlx::Error> {
    sqlx::query(concat!("select ", columns!(), " from scripts where case_id = $1 order by version desc")).bind(case_id).fetch_all(pool).await?.iter().map(record).collect()
}

/// 取脚本源码（执行节点回归时用）。
pub async fn load_files(pool: &PgPool, id: Uuid) -> Result<Option<BTreeMap<String, String>>, sqlx::Error> {
    let files: Option<serde_json::Value> = sqlx::query_scalar("select files from scripts where id = $1").bind(id).fetch_optional(pool).await?;
    Ok(files.map(|value| serde_json::from_value(value).expect("库里的 files 约束为字符串表")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn 追加说明_短的原样保留_长的截掉开头保留末尾和原因() {
        assert_eq!(append_note(Some("1 passed"), "获准", "可以"), "1 passed\n—— 获准：可以");
        assert_eq!(append_note(None, "下线", "改版"), "—— 下线：改版".to_string());
        assert_eq!(append_note(Some("  "), "下线", "改版"), "—— 下线：改版".to_string());
        let long = format!("{}尾巴", "头".repeat(5000));
        let note = append_note(Some(&long), "获准", &"因".repeat(300));
        assert!(note.chars().count() <= NOTE_LIMIT, "{}", note.chars().count());
        assert!(note.starts_with('…') && note.contains("尾巴\n—— 获准：") && note.ends_with(&"因".repeat(300)));
    }

    #[test]
    fn 编号转文件名_去连字符转小写() {
        assert_eq!(case_key("TC-04").as_deref(), Some("tc04"));
        assert_eq!(case_key("TC-06b").as_deref(), Some("tc06b"));
        assert_eq!(case_key("TC 04"), None, "空格不行");
        assert_eq!(case_key("../x"), None);
        assert_eq!(case_key(""), None);
    }

    #[test]
    fn 文件名白名单() {
        let ok = files(&[("test_tc04.py", "def test_x(): pass\n"), ("pages/tc04_pages.py", "class P: pass\n")]);
        assert!(check_files("TC-04", &ok).is_ok());
        assert!(check_files("TC-04", &files(&[("test_tc04.py", "x = 1\n")])).is_ok(), "页面文件可选");
        assert!(check_files("TC-04", &files(&[("pages/tc04_pages.py", "x = 1\n")])).is_err(), "必须有测试文件");
        for bad in ["../test_tc04.py", "/etc/passwd", "test_tc05.py", "conftest.py", "pages/other_pages.py", "sub/test_tc04.py"] {
            let all = files(&[("test_tc04.py", "x = 1\n"), (bad, "x = 1\n")]);
            assert!(check_files("TC-04", &all).is_err(), "{bad} 必须拒绝");
        }
    }

    #[test]
    fn 文件大小和内容限制() {
        assert!(check_files("TC-04", &files(&[("test_tc04.py", "   \n")])).is_err(), "空文件");
        assert!(check_files("TC-04", &files(&[("test_tc04.py", "a\0b")])).is_err(), "NUL");
        assert!(check_files("TC-04", &files(&[("test_tc04.py", &"x".repeat(MAX_FILE_BYTES + 1))])).is_err(), "超长");
        assert!(check_files("TC-04", &files(&[("test_tc04.py", &"x".repeat(MAX_FILE_BYTES))])).is_ok(), "正好上限");
        assert!(check_files("TC-04", &BTreeMap::new()).is_err(), "没有文件");
    }

    #[test]
    fn 摘要稳定且对内容和路径敏感() {
        let a = files(&[("test_tc04.py", "x = 1\n")]);
        assert_eq!(content_digest(&a), content_digest(&a.clone()), "同样内容摘要一致");
        assert_ne!(content_digest(&a), content_digest(&files(&[("test_tc04.py", "x = 2\n")])), "内容变了摘要要变");
        assert_ne!(content_digest(&files(&[("a", "x")])), content_digest(&files(&[("b", "x")])), "路径变了摘要也要变");
        assert_eq!(content_digest(&a).len(), 64);
        // 单文件摘要必须是生成端同一算法：空串的 SHA-256
        assert_eq!(file_digests(&files(&[("f", "")]))["f"], "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn 状态只能顺着走() {
        use ScriptStatus::*;
        assert!(Candidate.can_move_to(SelfCheckPassed));
        assert!(SelfCheckPassed.can_move_to(Approved));
        assert!(Approved.can_move_to(Retired));
        assert!(!Candidate.can_move_to(Approved), "不能跳过自检直接获准");
        assert!(!Rejected.can_move_to(Approved), "拒绝是终点");
        assert!(!Retired.can_move_to(Approved), "下线是终点");
        assert!(!Approved.can_move_to(Rejected), "获准后只能下线，不能再「拒绝」");
        assert!(!Approved.can_move_to(Candidate), "不能回退");
    }
}
