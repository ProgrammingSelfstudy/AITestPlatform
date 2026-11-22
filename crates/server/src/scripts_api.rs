//! P3 脚本归档的 HTTP 接口。
//!
//! 生成脚本要调模型、用的是发起人的会员额度，所以**只能在发起人自己的节点上生成**（规则 9），
//! 再由这个节点把候选上传到这里：认证用节点令牌，并且这次执行必须属于这个节点。
//! 和证据文件上传不同，这里**不限时间**：脚本可以在执行结束很久以后才生成。
//! 接口只收候选并归档，不碰模型；内容合规由生成端的静态校验负责，归档前的硬检查见 [`crate::scripts`]。

use std::collections::BTreeMap;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Path as RoutePath, Request, State};
use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::api::ApiState;
use crate::scripts::{self, NewScript, ScriptError, ScriptRecord};

/// 最多 6 个文件、每个 64 KiB，加上摘要和 JSON 开销，512 KiB 足够，超过就是异常请求。
const MAX_BODY: usize = 512 * 1024;
const MAX_NOTES_CHARS: usize = 4000;

type Failure = (StatusCode, String);

fn fail(status: StatusCode, message: &str) -> Failure {
    (status, message.to_string())
}

/// 节点上传的候选。拒绝未知字段：以后加字段必须显式改协议，不能被悄悄忽略。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    files: BTreeMap<String, String>,
    /// 生成端给的单文件摘要，服务端会重算比对
    sha256: BTreeMap<String, String>,
    #[serde(default)]
    notes: String,
    /// 生成时用的模型来源：`claude_cli` / `codex_exec`
    source: String,
    model: Option<String>,
    effort: Option<String>,
}

/// `PUT /api/runs/{id}/scripts`：节点为自己的一次执行上传候选脚本。
///
/// 新候选返回 201；同样内容重复上传返回 200 和已有记录（节点重试是安全的）；
/// 来源执行不合格、文件不合规、摘要对不上返回 422，正文是中文原因。
pub(crate) async fn put(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<(StatusCode, Json<ScriptRecord>), Failure> {
    // 先认证、先查归属，再读请求体：不让没有资格的调用方白白占用解析资源。
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let run_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这次执行"))?;
    let owner: Option<(Uuid, Uuid)> = sqlx::query_as("select node_id, case_id from runs where id = $1").bind(run_id).fetch_optional(state.app.pool()).await.map_err(|e| internal(&e))?;
    let (owner, case_id) = owner.ok_or_else(|| fail(StatusCode::NOT_FOUND, "没有这次执行"))?;
    if owner != node_id {
        return Err(fail(StatusCode::FORBIDDEN, "只能为自己节点上的执行归档脚本"));
    }

    let bytes = to_bytes(request.into_body(), MAX_BODY).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let upload: Upload = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    if !matches!(upload.source.as_str(), "claude_cli" | "codex_exec") {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "模型来源只能是 claude_cli 或 codex_exec"));
    }
    if upload.notes.chars().count() > MAX_NOTES_CHARS {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "备注太长"));
    }

    let new = NewScript {
        case_id,
        source_run_id: run_id,
        files: upload.files,
        file_sha256: upload.sha256,
        notes: upload.notes,
        gen_source: upload.source,
        gen_model: upload.model,
        gen_effort: upload.effort,
    };
    match scripts::create_candidate(state.app.pool(), &new).await {
        Ok(created) => Ok((StatusCode::CREATED, Json(created))),
        Err(ScriptError::Duplicate { id, .. }) => {
            let existing = scripts::get(state.app.pool(), id).await.map_err(|e| internal(&e))?;
            existing.map(|record| (StatusCode::OK, Json(record))).ok_or_else(|| fail(StatusCode::CONFLICT, "同样内容刚被删除，请重试"))
        }
        Err(e @ (ScriptError::InvalidFiles(_) | ScriptError::HashMismatch(_) | ScriptError::SourceRun(_))) => Err((StatusCode::UNPROCESSABLE_ENTITY, e.to_string())),
        Err(ScriptError::CaseNotFound) => Err(fail(StatusCode::NOT_FOUND, "用例不存在")),
        Err(ScriptError::Db(e)) => Err(internal(&e)),
        Err(e @ (ScriptError::NotFound | ScriptError::BadTransition { .. })) => {
            tracing::error!("归档候选时出现不该出现的错误：{e}");
            Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "归档失败"))
        }
    }
}

/// `GET /api/runs/{id}/script-source`：节点取生成脚本所需的来源，只能取自己节点的执行。
///
/// 页面用的 run 详情故意不返回用例的 `data` 和 `preconditions`，所以节点拼不出完整用例；
/// 这里给节点认证后的专用视图：完整用例快照、最终结果、逐条判定、这次任务里 Judge 的角色配置。
/// 用例 `data` 里只有 `{{account.xxx}}` 占位符，不含账号明文。是否满足「整体通过且每条预期都有通过项」
/// 这里不判断，由生成核心在调用模型**之前**判断，归档时服务端再判断一次。
pub(crate) async fn source(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<Json<serde_json::Value>, Failure> {
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let run_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这次执行"))?;
    let pool = state.app.pool();
    let row = sqlx::query(
        "select r.node_id, r.case_id, r.case_version, r.status, r.final_result, r.judge_status, r.case_spec, t.judge
         from runs r join tasks t on t.id = r.task_id where r.id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| internal(&e))?;
    let row = row.ok_or_else(|| fail(StatusCode::NOT_FOUND, "没有这次执行"))?;
    if row.try_get::<Uuid, _>("node_id").map_err(|e| internal(&e))? != node_id {
        return Err(fail(StatusCode::FORBIDDEN, "只能读取自己节点上的执行"));
    }
    let items = sqlx::query("select idx, verdict, evidence, file_refs from judge_items where run_id = $1 order by idx")
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(|e| internal(&e))?;
    let get = |name: &str| -> Result<serde_json::Value, Failure> {
        row.try_get::<Option<String>, _>(name).map(|v| serde_json::json!(v)).map_err(|e| internal(&e))
    };
    let judge_items = items
        .iter()
        .map(|item| {
            Ok(serde_json::json!({
                "idx": item.try_get::<i32, _>("idx").map_err(|e| internal(&e))?,
                "verdict": item.try_get::<String, _>("verdict").map_err(|e| internal(&e))?,
                "evidence": item.try_get::<String, _>("evidence").map_err(|e| internal(&e))?,
                "file_refs": item.try_get::<Vec<String>, _>("file_refs").map_err(|e| internal(&e))?,
            }))
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    Ok(Json(serde_json::json!({
        "run_id": run_id,
        "case_id": row.try_get::<Uuid, _>("case_id").map_err(|e| internal(&e))?,
        "case_version": row.try_get::<i32, _>("case_version").map_err(|e| internal(&e))?,
        "status": get("status")?,
        "final_result": get("final_result")?,
        "judge_status": get("judge_status")?,
        "case_spec": row.try_get::<serde_json::Value, _>("case_spec").map_err(|e| internal(&e))?,
        "judge_items": judge_items,
        "judge": row.try_get::<serde_json::Value, _>("judge").map_err(|e| internal(&e))?,
    })))
}

/// 认证节点并确认这条脚本来自本节点的执行：自检要占用本节点的手机，只能在脚本来源的节点上跑。
async fn own_script(state: &ApiState, headers: &axum::http::HeaderMap, id: &str) -> Result<(Uuid, Uuid), Failure> {
    let node_id = crate::node_identity(&state.app, headers).await.map_err(|status| (status, String::new()))?;
    let script_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个脚本"))?;
    let owner: Option<Uuid> = sqlx::query_scalar("select r.node_id from scripts s join runs r on r.id = s.source_run_id where s.id = $1")
        .bind(script_id)
        .fetch_optional(state.app.pool())
        .await
        .map_err(|e| internal(&e))?;
    match owner {
        None => Err(fail(StatusCode::NOT_FOUND, "没有这个脚本")),
        Some(owner) if owner != node_id => Err(fail(StatusCode::FORBIDDEN, "只能操作来源于自己节点的脚本")),
        Some(_) => Ok((script_id, node_id)),
    }
}

/// `GET /api/scripts/{id}/files`：节点取脚本源码和用例编号，用来在本机自检。只能取自己节点来源的脚本。
pub(crate) async fn files(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<Json<serde_json::Value>, Failure> {
    let (script_id, _) = own_script(&state, request.headers(), &id).await?;
    let row = sqlx::query("select s.status, s.version, s.files, c.code from scripts s join test_cases c on c.id = s.case_id where s.id = $1")
        .bind(script_id)
        .fetch_one(state.app.pool())
        .await
        .map_err(|e| internal(&e))?;
    Ok(Json(serde_json::json!({
        "id": script_id,
        "version": row.try_get::<i32, _>("version").map_err(|e| internal(&e))?,
        "status": row.try_get::<String, _>("status").map_err(|e| internal(&e))?,
        "case_code": row.try_get::<String, _>("code").map_err(|e| internal(&e))?,
        "files": row.try_get::<serde_json::Value, _>("files").map_err(|e| internal(&e))?,
    })))
}

const MAX_DETAIL_CHARS: usize = 4000;

/// 节点回报的自检结果。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfCheck {
    passed: bool,
    /// 通过时是简要说明，失败时是原始失败输出的摘要（规则 7：原样给人看）
    detail: String,
}

/// `PUT /api/scripts/{id}/self-check`：节点认证的写接口统一用 PUT（浏览器写保护会拦掉非浏览器的 POST）。
///节点回报自检结果。候选通过变为「自检通过」，失败变为「已拒绝」，
/// 都记下原因。只有候选能自检，其他状态返回 409。「获准」不在这里：那是人的决定。
pub(crate) async fn self_check(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<Json<ScriptRecord>, Failure> {
    let (script_id, _) = own_script(&state, request.headers(), &id).await?;
    let bytes = to_bytes(request.into_body(), 64 * 1024).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let report: SelfCheck = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    if report.detail.chars().count() > MAX_DETAIL_CHARS {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "说明太长"));
    }
    let to = if report.passed { scripts::ScriptStatus::SelfCheckPassed } else { scripts::ScriptStatus::Rejected };
    match scripts::transition(state.app.pool(), script_id, to, Some(&report.detail)).await {
        Ok(record) => Ok(Json(record)),
        Err(e @ ScriptError::BadTransition { .. }) => Err((StatusCode::CONFLICT, e.to_string())),
        Err(ScriptError::NotFound) => Err(fail(StatusCode::NOT_FOUND, "没有这个脚本")),
        Err(ScriptError::Db(e)) => Err(internal(&e)),
        Err(e) => {
            tracing::error!("回报自检时出现不该出现的错误：{e}");
            Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "回报失败"))
        }
    }
}

/// `GET /api/cases/{id}/scripts`：一个用例的全部脚本版本，新的在前。只有元数据，不含源码。
pub(crate) async fn list(State(state): State<ApiState>, RoutePath(id): RoutePath<String>) -> Result<Json<Vec<ScriptRecord>>, Failure> {
    let case_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个用例"))?;
    let exists: Option<i32> = sqlx::query_scalar("select 1 from test_cases where id = $1").bind(case_id).fetch_optional(state.app.pool()).await.map_err(|e| internal(&e))?;
    if exists.is_none() {
        return Err(fail(StatusCode::NOT_FOUND, "没有这个用例"));
    }
    scripts::list_by_case(state.app.pool(), case_id).await.map(Json).map_err(|e| internal(&e))
}

/// 原始数据库错误只写日志，不返回给调用方。
fn internal(error: &sqlx::Error) -> Failure {
    tracing::error!("脚本归档接口查询失败：{error}");
    fail(StatusCode::INTERNAL_SERVER_ERROR, "查询失败")
}
