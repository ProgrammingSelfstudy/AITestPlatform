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

use crate::api::{ApiState, require_browser_user};
use crate::regress_requests::{self, RequestError};
use crate::scripts::{self, NewScript, ScriptError, ScriptRecord};
use crate::triage::{self, TriageError};

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
        Err(e @ (ScriptError::NotFound | ScriptError::BadTransition { .. } | ScriptError::AnotherApproved { .. } | ScriptError::CaseChanged { .. } | ScriptError::NotApproved(_))) => {
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
    let row = sqlx::query("select s.status, s.status_note, s.version, s.files, c.code from scripts s join test_cases c on c.id = s.case_id where s.id = $1")
        .bind(script_id)
        .fetch_one(state.app.pool())
        .await
        .map_err(|e| internal(&e))?;
    Ok(Json(serde_json::json!({
        "id": script_id,
        "version": row.try_get::<i32, _>("version").map_err(|e| internal(&e))?,
        "status": row.try_get::<String, _>("status").map_err(|e| internal(&e))?,
        "status_note": row.try_get::<Option<String>, _>("status_note").map_err(|e| internal(&e))?,
        "case_code": row.try_get::<String, _>("code").map_err(|e| internal(&e))?,
        "files": row.try_get::<serde_json::Value, _>("files").map_err(|e| internal(&e))?,
    })))
}

/// 自检输出：库里状态说明上限 2000，要给之后获准或下线的原因留出空间。
const MAX_DETAIL_CHARS: usize = 1800;

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

/// 人的决定。原因是人写的，不是模型或脚本输出。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Decision {
    #[serde(default)]
    note: String,
}

/// 人工改状态：获准、下线、拒绝。认证用审核令牌（和创建任务、提交审核同一套，路由层校验），不是节点令牌：
/// 「哪个版本能被平台拿去回归」是人的决定，节点不能替人决定。
/// 状态机由 [`scripts::transition`] 保证：不能跳过自检直接获准，同一用例只能有一个获准版本，用例内容变过的旧脚本不能获准。
async fn decide(state: ApiState, id: String, to: scripts::ScriptStatus, label: &str, reason_required: bool, body: Decision) -> Result<Json<ScriptRecord>, Failure> {
    let script_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个脚本"))?;
    let reason = body.note.trim();
    if reason.chars().count() > scripts::REASON_LIMIT {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "原因太长，最多 300 字"));
    }
    if reason_required && reason.is_empty() {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, &format!("{label}要写明原因")));
    }
    let pool = state.app.pool();
    let old = scripts::get(pool, script_id).await.map_err(|e| internal(&e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "没有这个脚本"))?;
    let note = scripts::append_note(old.status_note.as_deref(), label, if reason.is_empty() { "（未写原因）" } else { reason });
    match scripts::transition(pool, script_id, to, Some(&note)).await {
        Ok(record) => Ok(Json(record)),
        Err(e @ (ScriptError::BadTransition { .. } | ScriptError::AnotherApproved { .. } | ScriptError::CaseChanged { .. })) => Err((StatusCode::CONFLICT, e.to_string())),
        Err(ScriptError::NotFound) => Err(fail(StatusCode::NOT_FOUND, "没有这个脚本")),
        Err(ScriptError::Db(e)) => Err(internal(&e)),
        Err(e) => {
            tracing::error!("改脚本状态时出现不该出现的错误：{e}");
            Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "操作失败"))
        }
    }
}

/// `POST /api/scripts/{id}/approve`：获准，只有「自检通过」的脚本可以。原因可不写。
pub(crate) async fn approve(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, Json(body): Json<Decision>) -> Result<Json<ScriptRecord>, Failure> {
    decide(state, id, scripts::ScriptStatus::Approved, "获准", false, body).await
}

/// `POST /api/scripts/{id}/retire`：下线已获准的脚本。要写明原因。
pub(crate) async fn retire(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, Json(body): Json<Decision>) -> Result<Json<ScriptRecord>, Failure> {
    decide(state, id, scripts::ScriptStatus::Retired, "下线", true, body).await
}

/// `POST /api/scripts/{id}/reject`：人工拒绝候选或自检通过但不想用的脚本。要写明原因。
pub(crate) async fn reject(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, Json(body): Json<Decision>) -> Result<Json<ScriptRecord>, Failure> {
    decide(state, id, scripts::ScriptStatus::Rejected, "拒绝", true, body).await
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

/// 页面用的一行：脚本元数据加所属用例的编号和标题。
#[derive(serde::Serialize)]
pub(crate) struct ProjectScript {
    case_code: String,
    case_title: String,
    #[serde(flatten)]
    script: ScriptRecord,
    /// 最近一次回归，没跑过就是 null
    last_regression: Option<scripts::Regression>,
    /// 最近一次从页面发起的回归请求（排队、运行中、已结束），没有就是 null
    last_request: Option<regress_requests::RequestView>,
    /// 最近一次自愈分诊，没有就是 null；只有它的 regression_id 等于 last_regression.id 才是针对最近这次失败的
    last_triage: Option<triage::TriageView>,
}

/// `GET /api/projects/{project}/scripts`：项目里所有用例的全部脚本版本，只有元数据，不含源码。
pub(crate) async fn list_project(State(state): State<ApiState>, RoutePath(project): RoutePath<String>) -> Result<Json<Vec<ProjectScript>>, Failure> {
    let rows = scripts::list_by_project(state.app.pool(), &project).await.map_err(|e| internal(&e))?;
    let ids: Vec<Uuid> = rows.iter().map(|(_, _, script)| script.id).collect();
    let mut latest = scripts::latest_regressions(state.app.pool(), &ids).await.map_err(|e| internal(&e))?;
    let mut requests = regress_requests::latest(state.app.pool(), &ids).await.map_err(|e| internal(&e))?;
    let mut triages = triage::latest(state.app.pool(), &ids).await.map_err(|e| internal(&e))?;
    Ok(Json(rows.into_iter().map(|(case_code, case_title, script)| ProjectScript { last_regression: latest.remove(&script.id), last_request: requests.remove(&script.id), last_triage: triages.remove(&script.id), case_code, case_title, script }).collect()))
}

/// `GET /api/projects/{project}/cases/{code}/approved-script`：节点取某用例当前已获准的脚本版本和源码，去本机回归。
/// 没有获准版本返回 404：没有人批准过的脚本不能被拿去回归。
pub(crate) async fn approved(State(state): State<ApiState>, RoutePath((project, code)): RoutePath<(String, String)>, request: Request) -> Result<Json<serde_json::Value>, Failure> {
    crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let found = scripts::approved_for_case(state.app.pool(), &project, &code).await.map_err(|e| internal(&e))?;
    let (id, version, files) = found.ok_or_else(|| fail(StatusCode::NOT_FOUND, "这个用例没有已获准的脚本"))?;
    Ok(Json(serde_json::json!({"id": id, "version": version, "case_code": code, "files": files})))
}

/// 节点回报的一次回归结果。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegressionReport {
    serial: String,
    passed: bool,
    detail: String,
    duration_ms: u32,
}

/// `PUT /api/scripts/{id}/regressions`：节点回报一次回归。只有已获准的版本能记；脚本不必来自这个节点
/// （获准的脚本是项目资产，任何认证节点都可以拿去在自己的设备上回归），但结果记在回报的节点名下。
pub(crate) async fn regression(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<StatusCode, Failure> {
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let script_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个脚本"))?;
    let bytes = to_bytes(request.into_body(), 64 * 1024).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let report: RegressionReport = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    if report.detail.chars().count() > MAX_DETAIL_CHARS || report.serial.trim().is_empty() || report.serial.chars().count() > 128 {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "设备序列号为空，或输出太长（最多 1800 字）"));
    }
    let duration = i32::try_from(report.duration_ms).map_err(|_| fail(StatusCode::UNPROCESSABLE_ENTITY, "耗时不合理"))?;
    match scripts::record_regression(state.app.pool(), script_id, node_id, &report.serial, report.passed, &report.detail, duration).await {
        Ok(()) => Ok(StatusCode::CREATED),
        Err(ScriptError::NotFound) => Err(fail(StatusCode::NOT_FOUND, "没有这个脚本")),
        Err(e @ ScriptError::NotApproved(_)) => Err((StatusCode::CONFLICT, e.to_string())),
        Err(ScriptError::Db(e)) => Err(internal(&e)),
        Err(e) => {
            tracing::error!("记录回归时出现不该出现的错误：{e}");
            Err(fail(StatusCode::INTERNAL_SERVER_ERROR, "记录失败"))
        }
    }
}

/// `GET /api/scripts/{id}/heal-source`：节点取自愈要用的材料。只能取来源于自己节点的脚本（自愈要用本节点的会员额度和本地轨迹）。
/// 只有已获准的版本才有可分诊的失败；没有失败（最近一次回归通过或没回归过）时 `regression` 为 null。
pub(crate) async fn heal_source(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<Json<serde_json::Value>, Failure> {
    let (script_id, _) = own_script(&state, request.headers(), &id).await?;
    let row = sqlx::query("select s.status, s.version, s.files, s.source_run_id, c.code from scripts s join test_cases c on c.id = s.case_id where s.id = $1")
        .bind(script_id)
        .fetch_one(state.app.pool())
        .await
        .map_err(|e| internal(&e))?;
    let regression = triage::latest_failure(state.app.pool(), script_id).await.map_err(|e| internal(&e))?;
    // 来源执行之后用例内容变过：旧执行不能再据此归档新版本（归档时会被拒绝），节点要在花额度之前就知道
    let case_changed: bool = sqlx::query_scalar(
        "select r.case_version <> c.version from scripts s join runs r on r.id = s.source_run_id join test_cases c on c.id = s.case_id where s.id = $1",
    )
    .bind(script_id)
    .fetch_one(state.app.pool())
    .await
    .map_err(|e| internal(&e))?;
    Ok(Json(serde_json::json!({
        "id": script_id,
        "status": row.try_get::<String, _>("status").map_err(|e| internal(&e))?,
        "version": row.try_get::<i32, _>("version").map_err(|e| internal(&e))?,
        "case_code": row.try_get::<String, _>("code").map_err(|e| internal(&e))?,
        "source_run_id": row.try_get::<Uuid, _>("source_run_id").map_err(|e| internal(&e))?,
        "files": row.try_get::<serde_json::Value, _>("files").map_err(|e| internal(&e))?,
        "case_changed": case_changed,
        "regression": regression,
    })))
}

/// 节点提交的分诊。拒绝未知字段。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TriageBody {
    regression_id: Uuid,
    verdict: String,
    reason: String,
    /// 只有 locator_broken 带这些
    files: Option<BTreeMap<String, String>>,
    sha256: Option<BTreeMap<String, String>>,
    #[serde(default)]
    notes: String,
    source: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// `PUT /api/scripts/{id}/triage`：节点回报分诊结论。「定位失效」带修复文件，归档成新的候选（记下来源版本）；
/// 其余结论不改任何脚本。同一次失败只能分诊一次。
pub(crate) async fn submit_triage(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<(StatusCode, Json<serde_json::Value>), Failure> {
    let (script_id, node_id) = own_script(&state, request.headers(), &id).await?;
    let bytes = to_bytes(request.into_body(), MAX_BODY).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let body: TriageBody = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    let verdict = triage::Verdict::parse(&body.verdict).ok_or_else(|| fail(StatusCode::UNPROCESSABLE_ENTITY, "分诊结论只能是 locator_broken、business_bug 或 unsure"))?;
    if body.notes.chars().count() > MAX_NOTES_CHARS {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "备注太长"));
    }
    let fix = match (body.files, body.sha256) {
        (None, None) => None,
        (Some(files), Some(file_sha256)) => {
            let source = body.source.unwrap_or_default();
            if !matches!(source.as_str(), "claude_cli" | "codex_exec") {
                return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "模型来源只能是 claude_cli 或 codex_exec"));
            }
            Some(triage::Fix { files, file_sha256, notes: body.notes, gen_source: source, gen_model: body.model, gen_effort: body.effort })
        }
        _ => return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "修复文件和它的摘要要一起给")),
    };
    let submission = triage::Submission { regression_id: body.regression_id, verdict, reason: body.reason, fix };
    match triage::submit(state.app.pool(), node_id, script_id, submission).await {
        Ok(healed) => Ok((StatusCode::CREATED, Json(serde_json::json!({"healed": healed})))),
        Err(e @ (TriageError::Invalid(_) | TriageError::Candidate(_))) => Err((StatusCode::UNPROCESSABLE_ENTITY, e.to_string())),
        Err(e @ (TriageError::NotApproved(_) | TriageError::NoFailure | TriageError::Stale | TriageError::AlreadyTriaged)) => Err((StatusCode::CONFLICT, e.to_string())),
        Err(TriageError::NotFound) => Err(fail(StatusCode::NOT_FOUND, "没有这个脚本")),
        Err(TriageError::Db(e)) => Err(internal(&e)),
    }
}

/// 页面发起回归的请求体。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegressRequest {
    node_id: Uuid,
}

/// 回归只能用发起人自己的节点和手机（规则 9），所以需要确认发起人和节点属主。
async fn require_owner(state: &ApiState, headers: &axum::http::HeaderMap, node_id: Uuid) -> Result<(), Failure> {
    let user = require_browser_user(state, headers).await?;
    match crate::users::check_node_owner(state.app.pool(), node_id, user).await.map_err(|e| internal(&e))? {
        crate::users::NodeOwner::Owned => Ok(()),
        crate::users::NodeOwner::Missing => Err(fail(StatusCode::NOT_FOUND, "没有这个节点")),
        crate::users::NodeOwner::NotYours => Err(fail(StatusCode::FORBIDDEN, "只能使用自己的节点发起回归")),
    }
}

fn request_failure(error: RequestError) -> Failure {
    match error {
        RequestError::ScriptNotFound | RequestError::NotFound => (StatusCode::NOT_FOUND, error.to_string()),
        RequestError::NotApproved(_) | RequestError::Duplicate | RequestError::Conflict(_) => (StatusCode::CONFLICT, error.to_string()),
        RequestError::NotYours => (StatusCode::FORBIDDEN, error.to_string()),
        RequestError::Db(e) => internal(&e),
    }
}

/// `POST /api/scripts/{id}/regress`：在自己的节点上排一次回归。只有已获准的版本能回归；不花模型额度，但会占用节点的手机。
/// 请求只是排队，由节点自己领取、在自己的手机上跑，结果通过 `regressions` 接口记录。
pub(crate) async fn request_regression(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, headers: axum::http::HeaderMap, Json(body): Json<RegressRequest>) -> Result<(StatusCode, Json<serde_json::Value>), Failure> {
    require_owner(&state, &headers, body.node_id).await?;
    let script_id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个脚本"))?;
    let request_id = regress_requests::create(state.app.pool(), script_id, body.node_id).await.map_err(request_failure)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": request_id}))))
}

/// `POST /api/regression-requests/{id}/cancel`：取消还在排队的请求。节点已经领走（手机在跑）的不能取消。
pub(crate) async fn cancel_request(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, headers: axum::http::HeaderMap) -> Result<StatusCode, Failure> {
    let id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个回归请求"))?;
    let node = regress_requests::node_of(state.app.pool(), id).await.map_err(|e| internal(&e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "没有这个回归请求"))?;
    require_owner(&state, &headers, node).await?;
    regress_requests::cancel(state.app.pool(), id).await.map_err(request_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    serial: String,
}

/// `PUT /api/node/regression-requests/claim`：节点带着它要用的设备来领一个回归请求。
/// 有就返回 200 和要回归的脚本；没有，或设备正忙，返回 204。
pub(crate) async fn claim_request(State(state): State<ApiState>, request: Request) -> Result<axum::response::Response, Failure> {
    use axum::response::IntoResponse;
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let bytes = to_bytes(request.into_body(), 4096).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let body: ClaimBody = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    if body.serial.trim().is_empty() || body.serial.chars().count() > 128 {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "设备序列号为空或太长"));
    }
    match regress_requests::claim(state.app.pool(), node_id, &body.serial).await.map_err(|e| internal(&e))? {
        Some(claimed) => Ok(Json(claimed).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `PUT /api/node/regression-requests/abandon`：节点进程刚启动时调用：它自己没有在跑回归，
/// 之前领走而没回报的请求都作废，免得一直占着设备。
pub(crate) async fn abandon_requests(State(state): State<ApiState>, request: Request) -> Result<StatusCode, Failure> {
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let bytes = to_bytes(request.into_body(), 4096).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let body: ClaimBody = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    let mut conn = state.app.pool().acquire().await.map_err(|e| internal(&e))?;
    regress_requests::fail_running(&mut conn, Some(node_id), Some(&body.serial), "节点进程重启，这次回归的结果没有回报").await.map_err(|e| internal(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishBody {
    /// 得到了回归结论（通过或失败都算，结论记在回归记录里）；否则是没能得到结论
    finished: bool,
    detail: String,
}

/// `PUT /api/regression-requests/{id}/result`：节点回报这个请求的结果。只有领走它的节点、且仍在运行中才能回报。
pub(crate) async fn finish_request(State(state): State<ApiState>, RoutePath(id): RoutePath<String>, request: Request) -> Result<StatusCode, Failure> {
    let node_id = crate::node_identity(&state.app, request.headers()).await.map_err(|status| (status, String::new()))?;
    let id: Uuid = id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "没有这个回归请求"))?;
    let bytes = to_bytes(request.into_body(), 64 * 1024).await.map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "请求体太大"))?;
    let body: FinishBody = serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "请求格式不对"))?;
    if body.detail.chars().count() > MAX_DETAIL_CHARS {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "说明太长（最多 1800 字）"));
    }
    regress_requests::finish(state.app.pool(), node_id, id, body.finished, &body.detail).await.map_err(request_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 原始数据库错误只写日志，不返回给调用方。
fn internal(error: &sqlx::Error) -> Failure {
    tracing::error!("脚本归档接口查询失败：{error}");
    fail(StatusCode::INTERNAL_SERVER_ERROR, "查询失败")
}
